# Wingman Documentation Index

Welcome to Wingman's technical documentation. This index guides you to the right resource based on what you're trying to do.

## Getting Started

- **New to Wingman?** Start with the [README.md](../README.md) — what makes it
  different, install, and quick start.
- **Want the full feature list?** [FEATURES.md](FEATURES.md).
- **Looking for a subcommand?** [CLI.md](CLI.md), or `wingman --help`.
- **Configuring it?** [CONFIGURATION.md](CONFIGURATION.md) — every knob, env
  vars, permission modes, on-disk layout.
- **Which models/providers work?** [PROVIDERS.md](PROVIDERS.md).
- **Extending it?** [EXTENDING.md](EXTENDING.md) — hooks, slash commands,
  custom tools.

## Understanding Wingman

### Architecture & Design

- **[ARCHITECTURE.md](ARCHITECTURE.md)** — System overview, core crates, data flow diagrams, threading model, feature flags.
  - Best for: Understanding how components fit together, design decisions, module responsibilities.
  - Read this if: You're extending Wingman, debugging, or understanding the agent loop.

- **[decisions/](decisions/README.md)** — Decision records: why a boundary sits
  where it does, what was rejected, and what was deferred with its trigger.
  - Best for: understanding a choice that looks arbitrary from the code.
  - Read this if: you are about to change something and want to know whether
    it was decided that way on purpose.

### Major Subsystems

- **[TREE-SITTER.md](TREE-SITTER.md)** — Language-aware parsing integration (Rust, Python, JavaScript, TypeScript, Go).
  - Best for: Understanding semantic chunking, symbol extraction, code understanding.
  - Read this if: You're working with the RAG index, diff tools, or adding new languages.

- **[LSP.md](LSP.md)** — Language Server Protocol client: resolved go-to-definition, references, hover, diagnostics, rename, and LSP-backed verification receipts.
  - Best for: Understanding the `lsp_*` tools, server detection, and how diagnostics feed the turn gate.
  - Read this if: You want resolved (not name-matched) code intelligence, or you're wiring the verification gate.

- **[LEARNING-LOOP.md](LEARNING-LOOP.md)** — Self-improving mechanism (memories, skill stats, session recall, nudges).
  - Best for: Understanding how Wingman learns from sessions, persists across projects, and builds institutional knowledge.
  - Read this if: You're using memory features, skill extraction, or cross-project recall.

- **[HTTP-API.md](HTTP-API.md)** — The `wingman serve` HTTP/SSE API: one daemon over an allowlist of repos, bearer auth, a permission ceiling requests cannot raise, SSE streams, and `--remote` client mode.
  - Best for: driving Wingman from another machine, a phone, CI, or your own app.
  - Read this if: you want pilot runs steerable from anywhere, or a scriptable surface over the CLI.
  - Build order and design record: [HTTP-API-PLAN.md](HTTP-API-PLAN.md).

- **[BRIDGES.md](BRIDGES.md)** — What to reach through MCP, ACP or the HTTP API instead of building into Wingman: chat channels via OpenClaw, browser automation via Playwright MCP, and the reasoning for each.
  - Best for: "Wingman should also do X" where X is not about understanding code.
  - Read this if: you want Wingman in Telegram or Slack, or want the agent to drive a browser.

- **[WEB-UI.md](WEB-UI.md)** — The web control panel `wingman serve` hosts: a React app embedded in the binary, alongside the TUI rather than replacing it.
  - Best for: driving the board, runs, and config from a browser or a phone.
  - Read this if: you want the board, runs, config or a chat from a browser — or a binary is serving "the web UI was not built".
  - Build order and design record: [WEB-UI-PLAN.md](WEB-UI-PLAN.md).

- **[TOOLS.md](TOOLS.md)** — Reference for all 20+ built-in tools (file I/O, search, shell, semantic search, memory, skills, session management).
  - Best for: Complete tool signatures, behavior, permission requirements, error handling.
  - Read this if: You want to know what tools are available or how to use a specific tool.

### Pilot Mode & Roadmap

- **[PILOT-MODE.md](PILOT-MODE.md)** — The shipped `wingman pilot` surface: capability tiers, quick start, run artefacts, status.
  - Best for: Actually running multi-agent orchestration.
  - Read this if: You want a goal planned, worked in isolated worktrees, and converged into a PR.

- **[AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md)** — Design doc for multi-task agent orchestration, now shipped as **Pilot mode** (`wingman pilot`).
  - Best for: Understanding the vision, data model, architecture, TUI integration.
  - Read this if: You're using pilot mode, contributing to it, or interested in the roadmap.
  - See also: **Pilot mode** in [PILOT-MODE.md](PILOT-MODE.md) for the shipped command surface.

- **[NOTIFIER.md](NOTIFIER.md)** — `wingman notify`: an always-on-top desktop popup for approval gates, agent questions and failures, answerable in place. Reaches every surface through a file inbox rather than a daemon.
  - Records: [0017](decisions/0017-notifications-are-a-file-inbox.md) (why a file), [0018](decisions/0018-the-notifier-is-not-a-workspace-member.md) (why it is not a workspace member).

- **[BOARD.md](BOARD.md)** — `wingman board`: a persistent, multi-project kanban board over pilot runs. Cards are goals that outlive their runs; columns are derived from run state, never stored.
  - Best for: seeing what is planned, running, in review and done across every repo — with the agent, model, cost and logs behind each task.
  - Read this if: you want a backlog that survives runs, or you're implementing it.
  - Spec: [BOARD-SPEC.md](BOARD-SPEC.md). Build order and design record: [BOARD-PLAN.md](BOARD-PLAN.md). Status: **shipped** — in the terminal and in the panel ([WEB-UI.md](WEB-UI.md)).

- **[DIFFERENTIATION.md](DIFFERENTIATION.md)** — Differentiation roadmap (model routing, warm repo index, verification receipts, team memory).
  - Best for: Understanding how Wingman plans to beat Claude Code/Codex on speed, trust, and retention.
  - Read this if: You're prioritizing single-agent features or positioning the product.

## Using Wingman

### Command Reference

See **CLI Reference** in [CLI.md](CLI.md) for all subcommands.

Key commands:
- `wingman` — launch TUI.
- `wingman --print "<prompt>"` — one-shot headless mode.
- `wingman config init` — scaffold config.
- `wingman login <provider>` — connect a provider (OS keyring); `wingman logout <provider>` to remove.
- `wingman review <pr#>` — code review.
- `wingman session list` — browse past sessions.
- `wingman memory export/import/diff` — share memory packs.
- `wingman knows` — show what Wingman knows about this project (memories, skills, routing, verification gate, index freshness).
- `wingman skill extract` — mine skills from sessions.
- `wingman discover` — find local LLMs.
- `wingman pilot run "<goal>"` — multi-agent orchestration → PR (see [PILOT-MODE.md](PILOT-MODE.md)).
- `wingman serve` — HTTP/SSE API for remote control, and the browser control panel (see [HTTP-API.md](HTTP-API.md), [WEB-UI.md](WEB-UI.md)).

### Configuration

See [CONFIGURATION.md](CONFIGURATION.md) for:
- Layered config resolution (defaults → global → project → env vars → CLI flags).
- Config sections (tokens, router, tui, providers, hooks, schedule, mcp, pilot).
- Permission modes (read-only, plan, auto-edit, yolo).
- Hooks (pre_tool_use, post_tool_use, stop, user_prompt_submit).

### TUI Usage

See **Quick start** in [README.md](../README.md#quick-start) for:
- Typing prompts and hitting Enter.
- Slash commands (`/model`, `/mode`, `/login`, `/mcp`, `/memory`, `/recall`, `/skills`, `/learn`, `/usage`).
- File sidebar (`Ctrl+B`).
- Themes and colors.
- Transcript search (`/find`, `/findnext`).

### Memory & Learning

See [LEARNING-LOOP.md](LEARNING-LOOP.md) for:
- Memory types and scope.
- Skill usage tracking and outcome scoring.
- Session embeddings and cross-project recall.
- Nudges (quiet-session reminders).

Then read **[LEARNING-LOOP.md](LEARNING-LOOP.md)** for deep dives into each subsystem.

## Contributing to Wingman

### Development Setup

```bash
git clone https://github.com/vedantnimbarte/Wingman.git
cd Wingman
cargo build --release
cargo test
```

See **Development** in [README.md](../README.md#development) for build, test, and run commands.

### Code Organization

Read **[ARCHITECTURE.md](ARCHITECTURE.md)** → **Crate Responsibilities** for:
- What each crate does.
- Where to find relevant code.
- Module boundaries and public APIs.

### Key Crates at a Glance

| Crate                | Purpose                                                |
|----------------------|--------------------------------------------------------|
| `wingman-cli`        | Binary entry point, CLI args, command dispatch.        |
| `wingman-core`       | Agent loop, Provider trait, tool dispatch.             |
| `wingman-config`     | Layered config loading, permission model.              |
| `wingman-providers`  | 73+ LLM provider adapters (native + OpenAI-compat).    |
| `wingman-tools`      | 20+ built-in tools + registry.                         |
| `wingman-tui`        | Interactive ratatui surface.                           |
| `wingman-session`    | Append-only JSONL session logging.                     |
| `wingman-rag`        | Semantic code index (SQLite + embeddings).             |
| `wingman-skills`     | Markdown skill library (global + project).             |
| `wingman-learn`      | Memory store, skill stats, session embedding.          |
| `wingman-ts`         | Tree-sitter facade (language parsing).                 |
| `wingman-mcp`        | MCP host: stdio/HTTP servers → `mcp__<server>__<tool>`. |
| `wingman-autonomous` | Pilot mode: multi-agent orchestrator → PR.             |

### Adding a New Tool

1. Create a new file in `crates/wingman-tools/src/builtin/<tool_name>.rs`.
2. Implement the `Tool` trait (`spec()` and `run()`).
3. Register in `crates/wingman-tools/src/registry.rs`.
4. Update **[TOOLS.md](TOOLS.md)** with the tool signature and examples.

### Adding Support for a Language

1. Update `crates/wingman-ts/Cargo.toml` — add grammar crate.
2. Update `crates/wingman-ts/src/lang.rs` — add Language variant, detection.
3. Update `crates/wingman-ts/src/parse.rs` — add parser initialization.
4. Update **[TREE-SITTER.md](TREE-SITTER.md)** with the new language.

## Feature Flags

Wingman uses feature flags to reduce build time and dependencies for builds that don't need certain subsystems.

```bash
# Build with all features (default)
cargo build --release

# Build without tree-sitter (faster, but no language parsing)
cargo build --release --no-default-features -p wingman-ts

# Build without embeddings (uses hash fallback for RAG)
cargo build --release --no-features embeddings -p wingman-rag
```

See **[ARCHITECTURE.md](ARCHITECTURE.md)** → **Feature Flags** for details.

## Troubleshooting

### Build Issues

**Windows MSVC: `error LNK1318` (PDB cap exceeded)**

See `Cargo.toml` — already configured with `debug = "line-tables-only"` to stay under the 4GB limit.

**Tree-sitter compilation fails**

Tree-sitter requires a C toolchain. Install Visual Studio Build Tools (Windows) or gcc (Unix), or disable tree-sitter:
```bash
cargo build --release --no-default-features
```

### Runtime Issues

**"No such file or directory" errors**

Wingman expects absolute paths. Use `glob` or `list_dir` to find files, then pass absolute paths to other tools.

**Permission errors**

Check your permission mode (`wingman config show | grep permission_mode`). Adjust in config.toml or pass `--mode auto-edit`.

**Token limit exceeded**

Compaction is triggered when history exceeds `tokens.compact_at_tokens` (default 120k). Older turns are summarized. See **[ARCHITECTURE.md](ARCHITECTURE.md)** → **Token Management**.

**Session embedding is slow**

Session embedding happens asynchronously after the session ends. This is normal on first run. Set `[learn] embed_sessions = false` in config to disable.

## Performance Tuning

- **Large codebases:** Increase `tokens.compact_at_tokens` and `tokens.tool_output_max_lines` so tool output isn't truncated too early.
- **Fast inference:** Use `router.fast_model = "anthropic/claude-haiku-4-5-20251001"` for quick tasks (Haiku is cheap and fast).
- **RAG indexing:** Set `rag.chunk_size = 500` (default) for larger chunks, or `200` for more granular search.
- **Disable RAG:** Set `rag.enabled = false` if you don't use `semantic_search`.

## References

### External Resources

- **Rust docs:** https://doc.rust-lang.org/
- **Tokio async:** https://tokio.rs/
- **Ratatui TUI:** https://ratatui.rs/
- **Tree-sitter:** https://tree-sitter.github.io/tree-sitter/
- **SQLite:** https://www.sqlite.org/
- **Anthropic Messages API:** https://docs.anthropic.com/messages/

### Internal References

- **Plan (pilot mode roadmap):** `plan.md` in repository root.
- **Changelog (if any):** `CHANGELOG.md` (when created).
- **Contributing guidelines:** `CONTRIBUTING.md` (when created).

## Document Map

```
docs/
├── INDEX.md                 (you are here)
├── ARCHITECTURE.md          (system design + data flows)
├── TREE-SITTER.md           (language parsing integration)
├── LEARNING-LOOP.md         (memories, skills, session recall)
├── TOOLS.md                 (complete tool reference)
├── HTTP-API.md              (the `wingman serve` API surface)
├── HTTP-API-PLAN.md         (its build order and design record)
├── NOTIFIER.md              (the desktop notification popup)
├── WEB-UI.md                (the browser control panel `serve` hosts)
├── WEB-UI-PLAN.md           (its build order and design record)
├── AUTONOMOUS-MODE.md       (pilot mode design — shipped as `wingman pilot`)
└── DIFFERENTIATION.md       (single-agent differentiation roadmap)

../README.md                 (main project overview)
../plan.md                   (pilot mode implementation plan)
../Cargo.toml                (workspace manifest)
```

## Quick Search

Looking for:

- **How to save a memory?** → [LEARNING-LOOP.md](LEARNING-LOOP.md#saving-memories) or [TOOLS.md](TOOLS.md#save_memory).
- **How to use semantic search?** → [TOOLS.md](TOOLS.md#semantic_search) or [ARCHITECTURE.md](ARCHITECTURE.md#wingman-rag).
- **How permissions work?** → [README.md](../README.md#permission-model) or [ARCHITECTURE.md](ARCHITECTURE.md#wingman-config).
- **How to add a new provider?** → See `crates/wingman-providers/src/` and read [ARCHITECTURE.md](ARCHITECTURE.md#wingman-providers).
- **What's tree-sitter doing?** → [TREE-SITTER.md](TREE-SITTER.md) or [ARCHITECTURE.md](ARCHITECTURE.md#wingman-ts).
- **How does the agent loop work?** → [ARCHITECTURE.md](ARCHITECTURE.md#agent-loop).
- **What's planned next?** → [AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md) or `../plan.md`.

## Feedback & Questions

If this documentation is unclear or missing something:
1. Check the code (`crates/*/src/lib.rs` files have doc comments).
2. See `git log` for recent changes and commit messages.
3. Open an issue on GitHub with your question.

---

**Last updated:** 2026-07-01 | **Wingman v0.0.1**
