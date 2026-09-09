# 0020 — Repetition is guarded twice, and only the second one stops a turn

**Status:** accepted
**Date:** 2026-09-09

## Context

`ToolRegistry` has had a repeat guard since early on: a chain of consecutive
calls with identical arguments, thresholds at `[3, 5, 8]`, an advisory appended
to the tool result at each one. It never blocks a call and never rewrites one.

That guard is good at what it does and has two holes, both structural rather
than bugs.

**It is consecutive-only.** The chain resets the moment the arguments change,
so this never registers as repetition at all:

```
grep "TurnGate" → read_file src/gate.rs → grep "TurnGate" → read_file src/gate.rs → …
```

which is the exact shape a model falls into when it cannot find something and
keeps re-checking the same two places. `repeat_exempt` exists because
*interleaved bookkeeping* was laundering runs the same way; alternation between
two real calls is the same problem one level up, and an exempt list cannot
reach it.

**It never stops anything.** An advisory the model ignores is an advisory. In a
terminal that is fine — a human is watching and can hit Esc. `wingman pilot`,
`spawn_subagent` and `wingman --print` in CI have nobody watching, and will
spend the whole `max_turns` budget on a call whose answer stopped changing
eight repetitions ago. `wingman cost` then reports the bill.

## Decision

Add a second layer in `wingman-core` rather than extending the first, and give
only the second one the authority to end a turn.

- **`wingman-tools`** keeps the consecutive chain: cheap, advisory, unchanged.
- **`wingman-core::LoopGuard`** keeps a rolling window (default 24 calls) and
  counts *occurrences* of a call, whatever came between. Warning at 4, and at
  8 the turn ends with a new `AgentStop::LoopDetected`.

The two share `canonical_args`, moved into `wingman-core` for the purpose. Two
copies would eventually disagree about what "the same call" means, and the
guard that disagreed would be the one that silently stopped matching.

## Why not extend the existing guard

That was the obvious move and it is the one to argue against, because it will
be proposed again.

Making the tools-layer chain a window changes what its existing thresholds
mean. `repeat_guard_resets_when_the_arguments_change` is a test asserting the
current semantics, and it asserts them *correctly* — a nudge on the third
consecutive identical call is a different and more confident signal than a
nudge on the third occurrence in a window of twenty-four. Widening it in place
would have made the cheap advisory noisier to buy the expensive one, and made
one knob mean two things.

Layers also put each control where it can act. Only the agent loop can end a
turn; the registry dispatches one call at a time and does not know what a turn
is. Putting the abort in `wingman-tools` would have meant inventing a channel
from a tool result back to the loop, which is a seam
[0013](0013-no-speculative-seams.md) says not to build without a second user.

## Why the abort is not simply "stop"

The loop refuses the whole batch and writes a `tool_result` for **every**
outstanding `tool_use` before stopping. An unanswered `tool_use` block makes
the *next* provider request malformed, so a turn that aborted by dropping the
calls would leave a session that cannot be resumed, forked, or `/rewind`ed —
the guard would trade a wasted budget for a corrupt transcript. There is a
regression test for exactly this.

## Why pollers are exempt

Wingman's own pilot manager found this within an hour of the guard existing:

```
assign_task {"task_id": "t1"}   ← every tick, byte-identical, until a worker
assign_task {"task_id": "t1"}      moves t1 out of Todo
```

That is not a loop, it is the mechanism. The manager re-reads run state each
tick and re-issues the same call while the state it is waiting on is unchanged;
the answer changes only when something *else* changes the world.

So `LoopGuard` takes an exempt list, `[tools].loop_exempt` sets it, and
`tools::ORCHESTRATION_TOOLS` names the manager's own. This is a general need
rather than a special case for pilot: an MCP server's "check job status" tool
has the identical shape, and without the list the guard would end a turn for
doing its job.

An exempt call is skipped entirely rather than merely not counted — it does not
enter the window at all, so interleaving bookkeeping cannot age a real loop out
of it. That mirrors what `repeat_exempt` means one layer down, and for the same
reason.

## The numbers

`loop_window = 24`, `loop_warn_at = 4`, `loop_abort_at = 8` are judgement, not
measurement, and are configurable because of it. The shape of the judgement:

- The warning lands at 4, one past the tools-layer nudge at 3, so an
  interactive session hears the cheap advice first and only meets this one when
  that advice did not take.
- Eight identical calls inside twenty-four is not a workflow. Legitimate
  repetition at that density is a poller, and pollers are exempt by name.
- `loop_abort_at = 0` disables the layer entirely and returns the previous
  behaviour, for anyone who disagrees with the paragraph above.

## Consequences

`AgentStop` gained a variant, so every exhaustive match over it had to say what
it means — ACP has no vocabulary for "it was going in circles" and maps it to
`refusal`, `--print --json` reports `loop_detected`. A caller that wants to
tell "gave up on a real problem" from "was wasting money" now can, which
`MaxTurns` alone did not allow.
