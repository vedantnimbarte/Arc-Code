//! Deferred tools end to end, against the real registry.
//!
//! The unit tests in `tool_search.rs` cover ranking. These cover the claims
//! that actually matter: that deferring shrinks the request, that a deferred
//! tool is still *reachable*, and — the one that would make this feature a
//! security bug rather than a saving — that reaching it through `tool_call`
//! is gated exactly as calling it directly would be.

use std::sync::Arc;

use serde_json::json;
use wingman_config::PermissionMode;
use wingman_core::ToolDispatcher;
use wingman_tools::builtin::{ToolCall, ToolSearch};
use wingman_tools::{ToolCtx, ToolRegistry};

/// A registry with the meta-tools wired the way `runtime.rs` wires them.
fn registry(mode: PermissionMode, root: &std::path::Path, defer: &[&str]) -> Arc<ToolRegistry> {
    let ctx = ToolCtx::new(mode, root.to_path_buf(), root.to_path_buf());
    let reg = Arc::new(
        ToolRegistry::new(ctx)
            .with_builtins()
            .with_deferred(defer.iter().map(|s| (*s).to_string()).collect()),
    );
    if reg.defers_anything() {
        let as_dispatcher: Arc<dyn ToolDispatcher> = reg.clone();
        let weak = Arc::downgrade(&as_dispatcher);
        reg.register_arc(Arc::new(ToolSearch::new(weak.clone())));
        reg.register_arc(Arc::new(ToolCall::new(weak)));
    }
    reg
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("wingman-tool-search-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn names(specs: &[wingman_core::ToolSpec]) -> Vec<String> {
    specs.iter().map(|s| s.name.clone()).collect()
}

/// The whole point: a deferred tool's schema is not in the request.
#[tokio::test]
async fn deferring_removes_the_schema_from_the_request() {
    let dir = tempdir("shrink");
    let reg = registry(PermissionMode::ReadOnly, &dir, &["lsp_*"]);

    let visible = names(&reg.specs());
    assert!(
        !visible.iter().any(|n| n.starts_with("lsp_")),
        "deferred tools must not be in the request: {visible:?}"
    );
    assert!(visible.contains(&"tool_search".to_string()));
    assert!(visible.contains(&"tool_call".to_string()));
    assert!(
        names(&reg.all_specs())
            .iter()
            .any(|n| n.starts_with("lsp_")),
        "deferring is not unregistering — the tool must still exist"
    );
}

/// Deferring nothing must not quietly cost two schemas.
#[tokio::test]
async fn no_defer_patterns_means_no_meta_tools() {
    let dir = tempdir("nodefer");
    let reg = registry(PermissionMode::ReadOnly, &dir, &[]);
    let visible = names(&reg.specs());
    assert!(!visible.contains(&"tool_search".to_string()));
    assert!(!visible.contains(&"tool_call".to_string()));
}

/// A pattern matching nothing registered is also nothing to find.
#[tokio::test]
async fn a_pattern_that_matches_nothing_registers_no_meta_tools() {
    let dir = tempdir("nomatch");
    let reg = registry(
        PermissionMode::ReadOnly,
        &dir,
        &["nothing_is_called_this_*"],
    );
    assert!(!names(&reg.specs()).contains(&"tool_search".to_string()));
}

/// `tool_search` must survive being asked for its own schema — it builds its
/// description from the deferred list, which is a call back into the registry.
/// Getting this wrong recursed forever and deadlocked the read lock.
#[tokio::test]
async fn building_the_search_schema_does_not_recurse() {
    let dir = tempdir("recurse");
    let reg = registry(PermissionMode::ReadOnly, &dir, &["lsp_*"]);

    let spec = reg
        .specs()
        .into_iter()
        .find(|s| s.name == "tool_search")
        .expect("tool_search registered");
    assert!(
        spec.description.contains("lsp_"),
        "the directory of deferred names should be in the description: {}",
        spec.description
    );
}

#[tokio::test]
async fn search_finds_a_deferred_tool_and_returns_its_schema() {
    let dir = tempdir("find");
    let reg = registry(PermissionMode::ReadOnly, &dir, &["lsp_*"]);

    let out = reg
        .dispatch("tool_search", json!({ "query": "hover type information" }))
        .await;
    assert!(!out.is_error, "{}", out.content);
    let v: serde_json::Value = serde_json::from_str(&out.content).expect("json results");
    let tools = v["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    assert!(
        tools.iter().all(|t| t["input_schema"].is_object()),
        "a match must carry the schema, or the model still cannot call it"
    );
}

/// The security claim, and the reason `tool_call` is not a back door: it
/// dispatches through the same gate, so read-only still refuses a write.
#[tokio::test]
async fn tool_call_cannot_write_in_read_only_mode() {
    let dir = tempdir("readonly");
    let target = dir.join("should-not-exist.txt");
    let reg = registry(PermissionMode::ReadOnly, &dir, &["write_file"]);

    let out = reg
        .dispatch(
            "tool_call",
            json!({
                "name": "write_file",
                "arguments": { "path": target.to_string_lossy(), "content": "nope" }
            }),
        )
        .await;

    assert!(out.is_error, "read-only must refuse: {}", out.content);
    assert!(
        !target.exists(),
        "the file must not exist — tool_call laundered a forbidden write"
    );
}

/// The same call, permitted, must actually go through — a gate that refuses
/// everything would pass the test above and be useless.
#[tokio::test]
async fn tool_call_reaches_a_deferred_tool_when_permitted() {
    let dir = tempdir("write");
    let target = dir.join("written.txt");
    let reg = registry(PermissionMode::AutoEdit, &dir, &["write_file"]);

    let out = reg
        .dispatch(
            "tool_call",
            json!({
                "name": "write_file",
                "arguments": { "path": target.to_string_lossy(), "content": "hello" }
            }),
        )
        .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&target).expect("file written"),
        "hello"
    );
}

#[tokio::test]
async fn tool_call_refuses_to_invoke_itself() {
    let dir = tempdir("selfcall");
    let reg = registry(PermissionMode::AutoEdit, &dir, &["lsp_*"]);

    for name in ["tool_call", "tool_search"] {
        let out = reg
            .dispatch("tool_call", json!({ "name": name, "arguments": {} }))
            .await;
        assert!(out.is_error, "{name} should be refused: {}", out.content);
    }
}

#[tokio::test]
async fn tool_call_on_an_unknown_name_says_so() {
    let dir = tempdir("unknown");
    let reg = registry(PermissionMode::AutoEdit, &dir, &["lsp_*"]);

    let out = reg
        .dispatch(
            "tool_call",
            json!({ "name": "no_such_tool", "arguments": {} }),
        )
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("no_such_tool"), "{}", out.content);
}

/// `disabled_tools` and `preset` unregister rather than hide, so a removed
/// tool is not something `tool_call` can reach back for.
#[tokio::test]
async fn tool_call_cannot_reach_a_removed_tool() {
    let dir = tempdir("removed");
    let ctx = ToolCtx::new(PermissionMode::AutoEdit, dir.clone(), dir.clone());
    let reg = Arc::new(
        ToolRegistry::new(ctx)
            // Removals before builtins, as `runtime.rs` arranges it: the
            // policy is enforced at registration, so a tool named here is
            // never registered rather than swept afterwards.
            .with_tool_removals(wingman_tools::ToolRemovals::new(
                None,
                vec!["write_file".to_string()],
            ))
            .with_builtins()
            .with_deferred(vec!["lsp_*".to_string()]),
    );
    let as_dispatcher: Arc<dyn ToolDispatcher> = reg.clone();
    let weak = Arc::downgrade(&as_dispatcher);
    reg.register_arc(Arc::new(ToolCall::new(weak)));

    let out = reg
        .dispatch(
            "tool_call",
            json!({
                "name": "write_file",
                "arguments": { "path": dir.join("x.txt").to_string_lossy(), "content": "no" }
            }),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(!dir.join("x.txt").exists());
}
