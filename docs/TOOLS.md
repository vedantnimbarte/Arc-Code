# Built-in Tools Reference

Wingman provides a comprehensive suite of built-in tools. This document describes each tool's signature, behavior, permission requirements, and use cases.

## Overview

Tools are registered in `ToolRegistry` (`crates/wingman-tools/src/registry.rs`). When the agent calls a tool:
1. Runs pre-tool hooks (if configured); a blocking hook can refuse the call.
2. Registry looks up the tool by name.
3. Checks permission mode (read-only, plan, auto-edit, yolo).
4. Executes the tool, under the `[tools].tool_timeout_secs` backstop deadline
   unless the tool bounds itself (`Tool::owns_timeout`).
5. Redacts high-confidence secrets from the output.
6. Truncates output per `tool_output_max_lines`. When that actually cuts
   something, the full text is written to `.wingman/spill/<session>/` and the
   result is prefixed with a locator line naming the file — so the model can
   recover the elided middle with `read_file`'s `offset`/`limit` instead of
   losing it (`[tools].spill_tool_output`).
7. Writes the audit record, then appends a repeat-guard advisory if this call
   has just reached a `[tools].repeat_thresholds` run length.
8. Runs post-tool hooks.

Which tools are registered at all depends on `[tools].preset` (`--preset`)
and `[tools].disabled_tools`. Run `wingman context` to see the active set and
what its schemas cost per turn.

Tools return `ToolOutcome`:
```rust
pub struct ToolOutcome {
    pub output: String,        // printed to user
    pub is_error: bool,
}
```

## File Operations

### `read_file`

Read a file and return its content with line numbers.

**Signature:**
```json
{
  "tool": "read_file",
  "args": {
    "path": "/absolute/path/to/file.rs"
  }
}
```

**Permission:** Always allowed (read-only mode).

**Returns:**
```
    1 | fn main() {
    2 |     println!("hello");
    3 | }
```

**Notes:**
- Absolute path required.
- `.ipynb` files are parsed; cells returned as fenced code blocks + markdown.
- `.pdf` files have their text extracted, so a spec or design doc handed over
  as a PDF is readable rather than "refusing to read binary file". Text only:
  layout and images are dropped. A scan with no text layer says it needs OCR
  instead of returning an empty string a model would read as an empty
  document. Behind the default-on `pdf` build feature.
- Output truncated per `tool_output_max_lines` (head + tail).

**Example:**
```
User: "What does the main function do?"
Agent calls: read_file(path="/path/to/main.rs")
Output: [file contents with line numbers]
Agent: "The main function prints 'hello'."
```

### `write_file`

Create or completely overwrite a file.

**Signature:**
```json
{
  "tool": "write_file",
  "args": {
    "path": "/absolute/path/to/new_file.rs",
    "content": "fn main() { ... }"
  }
}
```

**Permission:**
- Inside project tree: `auto-edit` or `yolo` (allowed); `read-only` or `plan` (prompts).
- Outside project tree: always prompts.

**Returns:**
```
✓ Wrote 42 bytes to /path/to/new_file.rs
```

**Notes:**
- Creates parent directories if missing.
- Overwrites existing file without warning (destructive).
- For safe edits, use `edit_file` instead.

### `edit_file`

Make a safe, exact-match string replacement in an existing file.

**Signature:**
```json
{
  "tool": "edit_file",
  "args": {
    "path": "/path/to/file.rs",
    "old_string": "fn foo(x: i32) {\n    x + 1\n}",
    "new_string": "fn foo(x: i32) -> i32 {\n    x + 1\n}"
  }
}
```

**Permission:**
- Inside project tree: `auto-edit` or `yolo` (allowed); `read-only` or `plan` (prompts).
- Outside project tree: always prompts.

**Returns:**
```
✓ Replaced 1 occurrence in /path/to/file.rs
```

**Error cases:**
- `old_string` not found (exact match required).
- `old_string` matches multiple times (ambiguous).

**Notes:**
- Exact match required; use `read_file` to find the exact text.
- Use `\n` for newlines (raw string literals don't expand escapes).
- Reversible if version control is in use.

**Example:**
```
Agent reads file, finds:
    42 | fn add(a: i32, b: i32) {
    43 |     a + b
    44 | }

Agent edits with:
  old_string: "fn add(a: i32, b: i32) {\n    a + b\n}"
  new_string: "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"
```

### `edit_symbol`

Replace the body of a named function or method using tree-sitter. Args:
`path`, `name`, `new_body` (all required). Returns a unified diff.

Safer than `edit_file` when the same text appears in several places, because
it targets the symbol rather than a string. `new_body` excludes the outer
braces for brace-delimited languages; for Python, supply the indented block.
Supported languages: rust, python, javascript, typescript, tsx, go.

### `apply_patch`

Apply a multi-file edit atomically. Updates, adds, and deletes are all-or-nothing.

**Signature:**
```json
{
  "tool": "apply_patch",
  "args": {
    "patch": "--- a/file1.rs\n+++ b/file1.rs\n@@ -1,2 +1,3 @@\n...\n--- /dev/null\n+++ b/newfile.rs\n@@ -0,0 +1,5 @@\n..."
  }
}
```

**Format:** Unified diff (git-style).

**Permission:** Same as `edit_file` (per-file checks).

**Returns:**
```
✓ Applied patch: 3 files modified, 1 added, 0 deleted
```

**Error cases:**
- Hunk doesn't apply cleanly → entire patch rejected.
- Partial file permissions → partial patch rejected.

**Notes:**
- Atomic: all changes or none. Prevents partial-write bugs.
- Useful for multi-file refactors.

## Search & Discovery

### `glob`

Find files matching a glob pattern.

**Signature:**
```json
{
  "tool": "glob",
  "args": {
    "pattern": "**/*.rs"
  }
}
```

**Returns:**
```
crates/wingman-cli/src/main.rs
crates/wingman-cli/src/cli.rs
crates/wingman-core/src/lib.rs
…
```

**Notes:**
- `**` matches directories recursively.
- Searches from project root (or cwd if outside project).
- Results sorted by modification time.

**Examples:**
```
glob(pattern="src/**/*.rs")     # all Rust files in src/
glob(pattern="**/*.{js,ts}")    # JS and TS files
glob(pattern="test_*.py")       # test files in current dir
```

### `grep`

Search file contents using ripgrep semantics.

**Signature:**
```json
{
  "tool": "grep",
  "args": {
    "pattern": "async fn.*Result",
    "glob": "**/*.rs",
    "context_lines": 2
  }
}
```

**Returns:**
```
crates/wingman-core/src/agent.rs:52:pub async fn stream(
crates/wingman-core/src/agent.rs:53:    request: &CompletionRequest,
crates/wingman-core/src/agent.rs:54-) -> Result<ProviderEventStream> {
…
```

**Parameters:**
- `pattern` — regex (case-insensitive by default; use `(?-i:...)` to force case-sensitive).
- `glob` — file pattern filter (optional; defaults to `**/*`).
- `context_lines` — lines before/after matches (default 0).

**Notes:**
- Full regex syntax supported.
- Results limited to first 50 (prevents token overflow).

**Examples:**
```
grep(pattern="fn main")                      # find main functions
grep(pattern="TODO|FIXME", glob="**/*.rs")   # all TODOs in Rust files
grep(pattern="class\\s+\\w+", glob="**/*.py") # class definitions in Python
```

### `list_dir`

List directory contents.

**Signature:**
```json
{
  "tool": "list_dir",
  "args": {
    "path": "/path/to/dir"
  }
}
```

**Returns:**
```
wingman-cli/          (dir)
wingman-core/         (dir)
wingman-config/       (dir)
Cargo.toml            (file, 2.4 KB)
README.md             (file, 30.5 KB)
```

**Notes:**
- Absolute path required.
- Shows file sizes and type (dir/file).
- Sorted alphabetically.

## Shell Execution

### `run_shell`

Execute a shell command. Blocked by denylist on `auto-edit` mode.

**Signature:**
```json
{
  "tool": "run_shell",
  "args": {
    "command": "cargo build --release",
    "timeout_secs": 60
  }
}
```

**Returns:**
```
Compiling wingman v0.0.1
Finished release [optimized] target(s) in 42.5s
```

**Permission:**
- Inside project tree: `auto-edit` or `yolo` (allowed, unless in denylist); `read-only` (prompts); `plan` (denied).
- Outside project tree: always prompts (`yolo` auto-allows).

**Notes:**
- Runs in project root (or cwd if outside project).
- Timeout enforced (default 60s, max 300s).
- Stdout and stderr combined.
- On Windows, runs via `cmd /C`; on Unix, via `sh -c`.

**Common denylisted commands:**
- `rm -rf /`
- `git push`
- `git commit --amend`
- `dd if=/dev/sda`

## Background Jobs

`run_shell` blocks the turn and is capped at 600s, which rules out dev
servers, watch processes, and cold builds of a large workspace. Passing
`background: true` starts the command and returns a job id instead of waiting.

A background command goes through exactly the same preparation as a
foreground one — permission mode, project denylist, OS sandbox, and the
credential scrub off the child's environment. It is not a less-guarded
command; it is the same command, not waited on.

```
run_shell(command="npm run dev", background=true)
→ started job-1
```

Output is buffered up to 128 KiB, keeping the **most recent** bytes — a
build's errors and a server's latest request are both at the end. When
earlier output is dropped the buffer says so, so a partial log never reads as
a complete one.

Every job is killed along with its whole process tree when the session ends,
so a forgotten dev server does not outlive the agent that started it.

### `job_output`

Read what a job has printed so far, plus its state (`running`, `exited(0)`,
`killed`). Args: `id` (required). Safe to call repeatedly while it runs.

### `job_send`

Write a line to a running job's stdin, so a process can be *driven* across
tool calls rather than one-shot. Args: `id`, `input` (both required). A
newline is appended if you omit one — line-buffered programs, which is most of
them, will not act until they see one.

```
run_shell(command="python3 -i", background=true)   → job-1
job_send(id="job-1", input="print(2 + 2)")
job_output(id="job-1")                             → 4
```

Sending to a finished job is an error that names the state, rather than a
silent write into a closed pipe.

Note this is a pipe, not a pseudo-terminal: the child sees `isatty() == false`,
so REPLs skip their `>>>` prompt and programs skip colour, progress bars, and
pagers — all of which are noise in a context window. Programs that *require* a
TTY do not work. See
[decision 0012](decisions/0012-stdin-instead-of-a-pty.md).

### `job_stop`

Stop a job and its whole process tree — killing `sh` alone would leave `npm`,
and `npm` would leave `node`. Args: `id` (required). Stopping an
already-finished job is not an error.

### `job_list`

One line per job: id, state, elapsed time, and the command. No arguments.

## Web Tools

### `web_fetch`

Download a URL and return text (HTML stripped).

**Signature:**
```json
{
  "tool": "web_fetch",
  "args": {
    "url": "https://example.com/article"
  }
}
```

**Returns:**
```
# Article Title

This is the article content. Headings and text are extracted;
HTML markup is removed.
```

**Notes:**
- Follows redirects.
- Timeout: 10s.
- Returns plain text (not raw HTML).
- No authentication; public URLs only.

### `web_search`

Search the web using DuckDuckGo (no API key).

**Signature:**
```json
{
  "tool": "web_search",
  "args": {
    "query": "rust async error handling best practices"
  }
}
```

**Returns:**
```
1. Title: "Async Rust Patterns" (example.com)
   Snippet: "Error handling in async code requires careful consideration…"
   URL: https://example.com/async

2. …
```

**Notes:**
- Top 10 results returned.
- Pairs well with `web_fetch` (search, then fetch top result).
- No API key needed.

## Semantic Search

### `semantic_search`

Search the project RAG index for relevant code chunks.

**Signature:**
```json
{
  "tool": "semantic_search",
  "args": {
    "query": "how do we handle concurrent requests",
    "top_k": 5
  }
}
```

**Returns:**
```
1. File: crates/wingman-core/src/agent.rs (lines 120-150)
   Relevance: 0.87
   Symbol: AgentLoop::run
   Preview: pub async fn run(&mut self, request: ...) { … }

2. …
```

**Notes:**
- RAG index must be built (automatic on first run).
- Embedding is semantic (understands meaning, not just keywords).
- Results include filename, line range, and code snippet.
- `top_k` defaults to 5; max 20.

## Symbol Graph

Tree-sitter-backed navigation (feature `treesitter`, on by default). Answers
"where is this defined / who uses it" in one tool call instead of several
grep→read round-trips. Supported languages: rust, python, javascript,
typescript, tsx, go.

### `find_symbol`

Locate where a symbol is *defined* (not merely mentioned). Args: `name`
(required), `glob`, `limit`, `case_insensitive`. Returns
`path:line  kind  name  signature` rows.

### `who_calls`

Find *references* to a symbol (call sites, mentions), each annotated with the
enclosing function/method — the part `grep` can't tell you. Skips the
definition line itself. Args: `name` (required), `glob`, `limit`.

**Returns:**
```
crates/wingman-ts/src/parse.rs:257  [in fn semantic_chunks]  let symbols = extract_symbols(lang, src);
crates/wingman-ts/src/parse.rs:370  [in fn outline]          let symbols = extract_symbols(lang, src);
```

**Notes:**
- Whole-word, case-sensitive name match — a name-based heuristic, not resolved
  references, so it can over-report same-named symbols and miss dynamic calls.
- Pair with `find_symbol` (definition) for the full picture of a symbol.

### `outline`

Signatures-only outline of a source file — one line per fn/struct/class — so
you can see a file's shape without reading every body. Args: `path`
(required). Supported languages: rust, python, javascript, typescript, tsx,
go; falls back to a short message for anything else.

Reach for this before `read_file` on an unfamiliar large file: it is the
cheapest way to decide *which* part is worth reading.

## LSP — Resolved Code Intelligence

Backed by whatever language server is on `PATH` (rust-analyzer, pyright,
typescript-language-server, gopls, and others — 11 languages). These resolve
imports, types, and re-exports rather than matching names, which is the
difference between a rename that is correct and a find-and-replace that
catches a comment. With no server installed they degrade to the tree-sitter
heuristics above rather than failing. See [LSP.md](LSP.md).

### `lsp_definition`

Resolve where a symbol is *defined*. Args: `path`, `line`, `character` or
`symbol`.

### `lsp_references`

Every *resolved* reference to a symbol — true references, not name matches.
Args: `path`, `line`, `character` or `symbol`.

### `lsp_hover`

The type, signature, and doc summary the language server shows on hover.
Args: `path`, `line`, `character` or `symbol`.

### `lsp_diagnostics`

The language server's live diagnostics for a file. Args: `path` (required),
`errors_only` (default false).

Also consumed by the verification gate: `[verify].lsp_diagnostics` folds the
diagnostics for *changed* files into the turn verdict.

### `lsp_rename`

Rename a symbol project-wide, updating every resolved reference. Args:
`path`, `line`, `character` or `symbol`, `new_name`. Requires write
permission.

### `lsp_code_action`

List or apply the language server's *own* code actions — add missing import,
implement trait, fix lint. Args: `path` (required), `line`, `character`,
`symbol`, `kind` (e.g. `"quickfix"`, `"source.organizeImports"`),
`organize_imports`. Applying an action requires write permission.

## Planning & Structured Output

### `update_tasks`

Maintain the visible checklist for a multi-step task. Args: `tasks`
(required) — the full list, in order, each `{ text, status }` with status one
of `pending` | `in_progress` | `done`.

Each call **replaces the whole list**. Use it for non-trivial work (roughly
3+ steps) and skip it for one-shot requests; keep exactly one task
`in_progress` at a time.

Exempt from the repeat-tool guard by default (see
[CONFIGURATION.md](CONFIGURATION.md)), since checklist bookkeeping
legitimately recurs.

### `ask_user`

Ask the user a question at a genuine decision fork or before an irreversible
action — which of two designs, an ambiguous requirement, deleting something
important. Args: `question` (required), `options` (optional suggested
answers).

Not for routine choices the agent can make itself. When no interactive
surface is available it returns a note saying so, and the agent is expected
to proceed on its best judgment and state the assumption.

Three routes, tried in order: the desktop popup when
`[tools].ask_user_desktop_timeout_secs` is non-zero *and* it is running; then
stdin when it is an interactive terminal; then the note. The popup comes first
because the TUI holds the terminal in raw mode, where a stdin read would fight
crossterm for keystrokes. On timeout the note is returned unchanged, so a
headless run is never blocked longer than that many seconds. See
[NOTIFIER.md](NOTIFIER.md).

### `present_plan`

Create a structured plan block. Required before edits in `plan` mode.

**Signature:**
```json
{
  "tool": "present_plan",
  "args": {
    "title": "Refactor token estimation",
    "steps": [
      "1. Analyze current estimate_tokens() function",
      "2. Identify inefficiencies",
      "3. Design new algorithm",
      "4. Update callers",
      "5. Test with large histories"
    ],
    "estimated_time_minutes": 30
  }
}
```

**Returns:**
```
✓ Plan presented. Awaiting approval…
```

**Notes:**
- In `plan` mode: blocks all writes/shell until user approves.
- In other modes: logs the plan but doesn't block.
- User can approve with `yes` / request edits with `edit` / reject with `no`.

## Memory & Learning

### `save_memory`

Persist a fact, preference, or instruction across sessions.

**Signature:**
```json
{
  "tool": "save_memory",
  "args": {
    "name": "user-pkg-manager",
    "type": "feedback",
    "description": "Use pnpm for package management",
    "body": "Always use pnpm instead of npm. It's faster and more reliable."
  }
}
```

**Returns:**
```
✓ Saved memory: user-pkg-manager (feedback)
```

**Memory types:**
- `user` — facts about the human (scope: global).
- `feedback` — behavioral preferences (scope: global).
- `project` — facts about this codebase (scope: project).
- `reference` — external pointers (scope: global).

**Notes:**
- Creates `<scope>/memory/<slug>.md` and updates `<scope>/memory/MEMORY.md`.
- Next session: index rendered into system prompt; full body available via `recall_memory`.

### `recall_memory`

Fetch the full body of a memory by name.

**Signature:**
```json
{
  "tool": "recall_memory",
  "args": {
    "name": "user-pkg-manager"
  }
}
```

**Returns:**
```
Always use pnpm instead of npm. It's faster and more reliable.

When setting up a new project, run: pnpm init && pnpm install
```

**Notes:**
- Index appears in system prompt automatically; use when you need the full body.

### `forget_memory`

Delete a memory permanently.

**Signature:**
```json
{
  "tool": "forget_memory",
  "args": {
    "name": "user-pkg-manager"
  }
}
```

**Returns:**
```
✓ Deleted memory: user-pkg-manager
```

## Skills

### `invoke_skill`

Load a skill for the current turn. Records usage and outcome.

**Signature:**
```json
{
  "tool": "invoke_skill",
  "args": {
    "name": "refactor-rust"
  }
}
```

**Returns:**
```
Skill: refactor-rust

When refactoring Rust code, follow these steps:
1. Run cargo clippy and address lints
2. Use iterators where possible
3. Keep public API stable
4. Write tests for public functions
```

**Notes:**
- Loads from global and project skills (project shadows global).
- Outcome tracked in `learn.db` and scored on next turn.
- Skills auto-loaded at startup appear in system prompt (no need to invoke unless the agent wants the full body).

## Session Management

### `recall_session`

Search past sessions across projects for relevant context.

**Signature:**
```json
{
  "tool": "recall_session",
  "args": {
    "query": "how do we handle race conditions in tokio code"
  }
}
```

**Returns:**
```
1. Project: Wingman (2024-05-27)
   Relevance: 0.91
   Session: 2024-05-27-143015.jsonl
   Snippet: "We use tokio::sync::Mutex for shared state…"

2. Project: MyServer (2024-04-15)
   Relevance: 0.78
   Session: 2024-04-15-090500.jsonl
   Snippet: "Message channels with select! prevent race conditions…"
```

**Notes:**
- Searches `~/.wingman/sessions.db` (embeddings of past sessions).
- Cross-project; helpful for pattern recall.
- Top 5 results returned.

### `read_session`

Fetch the full JSONL transcript of a session.

**Signature:**
```json
{
  "tool": "read_session",
  "args": {
    "session_id": "2024-05-27-143015"
  }
}
```

**Returns:**
```
[Full JSONL conversation log]
```

**Notes:**
- Used after `recall_session` returns relevant session ID.
- Enables "pattern replay" from past work.

## Pilot Mode

### `task_complete`

Terminal call for a pilot-mode worker: reports the final summary and the
files changed, after which the worker ends its turn and the orchestrator
takes over. Args: `summary` (required), `files_changed`, `outcome` (e.g.
`approve` / `rework` for reviewer tasks), `acceptance_results` (required when
the task carries acceptance checks).

Only registered for pilot workers — it is not part of an ordinary session's
tool set. See [PILOT-MODE.md](PILOT-MODE.md).

## User-Defined Tools

### `[[tools.custom]]`

Not one tool but a family: each `[[tools.custom]]` entry in config becomes a
tool the model can call, letting you extend the agent with a shell command
without recompiling. The tool input JSON arrives on stdin and in
`$WINGMAN_TOOL_INPUT`; stdout becomes the result.

Runs under the shell permission (auto-edit / yolo) and carries its own
`timeout_secs` (default 30), so it opts out of the registry's backstop
deadline. See [CONFIGURATION.md](CONFIGURATION.md) and
[EXTENDING.md](EXTENDING.md).

## Subagent Control

### `spawn_subagent`

Run an isolated inner agent on a sub-task. Depth capped at 1.

**Signature:**
```json
{
  "tool": "spawn_subagent",
  "args": {
    "task": "Fix the failing test in test_edit_file",
    "description": "You are a testing expert. Be concise.",
    "model": "",
    "task_class": "search"
  }
}
```

`task_class` (optional) drives model routing: classified subagents
(`search`, `summarize`, `codegen`, `reason`) resolve their model through
`[router.classes]` in config, so lookup/condense work can run on a cheaper,
faster model while the parent session keeps the strongest one. An explicit
`model` override always wins; empty class inherits the session model.

**Returns:**
```
✓ Subagent completed: Identified off-by-one error in line 42 of tests/edit.rs.
  Suggested fix: change `end_line + 1` to `end_line`.
```

**Notes:**
- Runs a fresh agent loop in the same workspace.
- Uses same config, tools, permissions as parent.
- Depth limited to 1 (no nested subagents).
- Useful for divide-and-conquer on complex tasks.

## Summary Table

| Tool                | Read | Write | Shell | Permission | Notes                          |
|---------------------|------|-------|-------|------------|--------------------------------|
| `read_file`         | Y    | —     | —     | always     | Line-numbered output           |
| `write_file`        | —    | Y     | —     | mode/tree  | Overwrites; destructive        |
| `edit_file`         | —    | Y     | —     | mode/tree  | Exact-match safe edit          |
| `apply_patch`       | —    | Y     | —     | mode/tree  | Atomic multi-file             |
| `glob`              | Y    | —     | —     | always     | File discovery                 |
| `grep`              | Y    | —     | —     | always     | Content search                 |
| `list_dir`          | Y    | —     | —     | always     | Directory listing              |
| `run_shell`         | —    | —     | Y     | mode/list  | Shell execution; `background` starts a job |
| `job_output`        | Y    | —     | —     | always     | Read a background job's output  |
| `job_send`          | —    | —     | Y     | mode/list  | Write a line to a job's stdin   |
| `job_stop`          | —    | —     | Y     | mode/list  | Kill a job and its process tree |
| `job_list`          | Y    | —     | —     | always     | List this session's jobs        |
| `web_fetch`         | Y    | —     | —     | always     | Download URL → text            |
| `web_search`        | Y    | —     | —     | always     | DuckDuckGo search (no key)     |
| `semantic_search`   | Y    | —     | —     | always     | RAG index search               |
| `find_symbol`       | Y    | —     | —     | always     | Where a symbol is defined      |
| `who_calls`         | Y    | —     | —     | always     | References + enclosing symbol  |
| `outline`           | Y    | —     | —     | always     | Signatures-only file shape     |
| `edit_symbol`       | —    | Y     | —     | mode/tree  | Replace a function body (AST)  |
| `lsp_definition`    | Y    | —     | —     | always     | Resolved go-to-definition      |
| `lsp_references`    | Y    | —     | —     | always     | Resolved references            |
| `lsp_hover`         | Y    | —     | —     | always     | Type / signature / docs        |
| `lsp_diagnostics`   | Y    | —     | —     | always     | Live errors + warnings         |
| `lsp_rename`        | —    | Y     | —     | mode/tree  | Project-wide resolved rename   |
| `lsp_code_action`   | Y    | Y     | —     | mode/tree  | Server's own fixes; write to apply |
| `present_plan`      | —    | —     | —     | always     | Structured plan (gates in plan mode) |
| `update_tasks`      | —    | —     | —     | always     | Visible checklist; replaces whole list |
| `ask_user`          | —    | —     | —     | always     | Pause and ask at a real fork   |
| `task_complete`     | —    | —     | —     | always     | Pilot workers only; ends the task |
| `save_memory`       | —    | Y     | —     | always     | Persist across sessions        |
| `recall_memory`     | Y    | —     | —     | always     | Fetch memory body              |
| `forget_memory`     | —    | Y     | —     | always     | Delete memory                  |
| `invoke_skill`      | Y    | —     | —     | always     | Load skill + track usage       |
| `recall_session`    | Y    | —     | —     | always     | Cross-project session search   |
| `read_session`      | Y    | —     | —     | always     | Fetch full session JSONL       |
| `spawn_subagent`    | —    | —     | —     | inherited  | Inner agent (depth=1)          |
| `run_plan`          | —    | —     | —     | per-step   | Chain dependent calls (opt-in) |

## Chained Calls

### `run_plan`

Run a short chain of tool calls in one round trip, feeding an earlier call's
output into a later call's arguments. **Off by default** — enable with
`[tools].run_plan = true`.

Reach for it only when a call *depends* on an earlier call's result.
Independent calls are already batched: the agent loop emits many tool calls per
assistant message and runs the read-only ones concurrently, so
`grep` + `glob` + `read_file` in one message is already one round trip and
already parallel. `run_plan` is slower and more expensive for that case.

What the loop cannot do is issue a call whose arguments are inside another
call's output, because the model has to see the first result before it can
write the second call. That is the whole gap this closes.

**Signature:**
```json
{
  "tool": "run_plan",
  "args": {
    "steps": [
      { "tool": "grep", "args": { "pattern": "TurnGate" } },
      { "tool": "read_file",
        "args": { "path": "{}" },
        "for_each": { "step": 0, "capture": "^([^:]+):" } }
    ]
  }
}
```

`for_each` runs a step once per value found in an earlier step's output:
`step` is that step's index (0-based) and `capture` is a regex with exactly one
capture group, applied per line. Distinct captures are used, in first-seen
order. Inside a `for_each` step, `{}` in any string argument is replaced with
the current value.

**Limits.** 8 steps, 32 calls per plan, 4000 characters kept per call. Plans
cannot nest. A step that fails stops the plan; a `for_each` item that fails
does not stop its siblings.

**Permissions.** A plan does not widen them. Every call inside it is dispatched
normally, so it passes the same capability gate, hooks, undo checkpoints, audit
trail and repeat guard as the identical call made on its own — a plan
containing `write_file` is refused in read-only mode exactly as `write_file`
would be. What a plan does widen is how much happens per model decision, which
is why it is opt-in and why a project config cannot enable it.

**Cost.** Its schema is about 430 tokens, roughly 10% of the whole tool list,
paid on every request. See [DSH-ADOPTION.md](DSH-ADOPTION.md) and
[decision 0015](decisions/0015-run-plan-not-code-mode.md) for the measurement
and why it is off by default.

## Error Handling

Most tools return `ToolOutcome { output: String, is_error: bool }`.

**Common error cases:**
- `read_file` on non-existent file → error, suggestion to `glob` or `list_dir`.
- `edit_file` with no match → error, shows first 200 chars of file.
- `grep` with invalid regex → error, invalid pattern reported.
- `run_shell` timeout → error, partial output before timeout.
- Permission denied → error, explains which mode allows it.

The agent can see errors and typically responds by adjusting the request or using a different tool.

## Performance Tips

1. **Use `glob` before `read_file`** to find the right file.
2. **Truncation aware** — large files are head/tail truncated per `tool_output_max_lines`. Use `grep` to find relevant sections first.
3. **Batch reads** — if you need multiple files, read them in sequence; async overhead is minimal.
4. **RAG first** — for code understanding, use `semantic_search` before grep; embeddings are faster than regex on large codebases.
5. **Shell commands** — cache output (don't re-run `cargo build` multiple times; save the result and reference it).

### `tool_search` / `tool_call`

Registered only when `[tools].defer` actually withholds something. See
[CONFIGURATION.md](CONFIGURATION.md) for the setting and
[FEATURES.md](FEATURES.md) for why.

`tool_search(query)` returns the matching deferred tools with their full input
schemas; `tool_call(name, arguments)` invokes one. `tool_call` exists because
a model cannot emit a `tool_use` block for a tool that was not in the request
— knowing the schema is not enough, the provider validates against the list it
was given — so discovery needs a matching invocation path.

**Permission:** the wrapper needs nothing; the wrapped call is gated on its own
capability inside `dispatch`, exactly as a direct call would be. `tool_call`
refuses to invoke itself or `tool_search`, and cannot reach a tool removed by
`[tools].preset` or `disabled_tools` — those unregister rather than hide.
