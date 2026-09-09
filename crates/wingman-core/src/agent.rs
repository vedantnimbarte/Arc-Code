//! The agent loop.
//!
//! `AgentLoop::run` drives a single user turn to completion: it calls the
//! provider, accumulates streamed text and tool-use blocks, dispatches tools
//! via the [`ToolDispatcher`], and re-invokes the provider until the model
//! emits `Stop::EndTurn` (or we hit `max_turns`).
//!
//! Output is a single stream of `AgentEvent`s. UIs (TUI, headless printer,
//! JSON logger) consume the same stream.

use crate::{
    tokens::{CompactPlan, Compactor, ToolOutputBudget, ToolResultPruner},
    CacheBreakpoint, CompletionRequest, ContentBlock, Message, Provider, ReasoningEffort, Role,
    StopReason, StreamEvent, ToolSpec, Usage,
};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Abstraction over the registry that actually runs tools. Lives here so
/// `AgentLoop` doesn't have to depend on `wingman-tools` (which depends on
/// us).
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    /// The tools whose schemas go into every request — what the model can
    /// call directly, and what it is billed for on every turn.
    fn specs(&self) -> Vec<ToolSpec>;
    /// Every registered tool, including any withheld from [`specs`] to keep
    /// the per-request cost down.
    ///
    /// Defaults to `specs()`, so a dispatcher that withholds nothing — which
    /// is most of them, and every one in the test suite — implements nothing.
    /// `tool_search` is the only caller: it is how a deferred tool becomes
    /// discoverable without its schema being resident.
    ///
    /// [`specs`]: ToolDispatcher::specs
    fn all_specs(&self) -> Vec<ToolSpec> {
        self.specs()
    }
    /// Just the tools withheld from [`specs`] — what `tool_search` searches.
    ///
    /// Separate from `all_specs() - specs()` on purpose. Computing it as a
    /// difference means asking for `specs()`, which asks every registered tool
    /// for its schema, including `tool_search` itself — whose schema embeds
    /// this very list. That recurses forever. A dispatcher that defers nothing
    /// returns nothing, which is the correct answer for it.
    ///
    /// [`specs`]: ToolDispatcher::specs
    fn deferred_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    /// Run a single tool call. Stringify any structured output before
    /// returning — the model sees a string.
    async fn dispatch(&self, name: &str, args: serde_json::Value) -> ToolOutcome;
}

/// Hook the agent loop calls at three well-known points so a side-channel
/// crate (`wingman-learn`) can implement the self-improvement loop without
/// `wingman-core` depending on it.
///
/// The default impl is a no-op so existing callers that don't supply a hook
/// pay nothing.
#[async_trait]
pub trait LearningHook: Send + Sync {
    /// Called once before the per-turn provider request. May return extra
    /// system text to splice onto `AgentConfig::system` for this turn only
    /// (memory recall, nudges, ephemeral skill injection, index retrieval).
    /// Async so implementations can hit I/O (e.g. a RAG search) without
    /// blocking the loop's runtime.
    async fn before_turn(&self, _history: &[Message]) -> Option<String> {
        None
    }
    /// Called after each assistant turn completes (tool round trip done).
    fn after_turn(&self, _history: &[Message]) {}
    /// Called once when the loop yields its final Stop event for a user
    /// turn. Use this to flush stats, kick off background indexing, etc.
    fn after_stop(&self, _history: &[Message]) {}
}

/// No-op default — used when the caller doesn't supply a hook.
pub struct NoopLearningHook;
#[async_trait]
impl LearningHook for NoopLearningHook {}

/// Result of one post-edit verification run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub passed: bool,
    /// Human-readable receipt: command, status, tail of output.
    pub summary: String,
}

/// Post-edit verification gate. When configured on [`AgentConfig`], the loop
/// runs it before accepting an `EndTurn` stop for any user turn in which a
/// mutating tool executed. A failing report is fed back to the model (bounded
/// by `gate_max_retries`) so it can self-correct instead of claiming "done"
/// with broken code.
#[async_trait]
pub trait TurnGate: Send + Sync {
    /// Short label shown in receipts (typically the command line).
    fn label(&self) -> String;
    async fn check(&self) -> GateReport;
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Streaming text from the assistant.
    TextDelta { text: String },
    /// Streaming reasoning from the assistant. Separate from `TextDelta` so a
    /// UI can fold it away — it is the model's working-out, not its answer.
    ThinkingDelta { text: String },
    /// A tool call about to execute.
    ToolStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a tool call.
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    /// Usage update (cumulative for the current turn).
    Usage { usage: Usage },
    /// A single provider response finished (one turn-step).
    TurnComplete,
    /// The whole user-turn finished.
    Stop { reason: AgentStop },
    /// Result of the post-edit verification gate. Emitted before `Stop`
    /// whenever a gate is configured and mutating tools ran this user turn.
    Verification { passed: bool, summary: String },
    /// A steer was folded into the running turn, so a UI can show it in the
    /// transcript where it actually landed rather than where it was typed.
    Steered { text: String },
    /// Recoverable error surfaced to the UI.
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStop {
    EndTurn,
    MaxTurns,
    MaxTokens,
    Error,
    /// The turn ended with the verification gate still red, after the
    /// retry budget (`gate_max_retries`) was spent. Distinct from
    /// `EndTurn` so callers — CI in particular — can tell "finished"
    /// from "gave up on a failing build".
    GateFailed,
    /// The turn was stopped by [`LoopGuard`](crate::loopguard::LoopGuard):
    /// the model kept making the same tool call and was not going to stop on
    /// its own. Distinct from `MaxTurns` because the budget was not spent —
    /// it was about to be spent on nothing.
    LoopDetected,
}

/// Construction-time options for the loop.
#[derive(Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_turns: usize,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// Cache after `system` + tools by default. Empty disables explicit caching.
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    /// Roll a cache breakpoint onto the last message each turn so the growing
    /// conversation prefix is cached and subsequent turns read it instead of
    /// re-billing every prior turn. Providers without explicit cache control
    /// ignore it. On by default; only pays off across multi-turn loops.
    pub cache_conversation: bool,
    /// Truncate large tool outputs before feeding them back to the model.
    pub tool_output_budget: ToolOutputBudget,
    /// Where to keep the full text of a truncated tool result, so the model
    /// can read the elided middle instead of losing it. `None` disables
    /// spilling and truncation stays one-way.
    pub spill: Option<Arc<crate::spill::SpillStore>>,
    /// Shrink oversized tool results in older turns before compaction folds
    /// whole turns away. Runs only under token pressure.
    pub pruner: ToolResultPruner,
    /// Where to record what the model actually saw. The loop emits a
    /// [`ContextFact`](crate::ContextFact) at every point it changes the
    /// context; `None` records nothing.
    pub context_sink: Option<Arc<dyn crate::ContextSink>>,
    /// Compaction policy. Compaction runs **before** each request if the
    /// estimated context size crosses `compactor.trigger_tokens`.
    pub compactor: Compactor,
    /// Optional learning hook. Called at before_turn / after_turn /
    /// after_stop; lets `wingman-learn` inject memory + nudges into the
    /// system prompt and track skill usage outcomes.
    pub learning: Option<Arc<dyn LearningHook>>,
    /// Optional post-edit verification gate (see [`TurnGate`]).
    pub gate: Option<Arc<dyn TurnGate>>,
    /// Gate failures fed back to the model before stopping anyway.
    pub gate_max_retries: usize,
    /// Tool names that count as "mutating" for gate purposes. A successful
    /// call to any of these arms the gate for the rest of the user turn.
    pub mutating_tools: Vec<String>,
    /// How hard the model should think before answering. `Off` by default;
    /// providers with no reasoning control ignore it.
    pub reasoning: ReasoningEffort,
    /// Tool names safe to dispatch concurrently. When *every* call in a turn's
    /// batch is on this list they run together; any other name and the whole
    /// batch falls back to sequential dispatch.
    ///
    /// Allowlist rather than "everything that isn't mutating": `ask_user` and
    /// `present_plan` mutate nothing but must not race, and MCP tools are
    /// deliberately absent because a stdio server that mishandles concurrent
    /// requests is a bug we cannot reproduce from here.
    pub parallel_safe_tools: Vec<String>,
    /// Ceiling on repeated tool calls. The tools layer nudges the model about
    /// consecutive repeats; this one counts occurrences in a rolling window,
    /// so it also sees alternating cycles, and it ends the turn rather than
    /// only advising. See [`LoopGuard`](crate::loopguard::LoopGuard).
    pub loop_guard: crate::loopguard::LoopGuard,
    /// Where a UI leaves guidance for a turn that is already running. Drained
    /// between provider round-trips. `None` means steering is unavailable and
    /// the caller should send an ordinary prompt instead.
    pub steer: Option<Arc<crate::steer::SteerInbox>>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model", &self.model)
            .field("system", &self.system)
            .field("max_turns", &self.max_turns)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("cache_breakpoints", &self.cache_breakpoints)
            .field("cache_conversation", &self.cache_conversation)
            .field("tool_output_budget", &self.tool_output_budget)
            .field("compactor", &self.compactor)
            .field("learning", &self.learning.as_ref().map(|_| "<hook>"))
            .field("gate", &self.gate.as_ref().map(|g| g.label()))
            .field("gate_max_retries", &self.gate_max_retries)
            .field("reasoning", &self.reasoning)
            .field("mutating_tools", &self.mutating_tools)
            .field("parallel_safe_tools", &self.parallel_safe_tools)
            .field("loop_guard", &self.loop_guard)
            .field("steer", &self.steer.as_ref().map(|_| "<inbox>"))
            .finish()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: None,
            max_turns: 16,
            max_tokens: 4096,
            temperature: None,
            cache_breakpoints: vec![CacheBreakpoint::AfterSystem, CacheBreakpoint::AfterTools],
            cache_conversation: true,
            tool_output_budget: ToolOutputBudget::default(),
            spill: None,
            pruner: ToolResultPruner::default(),
            context_sink: None,
            compactor: Compactor::default(),
            learning: None,
            gate: None,
            gate_max_retries: 2,
            reasoning: ReasoningEffort::Off,
            // Any tool that can change the working tree belongs here, or the
            // turn ends unverified. `lsp_rename` / `lsp_code_action` write via
            // the language server's WorkspaceEdit, which is exactly the kind of
            // sweeping multi-file change the gate exists to catch.
            mutating_tools: vec![
                "write_file".into(),
                "edit_file".into(),
                "apply_patch".into(),
                "edit_symbol".into(),
                "run_shell".into(),
                "lsp_rename".into(),
                "lsp_code_action".into(),
            ],
            // Pure reads with no shared mutable state between them. The model
            // routinely emits four of these in one batch; serialising them is
            // wall-clock spent for nothing.
            parallel_safe_tools: vec![
                "read_file".into(),
                "grep".into(),
                "glob".into(),
                "list_dir".into(),
                "outline".into(),
                "find_symbol".into(),
                "who_calls".into(),
                "semantic_search".into(),
                "lsp_definition".into(),
                "lsp_references".into(),
                "lsp_hover".into(),
                "lsp_diagnostics".into(),
                "recall_memory".into(),
                "recall_session".into(),
                "read_session".into(),
            ],
            loop_guard: crate::loopguard::LoopGuard::default(),
            steer: None,
        }
    }
}

pub struct AgentLoop {
    provider: Arc<dyn Provider>,
    tools: Arc<dyn ToolDispatcher>,
    config: AgentConfig,
    /// Conversation history that persists across calls to `run`.
    history: Vec<Message>,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolDispatcher>,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            config,
            history: Vec::new(),
        }
    }

    /// Swap the tool dispatcher after construction.
    ///
    /// For front ends that need to sit between the agent and its tools: the
    /// ACP server wraps the registry so an editor can approve or decline
    /// individual calls and serve file reads from its own unsaved buffers.
    /// Building the agent and then decorating it beats threading a decorator
    /// through every `build_*` signature for one caller.
    pub fn with_dispatcher(mut self, tools: Arc<dyn ToolDispatcher>) -> Self {
        self.tools = tools;
        self
    }

    /// Construct an `AgentLoop` with pre-loaded conversation history, useful
    /// for resuming a previous session via session records.
    pub fn with_history(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolDispatcher>,
        config: AgentConfig,
        history: Vec<Message>,
    ) -> Self {
        Self {
            provider,
            tools,
            config,
            history,
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Replace the conversation history — used by the TUI `/resume` command to
    /// reload a saved session's messages into a live agent.
    /// Point the loop at a sink for [`ContextFact`](crate::ContextFact)s.
    ///
    /// Set after construction because surfaces open their session log once
    /// they know where it lives, which is generally after the agent exists.
    pub fn set_context_sink(&mut self, sink: Arc<dyn crate::ContextSink>) {
        self.config.context_sink = Some(sink);
    }

    pub fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// Force a compaction pass now, regardless of the token threshold: fold
    /// everything but the most recent messages into a single recap. Returns
    /// the number of messages folded, or `None` if history is too short.
    pub fn compact_now(&mut self) -> Option<usize> {
        let plan = self.config.compactor.plan_forced(&self.history)?;
        let replaced = plan.replaced;
        self.history
            .splice(0..replaced, std::iter::once(plan.recap));
        Some(replaced)
    }

    /// Estimated tokens currently held in history (plus the system prompt).
    pub fn context_tokens(&self) -> u32 {
        crate::tokens::estimate_history_tokens(&self.history, self.config.system.as_deref())
    }

    /// Swap in a different provider. Conversation history is preserved so
    /// the new model picks up mid-stream — providers translate `Message`s
    /// through their own adapter on the next request.
    pub fn swap_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.model = model.into();
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn set_temperature(&mut self, t: Option<f32>) {
        self.config.temperature = t;
    }

    pub fn get_temperature(&self) -> Option<f32> {
        self.config.temperature
    }

    pub fn set_max_tokens(&mut self, n: u32) {
        self.config.max_tokens = n;
    }

    pub fn get_max_tokens(&self) -> u32 {
        self.config.max_tokens
    }

    pub fn set_reasoning(&mut self, r: ReasoningEffort) {
        self.config.reasoning = r;
    }

    pub fn get_reasoning(&self) -> ReasoningEffort {
        self.config.reasoning
    }

    /// Whether the active provider has any reasoning control. False means a
    /// configured level is dropped on the way out, so callers can say so
    /// instead of letting the user think it took effect.
    pub fn provider_supports_reasoning(&self) -> bool {
        self.provider.capabilities().reasoning
    }

    /// The base system prompt this loop sends, before any per-turn injection.
    /// Handle on this loop's steer inbox, for a UI that wants to redirect a
    /// turn while it runs. `None` when steering was not configured.
    pub fn steer_handle(&self) -> Option<Arc<crate::steer::SteerInbox>> {
        self.config.steer.clone()
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.config.system.as_deref()
    }

    pub fn get_model(&self) -> &str {
        &self.config.model
    }

    /// Drive a single user turn to completion. The returned stream yields
    /// events live and terminates after a `Stop` event.
    pub fn run(&mut self, user_prompt: String) -> BoxStream<'_, AgentEvent> {
        self.history.push(Message::user_text(user_prompt.clone()));

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let history = &mut self.history;
        let sink = config.context_sink.clone();

        let stream = async_stream::stream! {
            // Record the prompt before anything else: a turn that dies
            // mid-request should still show what was asked.
            if let Some(s) = &sink {
                s.record(crate::ContextFact::UserMessage { text: user_prompt })
                    .await;
            }
            let specs = tools.specs();
            // Armed when a mutating tool succeeds this user turn; checked by
            // the verification gate before an EndTurn stop is accepted.
            let mut mutated = false;
            let mut gate_attempts: usize = 0;
            // Spans the whole user turn, not one provider round-trip: a loop
            // that alternates across turns is still a loop.
            let mut loop_guard = config.loop_guard.clone();
            for turn in 0..config.max_turns {
                // Fold in anything the user said while this turn was running.
                // Here rather than mid-stream: the history has to be
                // well-formed when the next request is composed, and this is
                // the point where it is.
                if let Some(inbox) = &config.steer {
                    for message in inbox.drain() {
                        let framed = crate::steer::format(&message);
                        if let Some(s) = &sink {
                            s.record(crate::ContextFact::UserMessage {
                                text: framed.clone(),
                            })
                            .await;
                        }
                        history.push(Message::user_text(framed));
                        yield AgentEvent::Steered { text: message };
                    }
                }

                // Under token pressure, shrink oversized tool results in older
                // turns *before* considering compaction. The tokens are mostly
                // in tool output while the value is mostly in what the
                // assistant concluded from it, so this often gets back under
                // budget without folding away any reasoning at all.
                if crate::tokens::estimate_history_tokens(history, config.system.as_deref())
                    >= config.compactor.trigger_tokens
                {
                    let pruned = config.pruner.prune_reporting(history);
                    if !pruned.is_empty() {
                        tracing::debug!(
                            target: "wingman::tokens",
                            pruned = pruned.len(),
                            "pruned oversized tool results before compaction"
                        );
                        if let Some(s) = &sink {
                            for (id, content) in pruned {
                                s.record(crate::ContextFact::ToolResultPruned { id, content })
                                    .await;
                            }
                        }
                    }
                }

                // Compaction pass — fold the oldest non-recap span into a single
                // recap message when we are still over the trigger budget.
                if let Some(CompactPlan { recap, replaced }) =
                    config.compactor.plan(history, config.system.as_deref())
                {
                    if let Some(s) = &sink {
                        s.record(crate::ContextFact::Compacted {
                            replaced,
                            recap: recap
                                .content
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("
        "),
                        })
                        .await;
                    }
                    history.splice(0..replaced, std::iter::once(recap));
                }

                // Allow the learning hook to splice extra system text on a
                // per-turn basis (memory index, nudges, ephemeral skill body).
                let system_for_turn = match (config.system.as_deref(), &config.learning) {
                    (base, Some(hook)) => match hook.before_turn(history).await {
                        Some(extra) if !extra.trim().is_empty() => {
                            if let Some(s) = &sink {
                                s.record(crate::ContextFact::SystemInjected {
                                    text: extra.clone(),
                                })
                                .await;
                            }
                            let mut s = String::new();
                            if let Some(b) = base {
                                s.push_str(b);
                                if !s.ends_with('\n') {
                                    s.push('\n');
                                }
                                s.push('\n');
                            }
                            s.push_str(&extra);
                            Some(s)
                        }
                        _ => base.map(str::to_string),
                    },
                    (base, None) => base.map(str::to_string),
                };

                // Roll a cache breakpoint onto the last message so this turn's
                // request caches the whole conversation prefix and the next
                // turn reads it. Recomputed per turn because `history` grows.
                let mut cache_breakpoints = config.cache_breakpoints.clone();
                if config.cache_conversation && !history.is_empty() {
                    cache_breakpoints.push(CacheBreakpoint::AfterMessage(history.len() - 1));
                }

                let req = CompletionRequest {
                    model: config.model.clone(),
                    system: system_for_turn,
                    messages: history.clone(),
                    tools: specs.clone(),
                    max_tokens: config.max_tokens,
                    temperature: config.temperature,
                    cache_breakpoints,
                    reasoning: config.reasoning,
                };

                let mut event_stream = match provider.complete(req).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield AgentEvent::Error { message: e.to_string() };
                        yield AgentEvent::Stop { reason: AgentStop::Error };
                        return;
                    }
                };

                let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
                let mut current_text = String::new();
                let mut stop_reason: StopReason = StopReason::EndTurn;

                while let Some(evt) = event_stream.next().await {
                    let evt = match evt {
                        Ok(e) => e,
                        Err(e) => {
                            yield AgentEvent::Error { message: e.to_string() };
                            yield AgentEvent::Stop { reason: AgentStop::Error };
                            return;
                        }
                    };
                    match evt {
                        StreamEvent::TextDelta { text } => {
                            current_text.push_str(&text);
                            yield AgentEvent::TextDelta { text };
                        }
                        StreamEvent::ThinkingDelta { text } => {
                            yield AgentEvent::ThinkingDelta { text };
                        }
                        StreamEvent::Thinking { block } => {
                            // Reasoning precedes the answer, so anything already
                            // buffered as text belongs after it. In practice
                            // `current_text` is empty here; flushing anyway keeps
                            // block order faithful if a provider interleaves.
                            if !current_text.is_empty() {
                                assistant_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                            }
                            // Pushed verbatim, signature intact: Anthropic
                            // rejects the next tool-use request if this block
                            // comes back altered or missing.
                            assistant_blocks.push(block);
                        }
                        StreamEvent::ToolUse { block } => {
                            // Flush any pending text into its own block.
                            if !current_text.is_empty() {
                                assistant_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                            }
                            if let ContentBlock::ToolUse { id, name, input } = &block {
                                yield AgentEvent::ToolStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                };
                            }
                            assistant_blocks.push(block);
                        }
                        StreamEvent::Usage { usage } => {
                            if let Some(s) = &sink {
                                s.record(crate::ContextFact::Usage { usage }).await;
                            }
                            yield AgentEvent::Usage { usage };
                        }
                        StreamEvent::Stop { reason } => {
                            stop_reason = reason;
                        }
                    }
                }

                if !current_text.is_empty() {
                    assistant_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                }

                // Persist the assistant turn.
                if !assistant_blocks.is_empty() {
                    if let Some(s) = &sink {
                        s.record(crate::ContextFact::AssistantMessage {
                            blocks: assistant_blocks.clone(),
                        })
                        .await;
                    }
                    history.push(Message {
                        role: Role::Assistant,
                        content: assistant_blocks.clone(),
                    });
                }

                if let Some(hook) = &config.learning {
                    hook.after_turn(history);
                }

                yield AgentEvent::TurnComplete;

                // Decide whether to continue.
                let tool_calls: Vec<(String, String, serde_json::Value)> = assistant_blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();

                // If the model said end_turn but emitted tool calls anyway,
                // run them and keep going — this is a provider quirk we
                // observed with some non-Anthropic backends.
                let stop_now = match stop_reason {
                    StopReason::MaxTokens => Some(AgentStop::MaxTokens),
                    _ if tool_calls.is_empty() => Some(AgentStop::EndTurn),
                    _ => None,
                };

                if let Some(mut reason) = stop_now {
                    // Post-edit verification: when mutating tools ran this
                    // user turn, the gate must pass before an EndTurn stop is
                    // accepted. Failures are fed back to the model (bounded by
                    // gate_max_retries) so it self-corrects instead of
                    // claiming "done" with broken code.
                    if reason == AgentStop::EndTurn && mutated {
                        if let Some(gate) = &config.gate {
                            let report = gate.check().await;
                            yield AgentEvent::Verification {
                                passed: report.passed,
                                summary: report.summary.clone(),
                            };
                            if !report.passed
                                && gate_attempts < config.gate_max_retries
                                && turn + 1 < config.max_turns
                            {
                                gate_attempts += 1;
                                let feedback = format!(
                                    "[wingman verify] Turn gate failed after your edits \
                                     ({}). Fix the issues, then end the turn again.\n\n{}",
                                    gate.label(),
                                    report.summary,
                                );
                                if let Some(s) = &sink {
                                    s.record(crate::ContextFact::UserMessage {
                                        text: feedback.clone(),
                                    })
                                    .await;
                                }
                                history.push(Message::user_text(feedback));
                                continue;
                            }
                            // Retries exhausted (or disallowed) and still red:
                            // report it as such rather than as a clean finish.
                            if !report.passed {
                                reason = AgentStop::GateFailed;
                            }
                        }
                    }
                    if let Some(hook) = &config.learning {
                        hook.after_stop(history);
                    }
                    if let Some(s) = &sink {
                        s.record(crate::ContextFact::Stop {
                            reason: serde_json::to_string(&reason)
                                .unwrap_or_else(|_| "\"unknown\"".into()),
                        })
                        .await;
                    }
                    yield AgentEvent::Stop { reason };
                    return;
                }

                // Dispatch tools and append their results as a user-role message.
                //
                // A batch of pure reads (the model routinely emits four
                // `read_file`s at once) runs concurrently; anything else runs
                // one at a time, because an edit or a shell command must see
                // the workspace the call before it left behind.
                //
                // Eligibility checks *both* lists: requiring absence from
                // `mutating_tools` means adding a name to `parallel_safe_tools`
                // by mistake cannot silently race a write.
                // Repetition ceiling. Judge the whole batch before dispatching any
                // of it: a call that has already tripped the guard should not
                // run one more time just because it shared a batch.
                let mut advisories: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                let mut loop_abort: Option<String> = None;
                for (id, name, input) in &tool_calls {
                    match loop_guard.record(name, input) {
                        crate::loopguard::Verdict::Fine => {}
                        crate::loopguard::Verdict::Warn(msg) => {
                            advisories.insert(id.clone(), msg);
                        }
                        crate::loopguard::Verdict::Abort(msg) => {
                            loop_abort = Some(msg);
                            break;
                        }
                    }
                }

                if let Some(reason) = loop_abort {
                    // Every `tool_use` block must be answered by a matching
                    // `tool_result` or the next request is malformed — so the
                    // calls are refused explicitly rather than dropped. The
                    // history stays replayable and a `/rewind` or a fork of
                    // this session shows why the turn ended.
                    let refusal = format!("[wingman] Not dispatched: {reason}");
                    let mut refused: Vec<ContentBlock> = Vec::with_capacity(tool_calls.len());
                    for (id, _, _) in &tool_calls {
                        if let Some(s) = &sink {
                            s.record(crate::ContextFact::ToolResult {
                                id: id.clone(),
                                full: refusal.clone(),
                                model_view: None,
                                is_error: true,
                            })
                            .await;
                        }
                        yield AgentEvent::ToolResult {
                            id: id.clone(),
                            output: refusal.clone(),
                            is_error: true,
                        };
                        refused.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: refusal.clone(),
                            is_error: true,
                        });
                    }
                    history.push(Message::tool_results(refused));
                    if let Some(hook) = &config.learning {
                        hook.after_stop(history);
                    }
                    if let Some(s) = &sink {
                        s.record(crate::ContextFact::Stop {
                            reason: "loop_detected".into(),
                        })
                        .await;
                    }
                    yield AgentEvent::Error { message: reason };
                    yield AgentEvent::Stop { reason: AgentStop::LoopDetected };
                    return;
                }

                let count = tool_calls.len();
                let concurrency = if tool_calls.iter().all(|(_, name, _)| {
                    config.parallel_safe_tools.iter().any(|t| t == name)
                        && !config.mutating_tools.iter().any(|t| t == name)
                }) {
                    count.max(1)
                } else {
                    1
                };

                // `buffered` preserves input order, so results reach the UI and
                // the history in the order the model asked for them regardless
                // of which finished first.
                let mut dispatched = futures::stream::iter(tool_calls)
                    .map(|(id, name, input)| {
                        let tools = tools.clone();
                        async move {
                            // Always dispatch fresh. A per-turn result cache was
                            // removed because a cached read (`read_file`,
                            // `run_shell`) goes stale the moment a later tool
                            // mutates the workspace — which silently fed the
                            // model old output and defeated the post-edit
                            // verification loop.
                            let outcome = tools.dispatch(&name, input).await;
                            (id, name, outcome)
                        }
                    })
                    .buffered(concurrency);

                let mut results: Vec<ContentBlock> = Vec::with_capacity(count);
                while let Some((id, name, outcome)) = dispatched.next().await {
                    if !outcome.is_error && config.mutating_tools.iter().any(|t| t == &name) {
                        mutated = true;
                    }
                    let mut truncated = config.tool_output_budget.trim(&outcome.content);
                    // Truncation happened: the middle is now unreachable to the
                    // model. Write the full text somewhere it can read, and say
                    // where, so the cap stays a budget rather than a loss.
                    // Best-effort — a failed spill just leaves plain truncation.
                    if config.tool_output_budget.would_trim(&outcome.content) {
                        if let Some(path) = config
                            .spill
                            .as_ref()
                            .and_then(|s| s.save(&name, &outcome.content))
                        {
                            let lines = outcome.content.lines().count();
                            truncated = format!(
                                "{}\n{truncated}",
                                crate::spill::locator_line(&path, lines)
                            );
                        }
                    }
                    // A repetition warning rides on the result the call
                    // produced, where the model is already looking, rather
                    // than as a separate message it can skim past.
                    if let Some(msg) = advisories.get(&id) {
                        truncated = format!("{truncated}

{msg}");
                    }
                    // Record both forms. `full` is the audit trail and what a
                    // human is shown; `model_view` is what actually went into
                    // the request. Logging only the full text is how the log
                    // came to claim the model had seen more than it did.
                    if let Some(s) = &sink {
                        s.record(crate::ContextFact::ToolResult {
                            id: id.clone(),
                            full: outcome.content.clone(),
                            model_view: (truncated != outcome.content)
                                .then(|| truncated.clone()),
                            is_error: outcome.is_error,
                        })
                        .await;
                    }
                    // UIs see the *full* output so the user can scroll/copy;
                    // the *model* only sees the truncated version below.
                    yield AgentEvent::ToolResult {
                        id: id.clone(),
                        output: outcome.content,
                        is_error: outcome.is_error,
                    };
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: truncated,
                        is_error: outcome.is_error,
                    });
                }
                drop(dispatched);
                history.push(Message::tool_results(results));

                if turn + 1 == config.max_turns {
                    if let Some(hook) = &config.learning {
                        hook.after_stop(history);
                    }
                    if let Some(s) = &sink {
                        s.record(crate::ContextFact::Stop {
                            reason: "max_turns".into(),
                        })
                        .await;
                    }
                    yield AgentEvent::Stop { reason: AgentStop::MaxTurns };
                    return;
                }
            }
        };

        Box::pin(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Provider that replays a scripted sequence of responses, one per
    /// `complete` call.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::None,
                reasoning: false,
            }
        }
        async fn complete(&self, _req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("provider called more times than scripted");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    struct OkDispatcher;
    #[async_trait]
    impl ToolDispatcher for OkDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _name: &str, _args: serde_json::Value) -> ToolOutcome {
            ToolOutcome::ok("ok")
        }
    }

    /// Provider that records the `cache_breakpoints` of every request it
    /// receives, so tests can assert what the loop asked to be cached.
    struct CapturingProvider {
        captured: Arc<Mutex<Vec<Vec<CacheBreakpoint>>>>,
        responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            "capturing"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::Explicit,
                reasoning: false,
            }
        }
        async fn complete(&self, req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            self.captured
                .lock()
                .unwrap()
                .push(req.cache_breakpoints.clone());
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("provider called more times than scripted");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    /// Gate that fails the first `fail_first` checks, then passes. Counts calls.
    struct CountingGate {
        fail_first: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TurnGate for CountingGate {
        fn label(&self) -> String {
            "test-gate".into()
        }
        async fn check(&self) -> GateReport {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            GateReport {
                passed: n >= self.fail_first,
                summary: format!("check #{}", n + 1),
            }
        }
    }

    fn tool_use_response() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUse {
                block: ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({"path": "x.rs", "content": "fn main() {}"}),
                },
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ]
    }

    fn end_turn_response(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]
    }

    async fn collect_events(agent: &mut AgentLoop) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let mut stream = agent.run("do something".into());
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    fn capturing_agent(
        responses: Vec<Vec<StreamEvent>>,
        captured: Arc<Mutex<Vec<Vec<CacheBreakpoint>>>>,
        cfg: AgentConfig,
    ) -> AgentLoop {
        let provider = Arc::new(CapturingProvider {
            captured,
            responses: Mutex::new(responses.into()),
        });
        AgentLoop::new(provider, Arc::new(OkDispatcher), cfg)
    }

    #[tokio::test]
    async fn rolling_conversation_cache_breakpoint_is_injected() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut agent = capturing_agent(
            vec![end_turn_response("hi")],
            captured.clone(),
            AgentConfig {
                model: "m".into(),
                ..Default::default()
            },
        );
        let _ = collect_events(&mut agent).await;
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        // At request time history is just the one user message (index 0).
        assert!(
            reqs[0].contains(&CacheBreakpoint::AfterMessage(0)),
            "rolling breakpoint missing: {:?}",
            reqs[0]
        );
        // Default system/tools breakpoints are preserved alongside it.
        assert!(reqs[0].contains(&CacheBreakpoint::AfterSystem));
    }

    #[tokio::test]
    async fn rolling_cache_breakpoint_absent_when_disabled() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut agent = capturing_agent(
            vec![end_turn_response("hi")],
            captured.clone(),
            AgentConfig {
                model: "m".into(),
                cache_conversation: false,
                ..Default::default()
            },
        );
        let _ = collect_events(&mut agent).await;
        let reqs = captured.lock().unwrap();
        assert!(
            !reqs[0]
                .iter()
                .any(|b| matches!(b, CacheBreakpoint::AfterMessage(_))),
            "breakpoint should be absent when disabled: {:?}",
            reqs[0]
        );
    }

    fn agent_with_gate(responses: Vec<Vec<StreamEvent>>, gate: Arc<CountingGate>) -> AgentLoop {
        AgentLoop::new(
            Arc::new(ScriptedProvider::new(responses)),
            Arc::new(OkDispatcher),
            AgentConfig {
                model: "scripted/test".into(),
                gate: Some(gate),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn gate_runs_once_and_passes_after_mutation() {
        let gate = Arc::new(CountingGate {
            fail_first: 0,
            calls: AtomicUsize::new(0),
        });
        let mut agent = agent_with_gate(
            vec![tool_use_response(), end_turn_response("done")],
            gate.clone(),
        );
        let events = collect_events(&mut agent).await;

        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Verification { passed: true, .. })));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn gate_failure_feeds_back_then_stop_on_pass() {
        let gate = Arc::new(CountingGate {
            fail_first: 1,
            calls: AtomicUsize::new(0),
        });
        let mut agent = agent_with_gate(
            vec![
                tool_use_response(),
                end_turn_response("done (broken)"),
                end_turn_response("done (fixed)"),
            ],
            gate.clone(),
        );
        let events = collect_events(&mut agent).await;

        assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
        let verifications: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Verification { passed, .. } => Some(*passed),
                _ => None,
            })
            .collect();
        assert_eq!(verifications, vec![false, true]);
        // The failure was fed back to the model as a user message.
        assert!(agent.history().iter().any(|m| {
            m.role == Role::User
                && m.content.iter().any(|b| {
                    matches!(
                        b,
                        ContentBlock::Text { text } if text.contains("[wingman verify]")
                    )
                })
        }));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn gate_retries_exhausted_stops_with_failing_receipt() {
        let gate = Arc::new(CountingGate {
            fail_first: usize::MAX,
            calls: AtomicUsize::new(0),
        });
        let mut agent = AgentLoop::new(
            Arc::new(ScriptedProvider::new(vec![
                tool_use_response(),
                end_turn_response("a"),
                end_turn_response("b"),
            ])),
            Arc::new(OkDispatcher),
            AgentConfig {
                model: "scripted/test".into(),
                gate: Some(gate.clone()),
                gate_max_retries: 1,
                ..Default::default()
            },
        );
        let events = collect_events(&mut agent).await;

        // One failure fed back, second failure exhausts the budget: the loop
        // stops, but it must NOT look like a clean finish. `GateFailed` is what
        // lets `wingman --print` exit non-zero in CI instead of reporting
        // success on code that doesn't build.
        assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Stop {
                    reason: AgentStop::GateFailed
                })
            ),
            "expected GateFailed, got {:?}",
            events.last()
        );
    }

    /// The converse: a gate that passes must still stop with a plain EndTurn,
    /// or every successful verified turn would fail CI.
    #[tokio::test]
    async fn passing_gate_still_stops_with_end_turn() {
        // Passes on the very first check.
        let gate = Arc::new(CountingGate {
            fail_first: 0,
            calls: AtomicUsize::new(0),
        });
        let mut agent = AgentLoop::new(
            Arc::new(ScriptedProvider::new(vec![
                tool_use_response(),
                end_turn_response("a"),
            ])),
            Arc::new(OkDispatcher),
            AgentConfig {
                model: "scripted/test".into(),
                gate: Some(gate.clone()),
                gate_max_retries: 1,
                ..Default::default()
            },
        );
        let events = collect_events(&mut agent).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn gate_not_run_without_mutation() {
        let gate = Arc::new(CountingGate {
            fail_first: 0,
            calls: AtomicUsize::new(0),
        });
        let mut agent = agent_with_gate(vec![end_turn_response("pure chat")], gate.clone());
        let events = collect_events(&mut agent).await;

        assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::Verification { .. })));
    }
}

/// Concurrency behaviour of tool dispatch: a batch of declared-safe reads runs
/// together, anything else stays serialised, and either way results come back
/// in the order the model asked for them.
#[cfg(test)]
mod dispatch_concurrency_tests {
    use super::*;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Dispatcher that tracks how many calls are in flight at once and holds
    /// each call open long enough for an overlap to be observable.
    struct ConcurrencyProbe {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        /// Per-tool delay, so a test can make a later call finish first.
        delay_ms: fn(&str) -> u64,
    }

    impl ConcurrencyProbe {
        fn new(delay_ms: fn(&str) -> u64) -> Arc<Self> {
            Arc::new(Self {
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                delay_ms,
            })
        }
    }

    #[async_trait]
    impl ToolDispatcher for ConcurrencyProbe {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, name: &str, _args: serde_json::Value) -> ToolOutcome {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis((self.delay_ms)(name))).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            ToolOutcome::ok(name)
        }
    }

    struct Replay(Mutex<VecDeque<Vec<StreamEvent>>>);

    #[async_trait]
    impl Provider for Replay {
        fn id(&self) -> &str {
            "replay"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::None,
                reasoning: false,
            }
        }
        async fn complete(&self, _req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            let events = self.0.lock().unwrap().pop_front().expect("over-called");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    /// One assistant turn that calls each of `names`, then a turn that ends.
    fn batch(names: &[&str]) -> Vec<Vec<StreamEvent>> {
        let mut first: Vec<StreamEvent> = names
            .iter()
            .enumerate()
            .map(|(i, n)| StreamEvent::ToolUse {
                block: ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: (*n).into(),
                    input: serde_json::json!({}),
                },
            })
            .collect();
        first.push(StreamEvent::Stop {
            reason: StopReason::ToolUse,
        });
        vec![
            first,
            vec![StreamEvent::Stop {
                reason: StopReason::EndTurn,
            }],
        ]
    }

    /// Drive one user turn and hand back the tool-result ids in arrival order.
    async fn run(names: &[&str], probe: Arc<ConcurrencyProbe>) -> Vec<String> {
        let provider = Arc::new(Replay(Mutex::new(batch(names).into())));
        let mut agent = AgentLoop::new(
            provider,
            probe,
            AgentConfig {
                model: "m".into(),
                ..Default::default()
            },
        );
        let mut ids = Vec::new();
        let mut stream = agent.run("go".into());
        while let Some(ev) = stream.next().await {
            if let AgentEvent::ToolResult { id, .. } = ev {
                ids.push(id);
            }
        }
        ids
    }

    const SLOW: fn(&str) -> u64 = |_| 40;

    #[tokio::test]
    async fn read_batch_dispatches_concurrently() {
        let probe = ConcurrencyProbe::new(SLOW);
        let ids = run(&["read_file", "grep", "lsp_hover"], probe.clone()).await;
        assert_eq!(ids.len(), 3);
        assert_eq!(
            probe.peak.load(Ordering::SeqCst),
            3,
            "all three reads should have been in flight together"
        );
    }

    #[tokio::test]
    async fn batch_with_a_mutating_call_stays_sequential() {
        let probe = ConcurrencyProbe::new(SLOW);
        // One write among the reads is enough to serialise the whole batch:
        // the reads after it must observe the tree it left behind.
        let ids = run(&["read_file", "write_file", "grep"], probe.clone()).await;
        assert_eq!(ids.len(), 3);
        assert_eq!(
            probe.peak.load(Ordering::SeqCst),
            1,
            "a mutating call must not run alongside anything"
        );
    }

    #[tokio::test]
    async fn unknown_tool_is_not_assumed_safe() {
        let probe = ConcurrencyProbe::new(SLOW);
        // MCP tools land here: absent from the allowlist, so no concurrency.
        let ids = run(&["read_file", "mcp__srv__query"], probe.clone()).await;
        assert_eq!(ids.len(), 2);
        assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parallel_results_keep_model_order() {
        // `grep` finishes long after the two calls that bracket it, so an
        // implementation yielding on completion order would scramble these.
        let probe = ConcurrencyProbe::new(|n| if n == "grep" { 60 } else { 5 });
        let ids = run(&["grep", "read_file", "lsp_hover"], probe.clone()).await;
        assert_eq!(ids, vec!["t0", "t1", "t2"]);
        assert_eq!(probe.peak.load(Ordering::SeqCst), 3);
    }
}

/// Reasoning blocks have to survive the loop untouched: Anthropic verifies the
/// signature against the text on the next tool-use request, so a block that is
/// dropped, reordered, or re-serialised breaks the following turn.
#[cfg(test)]
mod reasoning_tests {
    use super::*;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Recorder {
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
        responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    #[async_trait]
    impl Provider for Recorder {
        fn id(&self) -> &str {
            "recorder"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::Explicit,
                reasoning: true,
            }
        }
        async fn complete(&self, req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            self.seen.lock().unwrap().push(req);
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("over-called");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    struct Echo;
    #[async_trait]
    impl ToolDispatcher for Echo {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _n: &str, _a: serde_json::Value) -> ToolOutcome {
            ToolOutcome::ok("done")
        }
    }

    fn signed(text: &str, sig: &str) -> ContentBlock {
        ContentBlock::Thinking {
            text: text.into(),
            signature: Some(sig.into()),
            redacted: false,
        }
    }

    /// Turn one: think, then call a tool. Turn two: end.
    fn thinking_then_tool() -> Vec<Vec<StreamEvent>> {
        vec![
            vec![
                StreamEvent::ThinkingDelta {
                    text: "let me check".into(),
                },
                StreamEvent::Thinking {
                    block: signed("let me check", "sig-1"),
                },
                StreamEvent::ToolUse {
                    block: ContentBlock::ToolUse {
                        id: "t0".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    },
                },
                StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                },
            ],
            vec![
                StreamEvent::TextDelta {
                    text: "the answer".into(),
                },
                StreamEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]
    }

    async fn drive(cfg: AgentConfig) -> (Vec<CompletionRequest>, Vec<AgentEvent>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Recorder {
            seen: seen.clone(),
            responses: Mutex::new(thinking_then_tool().into()),
        });
        let mut agent = AgentLoop::new(provider, Arc::new(Echo), cfg);
        let mut events = Vec::new();
        let mut stream = agent.run("go".into());
        while let Some(e) = stream.next().await {
            events.push(e);
        }
        let reqs = seen.lock().unwrap().clone();
        (reqs, events)
    }

    fn cfg(reasoning: ReasoningEffort) -> AgentConfig {
        AgentConfig {
            model: "m".into(),
            reasoning,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn configured_level_reaches_the_provider() {
        let (reqs, _) = drive(cfg(ReasoningEffort::High)).await;
        assert!(reqs.iter().all(|r| r.reasoning == ReasoningEffort::High));
    }

    #[tokio::test]
    async fn off_by_default() {
        assert_eq!(AgentConfig::default().reasoning, ReasoningEffort::Off);
    }

    #[tokio::test]
    async fn thinking_is_echoed_back_verbatim_on_the_next_request() {
        let (reqs, _) = drive(cfg(ReasoningEffort::Medium)).await;
        // Second request carries the assistant turn that did the thinking.
        let assistant = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .expect("assistant turn missing from history");
        let thinking: Vec<_> = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking {
                    text, signature, ..
                } => Some((text.clone(), signature.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking,
            vec![("let me check".to_string(), Some("sig-1".to_string()))],
            "thinking block must survive with its signature intact"
        );
    }

    #[tokio::test]
    async fn thinking_precedes_the_tool_call_it_led_to() {
        let (reqs, _) = drive(cfg(ReasoningEffort::Medium)).await;
        let assistant = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .unwrap();
        let think_at = assistant.content.iter().position(|b| b.is_thinking());
        let tool_at = assistant
            .content
            .iter()
            .position(|b| matches!(b, ContentBlock::ToolUse { .. }));
        assert!(
            think_at < tool_at,
            "block order must match what the model emitted: {:?}",
            assistant.content
        );
    }

    #[tokio::test]
    async fn thinking_deltas_surface_separately_from_answer_text() {
        let (_, events) = drive(cfg(ReasoningEffort::Low)).await;
        let thinking: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["let me check"]);
        assert_eq!(text, vec!["the answer"]);
    }

    #[test]
    fn reasoning_levels_parse_and_round_trip() {
        for (s, want) in [
            ("off", ReasoningEffort::Off),
            ("", ReasoningEffort::Off),
            ("LOW", ReasoningEffort::Low),
            (" medium ", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
        ] {
            assert_eq!(ReasoningEffort::parse(s), Some(want), "parsing {s:?}");
        }
        assert_eq!(ReasoningEffort::parse("sometimes"), None);
        // Every on-level must yield a budget, or the Anthropic path silently
        // sends no `thinking` block.
        for r in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ] {
            assert!(r.is_on() && r.budget_tokens().is_some(), "{r}");
        }
        assert!(ReasoningEffort::Off.budget_tokens().is_none());
    }

    #[test]
    fn thinking_counts_toward_context() {
        // Reasoning is re-sent every turn, so compaction has to see it.
        let with = vec![Message::assistant(vec![signed(&"x".repeat(400), "s")])];
        let without = vec![Message::assistant(vec![ContentBlock::text("")])];
        assert!(
            crate::tokens::estimate_history_tokens(&with, None)
                > crate::tokens::estimate_history_tokens(&without, None)
        );
    }
}

#[cfg(test)]
mod spill_tests {
    use super::*;
    use crate::spill::SpillStore;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Replay(Mutex<VecDeque<Vec<StreamEvent>>>);

    #[async_trait]
    impl Provider for Replay {
        fn id(&self) -> &str {
            "replay"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::None,
                reasoning: false,
            }
        }
        async fn complete(&self, _req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            let events = self.0.lock().unwrap().pop_front().expect("over-called");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    /// Emits `lines` numbered lines, so a test can tell head from tail from
    /// the elided middle.
    struct Chatty(usize);

    #[async_trait]
    impl ToolDispatcher for Chatty {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _name: &str, _args: serde_json::Value) -> ToolOutcome {
            ToolOutcome::ok(
                (0..self.0)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }

    fn one_tool_turn() -> VecDeque<Vec<StreamEvent>> {
        VecDeque::from(vec![
            vec![
                StreamEvent::ToolUse {
                    block: ContentBlock::ToolUse {
                        id: "t0".into(),
                        name: "run_shell".into(),
                        input: serde_json::json!({}),
                    },
                },
                StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                },
            ],
            vec![StreamEvent::Stop {
                reason: StopReason::EndTurn,
            }],
        ])
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wingman-agent-spill-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Drive one turn and return what the *model* ends up seeing as the tool
    /// result — which is the whole point: the UI gets the full text either
    /// way, the model is the one paying for it.
    async fn model_saw(lines: usize, spill: Option<Arc<SpillStore>>) -> String {
        let provider = Arc::new(Replay(Mutex::new(one_tool_turn())));
        let mut agent = AgentLoop::new(
            provider,
            Arc::new(Chatty(lines)),
            AgentConfig {
                model: "m".into(),
                tool_output_budget: ToolOutputBudget::new(10),
                spill,
                ..Default::default()
            },
        );
        let mut stream = agent.run("go".into());
        while stream.next().await.is_some() {}
        drop(stream);
        agent
            .history()
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("a tool result should be in history")
    }

    #[tokio::test]
    async fn a_truncated_result_hands_the_model_a_path_to_the_rest() {
        let dir = tmp("hit");
        let store = Arc::new(SpillStore::new(dir.clone()));
        let seen = model_saw(500, Some(store)).await;

        // The locator must be the FIRST line: the pruner later keeps a head
        // and a tail, so a pointer in the middle is what gets discarded.
        assert!(
            seen.starts_with("[wingman]"),
            "locator must lead the result: {}",
            &seen[..seen.len().min(200)]
        );
        assert!(seen.contains("read_file"), "must say how to get the rest");

        // The path it names must exist and hold the complete output — the
        // claim is only worth anything if the model can act on it.
        let path: std::path::PathBuf = seen
            .lines()
            .next()
            .unwrap()
            .split("Full text: ")
            .nth(1)
            .unwrap()
            .split(" — ")
            .next()
            .unwrap()
            .into();
        let full = std::fs::read_to_string(&path).expect("spill file should exist");
        assert!(full.contains("line 250"), "the elided middle must be there");
        assert_eq!(full.lines().count(), 500);
        // And the model still only paid for the truncated view.
        assert!(!seen.contains("line 250"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn output_that_fits_the_budget_is_not_spilled() {
        let dir = tmp("small");
        let store = Arc::new(SpillStore::new(dir.clone()));
        let seen = model_saw(3, Some(store)).await;
        assert!(!seen.contains("[wingman]"));
        assert!(seen.contains("line 2"));
        assert!(
            !dir.exists(),
            "a session that never overflows should leave no spill directory"
        );
    }

    #[tokio::test]
    async fn without_a_store_truncation_behaves_exactly_as_before() {
        let seen = model_saw(500, None).await;
        assert!(!seen.contains("[wingman]"));
        assert!(seen.contains("elided"));
        assert!(seen.contains("line 0"));
    }
}

#[cfg(test)]
mod loop_guard_tests {
    //! The loop must stop a model that will not stop itself, and it must do
    //! so leaving a history the next request can actually be built from.

    use super::*;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Always asks for the same tool call. Never ends the turn — the only way
    /// out is a guard, which is the point.
    struct BrokenRecordProvider;

    #[async_trait]
    impl Provider for BrokenRecordProvider {
        fn id(&self) -> &str {
            "broken-record"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::None,
                reasoning: false,
            }
        }
        async fn complete(&self, _req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            let events = vec![
                StreamEvent::ToolUse {
                    block: ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "grep".into(),
                        input: serde_json::json!({ "pattern": "needle" }),
                    },
                },
                StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    #[derive(Default)]
    struct CountingDispatcher(AtomicUsize);

    #[async_trait]
    impl ToolDispatcher for CountingDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _name: &str, _args: serde_json::Value) -> ToolOutcome {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok("no matches")
        }
    }

    async fn run_with(
        guard: crate::loopguard::LoopGuard,
    ) -> (Vec<AgentEvent>, usize, Vec<Message>) {
        let tools = Arc::new(CountingDispatcher::default());
        let mut agent = AgentLoop::new(
            Arc::new(BrokenRecordProvider),
            tools.clone(),
            AgentConfig {
                model: "m".into(),
                // Generous, so that a stop proves the guard fired and not that
                // the loop merely ran out of turns.
                max_turns: 50,
                loop_guard: guard,
                ..Default::default()
            },
        );
        let mut events = Vec::new();
        let mut stream = agent.run("find the needle".into());
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }
        drop(stream);
        let calls = tools.0.load(Ordering::SeqCst);
        (events, calls, agent.history().to_vec())
    }

    #[tokio::test]
    async fn a_repeating_model_is_stopped_before_it_spends_the_turn_budget() {
        let (events, calls, _) = run_with(crate::loopguard::LoopGuard::new(24, 0, 4)).await;

        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Stop {
                    reason: AgentStop::LoopDetected
                })
            ),
            "expected LoopDetected, got {:?}",
            events.last()
        );
        // Three dispatched; the fourth tripped the guard and was refused.
        assert_eq!(calls, 3, "the aborting call must not be dispatched");
    }

    /// A `tool_use` block with no matching `tool_result` makes the *next*
    /// provider request malformed. Aborting must not leave that behind.
    #[tokio::test]
    async fn every_refused_call_still_gets_a_tool_result() {
        let (_, _, history) = run_with(crate::loopguard::LoopGuard::new(24, 0, 3)).await;

        let uses: Vec<&str> = history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let results: Vec<&str> = history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            uses.len(),
            results.len(),
            "unanswered tool_use blocks: {uses:?} vs {results:?}"
        );
        assert!(!uses.is_empty());
    }

    #[tokio::test]
    async fn the_warning_reaches_the_model_on_the_result_itself() {
        let (events, _, history) = run_with(crate::loopguard::LoopGuard::new(24, 2, 4)).await;

        // The advisory is deliberately *not* on the AgentEvent, which carries
        // the tool's own untouched output for the user to read.
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult { output, .. } if output.contains("not always in a row")
            )),
            "the user-facing result should be the tool's own output"
        );
        let model_saw = history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .any(|c| c.contains("not always in a row"));
        assert!(model_saw, "the model's copy should carry the advisory");
    }

    #[tokio::test]
    async fn a_disabled_guard_leaves_the_turn_budget_in_charge() {
        let (events, _, _) = run_with(crate::loopguard::LoopGuard::disabled()).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::MaxTurns
            })
        ));
    }
}

#[cfg(test)]
mod steer_tests {
    //! A steer must reach the model *during* the turn it was sent for, and it
    //! must not be able to corrupt the history it is spliced into.

    use super::*;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::sync::Mutex;

    /// Calls a tool on the first round trip, then ends the turn — so there is
    /// a second request for a steer to land in front of. Records every request
    /// it is given.
    struct TwoStepProvider {
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
        /// Pushed to the inbox after the first request, imitating a user
        /// typing while the turn is in flight.
        inbox: Arc<crate::steer::SteerInbox>,
    }

    #[async_trait]
    impl Provider for TwoStepProvider {
        fn id(&self) -> &str {
            "two-step"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::None,
                reasoning: false,
            }
        }
        async fn complete(&self, req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            let n = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(req);
                seen.len()
            };
            let events = if n == 1 {
                self.inbox.push("actually, keep the patch small");
                vec![
                    StreamEvent::ToolUse {
                        block: ContentBlock::ToolUse {
                            id: "t1".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({ "path": "a.rs" }),
                        },
                    },
                    StreamEvent::Stop {
                        reason: StopReason::ToolUse,
                    },
                ]
            } else {
                vec![
                    StreamEvent::TextDelta {
                        text: "done".into(),
                    },
                    StreamEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ]
            };
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    struct OkTool;
    #[async_trait]
    impl ToolDispatcher for OkTool {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _n: &str, _a: serde_json::Value) -> ToolOutcome {
            ToolOutcome::ok("contents")
        }
    }

    async fn run() -> (Vec<CompletionRequest>, Vec<AgentEvent>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let inbox = Arc::new(crate::steer::SteerInbox::new());
        let mut agent = AgentLoop::new(
            Arc::new(TwoStepProvider {
                seen: seen.clone(),
                inbox: inbox.clone(),
            }),
            Arc::new(OkTool),
            AgentConfig {
                model: "m".into(),
                steer: Some(inbox),
                ..Default::default()
            },
        );
        let mut events = Vec::new();
        let mut stream = agent.run("do the thing".into());
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }
        drop(stream);
        let reqs = seen.lock().unwrap().clone();
        (reqs, events)
    }

    #[tokio::test]
    async fn a_steer_reaches_the_next_request_of_the_same_turn() {
        let (reqs, _) = run().await;
        assert_eq!(reqs.len(), 2, "expected a second round trip");

        let landed = reqs[1].messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains("keep the patch small"))
            })
        });
        assert!(landed, "the steer never reached the model");

        let in_first = reqs[0].messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains("keep the patch small"))
            })
        });
        assert!(!in_first, "it cannot reach a request that was already sent");
    }

    #[tokio::test]
    async fn the_ui_is_told_where_the_steer_landed() {
        let (_, events) = run().await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Steered { text } if text == "actually, keep the patch small")),
            "a Steered event should mark the point it was folded in"
        );
    }

    /// The steer is framed as an interjection, not passed off as a fresh
    /// request — otherwise "skip the tests" reads as the whole new task.
    #[tokio::test]
    async fn the_steer_is_framed_as_an_adjustment() {
        let (reqs, _) = run().await;
        let text = reqs[1]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .find(|t| t.contains("keep the patch small"))
            .expect("steer present");
        assert!(text.contains("[wingman steer]"));
        assert!(text.contains("not a new task"));
    }

    /// Every `tool_use` must still be answered by a `tool_result`. Splicing a
    /// user message into the wrong place would break that and make the next
    /// request malformed.
    #[tokio::test]
    async fn splicing_a_steer_leaves_the_history_well_formed() {
        let (reqs, _) = run().await;
        let uses = reqs[1]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count();
        let results = reqs[1]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        assert_eq!(uses, results, "unanswered tool_use after splicing a steer");
    }
}
