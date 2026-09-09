# CLI Reference

Every subcommand. Run `wingman --help` for the authoritative list.

```text
wingman [OPTIONS] [COMMAND]
```

**Top-level flags**

| Flag                     | Description                                                                 |
| ------------------------ | --------------------------------------------------------------------------- |
| `--mode <MODE>`          | `read-only` \| `auto-edit` \| `yolo`.                                       |
| `--model <MODEL>`        | Model id, optionally prefixed: `anthropic/claude-opus-4-7`. Env: `WINGMAN_MODEL`. |
| `--reasoning <LEVEL>`    | `off` \| `low` \| `medium` \| `high`. Maps onto each provider's own reasoning control. Env: `WINGMAN_REASONING`. |
| `--print <PROMPT>`       | Run a single prompt and exit (non-interactive).                              |
| `--batch <FILE>`         | Run a JSONL file of prompts non-interactively. Pairs with `--json`.          |
| `--json`                 | Emit newline-delimited JSON events instead of text. Use with `--print`/`--batch`. |
| `-v`, `-vv`              | Increase log verbosity.                                                      |
| `--quiet`                | Suppress non-error stderr output.                                            |
| `--version`              | Print version and exit.                                                      |
| `--help`                 | Print help.                                                                  |

**Subcommands**

| Command              | Description                                            |
| -------------------- | ------------------------------------------------------ |
| `config init`        | Write a starter `~/.wingman/config.toml`. `--force` to overwrite. |
| `config show`        | Print the merged effective configuration. `--json` for JSON output. |
| `config paths`       | Print the resolved global and project config paths.    |
| `login [provider]`   | Probe a provider key, store it in the OS keyring, record the default model. `--list` shows provider ids; `--oauth` forces the ChatGPT browser flow; `--no-probe` / `--no-default` / `--base-url` / `--model` refine it. |
| `logout <provider>`  | Delete a provider's stored credential from the OS keyring. |
| `knows`              | Show what Wingman knows about this project: memories, skills, model routing, the verification gate, and index freshness. |
| `doctor`             | Health check: config, provider credentials, local model servers, the semantic index, language servers on PATH, and git/gh tooling. `--fix` repairs config keys that are unambiguous misspellings (backing the file up first); `--lint` runs config checks only — read-only, no probes, non-zero on a problem, for CI; `--json` emits findings as JSON. |
| `mcp-serve`          | Expose Wingman itself as an MCP server over stdio (tools + memory resources). Read-only by default; raise with `--mode`. |
| `serve`              | Serve the HTTP/SSE API so another machine, a phone, or CI can drive Wingman. `--addr`, `--init-token`, `--list`, `--allow-yolo`, `--pair`. See [HTTP-API.md](HTTP-API.md). |
| `explain`            | Explain-and-teach the working diff (per-file what/why). `--local <base>`, `--staged`. |
| `bench`              | Benchmark harness: time-to-first-token, tokens/task, verified-done rate. `--suite <file.jsonl>`, `--json`. |
| `distill`            | Distill durable facts from a past session into a pending-review file. `--session <path>`. |
| `indexd`             | Keep this project's semantic index warm (reindex, then watch). `--status`. |
| `rewind [n]`         | Scrub back through per-edit checkpoints; `rewind <n>` reverts the last n edits. |
| `router stats`       | Per-class model win-rates (gate pass-rate) for this repo. `--all` across repos. |
| `router preset local`| Print a recommended local-first `[router]` preset. `--model <provider/model>`. |
| `init`               | Scan the current project and write a starter `WINGMAN.md`. `--force` to overwrite. |
| `checkpoint`         | Snapshot the working tree into a tagged `git stash`. `--label <text>` for a note. |
| `undo`               | Restore the most recent `wingman checkpoint` via `git stash pop`. |
| `cost`               | Show per-model token usage and estimated USD spend. `--json` for JSON. `--compare` reprices your volume against other models (provider-cost arbitrage). |
| `session list`       | List recent session JSONL files for this project.       |
| `session fork`       | Copy an existing session into a new file (`--at N` truncates). |
| `worktree create <branch>` | Create a `git worktree` under `.wingman/worktrees/<branch>` for sandboxed experiments. |
| `worktree list`      | `git worktree list` passthrough.                        |
| `worktree remove <path>` | Remove a worktree by path.                          |
| `memory export <out>` | Export the global memory dir to a directory or `.json` pack. |
| `memory import <path>` | Import a memory pack (`--force` to overwrite).        |
| `memory diff <a> <b>` | Show differences between two packs (or live dir vs. pack). |
| `memory sync [<ref>]` | Reconcile team-shared project memory: rebuild `MEMORY.md` from files (resolving index merge conflicts), optionally fold in a git ref's memories. |
| `memory push` / `memory pull` | Sync memories through a team HTTP endpoint (`[team]`), non-clobbering. |
| `memory review`      | Review distilled pending memories: list, or `--promote N` / `--discard N` / `--promote-all`. |
| `review <pr#>`       | Fetch a PR diff via `gh` and run a one-shot review prompt. `--local <base>` for git-local diff. `--template <file>` for a custom prompt. |
| `discover`           | Probe localhost for Ollama / LM Studio / vLLM and list their models. |
| `schedule [--all]`   | Run any `[[schedule]]` entries whose cadence is due (cron-callable). |
| `skill extract`      | Mine recent session JSONLs for repeated tool-call sequences and write proposed skill drafts under `~/.wingman/skills/proposed/`. `--min N` (default 2), `--force` to overwrite. |
| `skill import <path>` | Import portable `SKILL.md` skills (a file, its dir, or a dir of them). `--project`, `--force`. |
| `skill export <name> <dir>` | Export a wingman skill as a portable `<dir>/<name>/SKILL.md` bundle. |
| `review-multi`       | Run a code-review prompt across multiple `provider/model` reviewers in parallel and merge findings by file:line. `--models a,b,c`. |
| `diff <file>` / `diff --patch <p>` | Interactive hunk-by-hunk accept/reject reviewer that writes the merged result back to the working tree. |
| `pilot run "<goal>"` | Plan a goal, spawn worker agents in isolated worktrees, open a PR. Flags: `--plan-only`, `--yes`, `--review`, `--watch`, `--no-pr`, `--base <rev>`, `--max-agents <n>`, `--max-usd <f>`, `--sandbox <host\|container\|vm>`, `--await-approval`. |
| `pilot status [run-id]` | One-shot ASCII summary of a run.                  |
| `pilot watch [run-id]` | Live dashboard that redraws on `state.json` changes. |
| `pilot resume <run-id>` | Resume an interrupted run; re-queues stuck tasks. |
| `pilot daemon`       | Always-on discovery daemon (requires `[pilot.daemon] enabled`). |
| `pilot abort` / `pilot retry <task>` | Control a live run via its control channel. |
| `pilot approve` / `pilot veto` | Approve or reject a run waiting at the plan-approval gate. |
| `pilot tell "<msg>" [run-id]` | Inject a message into the live worker's next turn (`--task <id>` to address one). |
| `pilot ask "<msg>" [run-id]` | Same, but wait for the worker's reply and print it (`--wait <secs>`, default 120). |
| `pilot intake slack\|email` | External intake transports → pilot request files (Slack Events server, `.eml` ingestion). |
| `board`              | Kanban board over pilot runs: a persistent, multi-project backlog. Cards are goals that outlive their runs; columns are derived from run state. |
| `board add "<title>"` | Create a Backlog card. `--goal <text>` (the prompt sent to pilot; defaults to the title), `--project <id>`, `--label <l>`, `--notes <text>`. |
| `board list`         | List cards with derived columns. `--project`, `--column backlog\|planned\|in-progress\|review\|done`, `--label`, `--all`, `--json`. |
| `board show <card>`  | One card: goal, notes, dispatch history, and the newest run's tasks with agent, model and cost. |
| `board dispatch <card>` | Start a pilot run for a card. `--again` to dispatch past a live run; `-- <flags>` forwards verbatim to `pilot run`. |
| `board archive <card>` | Archive a card (`--restore` to undo). Never deletes. |
| `board rm <card>`    | Permanently delete a card and its dispatch history (`--yes` required once dispatched). The runs are not deleted. |
| `board projects`     | List registered projects. `--forget <id>` hides one (cards kept), `--restore <id>`, `--relocate <id> <path>`, `--all`. |
| `board export --json` | Dump cards and dispatch history — the backup story for a per-machine DB. |

Running `wingman` with no subcommand launches the TUI against the resolved
provider and model.

> `wingman autonomous "<goal>"` is a deprecated alias for `wingman pilot
> run` — kept through M3, removed at M4.

Card ids accept any unique prefix of at least 4 characters. Projects register
themselves the first time you run a `pilot` or `board` command in a repo. See
[BOARD.md](BOARD.md).
