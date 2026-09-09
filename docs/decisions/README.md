# Decision records

Short notes on decisions whose *reasoning* is not visible from the code, kept
so the next person — or the same person in six months — does not have to
re-derive it or, worse, quietly undo it.

## When to write one

A record earns its place when at least one of these is true:

- **The boundary is arbitrary-looking but load-bearing.** Someone reading the
  code would reasonably ask "why not the other way?" and the answer is not in
  the diff.
- **It was rejected.** A plausible idea that was considered and turned down is
  the most valuable thing here, because nothing else in the repo records it and
  it *will* be proposed again.
- **It was deferred, with a trigger.** "Not yet, and here is what would change
  that" beats an untracked intention.

## When not to

[CONTRIBUTING.md](../../CONTRIBUTING.md) asks you to explain the why in the
commit message, and that remains the default — a record is not a substitute
for a good one. Skip a record for an ordinary bug fix, a refactor whose
motivation is obvious from the diff, or anything a comment next to the code
would answer better. A record you have to hunt for is worse than a comment you
cannot miss.

Aim for a page. If it is longer than the change it explains, it is the wrong
shape.

## Status values

| Status | Meaning |
|---|---|
| `accepted` | In force. |
| `rejected` | Considered and declined. Kept so it is not re-proposed blind. |
| `deferred` | Not now. The record names the trigger that would change it. |
| `superseded by NNNN` | Replaced. The newer record explains what changed. |

A record is not edited to reverse a decision — it gets a status change and a
pointer. The history of what we believed is the point.

## Index

| # | Decision | Status |
|---|---|---|
| [0001](0001-no-plugin-architecture.md) | Wingman stays a fixed binary rather than a plugin runtime | rejected |
| [0002](0002-loop-owns-the-session-log.md) | The agent loop is the only writer of the session log | accepted |
| [0003](0003-spill-locator-goes-in-the-head.md) | A spill locator is the first line of a tool result, not part of the elision marker | accepted |
| [0004](0004-pruning-is-idempotent-by-construction.md) | Tool-result pruning is bounded so a second pass is a no-op | accepted |
| [0005](0005-repeat-guard-counts-denied-calls.md) | The repeat guard counts denied calls and treats exempt tools as transparent | accepted |
| [0006](0006-two-project-roots.md) | Containment root and owning root are separate questions | accepted |
| [0007](0007-bound-the-read-not-the-result.md) | Attachment and output caps bound the read, not just the result | accepted |
| [0008](0008-defer-background-jobs-and-pty.md) | Background jobs and persistent PTY wait for the 600s ceiling to actually bite | superseded by 0011, 0012 |
| [0009](0009-test-docs-rather-than-generate-them.md) | Docs are tested against the code, not generated from it | accepted |
| [0010](0010-claude-code-hooks-are-imported-not-adopted.md) | Claude Code hooks are imported opt-in, trust-gated, and translated | accepted |
| [0011](0011-background-jobs.md) | Background jobs are a process table, not a capability seam | accepted |
| [0012](0012-stdin-instead-of-a-pty.md) | Processes are driven over stdin; no PTY | accepted |
| [0013](0013-no-speculative-seams.md) | No speculative seams; extract when a second implementation arrives | superseded by 0014 |
| [0014](0014-the-a2-seams-were-built.md) | The A2 seams were built; what that cost and bought | accepted |
| [0015](0015-run-plan-not-code-mode.md) | `run_plan` instead of Code Mode; the loop already batches | accepted |
| [0016](0016-the-diff-is-not-green-and-red.md) | The panel's diff is told apart by ground and gutter, not by hue | accepted |
| [0017](0017-notifications-are-a-file-inbox.md) | Desktop notifications are a file inbox, and approvals bypass it | accepted |
| [0018](0018-the-notifier-is-not-a-workspace-member.md) | The desktop notifier is excluded from the workspace | accepted |
| [0019](0019-the-notifier-ships-an-unsigned-installer.md) | The notifier ships an installer, and it is unsigned | accepted |
| [0020](0020-repetition-is-guarded-twice.md) | Repetition is guarded twice, and only the second one stops a turn | accepted |
