//! Top-level TUI application.
//!
//! Owns the terminal, the [`AgentLoop`], and the screen state. Runs an
//! event-driven outer loop for the idle state and a streaming inner loop
//! that selects between crossterm events and agent events while a turn is
//! in flight.

use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{future::BoxFuture, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::Widget,
    Terminal,
};
use wingman_core::{AgentEvent, AgentLoop, AgentStop, Provider};

use crate::modal::{
    ActiveModal, FilePicker, HelpModal, LoginTask, LoginWizard, McpServerSummary, McpTask, McpView,
    ModalOutcome, ModalTask, ModePicker, ModelPicker, ParamsModal, SessionEntry, SessionPicker,
    SkillsView, UsageView,
};
use crate::usage_store::LifetimeUsage;
use crate::widgets::{
    composer::ComposerView,
    file_tree::{FileTree, FileTreeView},
    slash_suggest::SlashSuggest,
    status::StatusView,
    transcript::TranscriptView,
    welcome::WelcomeView,
    Composer, StatusLine, Transcript, TranscriptItem,
};

/// Closure passed in by the CLI/runtime that knows how to construct a
/// provider for a given `provider_id`. We don't want `wingman-tui` to
/// depend on `wingman-providers` directly — this keeps the dependency
/// graph one-way and lets the TUI swap providers mid-session.
pub type ProviderBuilder =
    Arc<dyn Fn(&str) -> std::result::Result<Arc<dyn Provider>, String> + Send + Sync>;

/// Closure that builds a fresh [`AgentLoop`] for a given provider+model.
/// Used when the TUI launches without a provider configured and the user
/// finishes the `/login` wizard. Async because building the agent connects
/// MCP servers and may do other I/O.
pub type AgentBuilder = Arc<
    dyn Fn(String, String) -> BoxFuture<'static, std::result::Result<AgentLoop, String>>
        + Send
        + Sync,
>;

/// Closure the host registers so `/mode` can re-gate the live session's
/// tools. Applies the new [`PermissionMode`] to the running agent's shared
/// tool registry; a no-op host closure means "display only".
pub type ModeSetter = Arc<dyn Fn(wingman_config::PermissionMode) + Send + Sync>;
/// Records the user's acceptance of a presented plan. In `plan` mode this
/// is what unlocks writes and shell — see `ToolCtx::approve_plan`.
pub type PlanApprover = Arc<dyn Fn() + Send + Sync>;

/// Closure the host registers so the `/login` modal can ask it to perform
/// async work (probe a freshly-entered key, persist credentials, etc.)
/// without the TUI crate having to depend on `wingman-providers` or
/// `wingman-config`.
pub type LoginRunner =
    Arc<dyn Fn(LoginTask) -> BoxFuture<'static, std::result::Result<(), String>> + Send + Sync>;

/// Optional callback to clear a stored credential. Used by `/logout`.
pub type LogoutRunner = Arc<dyn Fn(String) -> std::result::Result<(), String> + Send + Sync>;

/// Runs one MCP server-management task on behalf of the modal.
pub type McpRunner =
    Arc<dyn Fn(McpTask) -> BoxFuture<'static, std::result::Result<(), String>> + Send + Sync>;

/// Returns the current set of MCP server summaries for display.
pub type McpListRunner = Arc<dyn Fn() -> BoxFuture<'static, Vec<McpServerSummary>> + Send + Sync>;

/// Fetches the live model catalog for one provider id (e.g. via an
/// OpenAI-compatible `GET /models`). `Err` means the provider can't list
/// models or the request failed; the picker then keeps its static entries.
pub type ModelsRunner = Arc<
    dyn Fn(String) -> BoxFuture<'static, std::result::Result<Vec<String>, String>> + Send + Sync,
>;

/// One past-session recall hit: `(session_id, score, snippet)`.
pub type RecallHit = (String, f32, String);

/// Runs a cross-session semantic recall for a query and returns ranked hits.
/// `Err` means recall is unavailable (no session index / embedder). Wired
/// from the runtime's session index so `/recall` searches for real instead of
/// telling the user to ask the agent.
pub type RecallRunner = Arc<
    dyn Fn(String) -> BoxFuture<'static, std::result::Result<Vec<RecallHit>, String>> + Send + Sync,
>;

/// Indexes the given session file into the recall index. Called once at
/// shutdown so the just-finished interactive session is recallable without
/// waiting for the next launch's startup backfill. Best-effort (returns `()`).
pub type SessionIndexer = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct AppCtx {
    pub provider_id: String,
    pub model: String,
    pub mode: String,
    /// `[tui].show_token_usage` — whether the status line shows the token/cost
    /// segment.
    pub show_token_usage: bool,
    pub project_root: PathBuf,
    pub builder: ProviderBuilder,
    pub agent_builder: AgentBuilder,
    pub login_runner: LoginRunner,
    pub logout_runner: LogoutRunner,
    pub mcp_runner: McpRunner,
    pub mcp_list_runner: McpListRunner,
    pub models_runner: ModelsRunner,
    pub mode_setter: ModeSetter,
    pub plan_approver: PlanApprover,
    /// Runs `/recall <query>` against the session index. `None` when the
    /// learning loop / session index is unavailable.
    pub recall_runner: Option<RecallRunner>,
    /// Indexes the session at shutdown so it's immediately recallable. `None`
    /// when the session index is unavailable.
    pub session_indexer: Option<SessionIndexer>,
}

pub async fn run(agent: Option<AgentLoop>, ctx: AppCtx) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut agent = agent;

    // Persist the interactive session so /resume, the session picker, and the
    // cross-run recall/learning index have something to read. Best-effort: a
    // failure to open the log must not stop the TUI. Written to the same dir
    // the /resume picker reads (`<project>/.wingman/sessions`).
    let sessions_dir = ctx.project_root.join(".wingman").join("sessions");
    // The agent loop records what the model saw; the TUI only opens the file.
    // It used to write the log itself, from the prompt text and the streamed
    // assistant text — which meant a TUI session's log contained no tool calls
    // and no tool results, and `/resume` rebuilt a conversation in which the
    // agent had never used a tool.
    let session = wingman_session::SessionLog::create(&sessions_dir)
        .await
        .ok()
        .map(|log| std::sync::Arc::new(wingman_session::SessionLogSink::new(log)));
    if let (Some(sink), Some(a)) = (&session, agent.as_mut()) {
        a.set_context_sink(sink.clone());
    }
    // Clone the shutdown indexer out before `ctx` moves into the loop.
    let session_indexer = ctx.session_indexer.clone();
    // Names the feedback rows this session writes, so a rating can be traced
    // back to the conversation that prompted it.
    let session_id = session
        .as_ref()
        .and_then(|s| s.path().file_stem().and_then(|n| n.to_str()))
        .unwrap_or("interactive")
        .to_string();
    let res = run_inner(&mut terminal, &mut agent, ctx, session_id).await;
    restore_terminal(&mut terminal)?;
    // Index this session now so it's recallable next time without waiting for a
    // startup backfill. Best-effort; only runs if the session index is live.
    if let (Some(index), Some(sink)) = (session_indexer, session.as_ref()) {
        index(sink.path().to_path_buf()).await;
    }
    res
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).context("creating terminal")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

enum Cmd {
    Quit,
    ApprovePlan,
    Clear,
    Undo(usize),
    Compact,
    Help,
    Mode(Option<String>),
    Model(Option<String>),
    Reasoning(Option<String>),
    Login,
    Logout(Option<String>),
    Add(String),
    Usage(String),
    Commit(String),
    Pr(String),
    Skills,
    SkillsNew(String),
    SkillsInstall {
        source: String,
        project: bool,
    },
    Skill(String),
    Mcp,
    Export(String),
    Params,
    Resume,
    // self-improvement commands
    Memory(String),     // "" = list, "forget <name>" = delete
    Recall(String),     // query for cross-session search
    SkillStats(String), // "" = all skills, "<name>" = one skill
    /// Explicit rating of the last answer: good/bad plus an optional note.
    Feedback(String),
    Learn(String), // "status" | "on" | "off"
    Find(String),  // search transcript
    FindNext,
    FindPrev,
    FindClear,
    Submit(String),
    None,
}

fn parse_slash(line: &str) -> Cmd {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return Cmd::Submit(line.to_string());
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match head {
        "/quit" | "/exit" | "/q" => Cmd::Quit,
        "/clear" => Cmd::Clear,
        "/undo" => Cmd::Undo(arg.trim().parse().unwrap_or(1)),
        "/compact" => Cmd::Compact,
        // Accept the agent's plan: in `plan` mode this is what unlocks
        // writes and shell for the rest of the session.
        "/approve" | "/approve-plan" => Cmd::ApprovePlan,
        "/help" | "/?" => Cmd::Help,
        "/reasoning" | "/think" => Cmd::Reasoning(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        }),
        "/mode" => Cmd::Mode(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        }),
        "/model" => Cmd::Model(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        }),
        "/login" | "/connect" => Cmd::Login,
        "/logout" => Cmd::Logout(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        }),
        "/add" if !arg.is_empty() => Cmd::Add(arg.to_string()),
        "/usage" => Cmd::Usage(arg.to_string()),
        "/commit" => Cmd::Commit(arg.to_string()),
        "/pr" => Cmd::Pr(arg.to_string()),
        "/skills" => {
            let mut it = arg.split_whitespace();
            match it.next() {
                Some("new") => {
                    let name = it.collect::<Vec<_>>().join(" ");
                    if name.is_empty() {
                        Cmd::Skills
                    } else {
                        Cmd::SkillsNew(name)
                    }
                }
                Some("install") | Some("add") => {
                    let mut project = false;
                    let mut source = String::new();
                    for t in it {
                        if t == "--project" || t == "-p" {
                            project = true;
                        } else if source.is_empty() {
                            source = t.to_string();
                        }
                    }
                    if source.is_empty() {
                        Cmd::Skills
                    } else {
                        Cmd::SkillsInstall { source, project }
                    }
                }
                _ => Cmd::Skills,
            }
        }
        "/feedback" | "/rate" => Cmd::Feedback(arg.to_string()),
        "/skill" if !arg.is_empty() => {
            if let Some(rest) = arg.strip_prefix("stats") {
                Cmd::SkillStats(rest.trim().to_string())
            } else {
                Cmd::Skill(arg.to_string())
            }
        }
        "/memory" => Cmd::Memory(arg.to_string()),
        "/recall" => Cmd::Recall(arg.to_string()),
        "/learn" => Cmd::Learn(arg.to_string()),
        "/mcp" => Cmd::Mcp,
        "/export" => Cmd::Export(if arg.is_empty() {
            "md".into()
        } else {
            arg.to_string()
        }),
        "/params" => Cmd::Params,
        "/resume" => Cmd::Resume,
        "/find" if !arg.is_empty() => Cmd::Find(arg.to_string()),
        "/findnext" | "/fn" => Cmd::FindNext,
        "/findprev" | "/fp" => Cmd::FindPrev,
        "/findclear" | "/fc" => Cmd::FindClear,
        "" => Cmd::None,
        _ => {
            // User-defined slash commands: ~/.wingman/commands/<name>.md or
            // <project>/.wingman/commands/<name>.md. The file body is the
            // prompt template; `$ARGS` (literal) is substituted with `arg`.
            if let Some(name) = head.strip_prefix('/') {
                if let Some(template) = load_user_command(name) {
                    let expanded = template.replace("$ARGS", arg);
                    return Cmd::Submit(expanded);
                }
            }
            Cmd::Submit(line.to_string())
        }
    }
}

/// Provider ids the user is currently connected to: the keys of the merged
/// config's `[providers]` table, plus the active provider. The `/model`
/// picker is restricted to these so it lists only what the user has logged
/// into rather than every provider Wingman can talk to. Reloaded each time
/// the picker opens so a mid-session `/login` is reflected. On a config-load
/// failure this returns just the active provider (empty if none), and an
/// empty result makes the picker fall back to the full catalog.
fn connected_provider_ids(active: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let global = wingman_config::global_config_path().ok();
    let project_file = std::env::current_dir()
        .ok()
        .map(|cwd| wingman_config::ProjectPaths::discover(&cwd).config_file)
        .filter(|p| p.exists());
    if let Ok(cfg) = wingman_config::Config::load(global.as_deref(), project_file.as_deref()) {
        ids.extend(cfg.providers.keys().cloned());
    }
    if !active.is_empty() && !ids.iter().any(|i| i == active) {
        ids.push(active.to_string());
    }
    ids
}

/// Split a `/model` argument into `(provider, model)` using the same rule the
/// CLI uses: a `/` only separates a provider when that provider is configured,
/// so `/model deepseek/deepseek-chat` on an OpenRouter setup selects the
/// aggregator's model rather than looking for a `[providers.deepseek]` section
/// that isn't there. Falls back to a plain split when config can't be read, so
/// the command still works on a broken config.
fn split_model_arg(arg: &str) -> Option<(String, String)> {
    let global = wingman_config::global_config_path().ok();
    let project_file = std::env::current_dir()
        .ok()
        .map(|cwd| wingman_config::ProjectPaths::discover(&cwd).config_file)
        .filter(|p| p.exists());
    if let Ok(cfg) = wingman_config::Config::load(global.as_deref(), project_file.as_deref()) {
        if let Some(pair) = cfg.resolve_model_spec(arg) {
            return Some(pair);
        }
    }
    arg.split_once('/')
        .map(|(p, m)| (p.to_string(), m.to_string()))
}

/// Apply a permission-mode selection to the status line. Validates and
/// normalises `raw` via [`wingman_config::PermissionMode`]; on an unknown
/// value it surfaces an error and leaves the current mode unchanged. Shared
/// by the `/mode <name>` direct path and the `/mode` picker.
fn apply_mode(ui: &mut UiState, raw: &str, mode_setter: &ModeSetter) {
    match raw.parse::<wingman_config::PermissionMode>() {
        Ok(mode) => {
            let normalized = mode.to_string();
            ui.status.mode = normalized.clone();
            // Re-gate the running agent's tools, not just the status line.
            mode_setter(mode);
            ui.transcript
                .push(TranscriptItem::System(format!("mode set to {normalized}")));
        }
        Err(e) => {
            ui.transcript
                .push(TranscriptItem::Error(format!("/mode: {e}")));
        }
    }
}

fn load_user_command(name: &str) -> Option<String> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // Project-local first, then global.
    if let Ok(cwd) = std::env::current_dir() {
        let project = wingman_config::ProjectPaths::discover(&cwd);
        let p = project
            .root
            .join(".wingman")
            .join("commands")
            .join(format!("{name}.md"));
        if let Ok(text) = std::fs::read_to_string(&p) {
            return Some(text);
        }
    }
    if let Ok(global) = wingman_config::global_dir() {
        let p = global.join("commands").join(format!("{name}.md"));
        if let Ok(text) = std::fs::read_to_string(&p) {
            return Some(text);
        }
    }
    None
}

struct UiState {
    transcript: Transcript,
    composer: Composer,
    status: StatusLine,
    modal: ActiveModal,
    /// Snapshot of `~/.wingman/usage.json` as it was at startup. The
    /// `/usage` modal's "Lifetime" tab renders `lifetime + status.usage`.
    lifetime: LifetimeUsage,
    /// Skill chosen via `/skill <name>` or the skills modal; its body is
    /// prepended to the next user prompt and then cleared.
    pending_skill: Option<wingman_skills::Skill>,
    /// Inline slash-command autocomplete that floats above the composer.
    slash: SlashSuggest,
    /// Toggleable left sidebar file tree.
    sidebar: Option<FileTree>,
    /// Live checklist from the model's `update_tasks` tool. Empty = hidden.
    tasks: Vec<crate::widgets::TaskItem>,
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut Option<AgentLoop>,
    ctx: AppCtx,
    session_id_for_feedback: String,
) -> Result<()> {
    let mut ui = UiState {
        transcript: Transcript::default(),
        composer: Composer::default(),
        status: StatusLine {
            model: ctx.model.clone(),
            provider: ctx.provider_id.clone(),
            mode: ctx.mode.clone(),
            connected: agent.is_some(),
            show_token_usage: ctx.show_token_usage,
            ..Default::default()
        },
        modal: ActiveModal::None,
        lifetime: LifetimeUsage::load(),
        pending_skill: None,
        slash: SlashSuggest::default(),
        sidebar: None,
        tasks: Vec::new(),
    };
    ui.status.refresh_git(&ctx.project_root);
    let mut events = EventStream::new();
    loop {
        ui.composer.busy = false;
        draw(terminal, &ui)?;

        // Pump any async task the active modal has requested before we go
        // back to waiting on input. Each iteration handles one task; the
        // modal may queue another (e.g. Probe success → Commit).
        if let Some(task) = ui.modal.take_pending_task() {
            run_modal_task(task, &mut ui, agent, &ctx).await?;
            continue;
        }

        // Idle: wait for a user input event.
        let next_action = idle_step(
            &mut events,
            &mut ui,
            terminal,
            agent,
            &ctx,
            &session_id_for_feedback,
        )
        .await?;
        match next_action {
            IdleAction::Quit => {
                // Flush lifetime usage one last time on the way out.
                ui.lifetime.save_merged(&ui.status.usage);
                // Reap language servers. They are long-lived children
                // (rust-analyzer especially) held by a process-wide registry,
                // so without this they survived until the process died.
                wingman_lsp::shutdown_all_managers().await;
                return Ok(());
            }
            // A modal queued a task; loop back so the pump at the top of
            // the loop drains it (take_pending_task → run_modal_task).
            IdleAction::PumpModal => continue,
            IdleAction::Submit(prompt) => {
                ui.transcript
                    .push(TranscriptItem::UserPrompt(prompt.clone()));
                match agent.as_mut() {
                    Some(a) => {
                        // Inline any `@<path>` attachments before sending.
                        // The transcript shows the literal user input; the
                        // model sees the expanded form.
                        let exp = crate::attachments::expand(&prompt, &ctx.project_root);
                        for w in &exp.warnings {
                            ui.transcript
                                .push(TranscriptItem::System(format!("attachment: {w}")));
                        }
                        if exp.attached > 0 {
                            ui.transcript.push(TranscriptItem::System(format!(
                                "attached {} file{}",
                                exp.attached,
                                if exp.attached == 1 { "" } else { "s" }
                            )));
                        }
                        if !exp.images.is_empty() {
                            ui.transcript.push(TranscriptItem::System(format!(
                                "Vision: {} image(s) attached. Full image content requires provider vision support.",
                                exp.images.len()
                            )));
                        }
                        // If a skill was queued, prepend its body and clear
                        // the slot — skills are one-shot.
                        let final_prompt = match ui.pending_skill.take() {
                            Some(skill) => {
                                ui.transcript.push(TranscriptItem::System(format!(
                                    "applying skill '{}'",
                                    skill.name
                                )));
                                format!("{}\n\n{}", skill.body, exp.prompt)
                            }
                            None => exp.prompt,
                        };
                        ui.composer.busy = true;
                        let steer = a.steer_handle();
                        draw(terminal, &ui)?;
                        run_turn(
                            terminal,
                            a,
                            &mut events,
                            &mut ui,
                            final_prompt,
                            steer.clone(),
                        )
                        .await?;
                        // Persist after every turn: an LLM round-trip already
                        // took seconds, so one small atomic write is noise, and
                        // it means an external kill/SIGHUP between turns can't
                        // lose recorded usage. ponytail: only usage from a turn
                        // interrupted mid-stream is at risk, not worth a global
                        // signal-handler mirror to recover.
                        ui.lifetime.save_merged(&ui.status.usage);
                        // The agent may have edited files or committed; refresh
                        // the branch/dirty indicator.
                        ui.status.refresh_git(&ctx.project_root);
                    }
                    None => {
                        ui.transcript.push(TranscriptItem::Error(
                            "No provider configured — run /login to set one up.".into(),
                        ));
                    }
                }
            }
        }
    }
}

/// Execute one modal-requested task and report the result back to the
/// modal. On a successful commit, also constructs a fresh `AgentLoop` via
/// `agent_builder`, swaps it into the session, updates the status line,
/// and closes the modal.
async fn run_modal_task(
    task: ModalTask,
    ui: &mut UiState,
    agent: &mut Option<AgentLoop>,
    ctx: &AppCtx,
) -> Result<()> {
    match task {
        ModalTask::Models(provider_ids) => {
            // Fetch each connected provider's live catalog. A provider that
            // errors (no listing endpoint, network failure) is skipped and
            // keeps its static entries.
            let mut fetched: Vec<(String, Vec<String>)> = Vec::new();
            for id in provider_ids {
                if let Ok(models) = (ctx.models_runner)(id.clone()).await {
                    fetched.push((id, models));
                }
            }
            if let ActiveModal::ModelPicker(p) = &mut ui.modal {
                p.set_dynamic(fetched);
            }
        }
        ModalTask::Mcp(mcp_task) => {
            let result = (ctx.mcp_runner)(mcp_task).await;
            ui.modal.task_completed(result);
            // Refresh the server list either way — even a failed task may
            // have partially mutated registry state.
            let fresh = (ctx.mcp_list_runner)().await;
            if let ActiveModal::Mcp(v) = &mut ui.modal {
                v.set_servers(fresh);
            }
        }
        ModalTask::Login(login_task) => {
            // Remember which kind of task we ran so we can branch after the
            // result comes back — Commit success also has to build a new
            // agent and close the modal.
            let was_commit = matches!(login_task, LoginTask::Commit(_));
            let payload_after = if let LoginTask::Commit(ref p) = login_task {
                Some(p.clone())
            } else {
                None
            };

            let result = (ctx.login_runner)(login_task).await;
            ui.modal.task_completed(result.clone());

            if was_commit {
                if let Ok(()) = result {
                    let payload = payload_after.expect("commit task carries payload");
                    ui.status.provider = payload.provider_id.clone();
                    ui.status.model = payload.model.clone();
                    ui.modal = ActiveModal::None;
                    ui.transcript.push(TranscriptItem::System(format!(
                        "saving credentials for {}/{}…",
                        payload.provider_id, payload.model
                    )));

                    match (ctx.agent_builder)(payload.provider_id.clone(), payload.model.clone())
                        .await
                    {
                        Ok(new_agent) => {
                            *agent = Some(new_agent);
                            ui.status.connected = true;
                            ui.transcript.push(TranscriptItem::System(format!(
                                "connected to {}/{}",
                                payload.provider_id, payload.model
                            )));
                        }
                        Err(e) => {
                            ui.transcript.push(TranscriptItem::System(format!(
                                "failed to build agent: {e}"
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

enum IdleAction {
    Quit,
    Submit(String),
    /// A modal queued an async task (e.g. the /login commit). Hand control
    /// back to the outer loop so it drains the task via `take_pending_task`
    /// instead of blocking here on the next key.
    PumpModal,
}

async fn idle_step(
    events: &mut EventStream,
    ui: &mut UiState,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut Option<AgentLoop>,
    ctx: &AppCtx,
    session_id_for_feedback: &str,
) -> Result<IdleAction> {
    let builder = &ctx.builder;
    let logout_runner = &ctx.logout_runner;
    let project_root = &ctx.project_root;
    while let Some(ev) = events.next().await {
        match ev {
            Ok(CtEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c'))
                {
                    return Ok(IdleAction::Quit);
                }
                // Ctrl+B toggles the file-tree sidebar. When focused (i.e.
                // sidebar is Some AND the composer is empty), navigation
                // keys steer the sidebar instead of the composer.
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('b'))
                {
                    if ui.sidebar.is_some() {
                        ui.sidebar = None;
                    } else {
                        ui.sidebar = Some(FileTree::new(project_root.clone()));
                    }
                    draw(terminal, ui)?;
                    continue;
                }
                // When the sidebar is open AND composer is empty, j/k/Up/
                // Down move the sidebar selection; Enter picks; Tab/Backspace
                // descend/ascend; Esc closes the sidebar.
                if let Some(tree) = ui.sidebar.as_mut() {
                    if ui.composer.input.is_empty() {
                        match k.code {
                            KeyCode::Char('j') | KeyCode::Down => {
                                tree.move_down();
                                draw(terminal, ui)?;
                                continue;
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                tree.move_up();
                                draw(terminal, ui)?;
                                continue;
                            }
                            KeyCode::Enter => {
                                if let Some(path) = tree.enter() {
                                    let rel = tree.pick_relative(&path);
                                    ui.composer.input.push_str(&format!("@{rel} "));
                                    ui.sidebar = None;
                                }
                                draw(terminal, ui)?;
                                continue;
                            }
                            KeyCode::Tab => {
                                let _ = tree.enter();
                                draw(terminal, ui)?;
                                continue;
                            }
                            KeyCode::Backspace => {
                                if let Some(parent) = tree.cwd.parent() {
                                    if tree.cwd != tree.root {
                                        tree.cwd = parent.to_path_buf();
                                        tree.selected = 0;
                                        tree.refresh();
                                    }
                                }
                                draw(terminal, ui)?;
                                continue;
                            }
                            KeyCode::Esc => {
                                ui.sidebar = None;
                                draw(terminal, ui)?;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
                // If a modal is open, it gets first crack at the key. Esc
                // always closes the modal regardless of the modal's own
                // handler. Modal-specific finalization (e.g. file picker:
                // insert selected path into the composer) happens here on
                // a ModalOutcome::Close.
                if ui.modal.is_open() {
                    if matches!(k.code, KeyCode::Esc) {
                        ui.modal = ActiveModal::None;
                    } else {
                        match ui.modal.handle_key(k) {
                            ModalOutcome::Continue => {}
                            ModalOutcome::Close => {
                                match &mut ui.modal {
                                    ActiveModal::FilePicker(p) => {
                                        if let Some(path) = p.take_selected() {
                                            ui.composer.input.push_str(&format!("@{path} "));
                                        }
                                    }
                                    ActiveModal::ModelPicker(p) => {
                                        if let Some(choice) = p.take_selected() {
                                            swap_model(
                                                ui,
                                                agent,
                                                builder,
                                                &choice.provider_id,
                                                &choice.model,
                                            );
                                        }
                                    }
                                    ActiveModal::ModePicker(p) => {
                                        if let Some(mode) = p.take_selected() {
                                            apply_mode(ui, &mode, &ctx.mode_setter);
                                        }
                                    }
                                    ActiveModal::Skills(v) => {
                                        if let Some(s) = v.take_selected() {
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "skill '{}' queued for next prompt",
                                                s.name
                                            )));
                                            ui.pending_skill = Some(s);
                                        }
                                    }
                                    ActiveModal::Params(p) => {
                                        if let Some((temp, max_tok)) = p.take_result() {
                                            if let Some(a) = agent.as_mut() {
                                                a.set_temperature(temp);
                                                a.set_max_tokens(max_tok);
                                                ui.transcript.push(TranscriptItem::System(format!(
                                                    "params updated: temperature={}, max_tokens={max_tok}",
                                                    temp.map(|t| t.to_string())
                                                        .unwrap_or_else(|| "default".to_string()),
                                                )));
                                            }
                                        }
                                    }
                                    ActiveModal::SessionPicker(p) => {
                                        if let Some(entry) = p.take_selected() {
                                            resume_session(agent, ui, entry);
                                        }
                                    }
                                    _ => {}
                                }
                                ui.modal = ActiveModal::None;
                            }
                        }
                    }
                    draw(terminal, ui)?;
                    // If the modal just queued an async task (e.g. /login
                    // commit), return to the outer loop so it gets pumped;
                    // otherwise we'd block here on the next key and the
                    // modal would freeze mid-task (e.g. "Saving credentials…").
                    if ui.modal.has_pending_task() {
                        return Ok(IdleAction::PumpModal);
                    }
                    continue;
                }
                match k.code {
                    KeyCode::Enter => {
                        let raw = ui.composer.take_input();
                        ui.slash.update(&ui.composer.input);
                        if raw.trim().is_empty() {
                            draw(terminal, ui)?;
                            continue;
                        }
                        match parse_slash(&raw) {
                            Cmd::Quit => return Ok(IdleAction::Quit),
                            Cmd::Help => {
                                ui.transcript.push(TranscriptItem::System(help_text()));
                            }
                            Cmd::Clear => {
                                ui.transcript.clear();
                            }
                            Cmd::Undo(n) => {
                                let mut done = 0;
                                for _ in 0..n.max(1) {
                                    match wingman_core::checkpoint::undo_last(project_root) {
                                        Some(msg) => {
                                            done += 1;
                                            ui.transcript.push(TranscriptItem::System(msg));
                                        }
                                        None => break,
                                    }
                                }
                                if done == 0 {
                                    ui.transcript
                                        .push(TranscriptItem::System("nothing to undo".into()));
                                }
                            }
                            Cmd::ApprovePlan => {
                                if ui.status.mode == "plan" {
                                    (ctx.plan_approver)();
                                    ui.transcript.push(TranscriptItem::System(
                                        "plan approved — writes inside the project and shell are now permitted for this session (`/mode plan` again to revoke)"
                                            .to_string(),
                                    ));
                                } else {
                                    ui.transcript.push(TranscriptItem::System(format!(
                                        "/approve applies in plan mode (currently {}); run `/mode plan` first",
                                        ui.status.mode
                                    )));
                                }
                            }
                            Cmd::Compact => match agent.as_mut() {
                                Some(a) => {
                                    let before = a.context_tokens();
                                    match a.compact_now() {
                                        Some(n) => {
                                            let after = a.context_tokens();
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "compacted {n} message(s) into a recap · context ~{before} → ~{after} tokens"
                                            )));
                                        }
                                        None => ui.transcript.push(TranscriptItem::System(
                                            "nothing to compact yet".into(),
                                        )),
                                    }
                                }
                                None => ui.transcript.push(TranscriptItem::Error(
                                    "no provider — run /login first".into(),
                                )),
                            },
                            Cmd::Reasoning(arg) => match agent.as_mut() {
                                Some(a) => {
                                    let msg = match arg {
                                        None => format!("reasoning: {}", a.get_reasoning()),
                                        Some(v) => match wingman_core::ReasoningEffort::parse(&v) {
                                            Some(r) => {
                                                a.set_reasoning(r);
                                                if r.is_on() && !a.provider_supports_reasoning() {
                                                    format!(
                                                        "reasoning: {r} — but this provider has no reasoning control, so it will be ignored"
                                                    )
                                                } else {
                                                    format!("reasoning: {r}")
                                                }
                                            }
                                            None => {
                                                "usage: /reasoning [off|low|medium|high]".into()
                                            }
                                        },
                                    };
                                    ui.transcript.push(TranscriptItem::System(msg));
                                }
                                None => ui.transcript.push(TranscriptItem::Error(
                                    "no provider — run /login first".into(),
                                )),
                            },
                            Cmd::Mode(None) => {
                                ui.modal =
                                    ActiveModal::ModePicker(ModePicker::new(&ui.status.mode));
                            }
                            Cmd::Mode(Some(arg)) => {
                                apply_mode(ui, &arg, &ctx.mode_setter);
                            }
                            Cmd::Model(None) => {
                                let connected = connected_provider_ids(&ui.status.provider);
                                ui.modal = ActiveModal::ModelPicker(ModelPicker::new(&connected));
                                // Kick off the live-catalog fetch: draw the
                                // picker (with its "fetching…" hint) and hand
                                // control to the outer task pump.
                                if ui.modal.has_pending_task() {
                                    draw(terminal, ui)?;
                                    return Ok(IdleAction::PumpModal);
                                }
                            }
                            Cmd::Model(Some(arg)) => match split_model_arg(&arg) {
                                Some((provider_id, model_id)) => {
                                    swap_model(ui, agent, builder, &provider_id, &model_id);
                                }
                                None => {
                                    ui.transcript.push(TranscriptItem::Error(
                                            "/model expects provider/model_id (e.g. anthropic/claude-opus-4-7) or no argument to pick from a list".into(),
                                        ));
                                }
                            },
                            Cmd::Login => {
                                ui.modal = ActiveModal::Login(LoginWizard::new());
                            }
                            Cmd::Logout(target) => {
                                let provider_id =
                                    target.unwrap_or_else(|| ui.status.provider.clone());
                                if provider_id.is_empty() {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/logout: no provider to log out of".into(),
                                    ));
                                } else {
                                    match logout_runner(provider_id.clone()) {
                                        Ok(()) => {
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "logged out of {provider_id}"
                                            )));
                                            // If we just logged out of the
                                            // active provider, clear the
                                            // session agent so the user is
                                            // forced through /login again.
                                            if ui.status.provider == provider_id {
                                                *agent = None;
                                                ui.status.provider.clear();
                                                ui.status.model.clear();
                                                ui.status.connected = false;
                                            }
                                        }
                                        Err(e) => {
                                            ui.transcript.push(TranscriptItem::Error(format!(
                                                "/logout {provider_id}: {e}"
                                            )));
                                        }
                                    }
                                }
                            }
                            Cmd::Add(path) => {
                                ui.composer.input.push_str(&format!("@{} ", path.trim()));
                            }
                            Cmd::Commit(msg) => {
                                let msg = msg.trim();
                                if msg.is_empty() {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/commit <message> — or ask the agent to write one".into(),
                                    ));
                                } else {
                                    match git_commit(project_root, msg) {
                                        Ok(out) => ui.transcript.push(TranscriptItem::System(out)),
                                        Err(e) => ui
                                            .transcript
                                            .push(TranscriptItem::Error(format!("/commit: {e}"))),
                                    }
                                    ui.status.refresh_git(project_root);
                                }
                            }
                            Cmd::Pr(title) => {
                                ui.transcript
                                    .push(TranscriptItem::System("opening pull request…".into()));
                                draw(terminal, ui)?;
                                match git_open_pr(project_root, title.trim()) {
                                    Ok(out) => ui.transcript.push(TranscriptItem::System(out)),
                                    Err(e) => ui
                                        .transcript
                                        .push(TranscriptItem::Error(format!("/pr: {e}"))),
                                }
                                ui.status.refresh_git(project_root);
                            }
                            Cmd::Usage(arg) => {
                                if arg.trim().eq_ignore_ascii_case("clear") {
                                    ui.lifetime.clear();
                                    ui.status.usage.clear();
                                    ui.transcript.push(TranscriptItem::System(
                                        "usage cleared (session + lifetime)".into(),
                                    ));
                                } else {
                                    let lifetime = ui.lifetime.combined(&ui.status.usage);
                                    ui.modal = ActiveModal::Usage(UsageView::new(
                                        ui.status.usage.clone(),
                                        lifetime,
                                    ));
                                }
                            }
                            Cmd::Skills => {
                                let skills = wingman_skills::load_all(project_root);
                                ui.modal = ActiveModal::Skills(SkillsView::new(skills));
                            }
                            Cmd::Skill(name) => {
                                let skills = wingman_skills::load_all(project_root);
                                match skills.into_iter().find(|s| s.name == name) {
                                    Some(s) => {
                                        ui.transcript.push(TranscriptItem::System(format!(
                                            "skill '{}' queued for next prompt",
                                            s.name
                                        )));
                                        ui.pending_skill = Some(s);
                                    }
                                    None => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/skill: no skill named '{name}'"
                                        )));
                                    }
                                }
                            }
                            Cmd::Mcp => {
                                let servers = (ctx.mcp_list_runner)().await;
                                ui.modal = ActiveModal::Mcp(McpView::new(servers));
                            }
                            Cmd::Export(fmt) => {
                                let path = export_transcript(&ui.transcript, &fmt, project_root);
                                match path {
                                    Ok(p) => ui.transcript.push(TranscriptItem::System(format!(
                                        "exported to {}",
                                        p.display()
                                    ))),
                                    Err(e) => ui
                                        .transcript
                                        .push(TranscriptItem::Error(format!("/export: {e}"))),
                                }
                            }
                            Cmd::Params => {
                                if let Some(a) = agent.as_ref() {
                                    ui.modal = ActiveModal::Params(ParamsModal::new(
                                        a.get_temperature(),
                                        a.get_max_tokens(),
                                    ));
                                } else {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/params: no active agent — run /login first".into(),
                                    ));
                                }
                            }
                            Cmd::Resume => {
                                let sessions_dir = project_root.join(".wingman").join("sessions");
                                let paths = wingman_session::list_sessions(&sessions_dir);
                                let entries: Vec<SessionEntry> = paths
                                    .into_iter()
                                    .take(20)
                                    .map(|p| {
                                        let label = p
                                            .file_stem()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("unknown")
                                            .to_string();
                                        let (provider, model) =
                                            match wingman_session::load_session(&p) {
                                                Ok(records) => {
                                                    wingman_session::session_meta(&records)
                                                        .unwrap_or_else(|| {
                                                            ("unknown".into(), "unknown".into())
                                                        })
                                                }
                                                Err(_) => ("unknown".into(), "unknown".into()),
                                            };
                                        SessionEntry {
                                            path: p,
                                            label,
                                            provider,
                                            model,
                                        }
                                    })
                                    .collect();
                                ui.modal = ActiveModal::SessionPicker(SessionPicker::new(entries));
                            }
                            Cmd::Memory(arg) => {
                                let store =
                                    wingman_learn::memory::MemoryStore::new(project_root.clone());
                                if let Some(name) = arg.strip_prefix("forget ") {
                                    match store.forget(name.trim()) {
                                        Ok(true) => ui.transcript.push(TranscriptItem::System(
                                            format!("forgot memory '{}'", name.trim()),
                                        )),
                                        Ok(false) => ui.transcript.push(TranscriptItem::Error(
                                            format!("no memory named '{}'", name.trim()),
                                        )),
                                        Err(e) => ui.transcript.push(TranscriptItem::Error(
                                            format!("/memory forget: {e}"),
                                        )),
                                    }
                                } else if arg.is_empty() {
                                    let mems = store.load_all();
                                    if mems.is_empty() {
                                        ui.transcript.push(TranscriptItem::System(
                                            "(no memories yet — the agent will save them as it learns; \
                                             you can ask it to 'remember X' to trigger one now)"
                                                .into(),
                                        ));
                                    } else {
                                        let mut out = String::from("memories:\n");
                                        for m in mems {
                                            out.push_str(&format!(
                                                "  [{}] {} ({}) — {}\n",
                                                m.mtype.as_str(),
                                                m.name,
                                                m.scope.label(),
                                                m.description
                                            ));
                                        }
                                        ui.transcript.push(TranscriptItem::System(out));
                                    }
                                } else {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/memory: usage is `/memory` (list) or `/memory forget <name>`"
                                            .into(),
                                    ));
                                }
                            }
                            Cmd::Recall(query) => {
                                if query.is_empty() {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/recall: usage is `/recall <query>`".into(),
                                    ));
                                } else if let Some(runner) = &ctx.recall_runner {
                                    match runner(query.clone()).await {
                                        Ok(hits) if hits.is_empty() => {
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "/recall: no past sessions matched '{query}'"
                                            )));
                                        }
                                        Ok(hits) => {
                                            let mut out = format!("recall '{query}':\n");
                                            for (id, score, snippet) in hits.iter().take(8) {
                                                let one_line: String = snippet
                                                    .replace('\n', " ")
                                                    .chars()
                                                    .take(160)
                                                    .collect();
                                                out.push_str(&format!(
                                                    "  [{score:.2}] {id}: {one_line}\n"
                                                ));
                                            }
                                            ui.transcript.push(TranscriptItem::System(out));
                                        }
                                        Err(e) => ui.transcript.push(TranscriptItem::Error(
                                            format!("/recall failed: {e}"),
                                        )),
                                    }
                                } else {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/recall unavailable: the session index is disabled \
                                         (needs the learning loop)"
                                            .into(),
                                    ));
                                }
                            }
                            Cmd::Feedback(arg) => {
                                let (word, note) = match arg.split_once(char::is_whitespace) {
                                    Some((w, rest)) => (w, Some(rest.trim())),
                                    None => (arg.as_str(), None),
                                };
                                let note = note.filter(|n| !n.is_empty());
                                let Some(rating) = wingman_learn::stats::Rating::parse(word) else {
                                    ui.transcript.push(TranscriptItem::System(
                                        "usage: /feedback good|bad [note] — rates the last answer \
                                         so skill stats learn from what you actually thought"
                                            .into(),
                                    ));
                                    draw(terminal, ui)?;
                                    continue;
                                };
                                match wingman_learn::stats::StatsStore::open_default() {
                                    Ok(stats) => {
                                        // A rating refers to what you just saw.
                                        // Half an hour is generous for that and
                                        // still short enough that it cannot
                                        // land on yesterday's work.
                                        let applied = stats.apply_feedback(
                                            session_id_for_feedback,
                                            rating,
                                            note,
                                            chrono::Duration::minutes(30),
                                        );
                                        let msg = match applied {
                                            Ok(wingman_learn::stats::FeedbackApplied::ScoredSkill {
                                                skill_name,
                                            }) => format!(
                                                "recorded {} — scored skill '{skill_name}' from \
                                                 what you said, not the phrase heuristic",
                                                rating.as_str()
                                            ),
                                            Ok(wingman_learn::stats::FeedbackApplied::RecordedOnly) => {
                                                format!(
                                                    "recorded {} — no recent skill invocation to \
                                                     attach it to",
                                                    rating.as_str()
                                                )
                                            }
                                            Err(e) => format!("/feedback: {e}"),
                                        };
                                        ui.transcript.push(TranscriptItem::System(msg));
                                    }
                                    Err(e) => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/feedback: open learn.db: {e}"
                                        )));
                                    }
                                }
                                draw(terminal, ui)?;
                            }
                            Cmd::SkillStats(name) => {
                                let stats = match wingman_learn::stats::StatsStore::open_default() {
                                    Ok(s) => s,
                                    Err(e) => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/skill stats: open learn.db: {e}"
                                        )));
                                        draw(terminal, ui)?;
                                        continue;
                                    }
                                };
                                let mut out = String::new();
                                if name.is_empty() {
                                    let sum = stats.summary().unwrap_or_default();
                                    if sum.is_empty() {
                                        out.push_str("(no skill invocations recorded yet)");
                                    } else {
                                        out.push_str("skill stats:\n");
                                        for r in sum {
                                            let pct = (r.correction_rate() * 100.0) as u32;
                                            let flag = if r.needs_rewrite() {
                                                " (needs rewrite)"
                                            } else {
                                                ""
                                            };
                                            // `rated` is the count the user
                                            // stated outright. The rest was
                                            // inferred by a phrase heuristic
                                            // that scores any non-correction
                                            // as success, so a high ok= with
                                            // rated=0 means very little.
                                            out.push_str(&format!(
                                                "  {:<28} ok={:<4} corrected={:<4} unclear={:<4} rated={:<4} ({pct}% corrected){flag}\n",
                                                r.skill_name,
                                                r.success,
                                                r.corrected,
                                                r.unclear,
                                                r.explicit,
                                            ));
                                        }
                                    }
                                } else {
                                    let rows = stats.recent(&name, 20).unwrap_or_default();
                                    if rows.is_empty() {
                                        out.push_str(&format!("(no rows for '{name}')"));
                                    } else {
                                        out.push_str(&format!("recent invocations of '{name}':\n"));
                                        for r in rows {
                                            out.push_str(&format!(
                                                "  {} {} {}\n",
                                                r.ts,
                                                r.outcome.as_str(),
                                                r.signal.unwrap_or_default()
                                            ));
                                        }
                                    }
                                }
                                ui.transcript.push(TranscriptItem::System(out));
                            }
                            Cmd::Learn(arg) => {
                                let stats = match wingman_learn::stats::StatsStore::open_default() {
                                    Ok(s) => s,
                                    Err(e) => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/learn: open learn.db: {e}"
                                        )));
                                        draw(terminal, ui)?;
                                        continue;
                                    }
                                };
                                match arg.as_str() {
                                    "" | "status" => {
                                        let quiet =
                                            stats.counter_get("sessions_without_save").unwrap_or(0);
                                        let total_skills =
                                            stats.summary().map(|v| v.len()).unwrap_or(0);
                                        let store = wingman_learn::memory::MemoryStore::new(
                                            project_root.clone(),
                                        );
                                        let mem_count = store.load_all().len();
                                        ui.transcript.push(TranscriptItem::System(format!(
                                            "learn status:\n  \
                                             memories: {mem_count}\n  \
                                             tracked skills: {total_skills}\n  \
                                             sessions without a save: {quiet}\n  \
                                             nudges fire at: {}",
                                            wingman_learn::proposal::NUDGE_AFTER_N_QUIET_SESSIONS,
                                        )));
                                    }
                                    "reset" => {
                                        let _ = stats.counter_set("sessions_without_save", 0);
                                        ui.transcript.push(TranscriptItem::System(
                                            "learn: quiet-session counter reset".into(),
                                        ));
                                    }
                                    other => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/learn: unknown subcommand '{other}' (expected status|reset)"
                                        )));
                                    }
                                }
                            }
                            Cmd::SkillsNew(name) => match wingman_skills::new_global_path(&name) {
                                Ok(path) => {
                                    if !path.exists() {
                                        if let Err(e) = std::fs::write(
                                            &path,
                                            wingman_skills::starter_template(&name),
                                        ) {
                                            ui.transcript.push(TranscriptItem::Error(format!(
                                                "/skills new: write failed: {e}"
                                            )));
                                            draw(terminal, ui)?;
                                            continue;
                                        }
                                    }
                                    if let Err(e) = launch_editor(terminal, &path) {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/skills new: editor: {e}"
                                        )));
                                    } else {
                                        ui.transcript.push(TranscriptItem::System(format!(
                                            "skill saved at {}",
                                            path.display()
                                        )));
                                    }
                                }
                                Err(e) => {
                                    ui.transcript
                                        .push(TranscriptItem::Error(format!("/skills new: {e}")));
                                }
                            },
                            Cmd::SkillsInstall { source, project } => {
                                let scope = if project {
                                    wingman_skills::InstallScope::Project
                                } else {
                                    wingman_skills::InstallScope::Global
                                };
                                ui.transcript.push(TranscriptItem::System(format!(
                                    "installing skill from '{source}'…"
                                )));
                                draw(terminal, ui)?;
                                match install_skill(&source, project_root, scope).await {
                                    Ok(skill) => ui.transcript.push(TranscriptItem::System(
                                        format!(
                                            "installed skill '{}'{} — use /skill {} (or the agent can invoke it)",
                                            skill.name,
                                            if skill.description.is_empty() {
                                                String::new()
                                            } else {
                                                format!(": {}", skill.description)
                                            },
                                            skill.name,
                                        ),
                                    )),
                                    Err(e) => ui.transcript.push(TranscriptItem::Error(format!(
                                        "/skills install: {e}"
                                    ))),
                                }
                            }
                            Cmd::Find(q) => {
                                let n = ui.transcript.search_set(&q);
                                ui.transcript.push(TranscriptItem::System(format!(
                                    "/find: {n} match{} for '{q}' (/findnext, /findprev, /findclear)",
                                    if n == 1 { "" } else { "es" }
                                )));
                            }
                            Cmd::FindNext => {
                                if ui.transcript.search.is_some() {
                                    ui.transcript.search_next();
                                } else {
                                    ui.transcript.push(TranscriptItem::System(
                                        "/findnext: no active search (run /find <query>)".into(),
                                    ));
                                }
                            }
                            Cmd::FindPrev => {
                                if ui.transcript.search.is_some() {
                                    ui.transcript.search_prev();
                                } else {
                                    ui.transcript.push(TranscriptItem::System(
                                        "/findprev: no active search (run /find <query>)".into(),
                                    ));
                                }
                            }
                            Cmd::FindClear => {
                                ui.transcript.search_clear();
                                ui.transcript.push(TranscriptItem::System(
                                    "/findclear: search cleared".into(),
                                ));
                            }
                            Cmd::None => {}
                            Cmd::Submit(prompt) => return Ok(IdleAction::Submit(prompt)),
                        }
                    }
                    KeyCode::Backspace => {
                        ui.composer.input.pop();
                        ui.slash.update(&ui.composer.input);
                    }
                    KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => {
                        ui.transcript.scroll_up();
                    }
                    KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                        ui.transcript.scroll_down();
                    }
                    KeyCode::Up => {
                        if ui.slash.is_visible() {
                            ui.slash.move_up();
                        } else {
                            ui.composer.history_prev();
                            ui.slash.update(&ui.composer.input);
                        }
                    }
                    KeyCode::Down => {
                        if ui.slash.is_visible() {
                            ui.slash.move_down();
                        } else {
                            ui.composer.history_next();
                            ui.slash.update(&ui.composer.input);
                        }
                    }
                    KeyCode::PageUp => {
                        ui.transcript.scroll_up();
                    }
                    KeyCode::PageDown => {
                        ui.transcript.scroll_down();
                    }
                    KeyCode::Tab => {
                        // Tab completes the selected command, inserting a
                        // trailing space so the user can type an arg.
                        if let Some(name) = ui.slash.selected_command() {
                            ui.composer.input = format!("{name} ");
                            ui.slash.update(&ui.composer.input);
                        }
                    }
                    KeyCode::Esc => {
                        ui.composer.clear();
                        ui.slash.update(&ui.composer.input);
                    }
                    KeyCode::Char('@') => {
                        // `@` summons the fuzzy file picker. The `@` is not
                        // inserted into the composer until the user picks a
                        // file (see ModalOutcome::Close handler above).
                        ui.modal = ActiveModal::FilePicker(FilePicker::new(project_root.clone()));
                    }
                    KeyCode::Char('?') if ui.composer.input.is_empty() => {
                        ui.modal = ActiveModal::Help(HelpModal::new());
                    }
                    KeyCode::Char(c) => {
                        ui.composer.input.push(c);
                        ui.slash.update(&ui.composer.input);
                    }
                    _ => {}
                }
                draw(terminal, ui)?;
            }
            Ok(CtEvent::Resize(_, _)) => draw(terminal, ui)?,
            Ok(CtEvent::Mouse(m)) => {
                use crossterm::event::MouseEventKind;
                match m.kind {
                    MouseEventKind::ScrollUp => {
                        ui.transcript.scroll_up();
                        draw(terminal, ui)?;
                    }
                    MouseEventKind::ScrollDown => {
                        ui.transcript.scroll_down();
                        draw(terminal, ui)?;
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => {
                ui.transcript
                    .push(TranscriptItem::Error(format!("input: {e}")));
                draw(terminal, ui)?;
            }
        }
    }
    Ok(IdleAction::Quit)
}

async fn run_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut AgentLoop,
    events: &mut EventStream,
    ui: &mut UiState,
    prompt: String,
    steer: Option<std::sync::Arc<wingman_core::SteerInbox>>,
) -> Result<()> {
    // Persistence is the agent loop's job now — it is the only place that
    // knows what actually went into a request. This function only renders.
    let mut assistant_text = String::new();
    let mut stream = agent.run(prompt.clone());
    loop {
        let mut done = false;
        tokio::select! {
            biased;
            ev = events.next() => {
                if let Some(Ok(CtEvent::Key(k))) = ev {
                    if k.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(k.code, KeyCode::Char('c'))
                    {
                        ui.transcript.push(TranscriptItem::System(
                            "(cancel mid-turn arrives in M2; finishing current step)".into(),
                        ));
                        draw(terminal, ui)?;
                    } else if let Some(inbox) = &steer {
                        // Typing during a turn steers it. The alternative — a
                        // dead keyboard until the turn ends — is why the only
                        // way to redirect used to be Ctrl+C and retype, which
                        // throws away everything the turn had established.
                        match k.code {
                            KeyCode::Enter if !ui.composer.input.trim().is_empty() => {
                                let msg = ui.composer.take_input();
                                inbox.push(msg);
                                // Not echoed here: the loop emits `Steered`
                                // when it actually folds the message in, and
                                // showing it twice would misreport *when* the
                                // model saw it.
                                ui.slash.update(&ui.composer.input);
                            }
                            KeyCode::Backspace => {
                                ui.composer.input.pop();
                                ui.slash.update(&ui.composer.input);
                            }
                            KeyCode::Char(c) => {
                                ui.composer.input.push(c);
                                ui.slash.update(&ui.composer.input);
                            }
                            _ => {}
                        }
                        draw(terminal, ui)?;
                    }
                }
            }
            evt = stream.next() => {
                match evt {
                    Some(event) => {
                        // The task panel is driven by the model's update_tasks
                        // tool: grab its args off the ToolStart before the
                        // generic handler turns it into a transcript line.
                        if let AgentEvent::ToolStart { name, input, .. } = &event {
                            if name == "update_tasks" {
                                ui.tasks = crate::widgets::tasks::parse(input);
                            }
                        }
                        if let AgentEvent::TextDelta { text } = &event {
                            assistant_text.push_str(text);
                        }
                        apply_event(&event, &mut ui.transcript, &mut ui.status);
                        draw(terminal, ui)?;
                        if matches!(event, AgentEvent::Stop { .. }) {
                            done = true;
                        }
                    }
                    None => done = true,
                }
            }
        }
        if done {
            break;
        }
    }
    drop(stream);
    Ok(())
}

fn apply_event(event: &AgentEvent, transcript: &mut Transcript, status: &mut StatusLine) {
    match event {
        AgentEvent::Steered { text } => {
            transcript.push(TranscriptItem::System(format!("steered: {text}")));
        }
        AgentEvent::TextDelta { text } => transcript.append_assistant_text(text),
        AgentEvent::ThinkingDelta { text } => transcript.append_thinking(text),
        AgentEvent::ToolStart { name, input, .. } => {
            let summary = compact_args(input);
            transcript.push(TranscriptItem::ToolCall {
                name: name.clone(),
                summary,
            });
        }
        AgentEvent::ToolResult {
            output, is_error, ..
        } => {
            let first_line = output.lines().next().unwrap_or("").to_string();
            transcript.push(TranscriptItem::ToolResult {
                ok: !is_error,
                summary: truncate(first_line, 120),
            });
        }
        AgentEvent::Usage { usage } => status.merge_usage(usage),
        AgentEvent::Verification { passed, summary } => {
            let mark = if *passed {
                "✓ verified"
            } else {
                "✗ verification failed"
            };
            let first_line = summary.lines().next().unwrap_or("").to_string();
            transcript.push(TranscriptItem::System(format!(
                "{mark} — {}",
                truncate(first_line, 120)
            )));
        }
        AgentEvent::TurnComplete => {}
        AgentEvent::Stop { reason } => {
            if !matches!(reason, AgentStop::EndTurn) {
                transcript.push(TranscriptItem::System(format!("(stop: {reason:?})")));
            }
        }
        AgentEvent::Error { message } => {
            transcript.push(TranscriptItem::Error(message.clone()));
        }
    }
}

fn compact_args(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(s, 120)
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}

/// Suspend the TUI, launch `$EDITOR` (falling back to a platform default)
/// on `path` and block until it exits, then re-enter the alternate screen
/// and request a redraw on the next `draw` call.
/// Resolve a `/skills install` source to a URL, download it, and write the
/// skill into `scope`. Registry lookups read the index URL from the
/// `WINGMAN_SKILLS_REGISTRY` env var.
async fn install_skill(
    source: &str,
    project_root: &std::path::Path,
    scope: wingman_skills::InstallScope,
) -> std::result::Result<wingman_skills::Skill, String> {
    let (url, hint) = match wingman_skills::classify_source(source) {
        wingman_skills::SkillSourceKind::Url(u) => {
            let hint = url_basename(&u);
            (u, hint)
        }
        wingman_skills::SkillSourceKind::RegistryName(name) => {
            let reg = std::env::var("WINGMAN_SKILLS_REGISTRY").map_err(|_| {
                format!(
                    "'{name}' looks like a registry name, but no registry is configured. \
                     Set WINGMAN_SKILLS_REGISTRY to an index URL, or pass a full URL or owner/repo."
                )
            })?;
            let index = fetch_text(&reg).await?;
            let v: serde_json::Value =
                serde_json::from_str(&index).map_err(|e| format!("registry index parse: {e}"))?;
            let entries = v
                .as_array()
                .ok_or("registry index must be a JSON array of {name, url}")?;
            let url = entries
                .iter()
                .find(|e| e["name"].as_str() == Some(name.as_str()))
                .and_then(|e| e["url"].as_str())
                .ok_or_else(|| {
                    let names: Vec<&str> =
                        entries.iter().filter_map(|e| e["name"].as_str()).collect();
                    format!(
                        "'{name}' not in registry. available: {}",
                        if names.is_empty() {
                            "(none)".to_string()
                        } else {
                            names.join(", ")
                        }
                    )
                })?
                .to_string();
            (url, name)
        }
    };
    let content = fetch_text(&url).await?;
    wingman_skills::install_markdown(&content, &hint, project_root, scope)
        .map_err(|e| format!("{e}"))
}

/// Last path segment of a URL, minus a `.md` suffix — a fallback skill name.
fn url_basename(url: &str) -> String {
    let last = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("skill");
    last.trim_end_matches(".md").to_string()
}

/// GET `url` as UTF-8 text, with a timeout and a size cap so a hostile or
/// wrong URL can't hang the TUI or blow up memory.
async fn fetch_text(url: &str) -> std::result::Result<String, String> {
    const MAX_BYTES: usize = 512 * 1024;
    wingman_core::ensure_tls_provider();
    let resp = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "wingman")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} from {url}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "skill too large ({} bytes, max {MAX_BYTES})",
            bytes.len()
        ));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| "response was not UTF-8 text".to_string())
}

/// Run a git subcommand in `root`, returning stdout on success or the
/// trimmed stderr as the error.
fn git(root: &std::path::Path, args: &[&str]) -> std::result::Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git not available: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `git add -A && git commit -m <msg>`, reporting the new commit.
fn git_commit(root: &std::path::Path, msg: &str) -> std::result::Result<String, String> {
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-m", msg])?; // errors "nothing to commit" if clean
    let head = git(root, &["log", "-1", "--pretty=%h %s"])?;
    Ok(format!("committed {}", head.trim()))
}

/// Push the current branch and open a PR via `gh`. Returns the PR URL.
fn git_open_pr(root: &std::path::Path, title: &str) -> std::result::Result<String, String> {
    let branch = git(root, &["branch", "--show-current"])?.trim().to_string();
    if branch.is_empty() {
        return Err("detached HEAD — check out a branch first".into());
    }
    git(root, &["push", "-u", "origin", &branch]).map_err(|e| format!("push failed: {e}"))?;
    let mut args: Vec<String> = vec!["pr".into(), "create".into()];
    if title.is_empty() {
        args.push("--fill".into()); // title + body from commits
    } else {
        args.extend([
            "--title".into(),
            title.into(),
            "--body".into(),
            title.into(),
        ]);
    }
    let out = std::process::Command::new("gh")
        .args(&args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("gh not installed: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "gh pr create: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn launch_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    path: &std::path::Path,
) -> Result<()> {
    // Leave the alternate screen so the editor draws on the user's normal
    // terminal. We don't recreate the Terminal — same backend, just toggle
    // raw mode and screen state.
    disable_raw_mode().ok();
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture).ok();
    terminal.show_cursor().ok();

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| {
        if cfg!(windows) {
            "notepad".to_string()
        } else {
            "nano".to_string()
        }
    });
    let status = std::process::Command::new(&editor).arg(path).status();

    // Restore the TUI either way.
    enable_raw_mode().ok();
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture).ok();
    terminal.clear().ok();

    match status {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("could not launch '{editor}': {e}")),
    }
}

/// Switch the live agent to `provider_id/model_id`. Surfaces success or
/// failure as a transcript line; if there's no agent yet, asks the user to
/// run `/login` first instead of crashing.
fn swap_model(
    ui: &mut UiState,
    agent: &mut Option<AgentLoop>,
    builder: &ProviderBuilder,
    provider_id: &str,
    model_id: &str,
) {
    match agent.as_mut() {
        Some(a) => match builder(provider_id) {
            Ok(provider) => {
                a.swap_provider(provider);
                a.set_model(model_id.to_string());
                ui.status.provider = provider_id.to_string();
                ui.status.model = model_id.to_string();
                ui.transcript.push(TranscriptItem::System(format!(
                    "switched to {provider_id}/{model_id}"
                )));
            }
            Err(e) => {
                ui.transcript
                    .push(TranscriptItem::Error(format!("/model {provider_id}: {e}")));
            }
        },
        None => {
            ui.transcript.push(TranscriptItem::Error(
                "No provider configured — run /login first.".into(),
            ));
        }
    }
}

fn resume_session(agent: &mut Option<AgentLoop>, ui: &mut UiState, entry: SessionEntry) {
    match wingman_session::load_session(&entry.path) {
        Ok(records) => {
            let history = wingman_session::records_to_messages(&records);
            let count = history.len();
            match agent {
                Some(a) => {
                    a.set_history(history);
                    ui.transcript.push(TranscriptItem::System(format!(
                        "resumed session {} — {count} messages restored into the agent",
                        entry.label
                    )));
                }
                None => {
                    ui.transcript.push(TranscriptItem::Error(
                        "no active agent to resume into — run /login first".into(),
                    ));
                }
            }
        }
        Err(e) => {
            ui.transcript
                .push(TranscriptItem::Error(format!("resume: {e}")));
        }
    }
}

fn export_transcript(
    transcript: &Transcript,
    format: &str,
    project_root: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let ext = if format == "json" { "json" } else { "md" };
    let dir = project_root.join(".wingman").join("exports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{ts}.{ext}"));

    if format == "json" {
        let items: Vec<serde_json::Value> = transcript
            .items
            .iter()
            .map(|item| match item {
                TranscriptItem::UserPrompt(s) => serde_json::json!({"role": "user", "content": s}),
                TranscriptItem::AssistantText(s) => {
                    serde_json::json!({"role": "assistant", "content": s})
                }
                TranscriptItem::Thinking(s) => {
                    serde_json::json!({"role": "thinking", "content": s})
                }
                TranscriptItem::ToolCall { name, summary } => {
                    serde_json::json!({"role": "tool_call", "name": name, "summary": summary})
                }
                TranscriptItem::ToolResult { ok, summary } => {
                    serde_json::json!({"role": "tool_result", "ok": ok, "summary": summary})
                }
                TranscriptItem::System(s) => serde_json::json!({"role": "system", "content": s}),
                TranscriptItem::Error(s) => serde_json::json!({"role": "error", "content": s}),
            })
            .collect();
        std::fs::write(&path, serde_json::to_string_pretty(&items)?)?;
    } else {
        let mut md = String::new();
        for item in &transcript.items {
            match item {
                TranscriptItem::UserPrompt(s) => {
                    md.push_str(&format!("**You:** {s}\n\n"));
                }
                TranscriptItem::AssistantText(s) => {
                    md.push_str(&format!("{s}\n\n"));
                }
                // Collapsed in the export too — it is context for the answer,
                // not the answer.
                TranscriptItem::Thinking(s) => {
                    md.push_str(&format!(
                        "<details><summary>thinking</summary>\n\n{s}\n\n</details>\n\n"
                    ));
                }
                TranscriptItem::ToolCall { name, summary } => {
                    md.push_str(&format!("> `{name}` {summary}\n"));
                }
                TranscriptItem::ToolResult { ok, summary } => {
                    let glyph = if *ok { "✓" } else { "✗" };
                    md.push_str(&format!("> {glyph} {summary}\n\n"));
                }
                TranscriptItem::System(s) => {
                    md.push_str(&format!("*{s}*\n"));
                }
                TranscriptItem::Error(s) => {
                    md.push_str(&format!("**Error:** {s}\n\n"));
                }
            }
        }
        std::fs::write(&path, md)?;
    }
    Ok(path)
}

fn help_text() -> String {
    String::from(
        "Slash commands:\n  \
         /help                       this message\n  \
         /clear                      reset the conversation\n  \
         /login, /connect            set up a provider in a guided wizard\n  \
         /logout [provider]          remove a stored API key\n  \
         /model [provider/model]     switch model, or open a picker with no arg\n  \
         /mode [m]                   switch permission mode, or open a picker with no arg\n  \
         /add <path>                 attach a file to the next prompt\n  \
         /usage                      show per-model token + cost breakdown\n  \
         /skills                     browse and apply skills\n  \
         /skill <name>               queue a skill for the next prompt\n  \
         /skills new <name>          create a new skill in $EDITOR\n  \
         /skill stats [name]         skill usage and outcome counts\n  \
         /feedback good|bad [note]   rate the last answer (beats the guess)\n  \
         /memory                     list saved memories (use 'forget <name>' to delete)\n  \
         /recall <query>             search across past sessions\n  \
         /learn [status|reset]       self-learning loop dashboard\n  \
         /mcp                        manage MCP servers (add / connect / remove)\n  \
         /params                     adjust temperature and max_tokens\n  \
         /resume                     resume a previous session\n  \
         /export [md|json]           export conversation to file\n  \
         /quit                       exit\n\nKeys: \
         Enter submit, Up/Down history, Esc clear input, Ctrl-C exit, \
         PgUp/PgDn or Shift+Up/Down scroll transcript, ? show shortcuts. \
         Type @ to fuzzy-pick a file from the project.",
    )
}

fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, ui: &UiState) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let vchunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // transcript (+sidebar)
                Constraint::Length(3), // composer
                Constraint::Length(1), // status
            ])
            .split(area);
        // Optional left sidebar.
        let (sidebar_area, mut body_area) = if ui.sidebar.is_some() {
            let h = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(32), Constraint::Min(20)])
                .split(vchunks[0]);
            (Some(h[0]), h[1])
        } else {
            (None, vchunks[0])
        };
        // Optional right task panel, carved out of whatever body remains.
        let tasks_area = if !ui.tasks.is_empty() {
            let h = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(34)])
                .split(body_area);
            body_area = h[0];
            Some(h[1])
        } else {
            None
        };
        if let (Some(area_l), Some(tree)) = (sidebar_area, ui.sidebar.as_ref()) {
            FileTreeView { tree }.render(area_l, f.buffer_mut());
        }
        if let Some(area_r) = tasks_area {
            crate::widgets::TasksView { tasks: &ui.tasks }.render(area_r, f.buffer_mut());
        }
        let chunks = [body_area, vchunks[1], vchunks[2]];
        if ui.transcript.items.is_empty() && !ui.composer.busy {
            WelcomeView { status: &ui.status }.render(chunks[0], f.buffer_mut());
        } else {
            TranscriptView {
                transcript: &ui.transcript,
                busy: ui.composer.busy,
            }
            .render(chunks[0], f.buffer_mut());
        }
        ComposerView {
            composer: &ui.composer,
        }
        .render(chunks[1], f.buffer_mut());
        StatusView { status: &ui.status }.render(chunks[2], f.buffer_mut());

        // Floating slash-command popup sits directly *above* the composer.
        // We size it to fit within the rows available above the composer,
        // and place its bottom edge exactly on the row just above the
        // composer's top border — never overlapping it.
        if ui.slash.is_visible() {
            let composer = chunks[1];
            let room_above = composer.y; // rows 0..composer.y are available
            let height = ui.slash.rendered_height().min(room_above);
            if height >= 3 {
                let popup = Rect {
                    x: composer.x,
                    y: composer.y - height,
                    width: composer.width,
                    height,
                };
                ui.slash.render(popup, f.buffer_mut());
            }
        }

        ui.modal.render(area, f.buffer_mut());
    })?;
    Ok(())
}
