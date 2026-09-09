//! Config repair: turn "unknown field `foo`" into a fix.
//!
//! Every config struct in `wingman-config` carries `deny_unknown_fields`, so
//! one stale or mistyped key does not degrade gracefully — it fails the whole
//! load, for every command, with `toml parse error in <merged>: unknown field
//! "loop_abort"`. That message names no file, no line, and no correction, and
//! `<merged>` is not a path anyone can open.
//!
//! The contract this module implements is the one thing a pre-1.0 tool owes
//! its users: **a config change that invalidates existing config ships the
//! repair for it.** `wingman doctor` says what is wrong and where;
//! `wingman doctor --fix` backs the file up and rewrites it.
//!
//! The mechanism is deliberately parasitic on serde. A `deny_unknown_fields`
//! error already carries the offending key, a byte span pointing exactly at
//! it, and the list of keys that *would* have been accepted at that position.
//! That is everything a rename needs, and it stays correct for free as the
//! config grows — a hand-maintained table of renames would be one more thing
//! to forget to update.

use std::path::{Path, PathBuf};

use serde::Serialize;
use wingman_config::Config;

/// One thing `doctor` found wrong with a config file.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    /// A key that is almost certainly a misspelling of a real one.
    Rename {
        line: usize,
        from: String,
        to: String,
    },
    /// A key nothing matches. Either it was removed, or it is a typo too far
    /// from any real key to guess at safely.
    Unknown {
        line: usize,
        key: String,
        /// What was accepted at this position, for the error message.
        expected: Vec<String>,
    },
    /// The file is not valid TOML at all. No repair is possible.
    Malformed { line: usize, message: String },
}

impl Finding {
    /// Whether `--fix` can act on this without guessing.
    pub fn is_auto_fixable(&self) -> bool {
        matches!(self, Finding::Rename { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Finding::Rename { line, from, to } => {
                format!("line {line}: `{from}` is not a key — did you mean `{to}`?")
            }
            Finding::Unknown {
                line,
                key,
                expected,
            } => {
                let mut sample: Vec<&str> = expected.iter().take(6).map(String::as_str).collect();
                if expected.len() > sample.len() {
                    sample.push("…");
                }
                format!(
                    "line {line}: `{key}` is not a key here, and nothing close enough to \
                     rename it to. Accepted here: {}",
                    sample.join(", ")
                )
            }
            Finding::Malformed { line, message } => {
                format!("line {line}: not valid TOML — {message}")
            }
        }
    }
}

/// The result of analysing one config file.
#[derive(Debug, Clone, Serialize)]
pub struct FileReport {
    pub path: PathBuf,
    pub findings: Vec<Finding>,
    /// The repaired text, when every finding was auto-fixable. `None` means
    /// nothing to write — either the file was already clean or something in it
    /// needs a human.
    #[serde(skip)]
    pub repaired: Option<String>,
}

impl FileReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Analyse one config file, computing repairs without writing anything.
pub fn analyze(path: &Path) -> std::io::Result<FileReport> {
    let text = std::fs::read_to_string(path)?;
    Ok(analyze_text(path.to_path_buf(), &text))
}

/// The whole of the logic, split out so it is testable without a filesystem.
pub fn analyze_text(path: PathBuf, text: &str) -> FileReport {
    let mut current = text.to_string();
    let mut findings = Vec::new();
    let mut changed = false;

    // Serde reports one error per parse, so each pass fixes at most one key
    // and re-parses. Bounded because an unfixable finding stops the walk and a
    // fixable one strictly shrinks the problem — the cap is a backstop against
    // a rename that somehow does not, not an expected limit.
    for _ in 0..256 {
        let err = match toml::from_str::<Config>(&current) {
            Ok(_) => break,
            Err(e) => e,
        };
        let span = err.span();
        let line = span
            .as_ref()
            .map(|s| line_of(&current, s.start))
            .unwrap_or(0);

        let Some((field, expected)) = parse_unknown_field(err.message()) else {
            findings.push(Finding::Malformed {
                line,
                message: err.message().to_string(),
            });
            break;
        };

        // Only rewrite when the span really is the key: the splice is a byte
        // range edit, and acting on a span that pointed elsewhere would
        // corrupt the file we are supposed to be repairing.
        let usable_span = span.filter(|s| current.get(s.clone()) == Some(field.as_str()));

        match (nearest_key(&field, &expected), usable_span) {
            (Some(best), Some(s)) => {
                findings.push(Finding::Rename {
                    line,
                    from: field,
                    to: best.clone(),
                });
                current.replace_range(s, &best);
                changed = true;
            }
            _ => {
                findings.push(Finding::Unknown {
                    line,
                    key: field,
                    expected,
                });
                break;
            }
        }
    }

    let all_fixable = !findings.is_empty() && findings.iter().all(Finding::is_auto_fixable);
    FileReport {
        path,
        findings,
        repaired: (changed && all_fixable).then_some(current),
    }
}

/// Back up `path` next to itself, then write `text`.
///
/// The backup is the whole reason this is safe to run unattended: a repair
/// that guessed wrong is one `mv` away from being undone, and the timestamp
/// means a second run cannot clobber the first run's evidence.
pub fn write_repaired(path: &Path, text: &str) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_extension(format!("toml.bak-{stamp}"));
    std::fs::copy(path, &backup)?;
    std::fs::write(path, text)?;
    Ok(backup)
}

/// Pull the offending key and the accepted alternatives out of serde's
/// `deny_unknown_fields` message.
///
/// The shape is stable and machine-ish: "unknown field X, expected one of A,
/// B, C" — each name backticked — or, for a struct with a single field,
/// "expected A". Anything else is not an unknown-field error and is left for
/// the caller to report verbatim.
fn parse_unknown_field(msg: &str) -> Option<(String, Vec<String>)> {
    let rest = msg.strip_prefix("unknown field `")?;
    let (field, rest) = rest.split_once('`')?;
    let expected = rest
        .split_once("expected one of ")
        .map(|(_, list)| list)
        .or_else(|| rest.split_once("expected ").map(|(_, list)| list))
        .map(collect_backticked)
        .unwrap_or_default();
    Some((field.to_string(), expected))
}

fn collect_backticked(s: &str) -> Vec<String> {
    s.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The single closest candidate to `field`, or `None` when the guess would be
/// one.
///
/// Two ways to decline. A best match that is not clearly better than the
/// runner-up is a coin flip, and renaming a key to the wrong one of two
/// equally-plausible options is worse than saying so. And a distance beyond
/// roughly a third of the key's length is not a typo, it is a different word.
fn nearest_key(field: &str, candidates: &[String]) -> Option<String> {
    let limit = (field.len() / 3).max(2);
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .map(|c| (edit_distance(field, c), c))
        .filter(|(d, _)| *d <= limit)
        .collect();
    scored.sort_by_key(|(d, c)| (*d, c.len()));
    match scored.as_slice() {
        [] => None,
        [(_, best)] => Some((*best).clone()),
        [(d0, best), (d1, _), ..] if d0 < d1 => Some((*best).clone()),
        _ => None,
    }
}

/// Levenshtein distance, two rows rather than a full matrix.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 1-based line number of a byte offset.
///
/// Counting newlines rather than `lines()`: for an offset that sits just past
/// a newline — which is exactly where a key at the start of a line is —
/// `lines()` counts the lines *before* it and lands one short.
fn line_of(text: &str, byte: usize) -> usize {
    text.get(..byte)
        .map_or(0, |s| s.bytes().filter(|b| *b == b'\n').count() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(text: &str) -> FileReport {
        analyze_text(PathBuf::from("config.toml"), text)
    }

    #[test]
    fn a_clean_config_has_nothing_to_say() {
        let r = report("[tools]\nloop_abort_at = 8\n");
        assert!(r.is_clean());
        assert!(r.repaired.is_none());
    }

    #[test]
    fn a_near_miss_key_is_renamed() {
        let r = report("[tools]\nloop_abort = 3\n");
        assert_eq!(
            r.findings,
            vec![Finding::Rename {
                line: 2,
                from: "loop_abort".into(),
                to: "loop_abort_at".into(),
            }]
        );
        assert_eq!(
            r.repaired.as_deref(),
            Some("[tools]\nloop_abort_at = 3\n"),
            "the value, the layout, and everything else must survive"
        );
    }

    #[test]
    fn several_typos_are_all_repaired_in_one_pass() {
        let r = report("[tools]\nloop_abort = 3\nrun_plans = true\n");
        assert_eq!(r.findings.len(), 2);
        assert!(r.findings.iter().all(Finding::is_auto_fixable));
        let fixed = r.repaired.expect("both were fixable");
        assert!(fixed.contains("loop_abort_at = 3"));
        assert!(fixed.contains("run_plan = true"));
    }

    #[test]
    fn comments_and_formatting_are_preserved() {
        let text = "# my settings\n[tools]\n\n# how many\nloop_abort   =   3   # trailing\n";
        let fixed = report(text).repaired.expect("fixable");
        assert!(fixed.contains("# my settings"));
        assert!(fixed.contains("# how many"));
        assert!(fixed.contains("loop_abort_at   =   3   # trailing"));
    }

    #[test]
    fn a_key_with_no_plausible_match_is_reported_not_guessed() {
        let r = report("[tools]\ncompletely_invented_thing = 1\n");
        assert!(matches!(r.findings.as_slice(), [Finding::Unknown { .. }]));
        assert!(
            r.repaired.is_none(),
            "an unfixable finding must not produce a write"
        );
    }

    /// The write path is gated on *every* finding being fixable, so one
    /// unknown key suppresses the rewrite of the typo above it. Better to fix
    /// nothing and explain than to half-fix a file and still fail to load.
    #[test]
    fn one_unfixable_finding_suppresses_the_whole_rewrite() {
        let r = report("[tools]\nloop_abort = 3\ncompletely_invented_thing = 1\n");
        assert!(r.findings.len() >= 2);
        assert!(r.repaired.is_none());
    }

    #[test]
    fn broken_toml_is_reported_as_such() {
        let r = report("[tools\nloop_abort_at = ");
        assert!(matches!(r.findings.as_slice(), [Finding::Malformed { .. }]));
        assert!(r.repaired.is_none());
    }

    #[test]
    fn the_reported_line_points_at_the_offending_key() {
        let r = report("[tools]\n\n\n\nloop_abort = 3\n");
        assert!(
            matches!(r.findings.as_slice(), [Finding::Rename { line: 5, .. }]),
            "got {:?}",
            r.findings
        );
    }

    #[test]
    fn serde_message_parsing_handles_both_shapes() {
        let (f, e) = parse_unknown_field("unknown field `x`, expected one of `a`, `b`").unwrap();
        assert_eq!(f, "x");
        assert_eq!(e, vec!["a", "b"]);

        let (f, e) = parse_unknown_field("unknown field `x`, expected `only`").unwrap();
        assert_eq!(f, "x");
        assert_eq!(e, vec!["only"]);

        assert!(parse_unknown_field("expected a table").is_none());
    }

    #[test]
    fn an_ambiguous_guess_is_declined() {
        // Equidistant from both: renaming to either would be a coin flip.
        let candidates = vec!["aaa".to_string(), "bbb".to_string()];
        assert_eq!(nearest_key("xxx", &candidates), None);
    }

    #[test]
    fn a_distant_key_is_not_a_typo() {
        let candidates = vec!["shell_sandbox".to_string()];
        assert_eq!(nearest_key("completely_different", &candidates), None);
    }

    #[test]
    fn edit_distance_is_symmetric_and_zero_on_equality() {
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("loop_abort", "loop_abort_at"), 3);
        assert_eq!(edit_distance("abc", "xyz"), edit_distance("xyz", "abc"));
    }
}
