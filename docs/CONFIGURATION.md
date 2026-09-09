# Configuration

Layered config, every knob, environment variables, and the permission model.
See [README](../README.md#configuration) for the short version.

`wingman` resolves configuration in this order (lowest to highest precedence):

1. Built-in defaults.
2. `~/.wingman/config.toml` (global).
3. `<project>/.wingman/config.toml` (project-local).
4. `WINGMAN_*` environment variables.
5. CLI flags.

TOML sub-tables are merged at the raw-TOML level, so an absent section in the
project file does **not** wipe out the global values for that section.

## Example `~/.wingman/config.toml`

```toml
default_provider = "anthropic"
permission_mode = "read-only"
reasoning = "off"              # off | low | medium | high

[tokens]
compact_at_tokens = 120000
tool_output_max_lines = 400
prompt_cache = true
# max_usd_per_session = 5.0   # soft warning when estimated spend crosses this

[router]
fast_model = "anthropic/claude-haiku-4-5-20251001"
# local_model = "ollama/llama3.1"   # target of the `local` class keyword

[tui]
theme = "default"
show_token_usage = true

[tools]
# web_fetch/web_search are gated to auto-edit/yolo by default (network egress
# is an exfiltration channel). Set true to also allow them in read-only/plan.
allow_network = false
redact_output_secrets = true   # redact secret tokens in tool output (default on)

# Offer `run_plan`: let the model chain a few tool calls in one round trip,
# feeding an earlier call's output into a later call's arguments. Off by
# default. It does NOT widen permissions — every call in a plan is gated
# exactly as it would be alone — but it does widen how much happens per model
# decision, and its schema costs ~430 tokens (~10% of the tool list) on every
# request. Independent calls are already batched and parallel without it, so
# turn this on only if dependent chains (grep, then read what it matched) are
# common in your work. A project config cannot enable it.
run_plan = false

# Backstop deadline for one tool call. Without it a wedged language server or
# an unresponsive MCP server hangs the turn with no upper bound. Tools that
# bound themselves — run_shell, custom command tools, spawn_subagent — opt
# out, so raising this does not extend them. 0 disables the backstop.
tool_timeout_secs = 120

# Loop hygiene. A run of calls to the same tool with identical arguments is
# almost always a loop the model cannot see itself in; at each threshold the
# tool result gains an advisory to re-read the last result and change
# approach or conclude. Never blocks a call. [] disables the guard.
repeat_thresholds = [3, 5, 8]
# Tools transparent to the chain: an excluded call neither increments the
# counter nor resets it, so bookkeeping interleaved into a loop cannot
# launder it. Trailing `*` matches by prefix.
repeat_exempt = ["update_tasks", "task_complete"]

# The ceiling on repetition, and the one place a turn is *ended* over it.
#
# `repeat_thresholds` above only ever sees a run of identical calls — its
# chain resets the moment the arguments change, so an alternating
# `grep X -> read_file Y -> grep X -> read_file Y` cycle is invisible to it,
# and it only ever advises. That is right for an interactive session where
# someone is watching and can hit Esc. It is not a control for `wingman
# pilot`, a subagent, or a --print run in CI, any of which will otherwise
# spend the whole turn budget on a call whose answer stopped changing.
#
# This window counts how often the same call appears among the last N,
# whatever came in between: a warning at `loop_warn_at`, and the turn ends at
# `loop_abort_at` with stop reason `loop_detected`. Set loop_abort_at = 0 to
# disable.
loop_window = 24
loop_warn_at = 4
loop_abort_at = 8
# Tools the cycle guard ignores. Some tools repeat with identical arguments
# because that is what they are for: "is the job done yet" is byte-identical
# every time and its answer changes only when something else changes the
# world. An MCP status-poll tool is the common case. Trailing `*` matches by
# prefix. (`wingman pilot`'s manager exempts its own orchestration tools for
# the same reason, without needing this key.)
loop_exempt = ["update_tasks", "task_complete"]

# Withhold these tools' schemas from every request; the model reaches them
# through `tool_search` (find by keyword, get the schema) and `tool_call`
# (invoke by name) instead. Trailing `*` matches by prefix. Empty = every
# tool is in every request.
#
# `preset` below is the static answer to the same cost: it decides what a
# session is *for* and drops the rest. This is the dynamic one, for tools
# worth having but rarely used. An MCP server's twenty schemas are billed on
# every turn and wanted on perhaps one, so this trades a round trip in that
# turn for the schemas in the other forty-nine.
#
# Deferring is not disabling: a deferred tool is registered, permission-gated
# and callable exactly as before, and `tool_call` dispatches it through the
# same gate as a direct call. The two meta-tools cost two schemas, so this is
# a saving only when it hides more than it adds — `wingman context` reports
# the real number either way, including them.
defer = []                    # e.g. ["mcp__*"]

# Seconds `ask_user` waits for an answer from the Wingman desktop popup when
# it is running. 0 (default) keeps the tool's long-standing behaviour: with no
# interactive terminal it returns "proceed with your best judgment" at once.
# One key is both the switch and the deadline, so they cannot drift apart. A
# headless run is never blocked longer than this, and if the popup is not
# running the question is not routed to it at all. See docs/NOTIFIER.md.
ask_user_desktop_timeout_secs = 0

# Restrict the session to a named tool preset (or pass `--preset`). Every
# tool's schema is billed on every request, so a session that only reads code
# pays for write_file and run_shell on every turn. Built-ins: "review"
# (read/search/navigate/recall, no writes) and "minimal" (find, change,
# check). Empty = every registered tool. Compare with `wingman context`.
preset = ""


# `tool_output_max_lines` caps what a tool result costs by keeping the head
# and tail. Without spilling, the elided middle is gone for good. With it, the
# full text is written to .wingman/spill/<session>/ and the model is handed
# the path, so it can re-read any span with read_file's offset/limit — same
# context cost, minus the one-way door. Swept after 7 days.
spill_tool_output = true

# Prune a tool result larger than this many characters once the session is
# over [tokens].compact_at_tokens. Compaction folds whole turns into a recap,
# discarding the assistant's reasoning along with the bulk; pruning takes the
# bulk only, and usually postpones compaction entirely. The most recent turns
# are never pruned — the model is still working from them. 0 disables.
prune_threshold_chars = 8192

# Define or override a preset. A name here shadows the built-in. Keep this
# subtable LAST in [tools]: every scalar key after it would be read as part
# of it, not as a [tools] key.
# [tools.presets]
# docs = ["read_file", "write_file", "glob", "grep", "lsp_*"]

# Also run hooks from an existing Claude Code settings.json, so you do not
# have to rewrite a working hooks block to try Wingman. Off by default: hooks
# execute shell commands, and running another tool's config because it happens
# to be on disk is a surprise. `wingman doctor` says when an importable file
# is present.
#
# ~/.claude/settings.json is yours and imports as-is. A project-level
# .claude/settings.json is part of the repo you cloned, so it imports only
# after `wingman trust` on that file — the same rule .wingman/config.toml
# follows. Matchers are translated (Bash -> run_shell, Edit -> edit_file, …);
# anything untranslatable is reported rather than left to silently never fire.
[hooks]
import_claude_code = false

# Verification gate (runs after edits): compile check + affected tests + LSP
# diagnostics, and optional headless-browser visual check.
[verify]
turn_gate = "auto"        # "auto" | "off" | an explicit command
affected_tests = true
lsp_diagnostics = true
# [verify.browser]
# url = "http://localhost:5173"
# baseline = "tests/baseline.png"

# Aider-style: commit each turn's edits automatically.
[git]
auto_commit = false

# Append a compliance audit trail of every tool call.
[audit]
enabled = false

# Server-backed team memory (beyond the git-backed `memory sync`).
# [team]
# endpoint = "https://memory.example.com"
# token = "${WINGMAN_TEAM_TOKEN}"

# Extend the agent with your own shell-command tools (no recompile).
# [[tools.custom]]
# name = "run_migration"
# description = "Apply the latest DB migration"
# command = "make migrate"

[providers.anthropic]
api_key = "${ANTHROPIC_API_KEY}"
model = "claude-opus-4-7"

[providers.openai]
api_key = "${OPENAI_API_KEY}"
model = "gpt-4.1"

[providers.gemini]
api_key = "${GOOGLE_API_KEY}"
model = "gemini-2.5-pro"

[providers.openrouter]
api_key = "${OPENROUTER_API_KEY}"
model = "anthropic/claude-opus-4-7"

[providers.ollama]
base_url = "http://localhost:11434/v1"
model = "llama3.1:8b"

[providers.lmstudio]
base_url = "http://localhost:1234/v1"
model = "local-model"

[providers.vllm]
base_url = "http://localhost:8000/v1"
model = "local-model"

[providers.litellm]
api_key = "${LITELLM_API_KEY}"
base_url = "http://localhost:4000/v1"
model = "anthropic/claude-opus-4-7"

# MCP servers — each becomes a set of `mcp__<name>__<tool>` tools.
[mcp.filesystem]
transport = "stdio"                 # "stdio" (default) or "http"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { API_KEY = "${SOME_KEY}" }   # env vars for the child process
# cwd = "/path/to/run/in"           # working directory for the child
# trusted = false                   # MCP tools are gated to auto-edit/yolo
                                    # unless a server is marked trusted (it
                                    # may run in read-only/plan mode then)

[mcp.remote]
transport = "http"                  # spec-compliant Streamable-HTTP client
url = "https://mcp.example.com/mcp" # (initialize handshake, SSE, session id)
headers = { Authorization = "Bearer ${MCP_TOKEN}" }  # auth / custom headers

[logging]
filter = "info,wingman=info"
file = true
```

## Environment variables

| Variable                            | Effect                                                              |
| ----------------------------------- | ------------------------------------------------------------------- |
| `WINGMAN_HOME`                      | The global directory itself, replacing `~/.wingman`. Absolute only. |
| `WINGMAN_MODEL`                     | Overrides `default_model`. Same syntax as `--model`.                |
| `WINGMAN_PROVIDER`                  | Overrides `default_provider`.                                       |
| `WINGMAN_PERMISSION_MODE`           | `read-only` \| `auto-edit` \| `yolo`.                               |
| `WINGMAN_REASONING`                 | `off` \| `low` \| `medium` \| `high`. Rejects anything else.        |
| `WINGMAN_LOG`                       | `tracing-subscriber` env-filter directive.                          |
| `WINGMAN_<PROVIDER>_API_KEY`        | Sets `providers.<provider>.api_key`.                                |
| `WINGMAN_<PROVIDER>_BASE_URL`       | Sets `providers.<provider>.base_url`.                               |
| `WINGMAN_<PROVIDER>_MODEL`          | Sets `providers.<provider>.model`.                                  |
| `WINGMAN_REMOTE`                    | Default `--remote` server URL for this shell.                       |
| `WINGMAN_SERVE_TOKEN`               | Bearer token `--remote` presents (else the OS keyring entry).       |
| `WINGMAN_PROJECT`                   | Default `--project` id for `--remote`.                              |

Any string field of the form `${ENV_VAR}` (e.g. `api_key = "${ANTHROPIC_API_KEY}"`)
is resolved against the environment at load time.

### `WINGMAN_HOME`

Named and shaped after `CARGO_HOME` and `RUSTUP_HOME`: the value **is** the
directory, with no `.wingman` appended to it.

```bash
WINGMAN_HOME=/tmp/wm-scratch wingman serve
```

It moves the whole global directory — config, credentials, logs, memory, the
notification inbox — so an install can be pointed at a scratch directory for a
throwaway `serve`, a sandboxed agent, or two versions side by side.

Three things about it are deliberate:

- **It must be absolute.** A relative value is refused at startup rather than
  resolved. Project discovery moves the process's working directory, so a
  relative path would bind the global directory to whichever directory happened
  to be current at first use — a config file that moves is worse than a refusal.
- **It is read once per process.** A global directory that moved mid-run would
  write credentials to one place and read them from another.
- **It does not move other tools' directories.** `~/.claude/settings.json` and
  `~/.claude/skills` are still read from the real home. They belong to another
  tool, and relocating them from a Wingman variable would be presumptuous.

## Reasoning

`reasoning` sets how hard the model thinks before answering — one portable
level rather than each vendor's own parameter:

| Level | Anthropic `thinking.budget_tokens` | OpenAI `reasoning_effort` | Gemini `thinkingBudget` |
| --- | --- | --- | --- |
| `off` (default) | not sent | not sent | not sent |
| `low` | 4096 | `low` | 4096 |
| `medium` | 16384 | `medium` | 16384 |
| `high` | 32768 | `high` | 32768 |

Backends with no reasoning control (Cohere, watsonx, ChatGPT-OAuth) ignore the
setting; `wingman doctor` says so rather than leaving you to guess. On OpenAI
the parameter is sent only to reasoning-family models — `gpt-4.1` rejects it
outright, so it is omitted there even when the level is set.

Layered like everything else: config file → `WINGMAN_REASONING` → `--reasoning`.
In the TUI, `/reasoning [level]` changes it live and reports the current value
with no argument.

Two things worth knowing before turning it on:

- **Thinking tokens bill at the output rate.** `off` is the default for that
  reason — enabling it is a cost decision.
- **On Anthropic, `max_tokens` is raised automatically** to sit above the
  thinking budget plus room for a reply, and `temperature` is dropped, because
  extended thinking requires default sampling.

Reasoning is streamed to the UI as it arrives (dimmed, collapsing to one line
once the answer begins) and, on Anthropic, carried back through history with
its signature intact so multi-turn tool use keeps working. In `--print` mode it
goes to stderr, so redirecting stdout still captures only the answer.

## Permission modes

| Mode         | Reads / Search | Writes inside project | Shell                        | Out-of-tree paths |
| ------------ | -------------- | --------------------- | ---------------------------- | ----------------- |
| `read-only`  | allowed        | denied                | denied                       | denied            |
| `plan`       | allowed        | after `/approve`      | after `/approve`             | denied            |
| `auto-edit`  | allowed        | auto-allowed          | auto-allowed except denylist | denied for writes |
| `yolo`       | allowed        | auto-allowed          | auto-allowed                 | allowed           |

**There are no interactive approval prompts.** A tool call that the active
mode doesn't permit is refused outright, with the reason returned to the
model. Choose the mode that matches the latitude you want to grant.

Enforcement is central: every tool declares what it needs
(`read` / `write` / `shell` / `network`), and the registry refuses the call
before the tool runs. A tool that declares nothing can only do pure
computation, so the default for anything new is deny.

**Protected paths.** `.git/`, `.wingman/config.toml`, `.wingman/skills/`, and
`.wingman/trusted.toml` are never writable — including in `yolo`. Each of
those turns a single bad edit into a change that outlives the session (a git
hook that fires on your next commit, a config that grants the agent more
permission next time). Edit them yourself if you mean to.

**About `plan`.** `plan` starts out identical to `read-only`: the agent can
read and search, but every write and shell call is refused. It calls
`present_plan` to show you what it intends to do; you run **`/approve`** in
the TUI to accept, and only then does it behave like `auto-edit` for the rest
of the session (still project-confined, and protected paths still refused).
Switching modes clears the approval, so returning to `plan` later needs a
fresh one — consent applies to the plan you actually read.

Headless (`--print`) has nobody to approve, so `plan` there stays read-only
for the whole run. Use `auto-edit` for unattended work.

**About `auto-edit`.** Writes are confined to the project tree. Shell is
confined too *when the platform provides a mechanism* — `bwrap` on Linux,
`sandbox-exec` on macOS — which bounds `run_shell` writes to the project and
blocks reads of `~/.ssh`, `~/.aws`, `~/.gnupg`. Set it with
`[tools].shell_sandbox`:

| value      | behaviour                                                        |
| ---------- | ---------------------------------------------------------------- |
| `auto`     | default — confine as far as the platform allows, warn about the rest |
| `required` | refuse to run shell unless the filesystem is scoped                |
| `off`      | never wrap                                                        |

**Windows is the weaker platform, deliberately.** There is no `bwrap`
equivalent, so `auto` runs the command inside a Job Object: the process tree
cannot outlive its timeout, cannot read the clipboard or touch handles owned by
processes outside the job, and cannot fork-bomb. It still reaches the whole
filesystem. Because `required` means *credential directories are out of reach*,
it keeps refusing on Windows rather than accepting the weaker guarantee — path
scoping there needs AppContainer or a restricted primary token
([#124](https://github.com/vedantnimbarte/Wingman/issues/124)).

`wingman doctor` reports which mechanism is active and what it does not cover,
so it is a claim you can check rather than take on trust. The other honest
limit: this confines the filesystem, not the network — a sandboxed command can
still `curl`. The shell denylist remains a convenience, not a boundary.

`yolo` is per-session only — never persisted to config.

## Project layout on disk

```
.
├── Cargo.toml              # workspace manifest
├── Cargo.lock
├── rustfmt.toml
├── crates/
│   ├── wingman-cli/        # binary entry point
│   ├── wingman-config/     # config loading + merge
│   ├── wingman-core/       # provider-agnostic types + agent loop + LearningHook
│   ├── wingman-learn/      # memory, skill stats, session recall, hooks
│   ├── wingman-mcp/        # MCP host (M3)
│   ├── wingman-providers/  # Anthropic, ChatGPT, Gemini, OpenAI-compat
│   ├── wingman-rag/        # repo + session index (SQLite + fastembed/hash)
│   ├── wingman-session/    # JSONL session log + replay
│   ├── wingman-skills/     # markdown-frontmatter skills loader
│   ├── wingman-tools/      # built-in tools + registry
│   └── wingman-tui/        # ratatui surface
└── target/                 # build output (gitignored)
```

On the user's machine (or wherever `WINGMAN_HOME` points):

```
~/.wingman/
├── config.toml             # global config
├── credentials.toml        # provider credentials (optional)
├── logs/                   # tracing output
├── skills/                 # global skills (*.md)
├── memory/                 # global memories
│   ├── MEMORY.md           #   index — one bullet per memory
│   └── <slug>.md           #   per-memory body
├── learn.db                # skill usage + outcome stats (SQLite)
└── sessions.db             # cross-project session embeddings (SQLite)
```

```
<project-root>/.wingman/
├── config.toml             # project-local overrides
├── sessions/               # per-session JSONL logs (append-only)
├── index.db                # project RAG index (SQLite + embeddings)
├── skills/                 # project-scoped skills (override globals by name)
└── memory/                 # project-scoped memories
    ├── MEMORY.md
    └── <slug>.md
```
