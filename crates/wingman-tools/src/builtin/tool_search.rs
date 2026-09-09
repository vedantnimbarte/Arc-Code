//! `tool_search` / `tool_call`: pay for a tool's schema when you use it, not
//! on every request of every session.
//!
//! # The problem this exists for
//!
//! `wingman context` prints the per-turn context tax, and the tool schemas are
//! most of it — the README's own example is 3653 of 4236 first-turn tokens
//! across 24 tools. That is billed on every request for the whole session,
//! whether or not a single one of those tools is called. Every MCP server
//! makes it worse: connect three and their tools land in the prompt of every
//! turn, forever, alongside the ones the session actually needs.
//!
//! `[tools].preset` already answers the static half of this — a session that
//! only reads code should not carry `write_file`. What it cannot answer is the
//! long tail: tools that are worth *having* but are used in maybe one turn out
//! of fifty. Paying their full schema on the other forty-nine is the waste.
//!
//! # How it works
//!
//! Tools matching `[tools].defer` are withheld from the request. In their
//! place the model gets two schemas and a bounded directory of the deferred
//! tool *names*, which is a few tokens each instead of a few hundred:
//!
//!   - `tool_search` — find deferred tools by keyword; returns their full
//!     schemas, but only for the handful that matched, and only in the turn
//!     that asked.
//!   - `tool_call` — invoke one by name.
//!
//! `tool_call` exists because a model cannot emit a `tool_use` block for a
//! tool that was not in the request: knowing the schema is not enough, the
//! provider validates against the list it was given. So discovery needs a
//! matching invocation path, and that is the whole of it.
//!
//! # Security
//!
//! `tool_call` is not a hole in the permission model, because it is not a
//! second dispatch path. Every call goes back through
//! [`ToolDispatcher::dispatch`] — the same choke point `run_plan` uses, and
//! for the same reason: the capability gate, pre/post hooks, checkpoints, the
//! audit trail, secret redaction, the repeat guard and the per-call deadline
//! all live there. A deferred `write_file` reached through `tool_call` is
//! gated exactly as a direct `write_file` would be.
//!
//! Two things it must not do, both enforced below: recurse into itself, and
//! reach a tool the session removed. The latter is structural — `preset` and
//! `disabled_tools` unregister at registration time, so a removed tool is not
//! in the registry for `tool_call` to find.

use std::sync::Weak;

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolDispatcher, ToolOutcome, ToolSpec};

/// Matches returned by one search. Enough to pick from; small enough that a
/// scattergun query cannot pull the whole catalogue into the turn.
const MAX_RESULTS: usize = 5;

/// Names listed in the `tool_search` description. Past this the directory is
/// itself a context cost, and a keyword search is the better affordance.
const MAX_DIRECTORY: usize = 60;

/// The two tools this module registers. Neither may be deferred or invoked
/// through `tool_call` — deferring the discovery mechanism would leave nothing
/// to discover it with.
pub const SEARCH_TOOL: &str = "tool_search";
pub const CALL_TOOL: &str = "tool_call";

/// Whether `name` is one of the meta-tools, which are never deferrable.
pub fn is_meta_tool(name: &str) -> bool {
    name == SEARCH_TOOL || name == CALL_TOOL
}

pub struct ToolSearch {
    /// Weak so the tool can live in the registry it reads without forming an
    /// `Arc` cycle that leaks it. Same reasoning as `run_plan`.
    dispatcher: Weak<dyn ToolDispatcher>,
}

impl ToolSearch {
    pub fn new(dispatcher: Weak<dyn ToolDispatcher>) -> Self {
        Self { dispatcher }
    }

    /// The deferred tools.
    ///
    /// Asks the registry directly rather than diffing `all_specs()` against
    /// `specs()`: this runs from inside `spec()`, and `specs()` would ask
    /// *this* tool for its schema again, forever.
    fn deferred(&self) -> Vec<ToolSpec> {
        self.dispatcher
            .upgrade()
            .map(|d| d.deferred_specs())
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
}

#[async_trait]
impl Tool for ToolSearch {
    fn spec(&self) -> ToolSpec {
        let deferred = self.deferred();
        // The directory is names only. It is what makes the deferred set
        // *discoverable* — a model that cannot see a capability exists will
        // not think to search for it — without paying for descriptions and
        // JSON schemas it will not use.
        let names: Vec<&str> = deferred
            .iter()
            .take(MAX_DIRECTORY)
            .map(|s| s.name.as_str())
            .collect();
        let directory = if names.is_empty() {
            "No tools are currently deferred.".to_string()
        } else {
            let more = deferred.len().saturating_sub(names.len());
            let tail = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            format!("Available: {}{tail}", names.join(", "))
        };
        ToolSpec {
            name: SEARCH_TOOL.into(),
            description: format!(
                "Look up tools that are available but whose full schemas are not loaded. \
                 Returns each match's exact name, description and input schema; call one \
                 with `{CALL_TOOL}`. Search before assuming a capability is missing.\n\n\
                 {directory}"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords describing the capability, e.g. \
                                        \"take a screenshot\" or \"create an issue\"."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn capabilities(&self) -> Capability {
        // Reading a list of names the registry already holds. The tools it
        // *describes* are gated when they are called, not when they are named.
        Capability::READ
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let Ok(SearchArgs { query }) = serde_json::from_value(args) else {
            return ToolOutcome::err("tool_search: expected { query: string }");
        };
        let matches = rank(&self.deferred(), &query, MAX_RESULTS);
        if matches.is_empty() {
            return ToolOutcome::ok(format!(
                "No deferred tool matches \"{query}\". The tools already in your list are \
                 all that is available for this."
            ));
        }
        // Full schemas, because the point is that the model can now write a
        // correct call — a description alone would just cost another round trip.
        let payload: Vec<Value> = matches
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "input_schema": s.input_schema,
                })
            })
            .collect();
        match serde_json::to_string_pretty(&json!({ "tools": payload })) {
            Ok(s) => ToolOutcome::ok(s),
            Err(e) => ToolOutcome::err(format!("tool_search: could not serialise results: {e}")),
        }
    }
}

pub struct ToolCall {
    dispatcher: Weak<dyn ToolDispatcher>,
}

impl ToolCall {
    pub fn new(dispatcher: Weak<dyn ToolDispatcher>) -> Self {
        Self { dispatcher }
    }
}

#[derive(Debug, Deserialize)]
struct CallArgs {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[async_trait]
impl Tool for ToolCall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CALL_TOOL.into(),
            description: format!(
                "Invoke a tool by name — for tools found with `{SEARCH_TOOL}` that are not in \
                 your tool list. Use the tool's own name and its exact input schema. Tools \
                 already in your list should be called directly instead."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": format!("Exact tool name, as returned by {SEARCH_TOOL}.")
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments matching that tool's input schema."
                    }
                },
                "required": ["name", "arguments"],
                "additionalProperties": false
            }),
        }
    }

    fn capabilities(&self) -> Capability {
        // The *wrapper* needs nothing. The wrapped call is gated on its own
        // capability inside `dispatch`, which is the only correct place for it:
        // declaring `Write` here would over-refuse reads, and declaring `Read`
        // would say something false about what this can set in motion.
        Capability::READ
    }

    /// The inner tool bounds itself through `dispatch`, which applies the
    /// per-call deadline. Wrapping that in a second one would cut a long
    /// `run_shell` short for no reason.
    fn owns_timeout(&self) -> bool {
        true
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let Ok(CallArgs { name, arguments }) = serde_json::from_value(args) else {
            return ToolOutcome::err("tool_call: expected { name: string, arguments: object }");
        };
        if is_meta_tool(&name) {
            return ToolOutcome::err(format!(
                "tool_call: refusing to invoke `{name}` through itself. Call it directly."
            ));
        }
        let Some(d) = self.dispatcher.upgrade() else {
            return ToolOutcome::err("tool_call: the tool registry is gone");
        };
        if !d.all_specs().iter().any(|s| s.name == name) {
            return ToolOutcome::err(format!(
                "tool_call: no tool named `{name}`. Use {SEARCH_TOOL} to find the exact name; \
                 a tool disabled for this session will not appear."
            ));
        }
        // Back through the front door: same gate, hooks, audit and redaction
        // as a direct call. See the module docs.
        d.dispatch(&name, arguments).await
    }
}

/// Score `specs` against `query` and return the best few.
///
/// Substring matching over name and description, weighted so a hit in the
/// name outranks one in the prose. Deliberately not the semantic index: this
/// runs inside a tool call on a list of tens of items, and loading an
/// embedding model to rank 40 short strings would cost more than the schemas
/// it is saving.
fn rank(specs: &[ToolSpec], query: &str, limit: usize) -> Vec<ToolSpec> {
    let q = query.to_lowercase();
    let terms: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .collect();
    if terms.is_empty() {
        // An empty query is "show me what is here" and gets the head of the
        // catalogue. A query that had words but no usable ones ("go to a") is
        // a bad query, and answering it with five arbitrary tools would be
        // worse than saying nothing matched — the model would take the list
        // as relevant.
        return if q.trim().is_empty() {
            specs.iter().take(limit).cloned().collect()
        } else {
            Vec::new()
        };
    }
    let mut scored: Vec<(usize, &ToolSpec)> = specs
        .iter()
        .map(|s| {
            let name = s.name.to_lowercase();
            let desc = s.description.to_lowercase();
            let score = terms
                .iter()
                .map(|t| {
                    if name.contains(t) {
                        3
                    } else if desc.contains(t) {
                        1
                    } else {
                        0
                    }
                })
                .sum();
            (score, s)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    // Stable tie-break by name so the same query twice gives the same answer.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, s)| s.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn catalogue() -> Vec<ToolSpec> {
        vec![
            spec(
                "mcp__playwright__browser_click",
                "Click an element on the page",
            ),
            spec("mcp__playwright__browser_navigate", "Navigate to a URL"),
            spec(
                "mcp__github__create_issue",
                "Create a GitHub issue in a repository",
            ),
            spec("mcp__github__list_prs", "List pull requests"),
            spec("read_file", "Read a file from disk"),
        ]
    }

    #[test]
    fn a_name_hit_outranks_a_description_hit() {
        let hits = rank(&catalogue(), "issue", 5);
        assert_eq!(hits[0].name, "mcp__github__create_issue");
    }

    #[test]
    fn matches_on_description_when_the_name_says_nothing() {
        let hits = rank(&catalogue(), "pull requests", 5);
        assert_eq!(hits[0].name, "mcp__github__list_prs");
    }

    #[test]
    fn a_query_matching_nothing_returns_nothing() {
        assert!(rank(&catalogue(), "quantum entanglement", 5).is_empty());
    }

    #[test]
    fn results_are_capped() {
        let hits = rank(&catalogue(), "a e i o u browser page url", 2);
        assert!(hits.len() <= 2);
    }

    /// Same query, same order — a model that searches twice should not get a
    /// different answer and re-plan around it.
    #[test]
    fn ranking_is_stable() {
        let a = rank(&catalogue(), "browser", 5);
        let b = rank(&catalogue(), "browser", 5);
        assert_eq!(
            a.iter().map(|s| &s.name).collect::<Vec<_>>(),
            b.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    /// Short tokens are dropped, so "to", "a" and "an" in a natural-language
    /// query do not match half the catalogue on the letter.
    #[test]
    fn noise_words_do_not_match_everything() {
        let hits = rank(&catalogue(), "go to a", 5);
        assert!(hits.is_empty(), "got {hits:?}");
    }

    #[test]
    fn an_empty_query_browses_the_catalogue() {
        let hits = rank(&catalogue(), "   ", 3);
        assert_eq!(
            hits.len(),
            3,
            "an empty query should list what is available"
        );
    }

    #[test]
    fn meta_tools_are_recognised() {
        assert!(is_meta_tool("tool_search"));
        assert!(is_meta_tool("tool_call"));
        assert!(!is_meta_tool("read_file"));
    }
}
