use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde_json::Value;
use wingman_config::HooksConfig;
use wingman_core::{ToolDispatcher, ToolOutcome, ToolSpec};

pub struct ToolRegistry {
    /// Interior-mutable so MCP servers can be added/removed at runtime
    /// from behind an `Arc<ToolRegistry>` shared with the running agent.
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    ctx: ToolCtx,
    hooks: HooksConfig,
    /// Optional append-only audit log path. When set, each dispatch appends a
    /// JSONL record — an enterprise/compliance trail of what the agent did.
    audit: Option<std::path::PathBuf>,
    /// Redact high-confidence secret tokens in tool output before it reaches
    /// the model (data-exfiltration guard). Default on.
    redact_output: bool,
    /// Backstop deadline for one tool call. `None` disables it. Tools that
    /// bound themselves opt out via [`Tool::owns_timeout`].
    tool_timeout: Option<std::time::Duration>,
    /// Repeat-guard policy: thresholds and the tools transparent to the chain.
    repeat: RepeatPolicy,
    /// The live repeat chain: the last tracked call and how many times in a
    /// row it has been made.
    ///
    /// One chain per registry is one chain per agent — a subagent is given
    /// its own `ToolRegistry`, so a child's repetition can never trip its
    /// parent's counter and no per-agent keying is needed here.
    chain: std::sync::Mutex<Option<RepeatChain>>,
    /// Name patterns whose schemas are withheld from `specs()` and reached
    /// through `tool_search` / `tool_call` instead (`[tools].defer`). Empty
    /// means every registered tool is in every request, which is the
    /// behaviour this registry has always had.
    defer: Vec<String>,
    /// Tools this registry refuses to hold, from `[tools].preset` and
    /// `[tools].disabled_tools`.
    ///
    /// Enforced at registration rather than swept afterwards. A sweep only
    /// removes what is registered when it runs, and several tools register
    /// later than that — `spawn_subagent` and `run_plan` after the `Arc`
    /// exists, every MCP tool when its server connects. Naming those in
    /// `disabled_tools` used to do nothing at all, silently.
    removals: ToolRemovals,
}

/// Which tools a registry will accept.
///
/// `keep` is a preset keep-list (`None` keeps everything); `disabled` is the
/// standing denylist. Denylist wins: a preset says what a session is *for*,
/// `disabled_tools` says "not this one, ever", and an entry that survived a
/// keep-list would be the wrong way round.
#[derive(Debug, Clone, Default)]
pub struct ToolRemovals {
    keep: Option<Vec<String>>,
    disabled: Vec<String>,
}

impl ToolRemovals {
    pub fn new(keep: Option<Vec<String>>, disabled: Vec<String>) -> Self {
        Self { keep, disabled }
    }

    /// Whether `name` is excluded.
    pub fn excludes(&self, name: &str) -> bool {
        if self.disabled.iter().any(|d| d == name) {
            return true;
        }
        match &self.keep {
            Some(keep) => !keep.iter().any(|p| preset_matches(name, p)),
            None => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keep.is_none() && self.disabled.is_empty()
    }
}

/// Match a tool name against a preset keep-list entry. A trailing `*` matches
/// by prefix, so `lsp_*` keeps the whole language-server family without the
/// list going stale each time one is added.
fn preset_matches(name: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Repeat-guard configuration. Empty `thresholds` disables the guard.
#[derive(Debug, Clone, Default)]
struct RepeatPolicy {
    thresholds: Vec<u32>,
    exempt: Vec<String>,
}

/// A run of identical consecutive tool calls.
#[derive(Debug, Clone)]
struct RepeatChain {
    /// Tool name plus canonicalized arguments.
    key: String,
    /// How many times in a row, including the call just made.
    count: u32,
}

/// Tools that ship but are **not** registered by [`ToolRegistry::with_builtins`]
/// — they need a dependency the registry cannot build for itself (a semantic
/// index, learning handles, a subagent runner) or sit behind a config flag.
///
/// Kept here rather than in each test that needs it: two tests already assert
/// against this set (documentation coverage and preset keep-lists), and a
/// second copy would drift the moment a conditional tool is added.
pub const CONDITIONALLY_REGISTERED: &[&str] = &[
    "semantic_search",
    "save_memory",
    "recall_memory",
    "forget_memory",
    "invoke_skill",
    "recall_session",
    "read_session",
    "spawn_subagent",
    "task_complete",
    "run_plan",
];

impl ToolRegistry {
    pub fn new(ctx: ToolCtx) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            ctx,
            hooks: HooksConfig::default(),
            audit: None,
            redact_output: true,
            tool_timeout: None,
            repeat: RepeatPolicy::default(),
            defer: Vec::new(),
            chain: std::sync::Mutex::new(None),
            removals: ToolRemovals::default(),
        }
    }

    /// Refuse the tools `[tools].preset` and `[tools].disabled_tools` exclude,
    /// for the whole life of the registry rather than at one moment in its
    /// construction. Builder-style.
    pub fn with_tool_removals(mut self, removals: ToolRemovals) -> Self {
        self.removals = removals;
        self
    }

    /// Same, for a registry already built. Used by the session builder, which
    /// registers builtins before it has parsed the config that removes them.
    pub fn set_removals(&mut self, removals: ToolRemovals) {
        self.removals = removals;
    }

    /// The active removal policy.
    pub fn removals(&self) -> &ToolRemovals {
        &self.removals
    }

    /// Arm the per-call backstop deadline. `0` disables it. Builder-style.
    pub fn with_tool_timeout(mut self, secs: u64) -> Self {
        self.tool_timeout = (secs > 0).then(|| std::time::Duration::from_secs(secs));
        self
    }

    /// Withhold tools matching any of `patterns` from the request, so their
    /// schemas stop being billed on every turn. A trailing `*` matches by
    /// prefix (`mcp__*`).
    ///
    /// The meta-tools are never deferrable: withholding the mechanism that
    /// finds withheld tools would leave the deferred set unreachable.
    #[must_use]
    pub fn with_deferred(mut self, patterns: Vec<String>) -> Self {
        self.defer = patterns;
        self
    }

    /// Whether anything is actually deferred, so the caller only registers the
    /// two meta-tools when they have something to do. Registering them
    /// unconditionally would *add* two schemas to every session to save none.
    pub fn defers_anything(&self) -> bool {
        if self.defer.is_empty() {
            return false;
        }
        let guard = self.tools.read().unwrap_or_else(|e| e.into_inner());
        guard.keys().any(|name| self.is_deferred(name))
    }

    /// Whether `name` is withheld from the request.
    fn is_deferred(&self, name: &str) -> bool {
        !crate::builtin::is_meta_tool(name) && self.defer.iter().any(|p| tool_matches(name, p))
    }

    /// Schemas of the registered tools whose names satisfy `keep`.
    ///
    /// The clone-then-drop is load-bearing, not tidiness: `Tool::spec()` is
    /// allowed to consult the registry — `tool_search` builds its description
    /// from the deferred list — and calling it while still holding the read
    /// guard re-enters the lock. A recursive shared acquisition is not
    /// guaranteed to succeed by `std`, and on Windows' SRWLock it deadlocks
    /// outright, which is exactly how this first showed up.
    fn collect_specs(&self, keep: impl Fn(&str) -> bool) -> Vec<ToolSpec> {
        let held: Vec<Arc<dyn Tool>> = {
            let guard = self.tools.read().unwrap_or_else(|e| e.into_inner());
            guard
                .iter()
                .filter(|(name, _)| keep(name))
                .map(|(_, t)| t.clone())
                .collect()
        };
        let mut out: Vec<ToolSpec> = held.iter().map(|t| t.spec()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Configure the repeat guard. Empty `thresholds` disables it.
    /// Builder-style.
    pub fn with_repeat_guard(mut self, thresholds: Vec<u32>, exempt: Vec<String>) -> Self {
        // Ascending, deduplicated, and never below 2 — a "threshold" of 1
        // would fire on every first call, and an unsorted list would make
        // the first (generic) reminder land at an arbitrary run length.
        let mut thresholds: Vec<u32> = thresholds.into_iter().filter(|n| *n >= 2).collect();
        thresholds.sort_unstable();
        thresholds.dedup();
        // `tool_matches` treats an empty pattern as "matches everything", so
        // a stray "" in the exempt list would silently disable the guard.
        let exempt = exempt.into_iter().filter(|p| !p.is_empty()).collect();
        self.repeat = RepeatPolicy { thresholds, exempt };
        self
    }

    /// Attach a hooks configuration. Returns `self` for builder-style chaining.
    pub fn with_hooks(mut self, hooks: HooksConfig) -> Self {
        self.hooks = hooks;
        self
    }

    /// Enable the append-only audit log at `path`. Builder-style.
    pub fn with_audit(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.audit = path;
        self
    }

    /// Toggle high-confidence secret redaction of tool output. Builder-style.
    pub fn with_output_redaction(mut self, on: bool) -> Self {
        self.redact_output = on;
        self
    }

    /// Append one audit record for a dispatched tool call. Best-effort: a
    /// logging failure never blocks the tool. The input is truncated so the
    /// trail stays compact and doesn't balloon with large tool arguments.
    fn write_audit(&self, name: &str, args: &Value, is_error: bool) {
        let Some(path) = &self.audit else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // Redact secret-looking values so the compliance trail never itself
        // leaks credentials, then bound the size.
        let redacted = redact_secrets(args);
        let mut input = redacted.to_string();
        if input.len() > 400 {
            input.truncate(400);
            input.push('…');
        }
        let record = serde_json::json!({
            "ts_ms": ts,
            "tool": name,
            "input": input,
            "is_error": is_error,
        });
        // Concurrent subagents dispatch tools at the same time, so this is a
        // genuinely shared file; an audit log with two records on one line is
        // an audit log that will not parse. Errors stay ignored — auditing must
        // never fail a tool call.
        let _ = wingman_config::append_line(path, &record.to_string());
    }

    /// Record one dispatched call against the repeat chain and return an
    /// advisory when the run length has just reached a configured threshold.
    ///
    /// Advisory only: it never blocks a call and never rewrites one. The
    /// decision — retry differently, gather more evidence, or conclude —
    /// stays with the model, and a legitimately repeated call is delayed by
    /// nothing.
    fn note_call_and_advise(&self, name: &str, args: &Value) -> Option<String> {
        if self.repeat.thresholds.is_empty() {
            return None;
        }
        // An exempt tool is *transparent* to the chain: it neither increments
        // the counter nor resets it. That is what makes exemption useful —
        // bookkeeping interleaved into a loop must not launder it, so
        // `grep X → update_tasks → grep X` still reads as two consecutive
        // `grep X`.
        if self
            .repeat
            .exempt
            .iter()
            .any(|pattern| tool_matches(name, pattern))
        {
            return None;
        }

        let key = format!("{name}\u{1}{}", wingman_core::canonical_args(args));
        let count = {
            let mut guard = self.chain.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(chain) if chain.key == key => {
                    chain.count = chain.count.saturating_add(1);
                    chain.count
                }
                _ => {
                    *guard = Some(RepeatChain { key, count: 1 });
                    1
                }
            }
        };

        // Fire only on the exact threshold, so one long run produces one
        // reminder per threshold rather than one per call after the first.
        if !self.repeat.thresholds.contains(&count) {
            return None;
        }
        // The first threshold is a light nudge; later ones name the tool and
        // the run length, because by then the model has ignored the nudge.
        Some(if count == self.repeat.thresholds[0] {
            "[wingman] That call repeated with identical arguments. Re-read the \
             previous result before trying it again."
                .to_string()
        } else {
            format!(
                "[wingman] `{name}` has now been called {count} times in a row with \
                 identical arguments, and the result will not change. Re-read the \
                 last result, then either change approach or conclude with what \
                 you already have."
            )
        })
    }

    pub fn hooks(&self) -> &HooksConfig {
        &self.hooks
    }

    /// Fire `[[hooks.user_prompt_submit]]` for a submitted prompt.
    ///
    /// Returns `Err(reason)` when a hook with `block = true` exits non-zero, so
    /// the caller can refuse to send the prompt. This is the natural place for
    /// a policy or content filter, which is why it failing silently was worse
    /// than not offering it: the hooks parsed fine from config and then never
    /// ran at all.
    pub async fn run_user_prompt_submit_hooks(&self, prompt: &str) -> Result<(), String> {
        for hook in &self.hooks.user_prompt_submit {
            let env = vec![("WINGMAN_USER_PROMPT", prompt.to_string())];
            let res = run_hook(&hook.command, hook.timeout_secs, &env).await;
            if !res.success {
                tracing::warn!(
                    target: "wingman::hooks",
                    blocking = hook.block,
                    "user_prompt_submit hook failed: {}",
                    res.stderr.trim()
                );
                if hook.block {
                    return Err(res.stderr.trim().to_string());
                }
            }
        }
        Ok(())
    }

    /// Fire `[[hooks.stop]]` when a user turn finishes. `reason` is the agent's
    /// stop reason (`end_turn`, `max_turns`, `gate_failed`, …).
    ///
    /// Advisory: there is nothing left to block by the time the turn is over,
    /// so failures are logged rather than propagated.
    pub async fn run_stop_hooks(&self, reason: &str) {
        for hook in &self.hooks.stop {
            let env = vec![("WINGMAN_STOP_REASON", reason.to_string())];
            let res = run_hook(&hook.command, hook.timeout_secs, &env).await;
            if !res.success {
                tracing::warn!(
                    target: "wingman::hooks",
                    "stop hook failed: {}",
                    res.stderr.trim()
                );
            }
        }
    }

    /// Register a concrete tool at build time. Used by the chained-builder
    /// flow (`ToolRegistry::new(ctx).with_builtins()`) before the registry
    /// is wrapped in `Arc`.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> &mut Self {
        self.register_arc(Arc::new(tool));
        self
    }

    /// Insert a tool at runtime through shared (`&self`) access. Returns
    /// the previous tool with the same name, if any, so a caller can swap
    /// implementations without dropping live work.
    pub fn register_arc(&self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        let spec = tool.spec();
        // The single choke point: `register` delegates here, so one check
        // covers every path in, whenever it happens.
        if self.removals.excludes(&spec.name) {
            tracing::info!(
                target: "wingman::tools",
                tool = %spec.name,
                "not registered: excluded by [tools].preset / [tools].disabled_tools"
            );
            return None;
        }
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(spec.name, tool)
    }

    /// Remove a tool by name. Idempotent.
    pub fn unregister(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
    }

    /// Tool names currently registered.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// What a registered tool is allowed to touch, by name.
    ///
    /// Front ends use this to decide which calls are worth interrupting a
    /// human for: a `read_file` is not the same ask as a `run_shell`.
    pub fn capability_of(&self, name: &str) -> Option<Capability> {
        let guard = self.tools.read().unwrap_or_else(|e| e.into_inner());
        guard.get(name).map(|t| t.capabilities())
    }

    pub fn ctx(&self) -> &ToolCtx {
        &self.ctx
    }

    /// Switch the permission mode live. Takes `&self` (the mode lives behind
    /// an atomic in [`ToolCtx`]) so it works through the `Arc<ToolRegistry>`
    /// the running agent shares — the next tool call is gated by `mode`.
    pub fn set_mode(&self, mode: wingman_config::PermissionMode) {
        self.ctx.set_mode(mode);
    }

    pub fn with_builtins(mut self) -> Self {
        self.register(crate::builtin::ReadFile);
        self.register(crate::builtin::WriteFile);
        self.register(crate::builtin::EditFile);
        self.register(crate::builtin::ApplyPatch);
        self.register(crate::builtin::RunShell);
        self.register(crate::builtin::JobOutput);
        self.register(crate::builtin::JobSend);
        self.register(crate::builtin::JobStop);
        self.register(crate::builtin::JobList);
        self.register(crate::builtin::ListDir);
        self.register(crate::builtin::Glob);
        self.register(crate::builtin::Grep);
        self.register(crate::builtin::WebFetch);
        self.register(crate::builtin::WebSearch);
        self.register(crate::builtin::PresentPlan);
        self.register(crate::builtin::UpdateTasks);
        self.register(crate::builtin::AskUser);
        #[cfg(feature = "treesitter")]
        {
            self.register(crate::builtin::FindSymbol);
            self.register(crate::builtin::Outline);
            self.register(crate::builtin::EditSymbol);
            self.register(crate::builtin::WhoCalls);
        }
        // LSP-backed intelligence. Registered unconditionally — each tool
        // degrades gracefully (returns a "fall back to the tree-sitter tools"
        // note) when the user has no language server installed for the file.
        self.register(crate::builtin::LspDefinition);
        self.register(crate::builtin::LspReferences);
        self.register(crate::builtin::LspHover);
        self.register(crate::builtin::LspDiagnostics);
        self.register(crate::builtin::LspRename);
        self.register(crate::builtin::LspCodeAction);
        self
    }

    /// Register the `semantic_search` tool against a shared indexer. Call
    /// this on top of [`with_builtins`] when RAG is wired.
    pub fn with_semantic_search(mut self, indexer: std::sync::Arc<wingman_rag::Indexer>) -> Self {
        self.register(crate::builtin::SemanticSearch::new(indexer));
        self
    }

    /// Register user-defined command tools from `[[tools.custom]]` config.
    pub fn with_custom_tools(mut self, custom: &[wingman_config::CustomToolConfig]) -> Self {
        for c in custom {
            if c.name.trim().is_empty() || c.command.trim().is_empty() {
                continue;
            }
            self.register(crate::builtin::CommandTool::new(
                c.name.clone(),
                c.description.clone(),
                c.command.clone(),
                c.timeout_secs,
            ));
        }
        self
    }

    /// Register the `spawn_subagent` tool. The `runner` closure is supplied
    /// by the runtime (which knows how to build inner agents) so this
    /// crate stays provider-agnostic.
    pub fn with_subagent(mut self, runner: crate::builtin::SubagentRunner) -> Self {
        self.register(crate::builtin::SpawnSubagent::new(runner));
        self
    }
}

#[async_trait]
impl ToolDispatcher for ToolRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        self.collect_specs(|name| !self.is_deferred(name))
    }

    /// Everything registered, deferred or not.
    fn all_specs(&self) -> Vec<ToolSpec> {
        self.collect_specs(|_| true)
    }

    /// Only what is withheld from `specs()`.
    fn deferred_specs(&self) -> Vec<ToolSpec> {
        if self.defer.is_empty() {
            return Vec::new();
        }
        self.collect_specs(|name| self.is_deferred(name))
    }

    async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
        // Pre-tool-use hooks: if a blocking hook fails, return a tool error.
        for hook in &self.hooks.pre_tool_use {
            if !tool_matches(name, &hook.match_tool) {
                continue;
            }
            let env = vec![
                ("WINGMAN_TOOL_NAME", name.to_string()),
                (
                    "WINGMAN_TOOL_INPUT",
                    serde_json::to_string(&args).unwrap_or_default(),
                ),
            ];
            let res = run_hook(&hook.command, hook.timeout_secs, &env).await;
            if !res.success {
                // Always surface the failure. Previously a non-blocking hook's
                // result was dropped on the floor, so a policy hook whose
                // binary was missing, renamed, non-executable, or timing out
                // produced no output on any channel and every tool call
                // proceeded — a security control failing silently and open.
                // `post_tool_use` already warned; this matches it.
                tracing::warn!(
                    target: "wingman::hooks",
                    tool = %name,
                    blocking = hook.block,
                    "pre_tool_use hook failed: {}",
                    res.stderr.trim()
                );
            }
            if hook.block && !res.success {
                return ToolOutcome::err(format!(
                    "pre_tool_use hook blocked: {}",
                    res.stderr.trim()
                ));
            }
        }

        // Snapshot any files this tool is about to mutate, so /undo can
        // restore them. Captured before the edit; persisted only on success.
        let pres: Vec<_> = wingman_core::checkpoint::mutating_paths(name, &args)
            .iter()
            .map(|p| wingman_core::checkpoint::capture(&self.ctx.project_root, p))
            .collect();

        // Clone the Arc out of the lock before awaiting so we don't hold
        // a guard across an `.await` (std::sync guards aren't Send).
        let tool = self
            .tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned();
        let mut outcome = match tool {
            // Central capability gate. Enforcement lives here rather than in
            // each tool so that forgetting to check `ToolCtx` fails closed.
            Some(tool) => match capability_denial(name, tool.capabilities(), &self.ctx) {
                Some(msg) => ToolOutcome::err(msg),
                None => {
                    let run = tool.run(args.clone(), &self.ctx);
                    // Backstop deadline. A tool that bounds itself runs
                    // unwrapped, so its own (possibly longer) budget stands.
                    match self.tool_timeout.filter(|_| !tool.owns_timeout()) {
                        None => run.await,
                        Some(limit) => match tokio::time::timeout(limit, run).await {
                            Ok(outcome) => outcome,
                            Err(_) => {
                                tracing::warn!(
                                    target: "wingman::tools",
                                    tool = %name,
                                    secs = limit.as_secs(),
                                    "tool call exceeded the backstop deadline"
                                );
                                ToolOutcome::err(format!(
                                    "`{name}` exceeded the {}s deadline and was abandoned. \
                                     Raise [tools].tool_timeout_secs if this tool is \
                                     legitimately slow, or try a narrower request.",
                                    limit.as_secs()
                                ))
                            }
                        },
                    }
                }
            },
            None => ToolOutcome::err(format!("unknown tool: {name}")),
        };

        // Data-exfiltration guard: strip high-confidence secret tokens from the
        // output before it reaches the model, so a credential the agent read
        // (e.g. from a .env) can't be echoed back or smuggled out.
        if self.redact_output {
            let (redacted, n) = redact_output_secrets(&outcome.content);
            if n > 0 {
                outcome.content = redacted;
            }
        }

        if !outcome.is_error && !pres.is_empty() {
            wingman_core::checkpoint::commit(&self.ctx.project_root, pres);
        }

        // Append to the compliance audit trail, if enabled. Done before the
        // repeat advisory is attached, so the trail records the tool's own
        // verdict rather than Wingman's commentary on it.
        self.write_audit(name, &args, outcome.is_error);

        // Loop hygiene. Counted here, after the capability gate, so a call
        // the permission mode keeps refusing still counts — a model hammering
        // a denied call is exactly the loop worth breaking.
        if let Some(advisory) = self.note_call_and_advise(name, &args) {
            if !outcome.content.is_empty() {
                outcome.content.push_str("\n\n");
            }
            outcome.content.push_str(&advisory);
        }

        // Post-tool-use hooks: fire-and-forget; failures only logged.
        for hook in &self.hooks.post_tool_use {
            if !tool_matches(name, &hook.match_tool) {
                continue;
            }
            let env = vec![
                ("WINGMAN_TOOL_NAME", name.to_string()),
                (
                    "WINGMAN_TOOL_INPUT",
                    serde_json::to_string(&args).unwrap_or_default(),
                ),
                ("WINGMAN_TOOL_OUTPUT", outcome.content.clone()),
                ("WINGMAN_TOOL_IS_ERROR", outcome.is_error.to_string()),
            ];
            let res = run_hook(&hook.command, hook.timeout_secs, &env).await;
            if !res.success {
                tracing::warn!(
                    "post_tool_use hook failed (tool={}): {}",
                    name,
                    res.stderr.trim()
                );
            }
        }

        outcome
    }
}

/// Recursively redact secret-looking values in a JSON value: any object key
/// that looks credential-bearing (`key`, `token`, `secret`, `password`,
/// `authorization`, `api_key`, …) has its string value replaced with
/// `"[redacted]"`. Used so the audit trail can't itself leak credentials.
fn redact_secrets(v: &Value) -> Value {
    fn is_secret_key(k: &str) -> bool {
        let k = k.to_ascii_lowercase();
        const NEEDLES: &[&str] = &[
            "password",
            "passwd",
            "secret",
            "token",
            "api_key",
            "apikey",
            "authorization",
            "auth",
            "credential",
            "private_key",
            "access_key",
        ];
        // `key` alone is too broad (matches innocuous "key"), so require it to
        // be a suffix/compound (api_key, access_key) or one of the needles.
        NEEDLES.iter().any(|n| k.contains(n)) || k == "key" || k.ends_with("_key")
    }
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                if is_secret_key(k) && val.is_string() {
                    out.insert(k.clone(), Value::String("[redacted]".into()));
                } else {
                    out.insert(k.clone(), redact_secrets(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact_secrets).collect()),
        other => other.clone(),
    }
}

/// Redact high-confidence secret tokens in `text`, returning `(redacted, n)`.
/// Only matches unambiguous credential shapes so it doesn't mangle normal
/// output. Used to stop the agent surfacing/exfiltrating a secret it read.
pub fn redact_output_secrets(text: &str) -> (String, usize) {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"sk-[A-Za-z0-9_-]{20,}",        // OpenAI / Anthropic-style
            r"gh[pousr]_[A-Za-z0-9]{30,}",   // GitHub tokens
            r"AKIA[0-9A-Z]{16}",             // AWS access key id
            r"xox[baprs]-[A-Za-z0-9-]{10,}", // Slack tokens
            r"AIza[0-9A-Za-z_-]{35}",        // Google API key
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", // JWT
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", // PEM keys
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    });
    let mut out = text.to_string();
    let mut n = 0usize;
    for re in patterns {
        let count = re.find_iter(&out).count();
        if count > 0 {
            n += count;
            out = re.replace_all(&out, "[redacted-secret]").into_owned();
        }
    }
    (out, n)
}

fn tool_matches(name: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if let Some(rest) = pattern.strip_suffix('*') {
        name.starts_with(rest)
    } else if let Some(rest) = pattern.strip_prefix('*') {
        name.ends_with(rest)
    } else {
        name == pattern
    }
}

pub struct HookResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Refuse a tool whose declared capabilities the active permission mode does
/// not grant. Returns the message to hand back to the model, or `None` to let
/// the call proceed.
///
/// Deliberately coarse: it does not know which path a tool will touch, only
/// whether writing / shelling out / reaching the network is permitted at all
/// right now. Path containment stays inside the tools, which know their own
/// arguments.
fn capability_denial(name: &str, caps: crate::Capability, ctx: &ToolCtx) -> Option<String> {
    use crate::Capability;

    if caps.contains(Capability::WRITE) && !ctx.allows_any_write() {
        return Some(format!(
            "{name} modifies files, which is not permitted in {:?} mode \
             (switch to auto-edit or yolo)",
            ctx.mode()
        ));
    }
    if caps.contains(Capability::SHELL) && !ctx.allows_shell() {
        return Some(format!(
            "{name} executes commands, which is not permitted in {:?} mode \
             (switch to auto-edit or yolo)",
            ctx.mode()
        ));
    }
    if caps.contains(Capability::NETWORK) && !ctx.allows_network() {
        return Some(format!(
            "{name} accesses the network, which is not permitted in {:?} mode \
             (set [tools].allow_network, or switch to auto-edit/yolo)",
            ctx.mode()
        ));
    }
    None
}

pub async fn run_hook(command: &str, timeout_secs: u64, env: &[(&str, String)]) -> HookResult {
    let command = command.to_string();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();

    let fut = tokio::task::spawn_blocking(move || {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", &command]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", &command]);
            c
        };
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output()
    });

    let output = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await
    {
        Ok(Ok(Ok(o))) => o,
        Ok(Ok(Err(e))) => {
            return HookResult {
                success: false,
                stdout: String::new(),
                stderr: format!("hook spawn: {e}"),
            }
        }
        Ok(Err(e)) => {
            return HookResult {
                success: false,
                stdout: String::new(),
                stderr: format!("hook join: {e}"),
            }
        }
        Err(_) => {
            return HookResult {
                success: false,
                stdout: String::new(),
                stderr: format!("hook timed out after {timeout_secs}s"),
            }
        }
    };

    HookResult {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};
    use async_trait::async_trait;
    use wingman_config::PermissionMode;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutcome {
            ToolOutcome::ok("ok")
        }
    }

    /// A tool that declares a capability but would happily do the work — the
    /// point is that the registry stops it before `run` is reached.
    struct CapTool(&'static str, crate::Capability);

    #[async_trait]
    impl Tool for CapTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "cap".into(),
                input_schema: serde_json::json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutcome {
            ToolOutcome::ok("DID THE THING")
        }
        fn capabilities(&self) -> crate::Capability {
            self.1
        }
    }

    fn ctx_in(mode: PermissionMode) -> ToolCtx {
        let cwd = std::env::temp_dir();
        ToolCtx::new(mode, cwd.clone(), cwd)
    }

    /// The gate this table encodes is the whole point of declaring
    /// capabilities: a tool that forgets to consult `ToolCtx` must still be
    /// refused. Previously `save_memory`/`forget_memory` wrote files in
    /// read-only mode precisely because nothing checked centrally.
    #[tokio::test]
    async fn capability_gate_matrix() {
        use crate::Capability;
        use wingman_core::ToolDispatcher;

        // (capability, mode, should_be_allowed)
        let cases = [
            (Capability::WRITE, PermissionMode::ReadOnly, false),
            (Capability::WRITE, PermissionMode::Plan, false),
            (Capability::WRITE, PermissionMode::AutoEdit, true),
            (Capability::WRITE, PermissionMode::Yolo, true),
            (Capability::SHELL, PermissionMode::ReadOnly, false),
            (Capability::SHELL, PermissionMode::Plan, false),
            (Capability::SHELL, PermissionMode::AutoEdit, true),
            (Capability::SHELL, PermissionMode::Yolo, true),
            (Capability::NETWORK, PermissionMode::ReadOnly, false),
            (Capability::NETWORK, PermissionMode::Plan, false),
            (Capability::NETWORK, PermissionMode::AutoEdit, true),
            (Capability::NETWORK, PermissionMode::Yolo, true),
            // Reads are allowed in every mode; containment is per-path.
            (Capability::READ, PermissionMode::ReadOnly, true),
            (Capability::NONE, PermissionMode::ReadOnly, true),
        ];

        for (cap, mode, allowed) in cases {
            let mut reg = ToolRegistry::new(ctx_in(mode));
            reg.register(CapTool("t", cap));
            let out = reg.dispatch("t", serde_json::json!({})).await;
            assert_eq!(
                !out.is_error, allowed,
                "cap {cap:?} in {mode:?}: expected allowed={allowed}, got {out:?}"
            );
            if allowed {
                assert_eq!(out.content, "DID THE THING");
            }
        }
    }

    #[tokio::test]
    async fn write_capable_tool_is_refused_before_it_runs_in_read_only() {
        use crate::Capability;
        use wingman_core::ToolDispatcher;

        let mut reg = ToolRegistry::new(ctx_in(PermissionMode::ReadOnly));
        reg.register(CapTool("save_memory", Capability::WRITE));
        let out = reg.dispatch("save_memory", serde_json::json!({})).await;

        assert!(out.is_error);
        assert_ne!(
            out.content, "DID THE THING",
            "the tool body must not have executed"
        );
        assert!(out.content.contains("read-only") || out.content.contains("ReadOnly"));
    }

    /// These two hooks were documented with working examples and never fired —
    /// the env vars they promise appeared only in config docstrings. A policy
    /// hook that silently does nothing is worse than one that isn't offered.
    #[tokio::test]
    async fn user_prompt_submit_hook_can_block_a_prompt() {
        use wingman_config::{Hook, HooksConfig};

        let blocking = HooksConfig {
            user_prompt_submit: vec![Hook {
                // Non-zero exit == refuse. `exit 1` works in both cmd and sh.
                command: "exit 1".into(),
                match_tool: String::new(),
                block: true,
                timeout_secs: 10,
            }],
            ..Default::default()
        };
        let reg = ToolRegistry::new(ctx_in(PermissionMode::ReadOnly)).with_hooks(blocking);
        assert!(
            reg.run_user_prompt_submit_hooks("anything").await.is_err(),
            "a blocking hook exiting non-zero must refuse the prompt"
        );
    }

    #[tokio::test]
    async fn non_blocking_user_prompt_submit_hook_does_not_refuse() {
        use wingman_config::{Hook, HooksConfig};

        let advisory = HooksConfig {
            user_prompt_submit: vec![Hook {
                command: "exit 1".into(),
                match_tool: String::new(),
                block: false,
                timeout_secs: 10,
            }],
            ..Default::default()
        };
        let reg = ToolRegistry::new(ctx_in(PermissionMode::ReadOnly)).with_hooks(advisory);
        assert!(reg.run_user_prompt_submit_hooks("anything").await.is_ok());
    }

    /// `WINGMAN_STOP_REASON` must actually reach the hook process — that env
    /// var existed only in a config docstring before this.
    #[tokio::test]
    async fn stop_reason_reaches_the_hook_process() {
        let cmd = if cfg!(windows) {
            "echo %WINGMAN_STOP_REASON%"
        } else {
            "echo \"$WINGMAN_STOP_REASON\""
        };
        let res = run_hook(
            cmd,
            10,
            &[("WINGMAN_STOP_REASON", "gate_failed".to_string())],
        )
        .await;
        assert!(res.success, "hook failed: {}", res.stderr);
        assert!(
            res.stdout.contains("gate_failed"),
            "hook did not receive WINGMAN_STOP_REASON, stdout was {:?}",
            res.stdout
        );
    }

    /// Likewise for the prompt hook's env var.
    #[tokio::test]
    async fn user_prompt_reaches_the_hook_process() {
        let cmd = if cfg!(windows) {
            "echo %WINGMAN_USER_PROMPT%"
        } else {
            "echo \"$WINGMAN_USER_PROMPT\""
        };
        let res = run_hook(
            cmd,
            10,
            &[("WINGMAN_USER_PROMPT", "refactor auth".to_string())],
        )
        .await;
        assert!(res.success, "hook failed: {}", res.stderr);
        assert!(res.stdout.contains("refactor auth"));
    }

    #[tokio::test]
    async fn stop_hooks_run_without_blocking_the_caller() {
        use wingman_config::{Hook, HooksConfig};

        // A failing stop hook is advisory — the turn is already over, so there
        // is nothing left to block. It must not panic or hang.
        let hooks = HooksConfig {
            stop: vec![Hook {
                command: "exit 3".into(),
                match_tool: String::new(),
                block: true,
                timeout_secs: 10,
            }],
            ..Default::default()
        };
        let reg = ToolRegistry::new(ctx_in(PermissionMode::ReadOnly)).with_hooks(hooks);
        reg.run_stop_hooks("end_turn").await;
    }

    #[tokio::test]
    async fn audit_log_records_each_dispatch() {
        let dir = std::env::temp_dir().join(format!("wm-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("audit.log");
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let mut reg = ToolRegistry::new(ctx).with_audit(Some(log.clone()));
        reg.register(EchoTool);
        let _ = reg.dispatch("echo", serde_json::json!({ "x": 1 })).await;
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("\"tool\":\"echo\""),
            "audit missing tool: {body}"
        );
        assert!(body.contains("\"is_error\":false"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redacts_high_confidence_output_tokens() {
        let (out, n) = super::redact_output_secrets(
            "key=sk-abcdefghij0123456789ABCDEF and AKIAIOSFODNN7EXAMPLE plus normal text",
        );
        assert_eq!(n, 2);
        assert!(!out.contains("sk-abcdefghij"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(out.contains("normal text"));
        assert!(out.contains("[redacted-secret]"));
    }

    #[test]
    fn leaves_ordinary_output_untouched() {
        let text = "fn main() { let key = compute(); println!(\"{key}\"); }";
        let (out, n) = super::redact_output_secrets(text);
        assert_eq!(n, 0);
        assert_eq!(out, text);
    }

    #[test]
    fn redacts_secret_looking_keys() {
        let v = serde_json::json!({
            "path": "src/x.rs",
            "api_key": "sk-secret",
            "nested": { "token": "abc", "note": "keep" },
            "arr": [ { "password": "p" } ]
        });
        let r = super::redact_secrets(&v);
        assert_eq!(r["path"], "src/x.rs");
        assert_eq!(r["api_key"], "[redacted]");
        assert_eq!(r["nested"]["token"], "[redacted]");
        assert_eq!(r["nested"]["note"], "keep");
        assert_eq!(r["arr"][0]["password"], "[redacted]");
    }

    #[tokio::test]
    async fn audit_redacts_secrets_in_the_log() {
        let dir = std::env::temp_dir().join(format!("wm-audit-red-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("audit.log");
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let mut reg = ToolRegistry::new(ctx).with_audit(Some(log.clone()));
        reg.register(EchoTool);
        let _ = reg
            .dispatch("echo", serde_json::json!({ "api_key": "sk-leak" }))
            .await;
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains("sk-leak"),
            "secret leaked into audit log: {body}"
        );
        assert!(body.contains("[redacted]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_audit_when_disabled() {
        let dir = std::env::temp_dir().join(format!("wm-audit-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let mut reg = ToolRegistry::new(ctx); // no audit
        reg.register(EchoTool);
        let _ = reg.dispatch("echo", serde_json::json!({})).await;
        assert!(!dir.join("audit.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tool that sleeps longer than any deadline the tests arm.
    struct SlowTool(bool);

    #[async_trait]
    impl Tool for SlowTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "slow".into(),
                description: "slow".into(),
                input_schema: serde_json::json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutcome {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            ToolOutcome::ok("finished after all")
        }
        fn owns_timeout(&self) -> bool {
            self.0
        }
    }

    fn guarded(thresholds: Vec<u32>, exempt: Vec<&str>) -> ToolRegistry {
        let mut reg = ToolRegistry::new(ctx_in(PermissionMode::Yolo))
            .with_repeat_guard(thresholds, exempt.iter().map(|s| s.to_string()).collect());
        reg.register(EchoTool);
        reg
    }

    #[tokio::test(start_paused = true)]
    async fn backstop_deadline_abandons_a_wedged_tool() {
        let mut reg = ToolRegistry::new(ctx_in(PermissionMode::Yolo)).with_tool_timeout(5);
        reg.register(SlowTool(false));
        let out = reg.dispatch("slow", serde_json::json!({})).await;
        assert!(out.is_error, "a tool past its deadline must fail the call");
        assert!(out.content.contains("5s deadline"), "{}", out.content);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tool_that_owns_its_timeout_is_not_cut_short() {
        let mut reg = ToolRegistry::new(ctx_in(PermissionMode::Yolo)).with_tool_timeout(5);
        reg.register(SlowTool(true));
        let out = reg.dispatch("slow", serde_json::json!({})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "finished after all");
    }

    #[tokio::test]
    async fn repeat_guard_nudges_only_on_each_threshold() {
        let reg = guarded(vec![3, 5], vec![]);
        let args = serde_json::json!({ "q": "x" });
        let mut fired = Vec::new();
        for call in 1..=6 {
            // Sequential on purpose: the chain counts a *run*, so racing the
            // calls would make which one crosses the threshold arbitrary.
            if reg
                .dispatch("echo", args.clone())
                .await
                .content
                .contains("[wingman]")
            {
                fired.push(call);
            }
        }
        // 3rd call: first threshold. 5th: second. Not the 4th or 6th — one
        // reminder per threshold, not one per call once the run is long.
        assert_eq!(fired, vec![3, 5]);
    }

    #[tokio::test]
    async fn repeat_guard_resets_when_the_arguments_change() {
        let reg = guarded(vec![2], vec![]);
        for q in ["a", "b", "c"] {
            let out = reg.dispatch("echo", serde_json::json!({ "q": q })).await;
            assert!(!out.content.contains("[wingman]"), "reset failed at {q}");
        }
    }

    #[tokio::test]
    async fn argument_order_does_not_launder_a_repeat() {
        let reg = guarded(vec![2], vec![]);
        reg.dispatch("echo", serde_json::json!({ "a": 1, "b": 2 }))
            .await;
        // Same call, keys written the other way round.
        let out = reg
            .dispatch("echo", serde_json::json!({ "b": 2, "a": 1 }))
            .await;
        assert!(out.content.contains("[wingman]"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_exempt_tool_is_transparent_rather_than_resetting() {
        let mut reg = guarded(vec![2], vec!["book*"]);
        reg.register(CapTool("bookkeep", crate::Capability::NONE));
        reg.dispatch("echo", serde_json::json!({ "q": "x" })).await;
        // Interleaved bookkeeping must neither count nor clear the chain.
        let noise = reg.dispatch("bookkeep", serde_json::json!({})).await;
        assert!(!noise.content.contains("[wingman]"));
        let out = reg.dispatch("echo", serde_json::json!({ "q": "x" })).await;
        assert!(
            out.content.contains("[wingman]"),
            "bookkeeping laundered the loop: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn a_denied_call_still_counts_toward_the_chain() {
        let mut reg =
            ToolRegistry::new(ctx_in(PermissionMode::ReadOnly)).with_repeat_guard(vec![2], vec![]);
        reg.register(CapTool("writer", crate::Capability::WRITE));
        let first = reg.dispatch("writer", serde_json::json!({})).await;
        assert!(first.is_error, "read-only must refuse a write tool");
        let second = reg.dispatch("writer", serde_json::json!({})).await;
        assert!(
            second.content.contains("[wingman]"),
            "hammering a denied call is exactly the loop worth breaking: {}",
            second.content
        );
    }
}

#[cfg(test)]
mod documentation_drift_tests {
    use super::*;
    use crate::ToolCtx;
    use wingman_config::PermissionMode;

    use super::CONDITIONALLY_REGISTERED;

    fn documented_tool_names() -> Vec<String> {
        let reg = ToolRegistry::new(ToolCtx::new(
            PermissionMode::ReadOnly,
            std::env::temp_dir(),
            std::env::temp_dir(),
        ))
        .with_builtins();
        let mut names = reg.tool_names();
        names.extend(CONDITIONALLY_REGISTERED.iter().map(|s| s.to_string()));
        names.sort();
        names.dedup();
        names
    }

    /// Every tool the agent can call has a section in docs/TOOLS.md.
    ///
    /// This doc had drifted by seven tools — `ask_user`, `edit_symbol`,
    /// `outline`, `update_tasks` and the whole `lsp_*` family shipped
    /// undocumented. Drift is one-directional and invisible: tools get added,
    /// the doc does not, and nobody notices because nothing fails.
    ///
    /// Asserted rather than generated on purpose. TOOLS.md is mostly prose —
    /// when to reach for a tool, what it costs, how it fails — and generating
    /// it would replace explanation with a schema dump. A test keeps the
    /// enumeration honest and leaves the writing alone.
    #[test]
    fn every_tool_has_a_documented_section() {
        let doc = include_str!("../../../docs/TOOLS.md");
        let missing: Vec<String> = documented_tool_names()
            .into_iter()
            .filter(|name| !doc.contains(&format!("### `{name}`")))
            .collect();
        assert!(
            missing.is_empty(),
            "these tools ship undocumented - add a `### `name`` section to \
             docs/TOOLS.md for each: {missing:?}"
        );
    }

    /// …and a row in the summary table, which is what most readers scan.
    #[test]
    fn every_tool_has_a_summary_table_row() {
        let doc = include_str!("../../../docs/TOOLS.md");
        let missing: Vec<String> = documented_tool_names()
            .into_iter()
            .filter(|name| !doc.contains(&format!("| `{name}`")))
            .collect();
        assert!(
            missing.is_empty(),
            "these tools have a section but no summary-table row in \
             docs/TOOLS.md: {missing:?}"
        );
    }
}
