//! Headless modes: `--print` for a single text-streaming response and
//! `--json` for newline-delimited structured events.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Context, Result};
use futures::StreamExt;
use wingman_config::{Config, PermissionMode, ProjectPaths};
use wingman_core::AgentEvent;
use wingman_session::{SessionLog, SessionRecord};

use crate::runtime;

pub struct HeadlessOptions {
    pub prompt: String,
    pub json: bool,
    pub mode_override: Option<PermissionMode>,
    pub model_override: Option<String>,
    /// Name the session log instead of minting a timestamped one, so a later
    /// `--resume` can find it. From `--session-id`.
    pub session_id: Option<String>,
    /// Continue the named session: its transcript is replayed into the agent
    /// as history before the prompt runs. From `--resume`.
    pub resume: Option<String>,
}

pub async fn run(cfg: Config, opts: HeadlessOptions) -> Result<ExitCode> {
    let mode = opts.mode_override.unwrap_or(cfg.permission_mode);
    let selection = runtime::resolve_selection(&cfg, opts.model_override.as_deref())?;
    let (mut agent, registry) =
        runtime::build_agent_registry_with_fallback(&cfg, &selection, mode).await?;
    // Seed MCP servers so `mcp__*` tools are available in headless mode too.
    // Held for the whole run; dropping it tears down the server subprocesses.
    let hooks_registry = registry.clone();
    let _mcp = runtime::seed_mcp(&cfg, registry).await;

    // Open session log under the project's .wingman/sessions/ dir.
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    // `--resume` replays a previous transcript into the agent, and writes
    // this turn into that same log — so a conversation can continue across
    // processes (which is what the HTTP API's server-held sessions ride on).
    if let Some(id) = opts.resume.as_deref() {
        match wingman_session::session_path(&paths.sessions_dir, id) {
            Some(path) => {
                let records = wingman_session::load_session(&path)
                    .with_context(|| format!("reading session {id}"))?;
                let history = wingman_session::records_to_messages(&records);
                if !opts.json {
                    eprintln!("wingman: resumed session {id} ({} messages)", history.len());
                }
                agent.set_history(history);
            }
            None => anyhow::bail!("no session '{id}' under {}", paths.sessions_dir.display()),
        }
    }

    // A named log (`--session-id`, implied by `--resume`) appends to one file
    // across turns; without one, each run gets a fresh timestamped session.
    let log_name = opts.session_id.clone().or_else(|| opts.resume.clone());
    let opened = match log_name.as_deref() {
        Some(id) => SessionLog::open_named(&paths.sessions_dir, id).await,
        None => SessionLog::create(&paths.sessions_dir).await,
    };
    let mut session = match opened {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("session log disabled: {e}");
            None
        }
    };
    if let Some(s) = session.as_mut() {
        let _ = s
            .write(SessionRecord::SessionStart {
                ts: chrono_rfc3339(),
                model: selection.model.clone(),
                provider: selection.provider_id.clone(),
                // Identifies the base prompt this session ran under. It was
                // always `None`, so the log recorded nothing at all about the
                // system prompt — the per-turn additions now arrive as
                // `InjectedContext`, and this pins what they were added to.
                system_hash: agent.system_prompt().map(wingman_session::system_hash),
            })
            .await;
        // The user prompt is recorded by the agent loop, which is also where
        // loop-authored user messages (verification-gate feedback) come from.
        // Writing it here as well would double it.
    }
    // Hand the loop the log: it is the only place that knows what actually
    // went into each request.
    let session = session.map(|log| std::sync::Arc::new(wingman_session::SessionLogSink::new(log)));
    if let Some(sink) = &session {
        agent.set_context_sink(sink.clone());
    }

    if !opts.json {
        eprintln!(
            "wingman [{}/{}] mode={mode}",
            selection.provider_id, selection.model
        );
    }

    // Record per-turn routing outcomes (which model, did the gate pass) so
    // `wingman router stats` can show which model wins per class in this repo.
    let routing_stats = wingman_learn::StatsStore::open_default().ok();
    let repo = paths.root.to_string_lossy().to_string();

    // `[[hooks.user_prompt_submit]]` — the policy/content-filter hook. A
    // blocking hook that exits non-zero refuses the prompt outright.
    if let Err(reason) = hooks_registry
        .run_user_prompt_submit_hooks(&opts.prompt)
        .await
    {
        eprintln!("wingman: prompt blocked by user_prompt_submit hook: {reason}");
        return Ok(ExitCode::from(1));
    }

    // Keep the prompt for an auto-commit message after the turn.
    let prompt_for_commit = opts.prompt.clone();
    let mut events = agent.run(opts.prompt);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let mut exit = ExitCode::SUCCESS;
    let mut assistant_text = String::new();
    let mut budget_warned = false;

    while let Some(event) = events.next().await {
        // Exit code + assistant-text capture, independent of output mode — a
        // mid-stream error or an error stop must fail the process in `--json`
        // mode too (previously it only did in the human-readable branch).
        match &event {
            AgentEvent::TextDelta { text } => assistant_text.push_str(text),
            AgentEvent::Verification { passed, .. } => {
                if let Some(st) = &routing_stats {
                    let _ = st.record_routing("default", &selection.model, &repo, *passed);
                }
            }
            AgentEvent::Error { .. } => exit = ExitCode::from(1),
            AgentEvent::Stop {
                reason: wingman_core::AgentStop::Error,
            } => exit = ExitCode::from(1),
            // A turn that ended with the verification gate red must fail the
            // process. Exiting 0 here meant `wingman --print` in CI reported
            // success on code that did not compile or whose tests failed —
            // the exact thing the gate exists to prevent.
            AgentEvent::Stop {
                reason: wingman_core::AgentStop::GateFailed,
            } => exit = ExitCode::from(2),
            AgentEvent::Usage { usage } => {
                // Soft cost guardrail: warn once when the estimated spend
                // crosses `[tokens].max_usd_per_session`. Usage is cumulative
                // for the turn, so its cost is the running session cost here.
                if let Some(budget) = cfg.tokens.max_usd_per_session {
                    if !budget_warned {
                        if let Some(price) = wingman_core::pricing::price_for(&selection.model) {
                            let usd = price.cost(usage);
                            if usd > budget {
                                budget_warned = true;
                                eprintln!(
                                    "wingman: ⚠ estimated session cost ~${usd:.4} exceeded budget \
                                     ${budget:.2} ([tokens].max_usd_per_session)"
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        if opts.json {
            let line = serde_json::to_string(&event)
                .unwrap_or_else(|_| "{\"type\":\"serialize_error\"}".into());
            writeln!(stdout, "{line}").ok();
            stdout.flush().ok();
        } else {
            match &event {
                AgentEvent::TextDelta { text } => {
                    write!(stdout, "{text}").ok();
                    stdout.flush().ok();
                }
                // stderr, not stdout: `wingman --print … > out.txt` must still
                // capture the answer and nothing else.
                AgentEvent::ThinkingDelta { text } => {
                    write!(stderr, "{text}").ok();
                    stderr.flush().ok();
                }
                AgentEvent::ToolStart { name, .. } => {
                    writeln!(stderr, "\n[tool] {name}…").ok();
                }
                AgentEvent::ToolResult { is_error, .. } => {
                    writeln!(
                        stderr,
                        "[tool done{}]",
                        if *is_error { " error" } else { "" }
                    )
                    .ok();
                }
                AgentEvent::Usage { usage } => {
                    writeln!(
                        stderr,
                        "[tokens] in={} out={} cache_read={} cache_creation={}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_input_tokens,
                        usage.cache_creation_input_tokens,
                    )
                    .ok();
                }
                AgentEvent::Verification { passed, summary } => {
                    let mark = if *passed { "✓" } else { "✗" };
                    writeln!(stderr, "\n[verify {mark}] {summary}").ok();
                }
                AgentEvent::TurnComplete => {}
                AgentEvent::Stop { .. } => {
                    writeln!(stdout).ok();
                }
                AgentEvent::Error { message } => {
                    writeln!(stderr, "\n[error] {message}").ok();
                    exit = ExitCode::from(1);
                }
            }
        }

        if let AgentEvent::Stop { reason } = &event {
            let label = match reason {
                wingman_core::AgentStop::EndTurn => "end_turn",
                wingman_core::AgentStop::MaxTurns => "max_turns",
                wingman_core::AgentStop::MaxTokens => "max_tokens",
                wingman_core::AgentStop::Error => "error",
                wingman_core::AgentStop::GateFailed => "gate_failed",
                wingman_core::AgentStop::LoopDetected => "loop_detected",
            };
            hooks_registry.run_stop_hooks(label).await;
            break;
        }
    }

    // Git-native auto-commit: turn this run's edits into a reviewable commit
    // (only if `[git].auto_commit` and the work tree actually changed).
    if let Some(line) =
        crate::git_auto::auto_commit_if_enabled(&cfg, &paths.root, Some(&prompt_for_commit))
    {
        if !opts.json {
            eprintln!("wingman: committed {line}");
        }
    }

    // Queue this session for indexing rather than indexing it here.
    //
    // A `tokio::spawn` at this point never finished: `run` returns on the next
    // line, the runtime is dropped, and the task dies mid-embed — the source
    // of the old "session indexing failed: … task was cancelled" warning.
    // Awaiting it instead would put an embedding-model load plus the embed on
    // the exit path of every `--print`, every pilot worker, and every HTTP
    // turn.
    //
    // So exit does the cheap durable half — record that there is work to do —
    // and the next run does the embedding. Unlike the per-project backfill,
    // the queue is global, so a session written in a repo that is never opened
    // again still becomes recallable from anywhere.
    //
    // The interactive TUI keeps its own indexer: it awaits it, a person closes
    // it once, and that wait is not on a hot path.
    if let Some(s) = session.as_ref() {
        if let Err(e) = wingman_learn::session_index::enqueue_pending(s.path()) {
            tracing::warn!("could not queue session for indexing: {e}");
        }
    }

    Ok(exit)
}

fn chrono_rfc3339() -> String {
    // Minimal re-implementation to avoid a chrono dep in the CLI crate.
    // We just delegate to time-of-day via SystemTime; OK for log timestamps.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}
