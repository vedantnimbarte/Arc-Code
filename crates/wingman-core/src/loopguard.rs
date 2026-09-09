//! The ceiling on repetition.
//!
//! `wingman-tools`' repeat guard watches for a *run* of identical calls and
//! nudges the model when it sees one. That catches the common loop and costs
//! nothing, but it has two holes this module closes.
//!
//! First, it is consecutive-only: the chain resets the moment the arguments
//! change, so `grep X → read_file Y → grep X → read_file Y` — the shape a
//! model falls into when it cannot find something and keeps re-checking the
//! same two places — never registers as repetition at all.
//!
//! Second, it never stops anything. An advisory the model ignores is an
//! advisory, and an unattended `wingman pilot` run will happily spend its
//! whole `max_turns` budget looping. Nudging is the right default for an
//! interactive session where a human is watching; it is not a control.
//!
//! So: a rolling window over recent calls, counting *occurrences* rather than
//! consecutive runs, with one advisory and then a hard stop. Bounded
//! correction, the same shape as the verification gate — nudge, then give up
//! honestly rather than burn the budget.
//!
//! Nothing here inspects tool *results*. Two identical calls that return
//! different output are still the model asking the same question twice, and
//! keying on results would let a tool with a timestamp in its output defeat
//! the guard entirely.

use std::collections::VecDeque;

use serde_json::Value;

/// What the guard makes of the call just recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say.
    Fine,
    /// Repetition worth telling the model about. Appended to the tool result.
    Warn(String),
    /// The turn should end. Carries the reason shown to the user and model.
    Abort(String),
}

/// Rolling-window repetition guard. One per agent loop.
#[derive(Debug, Clone)]
pub struct LoopGuard {
    window: VecDeque<String>,
    /// How many recent calls stay in the window.
    capacity: usize,
    /// Occurrence count at which the model is warned. `0` disables the warning.
    warn_at: usize,
    /// Occurrence count at which the turn is aborted. `0` disables the guard.
    abort_at: usize,
    /// Tools that are allowed to repeat: pollers and bookkeeping.
    ///
    /// Some tools repeat with identical arguments *because that is what they
    /// are for*. An orchestrator asking "is task t1 done yet" gets a
    /// different answer only when something else changes the world, and the
    /// call that asks is byte-identical every time. Counting those as a loop
    /// would abort the one design where repetition is the mechanism.
    ///
    /// Trailing `*` matches by prefix, as elsewhere.
    exempt: Vec<String>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        // 8 identical calls inside a 24-call window is not a workflow. The
        // warning lands at 4, which is one past the tools-layer nudge at 3, so
        // an interactive session hears the cheap advice first and only sees
        // this one when that advice did not take.
        Self::new(24, 4, 8)
    }
}

impl LoopGuard {
    pub fn new(capacity: usize, warn_at: usize, abort_at: usize) -> Self {
        Self {
            window: VecDeque::new(),
            capacity: capacity.max(1),
            warn_at,
            abort_at,
            exempt: Vec::new(),
        }
    }

    /// Exempt tools whose whole job is to be called again (see [`exempt`]).
    ///
    /// [`exempt`]: LoopGuard::exempt
    #[must_use]
    pub fn with_exempt(mut self, patterns: Vec<String>) -> Self {
        self.exempt = patterns;
        self
    }

    fn is_exempt(&self, name: &str) -> bool {
        self.exempt.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == p,
        })
    }

    /// A guard that never fires (`[tools].loop_abort_at = 0`).
    pub fn disabled() -> Self {
        Self::new(1, 0, 0)
    }

    /// Whether this guard can ever fire, so callers can skip the bookkeeping.
    pub fn is_enabled(&self) -> bool {
        self.abort_at > 0 || self.warn_at > 0
    }

    /// Record one dispatched call and judge it.
    ///
    /// Fires on the exact count rather than `>=`, so one long loop produces
    /// one warning and one abort rather than a warning per call after the
    /// fourth.
    pub fn record(&mut self, name: &str, args: &Value) -> Verdict {
        if !self.is_enabled() || self.is_exempt(name) {
            return Verdict::Fine;
        }
        let key = format!("{name}\u{1}{}", crate::canonical_args(args));
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(key.clone());
        let count = self.window.iter().filter(|k| **k == key).count();

        if self.abort_at > 0 && count == self.abort_at {
            return Verdict::Abort(format!(
                "`{name}` has been called {count} times with identical arguments within the \
                 last {} tool calls. The answer is not going to change, so the turn is being \
                 stopped rather than spending the rest of its budget on the same call.",
                self.window.len()
            ));
        }
        if self.warn_at > 0 && count == self.warn_at {
            return Verdict::Warn(format!(
                "[wingman] `{name}` has now been called {count} times with identical arguments \
                 in this turn, not always in a row. Repeating it is not making progress: use \
                 what the earlier results already told you, try a genuinely different approach, \
                 or stop and say what is blocking you. This turn will be terminated if the \
                 call repeats {} times.",
                self.abort_at
            ));
        }
        Verdict::Fine
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(p: &str) -> Value {
        json!({ "path": p })
    }

    #[test]
    fn identical_consecutive_calls_warn_then_abort() {
        let mut g = LoopGuard::new(24, 4, 8);
        for _ in 0..3 {
            assert_eq!(g.record("read_file", &args("a.rs")), Verdict::Fine);
        }
        assert!(matches!(
            g.record("read_file", &args("a.rs")),
            Verdict::Warn(_)
        ));
        for _ in 0..3 {
            assert_eq!(g.record("read_file", &args("a.rs")), Verdict::Fine);
        }
        assert!(matches!(
            g.record("read_file", &args("a.rs")),
            Verdict::Abort(_)
        ));
    }

    /// The hole in the consecutive guard: alternating calls never reset here.
    #[test]
    fn alternating_calls_still_trip_the_guard() {
        let mut g = LoopGuard::new(24, 4, 8);
        let mut verdicts = Vec::new();
        for _ in 0..8 {
            verdicts.push(g.record("grep", &args("needle")));
            verdicts.push(g.record("read_file", &args("b.rs")));
        }
        assert!(
            verdicts.iter().any(|v| matches!(v, Verdict::Abort(_))),
            "an A→B→A→B cycle must eventually abort"
        );
    }

    #[test]
    fn differing_arguments_are_different_calls() {
        let mut g = LoopGuard::new(24, 4, 8);
        for i in 0..20 {
            assert_eq!(
                g.record("read_file", &args(&format!("file{i}.rs"))),
                Verdict::Fine
            );
        }
    }

    /// Argument order must not launder a repeat, exactly as in the tools-layer
    /// guard — both now share `canonical_args` for precisely this reason.
    #[test]
    fn argument_order_does_not_launder_a_repeat() {
        let mut g = LoopGuard::new(24, 3, 0);
        assert_eq!(g.record("t", &json!({"a": 1, "b": 2})), Verdict::Fine);
        assert_eq!(g.record("t", &json!({"b": 2, "a": 1})), Verdict::Fine);
        assert!(matches!(
            g.record("t", &json!({"a": 1, "b": 2})),
            Verdict::Warn(_)
        ));
    }

    #[test]
    fn old_calls_fall_out_of_the_window() {
        let mut g = LoopGuard::new(4, 0, 3);
        assert_eq!(g.record("t", &args("x")), Verdict::Fine);
        for i in 0..4 {
            assert_eq!(g.record("t", &args(&format!("y{i}"))), Verdict::Fine);
        }
        // The first `x` has aged out, so this is occurrence 1 again, not 2.
        assert_eq!(g.record("t", &args("x")), Verdict::Fine);
    }

    /// A poller is the one shape where identical repetition is the design.
    #[test]
    fn an_exempt_tool_may_repeat_forever() {
        let mut g = LoopGuard::new(24, 2, 4).with_exempt(vec!["assign_task".into()]);
        for _ in 0..50 {
            assert_eq!(g.record("assign_task", &args("t1")), Verdict::Fine);
        }
    }

    #[test]
    fn exempting_one_tool_does_not_exempt_the_others() {
        let mut g = LoopGuard::new(24, 0, 3).with_exempt(vec!["assign_task".into()]);
        g.record("assign_task", &args("t1"));
        g.record("grep", &args("x"));
        g.record("grep", &args("x"));
        assert!(matches!(g.record("grep", &args("x")), Verdict::Abort(_)));
    }

    #[test]
    fn exempt_patterns_match_by_prefix() {
        let mut g = LoopGuard::new(24, 0, 2).with_exempt(vec!["mcp__poll_*".into()]);
        for _ in 0..10 {
            assert_eq!(g.record("mcp__poll_status", &args("x")), Verdict::Fine);
        }
    }

    /// An exempt call must not push the window along either, or interleaving
    /// bookkeeping into a real loop would age the loop out of it.
    #[test]
    fn exempt_calls_do_not_launder_a_real_loop() {
        let mut g = LoopGuard::new(4, 0, 3).with_exempt(vec!["update_tasks".into()]);
        g.record("grep", &args("x"));
        for _ in 0..6 {
            g.record("update_tasks", &args("bookkeeping"));
        }
        g.record("grep", &args("x"));
        assert!(matches!(g.record("grep", &args("x")), Verdict::Abort(_)));
    }

    #[test]
    fn disabled_guard_never_fires() {
        let mut g = LoopGuard::disabled();
        for _ in 0..100 {
            assert_eq!(g.record("t", &args("x")), Verdict::Fine);
        }
    }
}
