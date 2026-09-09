# HTTP API (`wingman serve`)

Drive Wingman over HTTP from anything: a phone, another machine, a Shortcut, a
Slack bot, CI. One daemon serves several repos, streams live output over SSE,
and enforces a permission ceiling that a request cannot raise.

```bash
wingman serve                      # bind [serve].addr, serve the allowlisted projects
wingman serve --init-token         # generate a token into the OS keyring, print it once
wingman serve --addr 127.0.0.1:8787
wingman serve --list               # show resolved projects + effective ceiling, then exit
```

Status: **shipped.** The build order and the reasoning behind each decision are
in [HTTP-API-PLAN.md](HTTP-API-PLAN.md).

---

## Design in one paragraph

The server is a thin shell over things Wingman already does. Pilot state is
already an append-only `tasks.jsonl` plus an atomic `state.json` per run, and
pilot control is already "append a JSON line to `control.jsonl`" — so those
routes are filesystem reads and one-line appends, in-process, with no
orchestrator coupling. Agent turns run as child processes
(`wingman --print --json` with the project as its cwd), the same mechanism pilot
already uses for its workers: process-wide `current_dir` cannot be shared safely
between concurrent projects, a crashing turn cannot take the daemon with it, and
the child's existing NDJSON event stream maps line-for-line onto SSE. Everything
else is a declarative table of `(method, path) -> wingman <subcommand>`, so the
long tail of the CLI is reachable without forty hand-written handlers.

No web framework: HTTP/1.1 on `tokio::net::TcpListener`, reusing the request
parser already in `wingman-autonomous::httpsig`. The dependency budget for the
whole feature is zero new crates.

---

## Configuration

`[serve]` is a **global-config-only** section. It is not in
`PROJECT_SAFE_KEYS`, so a `.wingman/config.toml` inside a cloned repo can never
define a token, widen the project allowlist, or raise the ceiling — untrusted
project layers have those keys dropped at load time.

```toml
[serve]
# Bind address. Any-interface is allowed but requires a token (below).
addr = "0.0.0.0:8787"

# Bearer token. Supports ${ENV_VAR} indirection, or "keyring" to read the
# entry written by `wingman serve --init-token`.
token = "keyring"

# Highest permission mode any request may obtain: read-only | plan | auto-edit | yolo.
# A request may ask for less. It can never obtain more.
max_permission_mode = "auto-edit"

# Concurrent agent turns across all projects. Queued beyond this.
max_concurrent_turns = 2

# Per-request wall clock for turns and exec, in seconds.
request_timeout_secs = 1800

# Repos this server will serve. Nothing outside this list is reachable.
[[serve.projects]]
id   = "wingman"                       # url-safe; defaults to the directory name
root = "C:/Users/you/dev/Wingman"

[[serve.projects]]
id   = "api"
root = "/home/you/src/api"

# Outbound push: the server calls you, so a phone does not have to poll.
[serve.push]
url    = "${WINGMAN_PUSH_URL}"          # Slack incoming-webhook shape, or any POST target
events = ["run.finished", "run.awaiting_approval", "turn.finished", "verify.failed"]
```

### Startup refusals

The server exits non-zero rather than starting when:

| Condition | Why |
|---|---|
| No token and `addr` is not loopback | An open door to a coding agent with write access. |
| Token shorter than 32 chars on a non-loopback bind | Brute-forceable over a network. |
| `[[serve.projects]]` is empty | Nothing to serve; likely a config mistake. |
| A project `root` does not exist, or two entries share an `id` | Ambiguous routing. |
| `max_permission_mode = "yolo"` without `--allow-yolo` on the command line | Remote arbitrary shell needs a deliberate act at launch, not a config line someone forgot. |

Plaintext HTTP is the transport. Put it behind Tailscale, a WireGuard subnet, an
SSH tunnel, or a TLS reverse proxy (Caddy/nginx) — Wingman does not terminate
TLS and does not pretend to.

---

## Authentication

Every route except `GET /v1/health` and the two session routes below requires:

```
Authorization: Bearer <token>
```

Compared in constant time. A failed auth returns `401` with
`{"error":"unauthorized"}` and no detail. Clients may also pass the token as
`X-Wingman-Token` for environments where the `Authorization` header is awkward.

There is one token. Per-token ceilings and per-token project scoping are a
deliberate non-goal for now (see [Non-goals](#non-goals)).

**Browsers use a cookie instead**, because `EventSource` cannot set headers and
a token in a query string ends up in every access log.

| Method | Path | Effect |
|---|---|---|
| `POST` | `/v1/ui/session` | Body `{"token":"…"}`. Verifies it with the same constant-time comparison, then returns `Set-Cookie: wingman_token=…; Path=/; HttpOnly; SameSite=Strict`. A wrong token is `401` and sets nothing. Ungated, because this *is* the authentication. |
| `DELETE` | `/v1/ui/session` | Clears the cookie. Ungated, so a browser holding a cookie the server no longer accepts can still drop it. |

`GET /v1/health` reports `auth_required` so a client can tell whether any of
this is needed before asking for a secret that may not exist.

An explicit `Authorization` or `X-Wingman-Token` header wins over the cookie: a
script that sends a credential means it, and must not have a stale browser
cookie silently substituted. `Secure` is not set — the panel is served over
plain HTTP on loopback or a LAN address, where a `Secure` cookie is discarded;
see [WEB-UI.md](WEB-UI.md) for the full reasoning and what it costs.

### Pairing a device

Getting the token onto a phone otherwise means moving a 43-character secret
there by hand, which in practice means pasting it into a chat app or a note —
putting the durable credential somewhere it outlives the moment.

```bash
wingman serve --pair
```

prints a single-use code, valid for 10 minutes, that can be exchanged **once**
for the API token:

| Method | Path | Effect |
|---|---|---|
| `POST` | `/v1/pair/redeem` | Body `{"code":"…"}`. Returns `{"token":"…"}` on the first correct redemption and burns the code. Ungated, because redeeming *is* the authentication. `401` for a wrong code, `410` for one already used or expired, `404` when pairing was not opened, `409` when the server has no token to hand out. |

This is **enrolment, not authority**. Every paired device ends up holding the
same token with the same ceiling; per-token scopes remain a non-goal. What the
code buys is that it is short-lived and single-use, so an intercepted one is
worth minutes rather than forever — and using it is *detectable*, because the
intended device's redemption then fails saying the code was already spent. If
that happens, rotate with `wingman serve --init-token`.

There is no endpoint that mints codes: pairing opens only for a run started
with `--pair`, so someone who reaches the daemon cannot ask it to start.

---

## Project scoping

Every route that touches a repo takes a project, either as a path segment
(`/v1/projects/{id}/...`) or a `?project=` query parameter. It must resolve to a
configured `id`, or to a `root` that matches a configured root exactly. Anything
else returns `404 unknown project`. Paths inside request bodies resolve against
that root and are rejected if they escape it.

---

## Response conventions

- Success: `200`, `application/json`, the payload shape documented per route.
- Streams: `text/event-stream`, one JSON object per `data:` line, `event:` set
  to the event kind, a `:keepalive` comment every 15s.
- Errors: a non-2xx status and `{"error":"<message>"}`. `400` malformed,
  `401` unauthenticated, `403` ceiling violation, `404` unknown
  project/run/session, `409` conflicting state (approving a run that is not
  gated), `429` turn queue full, `500` internal, `504` timeout.
- `GET /v1/schema` dumps the route table as JSON — method, path, params, and the
  subcommand each table-driven route runs. Generated from the table, so it
  cannot drift from the implementation.

---

## Routes

### Meta

| Method | Path | Returns |
|---|---|---|
| `GET` | `/v1/health` | `{"ok":true,"version":"0.1.0","uptime_secs":n}`. Unauthenticated. |
| `GET` | `/v1/schema` | Route table: paths, params, backing subcommand. |
| `GET` | `/v1/projects` | Allowlisted projects: `id`, `root`, git branch, whether `indexd` is live, index age. |
| `GET` | `/v1/events` | SSE firehose of run transitions across every project (`run.started`, `run.awaiting_approval`, `run.finished`). Same detector outbound push uses, so the stream and a webhook cannot disagree. |

### Notifications

Global, not project-scoped: the inbox lives in `~/.wingman/` because the
processes that write to it — a detached `pilot run`, its workers, a TUI, a
`serve` child — span projects. Same file the desktop popup reads, so what the
panel shows and what the popup shows cannot disagree. See `docs/NOTIFIER.md`.

| Method | Path | Body | Effect |
|---|---|---|---|
| `GET` | `/v1/notifications` | — | `{"notifications":[…]}` — the open cards, oldest first. Answered, expired, and over-a-day-old ones are already filtered out. |
| `POST` | `/v1/notifications/{id}/reply` | `{"action":"…","text":"…"}`, both optional | Answer one card. Both absent is a dismissal, which still records a reply so the card does not come back. `404` if the card is not open, so a client stops showing something it could not act on. |

A button carrying a control command routes to the run's `control.jsonl` instead
of the replies file — the server decides which from the card, so a client never
has to know the `ControlCommand` vocabulary. The response says which happened:
`{"answered":"…","via":"reply"|"control"}`. A card naming a directory that is
not a pilot run is refused with `409`.

### Pilot — read

| Method | Path | Returns |
|---|---|---|
| `GET` | `/v1/projects/{p}/pilot/runs` | `RunSummary[]` — `run_id`, `goal`, `status`, `done`/`total`, `terminal`. Newest first. No cost or timestamps: `RunSummary` does not carry them — read one run for `totals`. |
| `GET` | `/v1/projects/{p}/pilot/runs/{run}` | Full `RunState` snapshot. |
| `GET` | `/v1/projects/{p}/pilot/runs/{run}/events?tail=n` | Last `n` events from `tasks.jsonl`. |
| `GET` | `/v1/projects/{p}/pilot/runs/{run}/stream` | SSE: events as they are appended. |
| `GET` | `/v1/projects/{p}/pilot/runs/{run}/log?tail=n` | The orchestrator's own stdout as `{text, total_lines, shown_lines}` (`tail` defaults to 500, max 5000). `events` is what the run *did*; this is what it *said* — the plan it costed, why it asked for approval or did not, and what it exited on. A run that failed while planning emitted no events worth reading and one line here that explains it. Both counts are returned so a client can say "the last 500 of 2,341 lines" rather than presenting a tail as the whole log. Colour is left in: it is the file as written, and stripping ANSI is the reader's job. |
| `GET` | `/v1/projects/{p}/pilot/runs/{run}/dashboard` | The ASCII dashboard `pilot watch` renders, as text — cheapest possible phone view. |

### Pilot — control

Each maps to one `ControlCommand` appended to the run's `control.jsonl`. The
orchestrator's existing watchdog applies it; the API does not reach into the
run's process.

| Method | Path | Body | Effect |
|---|---|---|---|
| `POST` | `/v1/projects/{p}/pilot/runs` | `{"goal":"…","yes":bool,"plan_only":bool,"model":"…"}` | Starts `wingman pilot run` as a child process; returns `{"run_id":"…"}` immediately. |
| `POST` | `/v1/projects/{p}/pilot/runs/{run}/approve` | — | Release the plan gate. |
| `POST` | `/v1/projects/{p}/pilot/runs/{run}/veto` | — | Reject the pending plan. |
| `POST` | `/v1/projects/{p}/pilot/runs/{run}/abort` | `{"task":"id"}` optional | Abort the run, or one task. |
| `POST` | `/v1/projects/{p}/pilot/runs/{run}/retry` | `{"task":"id"}` | Re-queue a failed or blocked task. |
| `POST` | `/v1/projects/{p}/pilot/goals` | `{"text":"…","author":"…"}` | Write an intake file for the discovery daemon. A body-claimed author never earns trust over the API — trust comes from `[pilot.daemon].trusted_authors` as the daemon matches it, not from a request asserting an identity. |

### Board

The board is **global** — one `~/.wingman/board.db` spanning every project — so
these routes are not project-scoped. `wingman-board` is called in-process, so
the column, roll-up and badges are the same derivation `wingman board` renders.

| Method | Path | Effect |
|---|---|---|
| `GET` | `/v1/board?project=&archived` | Columns, cards with derived column / roll-up / badges, and the project registry in one response. |
| `GET` | `/v1/board/projects` | The board's registry, each with whether its directory still exists. |
| `POST` | `/v1/board/cards` | `{"project":"…","title":"…","goal":"…?","notes":"…?","labels":[]}` → `{id, short}`. |
| `GET` | `/v1/board/cards/{card}` | One card and its dispatch history. `{card}` is an id or a unique prefix. |
| `PATCH` | `/v1/board/cards/{card}` | `{"title":"…?","goal":"…?"}`. Only the keys present are changed — an absent `goal` is left alone, not cleared. A card is durable and outlives its runs, so correcting a badly worded goal must not mean deleting the history that wording produced. |
| `POST` | `/v1/board/cards/{card}/dispatch` | `{"again":bool,"args":[]}` → `{run_id, project, pid}`, spawned detached. |
| `POST` | `/v1/board/cards/{card}/archive` | `{"restore":bool}` to unarchive instead. |
| `DELETE` | `/v1/board/cards/{card}` | Forgets the card and its dispatch history. The runs on disk are untouched. |

`badges` carry `{kind, text}` rather than the bare strings
`board list --json` emits, so a renderer can tell a `progress` badge from a
label a user typed without parsing formatted text.

**Dispatch is bounded by the allowlist.** The registry can name repos this
daemon does not serve; dispatching one is a `403`. Without that, the token
would start an agent with write access in any directory the board remembers.

`args` are forwarded to `pilot run` verbatim and validated by the same list the
CLI uses — `--worker-mode`, `--detached`, `-d` and `--watch` are refused with a
`400`.

### Sessions and turns

Conversations are server-held: the transcript is a normal
`<project>/.wingman/sessions/<id>.jsonl`, so a session survives a daemon restart
and shows up in `wingman session list` like any other.

| Method | Path | Body | Returns |
|---|---|---|---|
| `POST` | `/v1/projects/{p}/sessions` | `{"model":"…","mode":"…"}` | `{"session_id":"…"}` |
| `GET` | `/v1/projects/{p}/sessions` | — | Sessions with id, first prompt, model, turn count and `mtime` (Unix seconds), **newest first**. Directory order is neither stable across platforms nor meaningful, and "which conversation was I just in" is the only question a session list is opened to answer. |
| `GET` | `/v1/projects/{p}/sessions/{id}` | — | Full transcript as `SessionRecord[]`. |
| `POST` | `/v1/projects/{p}/sessions/{id}/turns` | `{"prompt":"…","mode":"…","model":"…"}` | SSE stream of `wingman_core::AgentEvent`: `text_delta`, `thinking_delta`, `tool_start`, `tool_result`, `usage`, `turn_complete`, `verification`, `stop`, `error`. The event name is the payload's own `type`, so this list is the enum. Resumes the session history. |
| `POST` | `/v1/projects/{p}/turns` | same | One-shot turn, no session continuity. |
| `DELETE` | `/v1/projects/{p}/sessions/{id}` | — | Forget the session: deletes the transcript **and** its entries in the global session index, so `recall_session` cannot resurface it. Reports `deindexed` so a partial delete is visible in the response, not a surprise later. |

A turn holds one slot of `max_concurrent_turns`; a second turn on the *same*
session returns `409` while the first is in flight.

### Cost over time

`cost` reports `~/.wingman/usage.json`: a machine-wide running total with no
timestamps in it, so it cannot answer "what did last week cost" or "what does
*this* repo cost". Both answers are already on disk — every transcript carries
`usage_delta` records with a timestamp, and `session_start` names the model
they were billed at — so this walks `<project>/.wingman/sessions/*.jsonl` and
prices each delta. It is a native handler rather than a table row because
there is no subcommand behind it.

| Method | Path | Params | Returns |
|---|---|---|---|
| `GET` | `/v1/projects/{p}/cost/timeline` | `days` — window ending today (default 30, max 365) | `{days[], models[], window_usd, total_usd, total_turns, first_day, last_day, sessions, unpriced_turns}` |

Three things it guarantees, because a spend chart that gets them wrong is
worse than no chart:

- **Days are gap-filled.** A quiet day is a zero in the series, not an
  absence — plotting three recorded days evenly spaced claims they were
  consecutive.
- **It never reads `usage.json`.** That total and this one measure different
  things, and they will disagree. Whichever you show, label which it is.
- **Unpriceable turns are counted, not dropped.** A model missing from the
  pricing table lands in `unpriced_turns`, so a total that is short says so
  instead of quietly understating the bill.

`first_day`/`last_day` describe the whole history behind the window, so a
client can say "nothing in the last 30 days, and the last session was in
February" rather than drawing an empty chart.

### Reads and admin (table-driven)

Each runs the named subcommand in the project root. Output that parses as JSON
is returned as JSON; anything else comes back as
`{"stdout":"...","stderr":"...","exit":n}`, which is honest about being text.

| Path | Subcommand |
|---|---|
| `GET /v1/projects/{p}/cost?compare` | `cost --json [--compare]` |
| `GET /v1/projects/{p}/context` | `context --json` |
| `GET /v1/projects/{p}/knows` | `knows` |
| `GET /v1/projects/{p}/doctor` | `doctor` |
| `GET /v1/projects/{p}/attest` | `attest` |
| `GET /v1/projects/{p}/diff?file=` | `diff [file]` |
| `GET /v1/projects/{p}/explain?base=&staged` | `explain [--local base] [--staged]` |
| `GET /v1/projects/{p}/review?pr=&base=` | `review [pr] [--local base]` |
| `GET /v1/projects/{p}/router/stats?all` | `router stats [--all]` |
| `GET /v1/projects/{p}/index/status` | `indexd --status` |
| `POST /v1/projects/{p}/index/reindex` | `indexd` |
| `GET /v1/projects/{p}/memory` | `memory review` |
| `POST /v1/projects/{p}/memory/sync?ref=` | `memory sync [ref]` |
| `GET /v1/projects/{p}/trust` / `POST .../trust` | `trust show` / `trust add` |
| `POST /v1/projects/{p}/checkpoints?label=` | `checkpoint [--label]` |
| `POST /v1/projects/{p}/rewind?steps=` | `rewind [steps]` |
| `POST /v1/projects/{p}/schedule/run?all` | `schedule [--all]` |
| `GET /v1/projects/{p}/config` | `config show --json` |
| `GET /v1/config` / `PATCH /v1/config` | the server's merged config; patch writes the **global** file |
| `GET /v1/config/schema` | JSON Schema derived from the config types, plus defaults, redacted keys, read-only sections, and the file a patch writes to |

Every table route is project-scoped, including the config-adjacent ones: the
merged view depends on which repo you are in. Query parameters are an allowlist
*per route* — a key a route does not declare is ignored, never appended as an
extra flag, and values are separate argv entries rather than text spliced into
a command line.

`GET /v1/config` redacts credentials (`api_key`, tokens, signing secrets,
webhook URLs): a config read must not become credential exfiltration for whoever
holds the API token. `PATCH /v1/config` merges into the global file only, never
a project's `.wingman/config.toml` — that is the untrusted layer, and an API that
could write it would be a way to smuggle executable keys into a repo. It refuses
`[serve]` outright, because a server that can rewrite its own token, ceiling, or
allowlist has no ceiling. Patches are validated by round-tripping through the
real config parser before anything is written.

**A patch is a minimal edit, not a rewrite.** The file is edited as a TOML
document, so changing one field yields a one-line diff and comments, key order
and formatting all survive — including the comment sitting above the key whose
value changed. Earlier builds parsed to a table and re-serialised it, which
reordered every section and discarded every comment in the file.

`GET /v1/config/schema` exists so a client can build a settings UI without
hard-coding anything about the config. It returns a JSON Schema derived from
the `wingman-config` types — every field's type, default, and `///`
documentation — alongside `defaults`, `redacted_keys`, `readonly_sections`, and
`writes_to`. The two lists are the same constants the server enforces on read
and on write, so a client cannot be holding a stale copy of either.

There is no `/v1/skills`, `/v1/mcp`, or `/v1/providers`: no CLI command backs a
listing for those today, and faking one would report something the tool cannot
actually tell you. `GET .../knows` and `GET .../doctor` cover the same ground.


### Passthrough

```http
POST /v1/projects/{p}/exec
{"args": ["review", "--staged"], "stream": true}
```

Runs `wingman <args>` in the project root. `stream: true` returns SSE with
interleaved `stdout`/`stderr` lines and a final `exit`; otherwise a JSON blob
with both buffers and the exit code.

This is the "everything else" escape hatch and it is deliberately narrow:

- `args` must be an array of strings — never a shell string, so there is no
  shell to inject into. No `sh -c`, ever.
- The first arg must be a known subcommand. `serve` (recursion), `login`,
  `logout`, and `--worker-mode` (a pilot-internal contract) are refused.
- `--mode` and `--yolo` are clamped to the ceiling; a `--mode` above it is a
  `403`, not a silent downgrade.
- The subprocess inherits the ceiling via `WINGMAN_PERMISSION_MODE`.

With `max_permission_mode = "yolo"` this endpoint is remote code execution by
design — which is why yolo needs `--allow-yolo` at launch.

---

## Outbound push

When `[serve.push]` is set, the server POSTs to `url` on each subscribed event so
a phone never has to poll:

```json
{
  "event": "run.awaiting_approval",
  "project": "wingman",
  "run_id": "20260818-a3f1",
  "text": "Run awaiting plan approval: add SSE keepalives — 4 tasks, est $0.42",
  "url": "http://host:8787/v1/projects/wingman/pilot/runs/20260818-a3f1"
}
```

`text` is Slack-incoming-webhook compatible, so an existing webhook URL works
unchanged. Delivery is best-effort with one retry; a dead endpoint logs and never
blocks a run. A daemon that has just started primes quietly — it records the
runs it finds without announcing last week's results.

Subscribable events are the run transitions above. Turn-level events
(`verify.failed` and friends) are not pushed: a turn already streams its events
to whoever asked for it, and a notification per verification would be noise.

---

## Remote client mode

```bash
export WINGMAN_REMOTE=http://box:8787
export WINGMAN_SERVE_TOKEN=…

wingman --remote --print "why is the index stale?"   # streams SSE to stdout
wingman --remote pilot watch                          # live dashboard over HTTP
wingman --remote pilot approve                        # control a remote run
wingman --remote cost --compare                       # any read, via passthrough
```

`--remote` (or `WINGMAN_REMOTE`) sends the command to a server instead of running
it locally. `pilot watch` polls the API instead of `state.json`; the read
commands route through `/v1/exec` and print the server's stdout verbatim, so a
new subcommand works remotely the day it lands locally.

The full interactive TUI over HTTP is **not** in scope — it needs bidirectional
permission prompting, which the SSE-plus-JSON shape cannot carry. `--print` and
`pilot watch` are the two surfaces that actually matter away from your desk.

---

## Non-goals

Stated so they do not read as oversights:

- **TLS.** Use a tunnel or a reverse proxy.
- **Multiple tokens with per-token scopes.** One token, one ceiling. If you need
  a read-only phone token and a read-write laptop token, run two servers with
  different ceilings on different ports.
- **A web UI.** Deliberately deferred; the API is the product, and a browser page
  can be added later without changing a route.
- **CORS.** No browser origin is expected to call this yet.
- **Auth rate limiting.** A 32-char random token on a private network does not
  need lockout logic; if the port is public, that was the mistake.
- **Interactive permission approvals.** The ceiling replaces them. Anything
  needing a human decision belongs in a pilot run, which already has gates.
