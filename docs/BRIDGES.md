# Bridges

Wingman speaks three protocols that other tools already know how to drive:
MCP (as both host and server), ACP, and a plain HTTP/SSE API. That means a
whole class of "Wingman should also do X" is answered by pointing an existing
tool at one of those seams, not by growing a subsystem inside Wingman.

This page is the list of bridges worth setting up, and — just as usefully —
the reasoning for why each one is a bridge rather than a feature.

The general rule: **Wingman is a coding agent.** A capability that makes it
better at understanding and changing code belongs inside it. A capability that
makes it *reachable from somewhere else*, or that drives a system Wingman is
not an expert in, belongs on the other side of a protocol. Every tool schema
Wingman registers is billed on every request of every session
(`wingman context` prints the exact number), so a feature that ships as a
bridge costs zero tokens for the people who never use it.

---

## Reach Wingman from a chat app

**What you want:** message Wingman from Telegram, WhatsApp, Slack, Discord,
Signal, or iMessage. Ask it something from your phone while away from the
terminal.

**Don't:** wait for Wingman to grow channel connectors. Each one is an API
client, an auth flow, a webhook listener, a reconnect policy, and a rate
limiter — for a surface that has nothing to do with reading code.

**Do:** put a personal-assistant runtime in front of Wingman and let it own
the channels. [OpenClaw](https://github.com/openclaw/openclaw) is the obvious
one: it already ships 25+ channels, and it is an MCP runtime, so it can
consume `wingman mcp-serve` directly.

```bash
wingman mcp-serve            # stdio; read-only by default
```

Point OpenClaw at it — in its config, alongside its other MCP servers:

```json5
{
  mcp: {
    servers: {
      wingman: {
        command: "wingman",
        args: ["mcp-serve"],
        cwd: "/path/to/your/repo",
      },
    },
  },
}
```

Every channel OpenClaw supports now reaches Wingman's tools. The two worth the
trip are `semantic_search` — a warm hybrid dense + BM25 index of the repo,
which is not something a general assistant can fake — and `recall_memory`,
which is the team memory under `<project>/.wingman/memory/`.

**Read-only is the right default here and you should leave it that way.** A
chat channel is an untrusted input surface: anyone who can message the bot is
driving the tool calls, and `wingman mcp-serve` is read-only precisely so that
"summarise what this repo does" cannot become "edit this file". If you want a
channel that can actually change code, do not widen `mcp-serve` — use
`wingman serve` instead, which has a per-server permission ceiling a request
cannot raise, and give it its own token.

### Or: HTTP, if you are writing the integration yourself

`wingman serve` is the better bridge when *you* control the client — a
Shortcut, a cron job, CI, a small bot of your own. It streams turns over SSE,
scopes to an allowlist of repos, and enforces a ceiling. See
[HTTP-API.md](HTTP-API.md).

Rule of thumb: **MCP when someone else's agent is calling Wingman; HTTP when
your own code is.**

---

## Drive a browser

**What you want:** the agent to click through a flow, fill a form, and check
the result — an end-to-end test, or verifying a UI change actually works.

**What Wingman already does:** loads a URL in headless Chrome, screenshots it,
and fails the verification gate if the render drifted
(`[verify].browser`, `crates/wingman-browser`). That is deliberately *not*
browser automation — it is a regression check, and it takes no input.

**Don't:** grow a click/type/navigate tool family. That is Playwright's job,
Playwright does it better, and it would add a dozen tool schemas to every
request made by every user who never opens a browser.

**Do:** register Playwright's MCP server. Wingman is an MCP host, so its tools
arrive namespaced as `mcp__playwright__*` and dispatch like built-ins.

```toml
# ~/.wingman/config.toml
[mcp.playwright]
transport = "stdio"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

Then `/mcp` in the TUI to confirm it connected.

Two things to know before you turn this on:

- **It is not free.** Those tool schemas are billed on every request for the
  whole session, not just the turns that use them. Check the damage with
  `wingman context`, and consider a `[tools.presets]` entry that includes
  the browser tools so you can opt into them per session rather than always:

  ```toml
  [tools.presets]
  e2e = ["read_file", "edit_file", "grep", "glob", "run_shell", "mcp__playwright__*"]
  ```

  ```bash
  wingman --preset e2e
  ```

- **Page content is untrusted.** Anything the browser reads is wrapped in
  Wingman's untrusted-content fence before it reaches the model, the same as
  `web_fetch` output. That is a mitigation, not a guarantee: a page that can
  convince a model to run a shell command is a real risk, so do not pair
  browser automation with `yolo` on a site you do not control.

---

## Read a PDF

Handled in-tree rather than bridged: `read_file` on a `.pdf` extracts its text,
the same way it already renders `.ipynb` cells instead of raw JSON. A design
doc or an API spec you have been handed as a PDF is ordinary coding context,
and shelling out to another process for it would be more moving parts than the
extraction is worth. See [TOOLS.md](TOOLS.md).

---

## What is deliberately not bridged

| Capability | Why not |
| --- | --- |
| Voice / TTS | Nothing to do with code. The desktop notifier already covers "tell me when the long run needs me". |
| Computer use / desktop control | A coding agent's world is the repo, the compiler, and the language server. Driving the desktop is a different product. |
| Media generation | Same. |
| A second control plane | `wingman serve` plus the pilot board is one. Adding a gateway that fronts it would be two things to secure and two places to look when something breaks. |
| A plugin ABI | MCP *is* the plugin ABI. A second extension surface would need its own versioning, sandboxing, and distribution story to do what MCP already does. |
