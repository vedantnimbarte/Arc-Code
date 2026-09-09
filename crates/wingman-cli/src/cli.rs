use crate::{commands, logging};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::process::ExitCode;
use wingman_config::{global_config_path, Config, PermissionMode, ProjectPaths};

/// wingman — multi-provider terminal coding agent.
#[derive(Parser, Debug)]
#[command(
    name = "wingman",
    version,
    about = "Multi-provider terminal coding agent",
    long_about = None,
)]
pub struct Cli {
    /// Permission mode for this session: read-only | plan | auto-edit | yolo.
    #[arg(long, value_name = "MODE", global = true)]
    pub mode: Option<String>,

    /// Model id, optionally prefixed with provider (e.g. `anthropic/claude-opus-4-7`).
    #[arg(long, value_name = "MODEL", global = true, env = "WINGMAN_MODEL")]
    pub model: Option<String>,

    /// How hard the model should think before answering: off | low | medium |
    /// high. Maps onto each provider's own reasoning control; backends that
    /// have none ignore it (`wingman doctor` says which).
    #[arg(
        long,
        value_name = "LEVEL",
        global = true,
        value_parser = ["off", "low", "medium", "high"],
    )]
    pub reasoning: Option<String>,

    /// Restrict this session to a named tool preset: `review` | `minimal`, or
    /// any name under `[tools.presets]`. Tools outside it are not registered,
    /// so their schemas are not billed — check the saving with
    /// `wingman --preset review context`.
    #[arg(long, value_name = "NAME", global = true, env = "WINGMAN_PRESET")]
    pub preset: Option<String>,

    /// Print a single response and exit (non-interactive).
    #[arg(long, value_name = "PROMPT")]
    pub print: Option<String>,

    /// Run prompts from a JSONL file non-interactively.
    #[arg(long, value_name = "FILE")]
    pub batch: Option<String>,

    /// Emit newline-delimited JSON events instead of text. Use with `--print`.
    #[arg(long)]
    pub json: bool,

    /// Send this command to a `wingman serve` daemon instead of running it
    /// locally: `--remote http://box:8787`. Works with `--print`,
    /// `pilot watch`, and any read subcommand.
    #[arg(long, value_name = "URL", global = true, env = "WINGMAN_REMOTE")]
    pub remote: Option<String>,

    /// Which project the remote server should operate on. Only needed when
    /// the server serves more than one.
    #[arg(long, value_name = "ID", global = true, env = "WINGMAN_PROJECT")]
    pub project: Option<String>,

    /// Continue a past session: replay its transcript as history, then run
    /// `--print`'s prompt in it and append to the same log. Takes a session
    /// id as shown by `wingman session list`.
    #[arg(long, value_name = "SESSION_ID")]
    pub resume: Option<String>,

    /// Preview only: with `--print`, force read-only and describe the changes
    /// that would be made without applying them.
    #[arg(long)]
    pub dry_run: bool,

    /// Run as a pilot-mode worker subprocess: load the role's system prompt,
    /// read the task spec from `--task-file`, run the agent loop with the
    /// task as the user prompt, emit a final `task_complete` event. Hidden
    /// from `--help`; only the orchestrator spawns workers this way.
    #[arg(long, hide = true)]
    pub worker_mode: bool,

    /// Path to a JSON file containing the [`wingman_autonomous::Task`] this
    /// worker should execute. Required with `--worker-mode`.
    #[arg(long, hide = true, value_name = "PATH")]
    pub task_file: Option<String>,

    /// Role name (e.g. `developer`, `designer`). Used to look up the role
    /// system prompt under `~/.wingman/agents/<role>.md`.
    #[arg(long, hide = true, value_name = "ROLE")]
    pub role: Option<String>,

    /// Session id under `<project>/.wingman/sessions/<id>.jsonl` for the
    /// worker's own transcript. The orchestrator records this so
    /// `wingman session fork` can target the worker's turns later.
    #[arg(long, hide = true, value_name = "ID", env = "WINGMAN_SESSION_ID")]
    pub session_id: Option<String>,

    /// Worktree path. The worker `cd`s here before running so all relative
    /// edits land inside the per-task worktree.
    #[arg(long, hide = true, value_name = "PATH")]
    pub worktree: Option<String>,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress non-error stderr output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Inspect or scaffold configuration.
    #[command(display_order = 1)]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Generate or refresh the project's agent instructions (AGENTS.md, or
    /// WINGMAN.md if the project already uses one) by introspecting it.
    #[command(display_order = 4)]
    Init {
        /// Overwrite an existing instructions file.
        #[arg(long)]
        force: bool,
    },
    /// Checkpoint the working tree into a named git stash for /undo recovery.
    #[command(display_order = 15)]
    Checkpoint {
        /// Optional label.
        #[arg(long)]
        label: Option<String>,
    },
    /// Restore the most recent wingman checkpoint via `git stash pop`.
    #[command(display_order = 13)]
    Undo,
    /// Scrub back through per-edit checkpoints. No args prints the timeline;
    /// `wingman rewind <n>` reverts the last n mutating edits (newest first).
    #[command(display_order = 14)]
    Rewind {
        /// Number of edits to revert. Omit to just print the timeline.
        steps: Option<usize>,
    },
    /// Show estimated token spend by model from ~/.wingman/usage.json.
    #[command(display_order = 17)]
    Cost {
        /// Output as JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Also reprice your actual token volume against other models
        /// (provider-cost arbitrage) — what the same work would cost elsewhere.
        #[arg(long)]
        compare: bool,
    },
    /// Session utilities.
    #[command(display_order = 16)]
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// `git worktree` helper — isolate an experiment under .wingman/worktrees.
    #[command(display_order = 45)]
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },
    /// Memory pack utilities.
    #[command(display_order = 23)]
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// One-shot code review of a PR or local diff.
    #[command(display_order = 10)]
    Review {
        /// PR number to review (uses `gh pr diff`).
        pr: Option<String>,
        /// Review local commits against this base ref instead.
        #[arg(long, value_name = "BASE")]
        local: Option<String>,
        /// Path to a custom review prompt template.
        #[arg(long, value_name = "FILE")]
        template: Option<String>,
    },
    /// Probe localhost for running Ollama / LM Studio / vLLM and print
    /// discovered models.
    #[command(display_order = 34)]
    Discover,
    /// Start the desktop notification popup: a small always-on-top window that
    /// shows plan-approval gates, questions and failures, and lets you answer
    /// them in place. See `docs/NOTIFIER.md`.
    #[command(display_order = 35)]
    Notify,
    /// Health check: config, provider credentials, local servers, the semantic
    /// index, language servers on PATH, and git/gh tooling.
    #[command(display_order = 3)]
    Doctor {
        /// Repair what can be repaired: rename config keys that are
        /// unambiguous misspellings of real ones, after backing the file up
        /// beside itself as `config.toml.bak-<timestamp>`.
        ///
        /// Only unambiguous renames are applied. A key nothing matches is
        /// reported and left alone, because guessing at it is how a repair
        /// tool loses someone's settings.
        #[arg(long)]
        fix: bool,
        /// Config checks only: no probing of providers, servers, or PATH.
        /// Read-only, fast, and exits non-zero on a problem — the shape CI
        /// wants for a preflight step.
        #[arg(long)]
        lint: bool,
        /// Emit findings as JSON instead of a checklist.
        #[arg(long)]
        json: bool,
    },
    /// Report the air-gapped / local-only posture (`[privacy].local_only`):
    /// what leaves the machine, verified against config, for compliance.
    #[command(display_order = 33)]
    Attest,
    /// Trust this project's `.wingman/config.toml` so its executable keys
    /// (`[hooks]`, `[mcp]`, `[verify]`, `[providers]`, `permission_mode`, …)
    /// are honoured. Untrusted project configs may only pick a model and tune
    /// presentation — a cloned repo must not be able to run commands. Trust is
    /// pinned to the file's contents and lapses whenever it changes.
    #[command(display_order = 32)]
    Trust {
        #[command(subcommand)]
        action: Option<TrustAction>,
    },
    /// Characterization / golden-master testing: snapshot a command's output,
    /// then fail if a later change alters it (regression net for legacy code).
    #[command(display_order = 22)]
    Golden {
        #[command(subcommand)]
        action: GoldenAction,
    },
    /// Show what Wingman knows about this project: memories, skills,
    /// model routing, the verification gate, and index freshness.
    #[command(display_order = 5)]
    Knows,
    /// Show what Wingman sends before you type: system-prompt and tool-schema
    /// token counts, the first-turn total, and what that costs per turn.
    #[command(display_order = 6)]
    Context {
        /// Machine-readable output, including every tool's size.
        #[arg(long)]
        json: bool,
    },
    /// Benchmark harness: run a suite of prompts and record time-to-first-token,
    /// tokens/task, wall time, and verified-done rate. Needs a live provider.
    #[command(display_order = 42)]
    Bench {
        /// JSONL suite file ({"id","prompt"} per line). Omit for a built-in suite.
        #[arg(long, value_name = "FILE")]
        suite: Option<String>,
        /// Output JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Model routing utilities.
    #[command(display_order = 41)]
    Router {
        #[command(subcommand)]
        action: RouterAction,
    },
    /// Expose Wingman itself as an MCP server over stdio: any MCP client
    /// (Claude Code, Cursor, another Wingman) can consume Wingman's tools —
    /// notably `semantic_search` (warm repo index) and `recall_memory` (team
    /// memory). Read-only by default; raise with `--mode`.
    #[command(name = "mcp-serve")]
    #[command(display_order = 30)]
    McpServe,
    /// Speak the Agent Client Protocol over stdio, so ACP-capable editors
    /// (Zed, JetBrains, Neovim, Emacs) can drive Wingman as their agent —
    /// one protocol instead of a plugin per editor.
    #[command(name = "acp")]
    #[command(display_order = 30)]
    Acp,
    /// Serve the HTTP/SSE API so you can drive Wingman from another machine,
    /// a phone, or CI. One daemon over an allowlist of repos; see
    /// `docs/HTTP-API.md`.
    #[command(display_order = 30)]
    Serve {
        /// Bind address, overriding `[serve].addr` (e.g. `0.0.0.0:8787`).
        #[arg(long, value_name = "ADDR")]
        addr: Option<String>,
        /// Generate an API token into the OS keyring, print it once, and exit.
        #[arg(long)]
        init_token: bool,
        /// Print the resolved projects and permission ceiling, then exit.
        #[arg(long)]
        list: bool,
        /// Required to run with `[serve].max_permission_mode = "yolo"`, which
        /// lets any request run arbitrary shell commands on this machine.
        #[arg(long)]
        allow_yolo: bool,
        /// Print a single-use pairing link so another device can fetch the
        /// API token once, instead of you carrying a 43-character secret to
        /// it by hand. Valid for 10 minutes; pairs one device.
        #[arg(long)]
        pair: bool,
    },
    /// Distill durable facts from a past session into a pending-review file
    /// (`.wingman/pending-memories.md`). Uses the fast model when configured.
    #[command(display_order = 25)]
    Distill {
        /// Session JSONL to distill. Defaults to the most recent session.
        #[arg(long)]
        session: Option<std::path::PathBuf>,
    },
    /// Keep this project's semantic index warm: initial reindex, then watch
    /// the tree and refresh on change until interrupted.
    #[command(display_order = 26)]
    Indexd {
        /// Report whether a daemon is running and index freshness, then exit.
        #[arg(long)]
        status: bool,
    },
    /// Run any [[schedule]] entries whose cadence is due.
    #[command(display_order = 43)]
    Schedule {
        /// Force-run all configured schedule entries regardless of cadence.
        #[arg(long)]
        all: bool,
    },
    /// Skill utilities.
    #[command(display_order = 24)]
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Multi-model code review: run review against several models in
    /// parallel and merge findings.
    #[command(display_order = 44)]
    ReviewMulti {
        /// PR number to review (uses `gh pr diff`).
        pr: Option<String>,
        /// Review local commits against this base ref instead.
        #[arg(long, value_name = "BASE")]
        local: Option<String>,
        /// Comma-separated list of provider/model pairs to consult.
        #[arg(long, value_name = "LIST")]
        models: String,
    },
    /// Onboard onto this codebase: architecture, entry points, key modules,
    /// conventions, and where to start reading.
    #[command(display_order = 20)]
    Tour {
        /// Optional focus area (e.g. "the agent loop", "auth").
        focus: Option<String>,
    },
    /// Implement an intent test-first: write failing tests, then implement
    /// until the verification gate is green. Runs in auto-edit.
    #[command(display_order = 21)]
    Spec {
        /// The intent to implement, in natural language.
        intent: String,
    },
    /// PR-native: address a PR's review comments and failing CI checks on the
    /// current branch (needs `gh`).
    #[command(display_order = 31)]
    Pr {
        #[command(subcommand)]
        action: PrAction,
    },
    /// Explain-and-teach the current changes: a per-file "what changed and
    /// why it matters" walkthrough of the working diff, aimed at a reviewer or
    /// junior. Routes to the fast model when configured.
    #[command(display_order = 12)]
    Explain {
        /// Explain the diff against this base ref instead of the working tree.
        #[arg(long, value_name = "BASE")]
        local: Option<String>,
        /// Explain only staged changes.
        #[arg(long)]
        staged: bool,
    },
    /// Interactive diff viewer: walk a unified diff hunk by hunk and
    /// accept or reject each one before writing the result.
    #[command(display_order = 11)]
    Diff {
        /// File to view the working-tree diff for (calls `git diff -- <file>`).
        file: Option<String>,
        /// Or supply a path to a unified-diff file directly.
        #[arg(long, value_name = "FILE")]
        patch: Option<String>,
    },
    /// Authenticate a provider non-interactively: probe the key and store it
    /// in the OS keyring + config. The TUI `/login` wizard equivalent.
    #[command(display_order = 2)]
    Login {
        /// Provider id (e.g. anthropic, openai, gemini). Omit with --list.
        provider: Option<String>,
        /// API key. If omitted, read from the provider's env var, else prompt.
        #[arg(long, value_name = "KEY")]
        api_key: Option<String>,
        /// Model id to record as this provider's default.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Base URL override (for local servers / proxies).
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Force the browser OAuth flow (chatgpt).
        #[arg(long)]
        oauth: bool,
        /// Skip the live connectivity test before saving.
        #[arg(long)]
        no_probe: bool,
        /// Register the provider without making it the default selection.
        #[arg(long)]
        no_default: bool,
        /// List the known provider ids and exit.
        #[arg(long)]
        list: bool,
    },
    /// Remove a provider's stored credential from the OS keyring.
    #[command(display_order = 35)]
    Logout {
        /// Provider id whose keyring entry to delete.
        provider: String,
    },
    /// Pilot mode: plan a multi-task goal, delegate to worker agents in
    /// isolated worktrees, converge into a PR.
    #[command(display_order = 40)]
    Pilot {
        #[command(subcommand)]
        action: PilotAction,
    },
    /// Kanban board over pilot runs: a persistent, multi-project backlog.
    ///
    /// Cards are goals you author; they outlive the runs that execute them.
    /// With no subcommand, opens the board.
    #[command(display_order = 7)]
    Board {
        #[command(subcommand)]
        action: Option<BoardAction>,
    },
    /// Deprecated alias for `wingman pilot` — kept through M3, removed at M4.
    #[command(hide = true)]
    Autonomous {
        goal: String,
        #[arg(long)]
        plan_only: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        review: bool,
        #[arg(long)]
        no_pr: bool,
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        #[arg(long, value_name = "N")]
        max_agents: Option<u32>,
        #[arg(long, value_name = "FLOAT")]
        max_usd: Option<f64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum BoardAction {
    /// Create a card in Backlog.
    Add {
        /// Card title.
        title: String,
        /// Prompt handed to `pilot run`. Defaults to the title.
        #[arg(long, value_name = "TEXT")]
        goal: Option<String>,
        /// Project id. Defaults to the current directory's project.
        #[arg(long, value_name = "ID")]
        project: Option<String>,
        /// Label; repeatable, or comma-separated.
        #[arg(long, value_name = "LABEL")]
        label: Vec<String>,
        /// Free-form notes.
        #[arg(long, value_name = "TEXT")]
        notes: Option<String>,
    },
    /// List cards with their derived columns.
    List {
        #[arg(long, value_name = "ID")]
        project: Option<String>,
        /// backlog | planned | in-progress | review | done
        #[arg(long, value_name = "COLUMN")]
        column: Option<String>,
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
        /// Include archived cards.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one card: goal, notes, dispatch history, and its newest run.
    Show {
        /// Card id or unique prefix (at least 4 characters).
        card: String,
    },
    /// Start a pilot run for a card.
    Dispatch {
        /// Card id or unique prefix.
        card: String,
        /// Dispatch even though a live run already exists.
        #[arg(long)]
        again: bool,
        /// Flags forwarded verbatim to `pilot run`.
        #[arg(last = true, value_name = "PILOT_FLAGS")]
        pilot_flags: Vec<String>,
    },
    /// Archive a card (reversible; never deletes).
    Archive {
        card: String,
        /// Un-archive instead.
        #[arg(long)]
        restore: bool,
    },
    /// Permanently delete a card and its dispatch history.
    Rm {
        card: String,
        /// Required when the card has been dispatched.
        #[arg(long)]
        yes: bool,
    },
    /// List or manage registered projects.
    Projects {
        /// Hide a project. Its cards are preserved.
        #[arg(long, value_name = "ID")]
        forget: Option<String>,
        /// Un-hide a forgotten project.
        #[arg(long, value_name = "ID")]
        restore: Option<String>,
        /// Point a project at a new path, for a repo that moved.
        #[arg(long, num_args = 2, value_names = ["ID", "PATH"])]
        relocate: Option<Vec<String>>,
        /// Include hidden projects.
        #[arg(long)]
        all: bool,
    },
    /// Dump cards and dispatch history — the backup story for a per-machine DB.
    Export {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PilotAction {
    /// Plan a goal, spawn workers, open a PR.
    Run {
        /// High-level objective in natural language.
        goal: String,
        /// Capability tier override.
        #[arg(long, value_name = "TIER")]
        tier: Option<String>,
        /// Plan and write tasks.jsonl, do not spawn workers.
        #[arg(long)]
        plan_only: bool,
        /// Auto-approve the plan (no interactive gate).
        #[arg(long)]
        yes: bool,
        /// Force hard plan-approval gate regardless of tier.
        #[arg(long)]
        review: bool,
        /// Tail the run in this terminal.
        #[arg(long)]
        watch: bool,
        /// Run in the background: print the run id and return the prompt.
        /// Watch/control it with `pilot watch|abort <id>`.
        #[arg(short = 'd', long, conflicts_with = "watch")]
        detached: bool,
        /// Skip `gh pr create` (just push the branch).
        #[arg(long)]
        no_pr: bool,
        /// Branch from <REV> instead of HEAD.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        /// Override pilot.max_concurrent_agents.
        #[arg(long, value_name = "N")]
        max_agents: Option<u32>,
        /// Override pilot.max_usd cost cap.
        #[arg(long, value_name = "FLOAT")]
        max_usd: Option<f64>,
        /// Override sandbox tier per run (host | container | vm).
        #[arg(long, value_name = "TIER")]
        sandbox: Option<String>,
        /// Notification channel for plan/completion notices.
        #[arg(long, value_name = "CHANNEL")]
        channel: Option<String>,
        /// For a headless hard-gate run, wait for `pilot approve` / `pilot
        /// veto` (or the watch UI) via the control channel instead of
        /// refusing outright. Times out to a rejection.
        #[arg(long)]
        await_approval: bool,
        /// Seconds to wait for `--await-approval` before rejecting the plan.
        #[arg(long, value_name = "SECS", default_value_t = 600)]
        approval_timeout: u64,
    },
    /// Print the latest run's status as ASCII; exits immediately.
    Status {
        /// Specific run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// Live-watch a run: redraw whenever its state.json changes.
    Watch {
        /// Specific run id; defaults to the most recently updated.
        run_id: Option<String>,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 250)]
        interval_ms: u64,
        /// Force plain-ASCII glyphs (for terminals that can't render the
        /// unicode status/spinner glyphs). Auto-detected when omitted.
        #[arg(long)]
        ascii: bool,
    },
    /// Resume an interrupted run.
    ///
    /// Loads the run's existing tasks.jsonl + state.json, marks any tasks
    /// stuck in InProgress (whose worker is gone) as Failed so the retry
    /// watchdog picks them up, and resumes the manager loop from there.
    Resume {
        /// Run id under `<project>/.wingman/autonomous/`.
        run_id: String,
        /// Skip `gh pr create` (just push the branch on completion).
        #[arg(long)]
        no_pr: bool,
    },
    /// Always-on discovery daemon (J2): poll configured sources, score
    /// candidates, and surface/queue work. Requires `[pilot.daemon].enabled`.
    Daemon {
        /// Run this many discovery cycles then exit; 0 = run forever.
        #[arg(long, default_value_t = 0)]
        cycles: usize,
        /// Log what auto-dispatch WOULD run without opening any PRs — use to
        /// validate `[pilot.daemon].auto_dispatch` trust config safely before
        /// letting the daemon act on its own.
        #[arg(long)]
        dry_run: bool,
    },
    /// J12 — install the skill packs listed in `[pilot.skills].packs` into
    /// `~/.wingman/packs/` and link their roles into `~/.wingman/agents/`.
    Skills,
    /// R4 — eval / regression gate. Summarize eval results, compare to the
    /// committed baseline, and exit non-zero on regression (the CI gate).
    Eval {
        /// Run each goal line in this file live, then gate on the results.
        /// Omit to gate on an existing `.wingman/eval/results.jsonl`.
        #[arg(long, value_name = "FILE")]
        goals: Option<std::path::PathBuf>,
        /// Allowed fractional drift before an axis counts as regressed.
        #[arg(long, default_value_t = 0.10)]
        threshold: f64,
        /// Rewrite the baseline from the current results instead of gating.
        #[arg(long)]
        update_baseline: bool,
    },
    /// R2 — post-merge feedback poller: for every run that opened a PR,
    /// query its terminal state (`gh`) and record a `pr.outcome` event the
    /// cross-run learner weights. Requires `gh` auth.
    Feedback {
        /// Run this many poll cycles then exit; 0 = run forever.
        #[arg(long, default_value_t = 0)]
        cycles: usize,
    },
    /// Abort a live run (or one of its tasks) via the control channel.
    Abort {
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
        /// Abort just this task instead of the whole run.
        #[arg(long, value_name = "TASK")]
        task: Option<String>,
    },
    /// Retry a failed/blocked task in a live run.
    Retry {
        /// Task id to retry.
        task: String,
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// Say something to a live run's worker(s) without stopping them.
    ///
    /// The message is injected into the next turn of whichever worker is
    /// holding the task, so it lands mid-flight: "also update the changelog",
    /// "the API changed, use v2". Without `--task`, every worker currently
    /// holding a task hears it.
    Tell {
        /// What to say.
        message: String,
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
        /// Address just this task's worker.
        #[arg(long, value_name = "TASK")]
        task: Option<String>,
    },
    /// Ask a live run's worker a question and wait for its reply.
    ///
    /// Same delivery as `pilot tell`, but the worker answers in its next
    /// message and that answer is printed here.
    Ask {
        /// The question.
        message: String,
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
        /// Address just this task's worker.
        #[arg(long, value_name = "TASK")]
        task: Option<String>,
        /// Seconds to wait for the answer before giving up. The message is
        /// still delivered either way.
        #[arg(long, value_name = "SECS", default_value_t = 120)]
        wait: u64,
    },
    /// Approve a run waiting at the plan-approval gate.
    Approve {
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// Reject a run waiting at the plan-approval gate.
    Veto {
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// External intake transports: turn Slack / email into pilot requests
    /// (written to the daemon's intake dir).
    Intake {
        #[command(subcommand)]
        channel: IntakeChannel,
    },
}

#[derive(Subcommand, Debug)]
pub enum IntakeChannel {
    /// Run a minimal HTTP server for the Slack Events API.
    Slack {
        /// Address to bind (e.g. 127.0.0.1:8899). Put TLS/ingress in front.
        #[arg(long, default_value = "127.0.0.1:8899")]
        addr: String,
    },
    /// Ingest `.eml` files delivered into a directory by your mail system.
    Email {
        /// Directory your mail delivery drops `.eml` files into.
        maildir: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SkillAction {
    /// Scan recent sessions and write proposed skill drafts under
    /// ~/.wingman/skills/proposed/.
    Extract {
        /// Minimum number of distinct sessions a pattern must appear in.
        #[arg(long, default_value_t = 2)]
        min: usize,
        /// Overwrite existing draft files.
        #[arg(long)]
        force: bool,
    },
    /// Import portable SKILL.md skills (a SKILL.md, its directory, or a
    /// directory of skill sub-directories) from other agents into wingman.
    Import {
        /// Path to a SKILL.md, a skill directory, or a directory of them.
        path: String,
        /// Import into the project's .wingman/skills instead of global.
        #[arg(long)]
        project: bool,
        /// Overwrite existing skills with the same name.
        #[arg(long)]
        force: bool,
    },
    /// Export a wingman skill as a portable <out-dir>/<name>/SKILL.md bundle
    /// usable by Claude Code, Codex, Cursor, and other SKILL.md agents.
    Export {
        /// Skill name (as shown by `/skills`).
        name: String,
        /// Output directory to write the skill bundle under.
        out_dir: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorktreeAction {
    /// Create a worktree under <project>/.wingman/worktrees/<branch>.
    Create { branch: String },
    /// List git worktrees (calls `git worktree list`).
    List,
    /// Remove a worktree by path.
    Remove { path: String },
}

#[derive(Subcommand, Debug)]
pub enum MemoryAction {
    /// Export the global memory directory to a target directory or single JSON pack file.
    Export { out: String },
    /// Import a memory pack (directory or JSON pack file) into the global memory directory.
    Import {
        path: String,
        /// Overwrite existing entries with the same name.
        #[arg(long)]
        force: bool,
    },
    /// Show a unified diff of two memory packs (or the live dir vs. a pack).
    Diff { a: String, b: String },
    /// Reconcile team-shared project memory: optionally fold in memory files
    /// from a git ref (without clobbering local ones), then rebuild MEMORY.md
    /// from the files on disk — resolving index merge conflicts.
    Sync {
        /// Git ref to pull teammate memory files from (e.g. `origin/main`).
        /// Omit to just rebuild the index from local files.
        git_ref: Option<String>,
    },
    /// Push this project's memories to the team server (`[team].endpoint`).
    Push,
    /// Pull team memories from the server and merge them (non-clobbering).
    Pull,
    /// Review distilled pending memories: list, or promote/discard by index.
    Review {
        /// Promote the Nth pending fact (1-based) to a project memory.
        #[arg(long, value_name = "N")]
        promote: Option<usize>,
        /// Discard the Nth pending fact (1-based).
        #[arg(long, value_name = "N")]
        discard: Option<usize>,
        /// Promote every pending fact and clear the queue.
        #[arg(long)]
        promote_all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionAction {
    /// List recent sessions for this project.
    List {
        /// Maximum number of entries.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Fork an existing session JSONL into a new file (optionally truncated).
    Fork {
        /// Path to the session JSONL to fork.
        src: String,
        /// Truncate the new file to the first N records.
        #[arg(long)]
        at: Option<usize>,
    },
    /// Re-run a past session's user prompts against the current code (read-only)
    /// to reproduce what happened — for debugging / regression.
    Replay {
        /// Path to the session JSONL to replay.
        src: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum PrAction {
    /// Address the PR's review comments and failing CI checks.
    Address {
        /// PR number.
        pr: String,
    },
}

#[derive(Subcommand, Debug, Clone, Copy, Default)]
pub enum TrustAction {
    /// Trust this project's config as it currently stands (the default).
    #[default]
    Add,
    /// Revoke trust for this project's config.
    Remove,
    /// Report whether this project's config is currently trusted.
    Show,
    /// List every trusted project config, flagging any that have changed.
    List,
}

#[derive(Subcommand, Debug)]
pub enum GoldenAction {
    /// Capture a command's output as a golden snapshot: `golden capture <name> -- <cmd…>`.
    Capture {
        /// Name for this golden.
        name: String,
        /// The command to run (everything after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Re-run goldens and fail on any behavior drift. Omit name to check all.
    Check {
        /// Limit to one golden by name.
        name: Option<String>,
    },
    /// List captured goldens and their commands.
    List,
}

#[derive(Subcommand, Debug)]
pub enum RouterAction {
    /// Show recorded per-class model win rates (gate pass-rate) for this repo.
    Stats {
        /// Aggregate across all repos instead of just the current one.
        #[arg(long)]
        all: bool,
    },
    /// Print a recommended [router] preset to paste into config. `local` keeps
    /// cheap steps (summarize/compaction/commit-message/title) on a local model.
    Preset {
        /// Preset name (currently: `local`).
        #[arg(default_value = "local")]
        name: String,
        /// Local model to use for the `local` preset (default ollama/llama3.1).
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Write a starter `~/.wingman/config.toml`.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the merged effective configuration.
    Show {
        /// Output as JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
    /// Print the resolved config file paths.
    Paths,
}

/// Dispatch an invocation to a remote `wingman serve`.
async fn run_remote(cli: &Cli, base: &str) -> Result<ExitCode> {
    let remote = crate::remote::Remote::connect(base, cli.project.as_deref()).await?;

    if let Some(prompt) = cli.print.clone() {
        return remote.print(&prompt, cli.json, cli.mode.as_deref()).await;
    }

    // `pilot watch` is the other surface worth a bespoke path: it redraws the
    // server's dashboard rather than returning once.
    if let Some(Command::Pilot {
        action: PilotAction::Watch { run_id, .. },
    }) = &cli.command
    {
        return remote.watch(run_id.as_deref()).await;
    }

    // Everything else: forward the argv as typed. A subcommand added to the
    // CLI tomorrow works remotely with no change here.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !is_remote_flag(a))
        .collect();
    if args.is_empty() {
        anyhow::bail!(
            "--remote needs something to run: a subcommand, or --print \"<prompt>\".\n\
             The interactive TUI cannot be remoted — it needs permission prompts \
             this transport does not carry."
        );
    }
    remote.exec(&args).await
}

/// Drop the client-side-only flags before forwarding an argv to the server.
/// `--remote`/`--project` take a value, so their values are dropped too; the
/// filter is applied to the flattened arg list, which is why the value forms
/// (`--remote=x`) and the split forms are both handled.
fn is_remote_flag(arg: &str) -> bool {
    // Values follow their flag; a bare value is filtered by the caller only
    // when it directly follows one of these, which `std::env::args` preserves.
    thread_local! {
        static SKIP_NEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    SKIP_NEXT.with(|skip| {
        if skip.get() {
            skip.set(false);
            return true;
        }
        if arg == "--remote" || arg == "--project" {
            skip.set(true);
            return true;
        }
        arg.starts_with("--remote=") || arg.starts_with("--project=")
    })
}

/// `--reasoning` applied at config-load time.
///
/// Set once from the parsed flag before anything loads config. The alternative
/// is threading an override through the ~20 `load_config()` call sites and the
/// per-command options struct each one owns, to deliver a value that is the
/// same for the whole process anyway.
static REASONING_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// `--preset` applied at config-load time, for the same reason as
/// [`REASONING_OVERRIDE`]: one process-wide value, ~20 `load_config()` call
/// sites.
static PRESET_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    if let Some(r) = cli.reasoning.clone() {
        let _ = REASONING_OVERRIDE.set(r);
    }
    if let Some(p) = cli.preset.clone() {
        let _ = PRESET_OVERRIDE.set(p);
    }
    // Suppress INFO logs during TUI mode (no verbose flag) so stderr output
    // doesn't bleed into the alternate-screen buffer and corrupt the display.
    let is_tui = cli.command.is_none() && cli.print.is_none() && cli.batch.is_none();
    let quiet_for_logging = cli.quiet || (is_tui && cli.verbose == 0);
    logging::install_tracing(cli.verbose, quiet_for_logging);

    // `--remote` reroutes the whole invocation to a server. Done before any
    // local config or provider resolution: the point is that this machine
    // need not be set up at all.
    if let Some(base) = cli.remote.clone() {
        return run_remote(&cli, &base).await;
    }

    if cli.worker_mode {
        let cfg = load_config()?;
        let opts = commands::worker::WorkerOptions {
            task_file: cli
                .task_file
                .ok_or_else(|| anyhow::anyhow!("--worker-mode requires --task-file"))?,
            role: cli
                .role
                .ok_or_else(|| anyhow::anyhow!("--worker-mode requires --role"))?,
            session_id: cli.session_id,
            worktree: cli.worktree,
            model_override: cli.model,
        };
        return commands::worker::run(cfg, opts).await;
    }

    if let Some(file) = cli.batch {
        let cfg = load_config()?;
        let mode_override = parse_mode(cli.mode.as_deref())?;
        let opts = commands::batch::BatchOptions {
            file,
            json: cli.json,
            mode_override,
            model_override: cli.model,
        };
        return commands::batch::run(cfg, opts).await;
    }

    if let Some(prompt) = cli.print {
        let cfg = load_config()?;
        // Dry run: force read-only (so nothing is mutated) and frame the task as
        // "show me what you WOULD do" — a safe preview of the plan and edits.
        let (prompt, mode_override) = if cli.dry_run {
            (
                format!(
                    "DRY RUN — do NOT make any changes. Investigate and then describe precisely the \
                     changes you would make (which files, what edits, what commands) and why, as a \
                     plan I can review. Task:\n\n{prompt}"
                ),
                Some(PermissionMode::ReadOnly),
            )
        } else {
            (prompt, parse_mode(cli.mode.as_deref())?)
        };
        let opts = commands::headless::HeadlessOptions {
            prompt,
            json: cli.json,
            mode_override,
            model_override: cli.model,
            session_id: cli.session_id,
            resume: cli.resume,
        };
        return commands::headless::run(cfg, opts).await;
    }

    match cli.command {
        Some(Command::Config { action }) => commands::config::run(action).await,
        Some(Command::Init { force }) => commands::init::run(force).await,
        Some(Command::Checkpoint { label }) => commands::checkpoint::create(label).await,
        Some(Command::Undo) => commands::checkpoint::undo().await,
        Some(Command::Rewind { steps }) => commands::rewind::run(steps).await,
        Some(Command::Cost { json, compare }) => commands::cost::run_with(json, compare).await,
        Some(Command::Session { action }) => commands::session::run(action).await,
        Some(Command::Worktree { action }) => match action {
            WorktreeAction::Create { branch } => commands::worktree::create(branch).await,
            WorktreeAction::List => commands::worktree::list().await,
            WorktreeAction::Remove { path } => commands::worktree::remove(path).await,
        },
        Some(Command::Memory { action }) => match action {
            MemoryAction::Export { out } => commands::memory::export(out).await,
            MemoryAction::Import { path, force } => commands::memory::import(path, force).await,
            MemoryAction::Diff { a, b } => commands::memory::diff(a, b).await,
            MemoryAction::Sync { git_ref } => commands::memory::sync(git_ref).await,
            MemoryAction::Push => commands::memory::push().await,
            MemoryAction::Pull => commands::memory::pull().await,
            MemoryAction::Review {
                promote,
                discard,
                promote_all,
            } => commands::memory::review(promote, discard, promote_all).await,
        },
        Some(Command::Review {
            pr,
            local,
            template,
        }) => commands::review::run(pr, local, template).await,
        Some(Command::Login {
            provider,
            api_key,
            model,
            base_url,
            oauth,
            no_probe,
            no_default,
            list,
        }) => {
            commands::login::run(commands::login::LoginOptions {
                provider,
                api_key,
                model,
                base_url,
                oauth,
                no_probe,
                no_default,
                list,
            })
            .await
        }
        Some(Command::Logout { provider }) => commands::login::logout(provider).await,
        Some(Command::Discover) => commands::discover::run().await,
        Some(Command::Notify) => commands::notify::run(),
        Some(Command::Doctor { fix, lint, json }) => {
            // Deliberately not `load_config()?`: a config with a bad key
            // fails to load, and that is the exact case doctor exists to
            // repair. Bailing out here would make the tool useless precisely
            // when it is needed.
            commands::doctor::run(load_config().unwrap_or_default(), fix, lint, json).await
        }
        Some(Command::Attest) => commands::attest::run(load_config()?).await,
        Some(Command::Context { json }) => commands::context::run(load_config()?, json).await,
        Some(Command::Acp) => {
            let cfg = load_config()?;
            let mode = cli
                .mode
                .as_deref()
                .and_then(|m| m.parse().ok())
                .unwrap_or(cfg.permission_mode);
            commands::acp_serve::run(cfg, mode).await
        }
        Some(Command::Serve {
            addr,
            init_token,
            list,
            allow_yolo,
            pair,
        }) => {
            crate::serve::run(
                load_config()?,
                crate::serve::ServeOptions {
                    addr,
                    init_token,
                    list,
                    allow_yolo,
                    pair,
                },
            )
            .await
        }
        Some(Command::Trust { action }) => commands::trust::run(action).await,
        Some(Command::Golden { action }) => match action {
            GoldenAction::Capture { name, command } => {
                commands::golden::capture(name, command).await
            }
            GoldenAction::Check { name } => commands::golden::check(name).await,
            GoldenAction::List => commands::golden::list().await,
        },
        Some(Command::Knows) => commands::knows::run(load_config()?).await,
        Some(Command::Bench { suite, json }) => commands::bench::run(suite, json).await,
        Some(Command::Router { action }) => commands::router::run(action).await,
        Some(Command::McpServe) => {
            // Read-only by default: exposing write/shell tools to an external
            // client is risky, so require an explicit --mode to raise it.
            let mode = parse_mode(cli.mode.as_deref())?.unwrap_or(PermissionMode::ReadOnly);
            commands::mcp_serve::run(load_config()?, mode).await
        }
        Some(Command::Distill { session }) => commands::distill::run(load_config()?, session).await,
        Some(Command::Indexd { status }) => commands::indexd::run(status).await,
        Some(Command::Schedule { all }) => commands::schedule::run(all).await,
        Some(Command::Skill { action }) => match action {
            SkillAction::Extract { min, force } => commands::skill::extract(min, force).await,
            SkillAction::Import {
                path,
                project,
                force,
            } => commands::skill::import(path, project, force).await,
            SkillAction::Export { name, out_dir } => commands::skill::export(name, out_dir).await,
        },
        Some(Command::ReviewMulti { pr, local, models }) => {
            commands::review_multi::run(pr, local, models).await
        }
        Some(Command::Tour { focus }) => commands::tour::run(focus).await,
        Some(Command::Spec { intent }) => commands::spec::run(intent).await,
        Some(Command::Pr { action }) => match action {
            PrAction::Address { pr } => commands::pr::address(pr).await,
        },
        Some(Command::Explain { local, staged }) => commands::explain::run(local, staged).await,
        Some(Command::Diff { file, patch }) => commands::diff::run(file, patch).await,
        Some(Command::Board { action }) => match action {
            None => commands::board_tui::run(commands::pilot::resolve_ascii(false)).await,
            Some(BoardAction::Add {
                title,
                goal,
                project,
                label,
                notes,
            }) => commands::board::add(title, goal, project, label, notes).await,
            Some(BoardAction::List {
                project,
                column,
                label,
                all,
                json,
            }) => commands::board::list(project, column, label, all, json).await,
            Some(BoardAction::Show { card }) => commands::board::show(card).await,
            Some(BoardAction::Dispatch {
                card,
                again,
                pilot_flags,
            }) => commands::board::dispatch(card, again, pilot_flags).await,
            Some(BoardAction::Archive { card, restore }) => {
                commands::board::archive(card, restore).await
            }
            Some(BoardAction::Rm { card, yes }) => commands::board::rm(card, yes).await,
            Some(BoardAction::Projects {
                forget,
                restore,
                relocate,
                all,
            }) => commands::board::projects(forget, restore, relocate, all).await,
            Some(BoardAction::Export { json }) => commands::board::export(json).await,
        },
        Some(Command::Pilot { action }) => match action {
            PilotAction::Run {
                goal,
                tier,
                plan_only,
                yes,
                review,
                watch,
                detached,
                no_pr,
                base,
                max_agents,
                max_usd,
                sandbox,
                channel,
                await_approval,
                approval_timeout,
            } => {
                let cfg = load_config()?;
                commands::pilot::run(
                    cfg,
                    commands::pilot::PilotOptions {
                        goal,
                        tier,
                        plan_only,
                        yes,
                        review,
                        watch,
                        detached,
                        no_pr,
                        base,
                        max_agents,
                        max_usd,
                        sandbox,
                        channel,
                        await_approval,
                        approval_timeout_secs: approval_timeout,
                        model_override: cli.model,
                    },
                )
                .await
            }
            PilotAction::Status { run_id } => commands::pilot::status(run_id).await,
            PilotAction::Watch {
                run_id,
                interval_ms,
                ascii,
            } => {
                commands::pilot::watch(run_id, interval_ms, commands::pilot::resolve_ascii(ascii))
                    .await
            }
            PilotAction::Resume { run_id, no_pr } => {
                let cfg = load_config()?;
                commands::pilot::resume(cfg, run_id, no_pr, cli.model).await
            }
            PilotAction::Feedback { cycles } => {
                let cfg = load_config()?;
                commands::pilot::feedback(cfg, cycles).await
            }
            PilotAction::Eval {
                goals,
                threshold,
                update_baseline,
            } => {
                let cfg = load_config()?;
                commands::pilot::eval(cfg, goals, threshold, update_baseline).await
            }
            PilotAction::Skills => {
                let cfg = load_config()?;
                commands::pilot::skills_install(cfg).await
            }
            PilotAction::Daemon { cycles, dry_run } => {
                let cfg = load_config()?;
                commands::pilot::daemon(cfg, cycles, dry_run).await
            }
            PilotAction::Abort { run_id, task } => {
                commands::pilot::control_abort(run_id, task).await
            }
            PilotAction::Retry { task, run_id } => {
                commands::pilot::control_retry(run_id, task).await
            }
            PilotAction::Tell {
                message,
                run_id,
                task,
            } => commands::pilot::control_tell(run_id, task, message).await,
            PilotAction::Ask {
                message,
                run_id,
                task,
                wait,
            } => commands::pilot::control_ask(run_id, task, message, wait).await,
            PilotAction::Approve { run_id } => commands::pilot::control_approve(run_id).await,
            PilotAction::Veto { run_id } => commands::pilot::control_veto(run_id).await,
            PilotAction::Intake { channel } => match channel {
                IntakeChannel::Slack { addr } => commands::pilot_intake::slack(addr).await,
                IntakeChannel::Email { maildir } => commands::pilot_intake::email(maildir).await,
            },
        },
        Some(Command::Autonomous {
            goal,
            plan_only,
            yes,
            review,
            no_pr,
            base,
            max_agents,
            max_usd,
        }) => {
            eprintln!(
                "[warn] `wingman autonomous` is deprecated and will be removed at M4 — use `wingman pilot` instead."
            );
            let cfg = load_config()?;
            commands::pilot::run(
                cfg,
                commands::pilot::PilotOptions {
                    goal,
                    tier: None,
                    plan_only,
                    yes,
                    review,
                    watch: false,
                    detached: false,
                    no_pr,
                    base,
                    max_agents,
                    max_usd,
                    sandbox: None,
                    channel: None,
                    await_approval: false,
                    approval_timeout_secs: 600,
                    model_override: cli.model,
                },
            )
            .await
        }
        None => {
            let cfg = load_config()?;
            let mode_override = parse_mode(cli.mode.as_deref())?;
            let mode = mode_override.unwrap_or(cfg.permission_mode);

            // Shared, replaceable MCP registry handle. Filled at startup
            // (if there's an agent), or by the agent_builder after /login.
            let mcp_handle: std::sync::Arc<
                std::sync::Mutex<Option<std::sync::Arc<crate::mcp_registry::McpRegistry>>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(None));

            // Try to resolve a provider/model and build the agent. If no
            // provider is configured (or the configured one fails to build),
            // we still open the TUI — the user can run /login to set one up.
            let (selection, agent) =
                match crate::runtime::resolve_selection(&cfg, cli.model.as_deref()) {
                    Ok(sel) => {
                        match crate::runtime::build_agent_and_registry(&cfg, &sel, mode).await {
                            Ok((a, registry)) => {
                                let mcp = std::sync::Arc::new(
                                    crate::mcp_registry::McpRegistry::new(registry),
                                );
                                mcp.seed(&cfg.mcp).await;
                                *mcp_handle.lock().expect("mcp_handle poisoned") = Some(mcp);
                                (Some(sel), Some(a))
                            }
                            Err(e) => {
                                tracing::warn!("failed to build agent at startup: {e}");
                                (Some(sel), None)
                            }
                        }
                    }
                    Err(e) => {
                        tracing::info!("no provider configured: {e}");
                        (None, None)
                    }
                };

            // Kick off background indexing for the project. The handle is
            // held until the TUI exits.
            let project = ProjectPaths::discover(&std::env::current_dir()?);
            let _watch_handle = match crate::runtime::build_indexer(&project)? {
                Some(indexer) => {
                    wingman_rag::spawn_background_indexer(indexer, project.root.clone())
                        .map_err(anyhow::Error::msg)
                        .ok()
                }
                None => None,
            };

            let cfg_for_builder = cfg.clone();
            let builder: wingman_tui::ProviderBuilder = std::sync::Arc::new(move |id: &str| {
                crate::runtime::build_provider(&cfg_for_builder, id).map_err(|e| e.to_string())
            });

            // Agent builder closure used by /login to construct a fresh
            // AgentLoop after the user finishes the wizard. Re-loads config
            // from disk each call so the wizard's keyring marker and
            // default-provider updates are picked up, and rebuilds the
            // MCP registry against the new ToolRegistry.
            let agent_builder_mcp = mcp_handle.clone();
            let agent_builder: wingman_tui::AgentBuilder =
                std::sync::Arc::new(move |provider_id: String, model: String| {
                    let mcp_slot = agent_builder_mcp.clone();
                    Box::pin(async move {
                        let cfg = load_config().map_err(|e| e.to_string())?;
                        let sel = crate::runtime::Selection { provider_id, model };
                        let (agent, registry) =
                            crate::runtime::build_agent_and_registry(&cfg, &sel, mode)
                                .await
                                .map_err(|e| e.to_string())?;
                        let mcp =
                            std::sync::Arc::new(crate::mcp_registry::McpRegistry::new(registry));
                        mcp.seed(&cfg.mcp).await;
                        *mcp_slot.lock().expect("mcp_handle poisoned") = Some(mcp);
                        Ok(agent)
                    })
                });

            // Login-task runner: probes a freshly-entered key, persists
            // credentials, or runs the ChatGPT OAuth browser flow.
            let login_runner: wingman_tui::LoginRunner =
                std::sync::Arc::new(move |task: wingman_tui::modal::LoginTask| {
                    Box::pin(async move {
                        // OAuthLogin is handled here rather than in login.rs
                        // because it needs async I/O (network + local server).
                        if let wingman_tui::modal::LoginTask::OAuthLogin { ref provider_id } = task
                        {
                            if provider_id == "chatgpt" {
                                return run_chatgpt_oauth().await;
                            }
                        }
                        crate::login::run_login_task(task).await
                    })
                });

            // Logout: clear a provider's keyring entry. Sync — the keyring
            // call is fast and blocking.
            let logout_runner: wingman_tui::LogoutRunner =
                std::sync::Arc::new(|provider_id: String| {
                    wingman_config::secrets::delete(&provider_id).map_err(|e| e.to_string())
                });

            // MCP task runner — dispatches the modal's `McpTask` against
            // the live registry, returning a friendly error if no registry
            // exists yet (i.e. user hasn't completed /login).
            let mcp_handle_for_run = mcp_handle.clone();
            let mcp_runner: wingman_tui::McpRunner = std::sync::Arc::new(move |task| {
                let mcp = mcp_handle_for_run
                    .lock()
                    .expect("mcp_handle poisoned")
                    .clone();
                Box::pin(async move {
                    let Some(mcp) = mcp else {
                        return Err("MCP needs an active provider — run /login first".into());
                    };
                    match task {
                        wingman_tui::modal::McpTask::Add(p) => {
                            let cfg = wingman_config::McpServerConfig {
                                transport: "stdio".into(),
                                command: Some(p.command),
                                args: p.args,
                                url: None,
                                ..Default::default()
                            };
                            mcp.add(p.name, cfg).await
                        }
                        wingman_tui::modal::McpTask::Remove(n) => mcp.remove(&n).await,
                        wingman_tui::modal::McpTask::Connect(n) => mcp.connect(&n).await,
                        wingman_tui::modal::McpTask::Disconnect(n) => mcp.disconnect(&n).await,
                    }
                })
            });

            // MCP list runner — returns the current snapshot.
            let mcp_handle_for_list = mcp_handle.clone();
            let mcp_list_runner: wingman_tui::McpListRunner = std::sync::Arc::new(move || {
                let mcp = mcp_handle_for_list
                    .lock()
                    .expect("mcp_handle poisoned")
                    .clone();
                Box::pin(async move {
                    let Some(mcp) = mcp else {
                        return Vec::new();
                    };
                    mcp.list()
                        .await
                        .into_iter()
                        .map(|v| wingman_tui::modal::McpServerSummary {
                            name: v.name,
                            command: format!(
                                "{} {}",
                                v.command.unwrap_or_default(),
                                v.args.join(" ")
                            )
                            .trim()
                            .to_string(),
                            connected: v.connected,
                            tool_count: v.tool_names.len(),
                        })
                        .collect()
                })
            });

            // Live model-catalog runner — builds the provider from the
            // current config and asks it to enumerate models. Config is
            // reloaded each call so a provider connected mid-session works.
            let models_runner: wingman_tui::ModelsRunner =
                std::sync::Arc::new(move |provider_id: String| {
                    Box::pin(async move {
                        let cfg = load_config().map_err(|e| e.to_string())?;
                        let provider = crate::runtime::build_provider(&cfg, &provider_id)
                            .map_err(|e| e.to_string())?;
                        provider.list_models().await.map_err(|e| e.to_string())
                    })
                });

            // Live permission-mode setter: `/mode` re-gates the running
            // agent by flipping the mode on the shared ToolRegistry (via the
            // current McpRegistry). No-op until a provider is active.
            let mcp_handle_for_mode = mcp_handle.clone();
            let mode_setter: wingman_tui::ModeSetter =
                std::sync::Arc::new(move |mode: wingman_config::PermissionMode| {
                    if let Some(mcp) = mcp_handle_for_mode
                        .lock()
                        .expect("mcp_handle poisoned")
                        .as_ref()
                    {
                        mcp.set_mode(mode);
                    }
                });

            // `/approve` in plan mode records consent on the shared ToolCtx,
            // which is what actually re-gates the next tool call. Reached
            // through the same handle the mode setter uses.
            let mcp_handle_for_approve = mcp_handle.clone();
            let plan_approver: wingman_tui::PlanApprover = std::sync::Arc::new(move || {
                if let Some(mcp) = mcp_handle_for_approve
                    .lock()
                    .expect("mcp_handle poisoned")
                    .as_ref()
                {
                    mcp.approve_plan();
                }
            });

            let (provider_id, model) = selection
                .as_ref()
                .map(|s| (s.provider_id.clone(), s.model.clone()))
                .unwrap_or_default();
            // Session index handle, shared by `/recall` and the session-end
            // indexer below. Lazily opens the global store on first use (via a
            // OnceCell) so the embedder model isn't loaded unless one of them
            // actually runs.
            type SessIndex = Option<(
                std::sync::Arc<dyn wingman_rag::Embedder>,
                std::sync::Arc<wingman_rag::IndexStore>,
            )>;
            let sess_cell: std::sync::Arc<tokio::sync::OnceCell<SessIndex>> =
                std::sync::Arc::new(tokio::sync::OnceCell::new());
            async fn open_sess_index(cell: &tokio::sync::OnceCell<SessIndex>) -> &SessIndex {
                cell.get_or_init(|| async {
                    let embedder = crate::runtime::pick_embedder_pub();
                    match wingman_learn::session_index::open_global_store(&*embedder) {
                        Ok(store) => Some((embedder, store)),
                        Err(e) => {
                            tracing::warn!("session index unavailable: {e}");
                            None
                        }
                    }
                })
                .await
            }

            // `/recall <query>` runner.
            let recall_runner: Option<wingman_tui::RecallRunner> = {
                let cell = sess_cell.clone();
                Some(std::sync::Arc::new(move |query: String| {
                    let cell = cell.clone();
                    Box::pin(async move {
                        let (embedder, store) = open_sess_index(&cell)
                            .await
                            .as_ref()
                            .ok_or_else(|| "session index unavailable".to_string())?;
                        let hits = wingman_learn::session_index::search_sessions(
                            store.as_ref(),
                            &**embedder,
                            &query,
                            8,
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                        Ok(hits
                            .into_iter()
                            .map(|h| (h.session_id, h.score, h.snippet))
                            .collect())
                    }) as futures::future::BoxFuture<'static, _>
                }))
            };

            // Session-end indexer: index the just-finished interactive session
            // so it's recallable immediately, instead of waiting for the next
            // launch's startup backfill.
            let session_indexer: Option<wingman_tui::SessionIndexer> = {
                let cell = sess_cell.clone();
                Some(std::sync::Arc::new(move |path: std::path::PathBuf| {
                    let cell = cell.clone();
                    Box::pin(async move {
                        if let Some((embedder, store)) = open_sess_index(&cell).await {
                            if let Err(e) = wingman_learn::session_index::index_session_into(
                                store.as_ref(),
                                &**embedder,
                                &path,
                            )
                            .await
                            {
                                tracing::warn!("session-end index failed: {e}");
                            }
                        }
                    }) as futures::future::BoxFuture<'static, ()>
                }))
            };

            let ctx = wingman_tui::AppCtx {
                provider_id,
                model,
                mode: mode.to_string(),
                show_token_usage: cfg.tui.show_token_usage,
                project_root: project.root.clone(),
                builder,
                agent_builder,
                login_runner,
                logout_runner,
                mcp_runner,
                mcp_list_runner,
                models_runner,
                mode_setter,
                plan_approver,
                recall_runner,
                session_indexer,
            };
            wingman_tui::init_theme(&cfg.tui);
            wingman_tui::run(agent, ctx).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = if project.config_file.exists() {
        Some(project.config_file)
    } else {
        None
    };
    let mut cfg = Config::load(Some(&global), project_file.as_deref())?;
    // CLI beats config and env, per the documented layering. Clap already
    // restricted this to the four valid levels.
    if let Some(r) = REASONING_OVERRIDE.get() {
        cfg.reasoning = r.clone();
    }
    if let Some(p) = PRESET_OVERRIDE.get() {
        cfg.tools.preset = p.clone();
    }
    Ok(cfg)
}

/// Run the ChatGPT OAuth PKCE flow and store the resulting tokens in the
/// OS keychain.  Called by the login runner when the user picks
/// "ChatGPT (subscription)" in the `/login` wizard.
async fn run_chatgpt_oauth() -> Result<(), String> {
    let (access_token, refresh_token) = crate::oauth::chatgpt_oauth_login()
        .await
        .map_err(|e| format!("OAuth login failed: {e}"))?;

    wingman_config::secrets::store("chatgpt", &access_token)
        .map_err(|e| format!("keyring (access token): {e}"))?;
    wingman_config::secrets::store("chatgpt_refresh", &refresh_token)
        .map_err(|e| format!("keyring (refresh token): {e}"))?;

    Ok(())
}

fn parse_mode(s: Option<&str>) -> Result<Option<PermissionMode>> {
    match s {
        None => Ok(None),
        Some(s) => s
            .parse::<PermissionMode>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!(e)),
    }
}
