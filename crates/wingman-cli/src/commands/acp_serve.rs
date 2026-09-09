//! `wingman acp` — speak the Agent Client Protocol over stdio.
//!
//! ACP is the cross-editor protocol for coding agents: one implementation
//! reaches Zed, JetBrains IDEs, Neovim, and Emacs, instead of hand-building a
//! plugin per editor. That is why this exists rather than a richer bespoke
//! VS Code panel — the ecosystem standardised the surface, so the leverage is
//! in speaking it.
//!
//! Transport is newline-delimited JSON-RPC 2.0 on stdin/stdout, the same shape
//! as `mcp-serve`. The agent side implements:
//!
//!   - `initialize`         — capability handshake
//!   - `session/new`        — start a session rooted at a cwd
//!   - `session/prompt`     — run one turn, streaming `session/update`
//!   - `session/cancel`     — notification; abandon the in-flight turn
//!
//! and calls back into the client for:
//!
//!   - `session/request_permission` — let the editor approve or decline a
//!     single tool call, on top of Wingman's own permission mode
//!   - `fs/read_text_file`          — read through the editor, so the agent
//!     sees the buffer the user is looking at rather than a stale file
//!   - `fs/write_text_file`         — hand the editor the result of a write,
//!     so its buffer shows the edit instead of going stale against the disk
//!
//! Because those are *requests* rather than notifications, the read loop can't
//! block on a turn: [`run`] pumps stdin in its own task and routes replies back
//! to whoever is awaiting them by id (`wm-<n>`, so agent-issued ids can never
//! collide with the client's).
//!
//! **The client is an extra gate, not a replacement.** A decline is honoured;
//! an *approval* buys nothing that Wingman's own permission mode and protected
//! paths didn't already allow, so a hostile client cannot widen what the agent
//! may touch. Same for reads: the path is checked against the project
//! containment rules before the client is asked for it.
//!
//! **Writes reach the editor after the registry, not instead of it.** The
//! obvious reading of `fs/write_text_file` is to put the client between the
//! agent and the disk. That is the wrong shape here: the registry's dispatch
//! path is where `/undo` checkpoints and the audit log are written, so
//! diverting writes around it would silently cost both — an agent edit that
//! `/undo` cannot reverse and that the audit log never saw.
//!
//! So the registry still performs every write, exactly as it does outside ACP,
//! and the editor is handed the result afterwards. `/undo` and the audit log
//! are untouched because nothing moved; the editor's buffer stops going stale
//! because it is told what the file now says. A client that declines the
//! capability, answers an error, or is not there at all changes nothing about
//! the write that already happened — which is the same "extra gate, never a
//! replacement" rule the permission and read paths follow.

use anyhow::Result;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};
use wingman_config::{Config, PermissionMode};
use wingman_core::{AgentEvent, AgentStop, ToolDispatcher, ToolOutcome, ToolSpec};
use wingman_tools::{Capability, ToolRegistry};

/// Protocol version implemented. Echoed back to the client when it asks for
/// this one; otherwise we still answer with ours so the client can decide.
const PROTOCOL_VERSION: u64 = 1;

/// What the client told us it can do, from `initialize`. Everything defaults
/// off: an absent capability means "don't call me", not "try it and see".
#[derive(Debug, Default, Clone, Copy)]
struct ClientCaps {
    read_text_file: bool,
    write_text_file: bool,
}

impl ClientCaps {
    fn from_initialize(params: &Value) -> Self {
        let fs = params.pointer("/clientCapabilities/fs");
        let flag = |k: &str| {
            fs.and_then(|f| f.get(k))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            read_text_file: flag("readTextFile"),
            write_text_file: flag("writeTextFile"),
        }
    }
}

/// The client half of the connection: writes messages to it, and awaits the
/// replies to the requests we send.
struct Client {
    stdout: Arc<Mutex<tokio::io::Stdout>>,
    pending: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_id: AtomicU64,
    caps: std::sync::RwLock<ClientCaps>,
    /// Cleared the first time the client answers `session/request_permission`
    /// with "method not found", so we ask once and then stop pestering it.
    permission_supported: AtomicBool,
    /// Test-only record of the methods we asked the client to run, so a test
    /// can assert that the editor was — or pointedly was not — called back.
    /// Absent from release builds: in a real session this would grow for as
    /// long as the editor stays open.
    #[cfg(test)]
    sent: std::sync::Mutex<Vec<String>>,
}

impl Client {
    fn new(stdout: Arc<Mutex<tokio::io::Stdout>>) -> Self {
        Self {
            stdout,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            caps: std::sync::RwLock::new(ClientCaps::default()),
            permission_supported: AtomicBool::new(true),
            #[cfg(test)]
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn caps(&self) -> ClientCaps {
        *self.caps.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Test-only view of [`Client::sent`].
    #[cfg(test)]
    fn sent_methods(&self) -> Vec<String> {
        self.sent.lock().expect("sent lock").clone()
    }

    fn set_caps(&self, caps: ClientCaps) {
        *self.caps.write().unwrap_or_else(|e| e.into_inner()) = caps;
    }

    /// Send a JSON-RPC request and wait for the client's reply.
    async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, ClientError> {
        #[cfg(test)]
        self.sent
            .lock()
            .expect("sent lock")
            .push(method.to_string());
        let id = format!("wm-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if write_line(&self.stdout, &msg).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(ClientError::Transport("stdout closed".into()));
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ClientError::Rpc(e)),
            // The sender is dropped when stdin closes: the editor is gone.
            Err(_) => Err(ClientError::Transport("client disconnected".into())),
        }
    }

    /// Route an incoming reply to whoever is awaiting it. Returns false if the
    /// id isn't one of ours, so the caller can treat the message as a request.
    async fn resolve(&self, id: &str, result: std::result::Result<Value, String>) -> bool {
        match self.pending.lock().await.remove(id) {
            Some(tx) => {
                let _ = tx.send(result);
                true
            }
            None => false,
        }
    }
}

#[derive(Debug)]
enum ClientError {
    /// The client answered, with an error.
    Rpc(String),
    /// We never got an answer.
    Transport(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "{e}"),
            Self::Transport(e) => write!(f, "{e}"),
        }
    }
}

/// Did the client say it doesn't implement this method? JSON-RPC's
/// method-not-found is -32601; not every client spells the text the same way,
/// so match on the code where it's present and the phrase otherwise.
fn is_method_not_found(e: &ClientError) -> bool {
    match e {
        ClientError::Rpc(msg) => {
            msg.contains("-32601")
                || msg.to_ascii_lowercase().contains("method not found")
                || msg.to_ascii_lowercase().contains("not supported")
        }
        ClientError::Transport(_) => false,
    }
}

pub async fn run(cfg: Config, mode: PermissionMode) -> Result<ExitCode> {
    eprintln!(
        "wingman acp: speaking Agent Client Protocol v{PROTOCOL_VERSION} over stdio (mode: {mode})"
    );

    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let client = Arc::new(Client::new(stdout.clone()));

    // Pump stdin in its own task. The main loop must stay free to answer the
    // client while a turn is in flight, because the turn itself makes requests
    // (permission, file reads) that only the client can answer.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // One session at a time is enough for an editor pane, and keeps the
    // cancellation story simple: there is exactly one turn to abandon.
    let mut session_id: Option<String> = None;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (done_tx, mut done_rx) =
        mpsc::unbounded_channel::<(Value, std::result::Result<&'static str, String>)>();
    let mut turn_in_flight = false;

    loop {
        let line = tokio::select! {
            // Finished turn: answer the `session/prompt` that started it.
            Some((id, outcome)) = done_rx.recv() => {
                turn_in_flight = false;
                respond(&stdout, Some(id), outcome.map(|r| json!({ "stopReason": r }))).await?;
                continue;
            }
            line = line_rx.recv() => match line {
                Some(l) => l,
                None => break, // client closed the pipe
            },
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            // Malformed input is the client's problem, but dropping it
            // silently makes debugging an editor integration miserable.
            eprintln!("wingman acp: ignoring unparseable line");
            continue;
        };

        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // A message with an id but no method is a reply to something we sent.
        if method.is_empty() {
            if let Some(id_str) = id.as_ref().and_then(Value::as_str) {
                let result = match msg.get("error") {
                    Some(e) => Err(e.to_string()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                if client.resolve(id_str, result).await {
                    continue;
                }
            }
        }

        match method {
            "initialize" => {
                client.set_caps(ClientCaps::from_initialize(&params));
                let reply = json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentInfo": {
                        "name": "wingman",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "agentCapabilities": {
                        // No auth: Wingman uses the credentials already in the
                        // user's keyring/config, so there is nothing for the
                        // editor to log into.
                        "loadSession": false,
                        "promptCapabilities": { "image": false, "audio": false },
                    },
                    "authMethods": [],
                });
                respond(&stdout, id, Ok(reply)).await?;
            }

            "session/new" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(std::path::PathBuf::from);
                // Root the session where the editor says the workspace is, so
                // project containment and the semantic index line up with what
                // the user sees.
                if let Some(dir) = cwd.as_ref() {
                    if let Err(e) = std::env::set_current_dir(dir) {
                        respond(
                            &stdout,
                            id,
                            Err(format!("cannot enter {}: {e}", dir.display())),
                        )
                        .await?;
                        continue;
                    }
                }
                let sid = new_session_id();
                session_id = Some(sid.clone());
                respond(&stdout, id, Ok(json!({ "sessionId": sid }))).await?;
            }

            "session/prompt" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if session_id.as_deref() != Some(sid.as_str()) {
                    respond(&stdout, id, Err(format!("unknown session '{sid}'"))).await?;
                    continue;
                }
                if turn_in_flight {
                    respond(
                        &stdout,
                        id,
                        Err("a turn is already running in this session".into()),
                    )
                    .await?;
                    continue;
                }
                let text = prompt_text(&params);
                if text.trim().is_empty() {
                    respond(&stdout, id, Err("empty prompt".into())).await?;
                    continue;
                }

                cancelled.store(false, Ordering::SeqCst);
                turn_in_flight = true;
                let (cfg, stdout_t, cancelled_t, client_t, done_t) = (
                    cfg.clone(),
                    stdout.clone(),
                    cancelled.clone(),
                    client.clone(),
                    done_tx.clone(),
                );
                let reply_id = id.unwrap_or(Value::Null);
                tokio::spawn(async move {
                    let stop =
                        run_turn(&cfg, mode, &sid, &text, &stdout_t, &cancelled_t, &client_t).await;
                    let _ = done_t.send((reply_id, stop));
                });
            }

            // Notification: no id, no response.
            "session/cancel" => {
                cancelled.store(true, Ordering::SeqCst);
            }

            _ => {
                if id.is_some() {
                    respond(&stdout, id, Err(format!("unknown method '{method}'"))).await?;
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Run one turn, streaming `session/update` notifications as it goes.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    cfg: &Config,
    mode: PermissionMode,
    session_id: &str,
    prompt: &str,
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    cancelled: &Arc<AtomicBool>,
    client: &Arc<Client>,
) -> std::result::Result<&'static str, String> {
    let selection = crate::runtime::resolve_selection(cfg, None).map_err(|e| e.to_string())?;
    let (agent, registry) =
        crate::runtime::build_agent_registry_with_fallback(cfg, &selection, mode)
            .await
            .map_err(|e| e.to_string())?;

    // Put the editor between the agent and its tools: it may decline a call,
    // and it serves file reads from the buffer the user is actually looking at.
    let gate = Arc::new(EditorGate {
        inner: registry.clone(),
        client: client.clone(),
        session_id: session_id.to_string(),
    });
    let mut agent = agent.with_dispatcher(gate);

    let mut events = agent.run(prompt.to_string());
    let mut stop = "end_turn";

    while let Some(event) = events.next().await {
        if cancelled.load(Ordering::SeqCst) {
            return Ok("cancelled");
        }
        match &event {
            AgentEvent::TextDelta { text } => {
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text },
                    }),
                )
                .await;
            }
            AgentEvent::ToolStart { id, name, .. } => {
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": name,
                        "kind": "other",
                        "status": "in_progress",
                    }),
                )
                .await;
            }
            AgentEvent::ToolResult { id, is_error, .. } => {
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "status": if *is_error { "failed" } else { "completed" },
                    }),
                )
                .await;
            }
            AgentEvent::Verification { passed, summary } => {
                // Surface the gate as a message chunk: an editor pane should
                // show that the build/tests ran and what they said.
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": {
                            "type": "text",
                            "text": format!(
                                "\n[verify {}] {summary}\n",
                                if *passed { "passed" } else { "FAILED" }
                            ),
                        },
                    }),
                )
                .await;
            }
            AgentEvent::Stop { reason } => {
                stop = match reason {
                    AgentStop::EndTurn => "end_turn",
                    AgentStop::MaxTurns => "max_turn_requests",
                    AgentStop::MaxTokens => "max_tokens",
                    // ACP has no "the build is red" reason; `refusal` is the
                    // closest honest mapping — the agent declined to call it
                    // done rather than finishing cleanly.
                    AgentStop::GateFailed => "refusal",
                    // Same reading: the agent stopped itself rather than
                    // finishing, and ACP has no vocabulary for "it was going
                    // in circles".
                    AgentStop::LoopDetected => "refusal",
                    AgentStop::Error => "refusal",
                };
                break;
            }
            AgentEvent::Error { message } => {
                return Err(message.clone());
            }
            _ => {}
        }
    }
    Ok(stop)
}

/// Sits between the agent loop and the real tool registry for the life of one
/// ACP turn.
struct EditorGate {
    inner: Arc<ToolRegistry>,
    client: Arc<Client>,
    session_id: String,
}

/// What the client said about a tool call.
enum Permission {
    Allow,
    Reject,
    /// The client can't or won't mediate — Wingman's own permission model
    /// stands on its own, which is what happened before ACP asked at all.
    Unmediated,
}

impl EditorGate {
    /// Is this call worth interrupting a human for? Reads are not: an editor
    /// prompting on every `read_file` trains the user to click Allow, which is
    /// worse than not asking. Anything that writes, shells out, or reaches the
    /// network is.
    fn needs_permission(&self, name: &str) -> bool {
        match self.inner.capability_of(name) {
            Some(cap) => {
                cap.contains(Capability::WRITE)
                    || cap.contains(Capability::SHELL)
                    || cap.contains(Capability::NETWORK)
            }
            // Unknown tool: the registry will reject it anyway, so don't ask.
            None => false,
        }
    }

    async fn ask_permission(&self, name: &str, args: &Value) -> Permission {
        if !self.client.permission_supported.load(Ordering::Relaxed) {
            return Permission::Unmediated;
        }
        let params = json!({
            "sessionId": self.session_id,
            "toolCall": {
                "toolCallId": format!("{name}-{}", self.client.next_id.load(Ordering::Relaxed)),
                "title": name,
                "kind": "other",
                "status": "pending",
                "rawInput": args,
            },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        });
        match self
            .client
            .request("session/request_permission", params)
            .await
        {
            Ok(v) => decide(&v),
            Err(e) => {
                if is_method_not_found(&e) {
                    // Ask once, then stop: a client that doesn't implement this
                    // shouldn't pay a round-trip per tool call.
                    self.client
                        .permission_supported
                        .store(false, Ordering::Relaxed);
                } else {
                    tracing::warn!(
                        target: "wingman::acp",
                        tool = %name,
                        "permission request failed ({e}); falling back to Wingman's own permission mode"
                    );
                }
                Permission::Unmediated
            }
        }
    }

    /// Serve `read_file` out of the editor's buffer instead of the disk.
    ///
    /// `None` means "not applicable, use the real tool" — for a summary read
    /// (tree-sitter outline), a path the permission mode won't allow reading
    /// anyway, or a client that couldn't produce the content.
    async fn read_via_client(&self, args: &Value) -> Option<ToolOutcome> {
        if !self.client.caps().read_text_file {
            return None;
        }
        // `summary: true` returns a tree-sitter outline, and notebooks get
        // rendered — both are the local tool's job, not a raw buffer read.
        if args.get("summary").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = self.inner.ctx().resolve(raw);
        if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
            return None;
        }
        // Check containment *before* asking. The editor is an additional gate,
        // never a way around the project boundary.
        if !self.inner.ctx().allows_read(&path) {
            return None; // let the real tool produce its own denial message
        }
        let mut params = json!({
            "sessionId": self.session_id,
            "path": path.display().to_string(),
        });
        if let Some(line) = args.get("offset").and_then(Value::as_u64) {
            params["line"] = json!(line);
        }
        if let Some(limit) = args.get("limit").and_then(Value::as_u64) {
            params["limit"] = json!(limit);
        }
        match self.client.request("fs/read_text_file", params).await {
            Ok(v) => v
                .get("content")
                .and_then(Value::as_str)
                .map(|c| ToolOutcome::ok(c.to_string())),
            Err(e) => {
                tracing::debug!(
                    target: "wingman::acp",
                    "fs/read_text_file failed ({e}); reading from disk instead"
                );
                None
            }
        }
    }

    /// The single file a successful call to `name` just wrote, if there is one.
    ///
    /// Derived from the tool's declared capability rather than a hard-coded
    /// list, so a write tool added later is covered without editing this.
    /// Tools that write more than one file (`apply_patch`) have no single
    /// `path` argument and so are skipped — telling the editor about one of
    /// several changed files would be worse than telling it about none.
    fn written_path(&self, name: &str, args: &Value) -> Option<std::path::PathBuf> {
        if !self.inner.capability_of(name)?.contains(Capability::WRITE) {
            return None;
        }
        let raw = args.get("path").and_then(Value::as_str)?;
        Some(self.inner.ctx().resolve(raw))
    }

    /// Tell the editor what a file says now that the registry has written it.
    ///
    /// Best-effort by construction: the write is already on disk and already
    /// checkpointed, so every failure here is logged and swallowed. The worst
    /// case is the buffer the user sees being stale, which is exactly where
    /// this started — never a lost or half-applied edit.
    async fn push_write_to_client(&self, name: &str, args: &Value) {
        if !self.client.caps().write_text_file {
            return;
        }
        let Some(path) = self.written_path(name, args) else {
            return;
        };
        // Containment is re-checked on the way out for the same reason it is
        // checked on the way in: the client is an extra gate, and must not be
        // handed a path the agent was never allowed to touch.
        if !self.inner.ctx().allows_write(&path) {
            return;
        }
        // Read back what the registry actually wrote rather than reconstructing
        // it from the arguments — for `edit_file` and `edit_symbol` the final
        // text is the result of applying an edit, not anything in `args`.
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            // Deleted, replaced by a directory, or not UTF-8. Nothing useful to
            // hand a text-file API.
            Err(e) => {
                tracing::debug!(
                    target: "wingman::acp",
                    "not syncing {}: {e}", path.display()
                );
                return;
            }
        };
        let params = json!({
            "sessionId": self.session_id,
            "path": path.display().to_string(),
            "content": content,
        });
        if let Err(e) = self.client.request("fs/write_text_file", params).await {
            tracing::debug!(
                target: "wingman::acp",
                "fs/write_text_file failed ({e}); the edit is on disk regardless"
            );
        }
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for EditorGate {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }

    async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
        if self.needs_permission(name) {
            match self.ask_permission(name, &args).await {
                Permission::Reject => {
                    return ToolOutcome::err(format!("{name} was declined in the editor"));
                }
                Permission::Allow | Permission::Unmediated => {}
            }
        }
        if name == "read_file" {
            if let Some(outcome) = self.read_via_client(&args).await {
                return outcome;
            }
        }
        let outcome = self.inner.dispatch(name, args.clone()).await;
        // Only after the registry has succeeded: a failed write has nothing to
        // tell the editor about, and a rejected one must not look applied.
        if !outcome.is_error {
            self.push_write_to_client(name, &args).await;
        }
        outcome
    }
}

/// Read a `session/request_permission` result. Anything that isn't an explicit
/// selection of an allow-shaped option counts as a refusal — a cancelled
/// prompt must not run the tool.
fn decide(result: &Value) -> Permission {
    let outcome = result.get("outcome").unwrap_or(result);
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => match outcome.get("optionId").and_then(Value::as_str) {
            Some(id) if id.starts_with("allow") => Permission::Allow,
            _ => Permission::Reject,
        },
        _ => Permission::Reject,
    }
}

/// Flatten ACP's ContentBlock array into the plain text the agent loop takes.
/// Non-text blocks are skipped rather than rendered as placeholders: an editor
/// that sends an image to an agent advertising `image: false` is better served
/// by the text it did send than by a confusing stub.
fn prompt_text(params: &Value) -> String {
    let Some(blocks) = params.get("prompt").and_then(Value::as_array) else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(Value::as_str) {
            Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
            Some("resource") => b
                .get("resource")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn new_session_id() -> String {
    // pid + millis alone collides when two sessions start in the same
    // millisecond, which a client reconnecting immediately will do. The
    // counter makes it unique regardless of clock resolution.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "wingman-{}-{}-{n}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

/// Write a JSON-RPC response (or error) for `id`.
async fn respond(
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    id: Option<Value>,
    result: std::result::Result<Value, String>,
) -> Result<()> {
    let Some(id) = id else {
        return Ok(()); // notification: nothing to answer
    };
    let msg = match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": e },
        }),
    };
    write_line(stdout, &msg).await
}

/// Send a `session/update` notification.
async fn notify(stdout: &Arc<Mutex<tokio::io::Stdout>>, session_id: &str, update: Value) {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": update },
    });
    let _ = write_line(stdout, &msg).await;
}

async fn write_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, msg: &Value) -> Result<()> {
    let mut out = stdout.lock().await;
    out.write_all(msg.to_string().as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_flattens_text_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "fix the flaky test" },
                { "type": "text", "text": "in auth.rs" },
            ]
        });
        assert_eq!(prompt_text(&params), "fix the flaky test\nin auth.rs");
    }

    #[test]
    fn prompt_text_reads_resource_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "explain" },
                { "type": "resource", "resource": { "uri": "file:///a.rs", "text": "fn main() {}" } },
            ]
        });
        assert_eq!(prompt_text(&params), "explain\nfn main() {}");
    }

    /// An unknown block type must not become a placeholder in the prompt —
    /// the model would treat the stub as content.
    #[test]
    fn prompt_text_skips_unsupported_blocks() {
        let params = json!({
            "prompt": [
                { "type": "image", "data": "…" },
                { "type": "text", "text": "only this" },
            ]
        });
        assert_eq!(prompt_text(&params), "only this");
    }

    #[test]
    fn prompt_text_handles_missing_or_empty() {
        assert_eq!(prompt_text(&json!({})), "");
        assert_eq!(prompt_text(&json!({ "prompt": [] })), "");
    }

    #[test]
    fn session_ids_are_unique_per_call() {
        assert_ne!(new_session_id(), new_session_id());
    }

    #[test]
    fn client_caps_default_to_off() {
        // No `clientCapabilities` at all: we must not start calling `fs/*`.
        let caps = ClientCaps::from_initialize(&json!({}));
        assert!(!caps.read_text_file);
        let caps = ClientCaps::from_initialize(&json!({ "clientCapabilities": { "fs": {} } }));
        assert!(!caps.read_text_file);
        assert!(!caps.write_text_file);
    }

    #[test]
    fn client_caps_read_the_fs_flags() {
        let caps = ClientCaps::from_initialize(&json!({
            "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } }
        }));
        assert!(caps.read_text_file);
        assert!(caps.write_text_file);
    }

    #[test]
    fn only_an_allow_selection_permits_the_call() {
        assert!(matches!(
            decide(&json!({ "outcome": { "outcome": "selected", "optionId": "allow" } })),
            Permission::Allow
        ));
        assert!(matches!(
            decide(&json!({ "outcome": { "outcome": "selected", "optionId": "allow_always" } })),
            Permission::Allow
        ));
        assert!(matches!(
            decide(&json!({ "outcome": { "outcome": "selected", "optionId": "reject" } })),
            Permission::Reject
        ));
        // A cancelled prompt is a refusal, not a shrug.
        assert!(matches!(
            decide(&json!({ "outcome": { "outcome": "cancelled" } })),
            Permission::Reject
        ));
        // Garbage from a broken client must not read as approval.
        assert!(matches!(decide(&json!({})), Permission::Reject));
        assert!(matches!(
            decide(&json!({ "outcome": null })),
            Permission::Reject
        ));
    }

    #[test]
    fn method_not_found_is_recognised_however_it_is_spelled() {
        assert!(is_method_not_found(&ClientError::Rpc(
            "{\"code\":-32601,\"message\":\"x\"}".into()
        )));
        assert!(is_method_not_found(&ClientError::Rpc(
            "Method not found".into()
        )));
        assert!(!is_method_not_found(&ClientError::Rpc(
            "user is away".into()
        )));
        // A dead pipe is not the client declining to implement something.
        assert!(!is_method_not_found(&ClientError::Transport(
            "client disconnected".into()
        )));
    }

    /// Stand in for an editor: answer whatever request the gate sends with a
    /// canned reply. Returns once it has answered `n` requests.
    fn fake_client(client: Arc<Client>, replies: Vec<std::result::Result<Value, String>>) {
        tokio::spawn(async move {
            for (i, reply) in replies.into_iter().enumerate() {
                let id = format!("wm-{}", i + 1);
                loop {
                    if client.resolve(&id, reply.clone()).await {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
            }
        });
    }

    fn gate_over(dir: &std::path::Path, client: Arc<Client>) -> EditorGate {
        let ctx = wingman_tools::ToolCtx::new(
            wingman_config::PermissionMode::AutoEdit,
            dir.to_path_buf(),
            dir.to_path_buf(),
        );
        EditorGate {
            inner: Arc::new(ToolRegistry::new(ctx).with_builtins()),
            client,
            session_id: "s1".into(),
        }
    }

    /// The acceptance criterion for the permission half: a decline in the
    /// editor must stop the call, not merely annotate it.
    #[tokio::test]
    async fn a_declined_tool_call_does_not_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("should-not-exist.txt");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));
        fake_client(
            client.clone(),
            vec![Ok(
                json!({ "outcome": { "outcome": "selected", "optionId": "reject" } }),
            )],
        );

        let gate = gate_over(dir.path(), client);
        let out = gate
            .dispatch(
                "write_file",
                json!({ "path": "should-not-exist.txt", "content": "nope" }),
            )
            .await;

        assert!(out.is_error, "a declined call must report as an error");
        assert!(out.content.contains("declined"), "got: {}", out.content);
        assert!(!target.exists(), "the file was written despite the decline");
    }

    /// The acceptance criterion for the write half: after the agent edits a
    /// file, the editor is told what it now says, so its buffer cannot go
    /// stale against the disk.
    ///
    /// Two replies are queued because a write is gated: the first answers
    /// `session/request_permission`, the second `fs/write_text_file`.
    #[tokio::test]
    async fn a_write_is_pushed_to_the_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));
        client.set_caps(ClientCaps {
            read_text_file: false,
            write_text_file: true,
        });
        fake_client(
            client.clone(),
            vec![
                Ok(json!({ "outcome": { "outcome": "selected", "optionId": "allow" } })),
                Ok(json!({})),
            ],
        );

        let gate = gate_over(dir.path(), client.clone());
        let out = gate
            .dispatch(
                "write_file",
                json!({ "path": "note.txt", "content": "hello" }),
            )
            .await;

        assert!(!out.is_error, "got: {}", out.content);
        // The registry still did the writing — that is what keeps `/undo`
        // checkpoints and the audit log intact.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("written"),
            "hello"
        );
        // And the client was asked to take the same content.
        assert!(
            client
                .sent_methods()
                .contains(&"fs/write_text_file".to_string()),
            "sent: {:?}",
            client.sent_methods()
        );
    }

    /// A client that never claimed `writeTextFile` must not be called for it,
    /// and the write must still land.
    #[tokio::test]
    async fn without_the_capability_the_write_still_lands_unannounced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));
        // Only the permission reply is queued: nothing else should be asked.
        fake_client(
            client.clone(),
            vec![Ok(
                json!({ "outcome": { "outcome": "selected", "optionId": "allow" } }),
            )],
        );

        let gate = gate_over(dir.path(), client.clone());
        let out = gate
            .dispatch("write_file", json!({ "path": "n.txt", "content": "x" }))
            .await;

        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).expect("written"),
            "x"
        );
        assert!(
            !client
                .sent_methods()
                .contains(&"fs/write_text_file".to_string()),
            "sent: {:?}",
            client.sent_methods()
        );
    }

    /// A declined call wrote nothing, so there is nothing to announce. Telling
    /// the editor here would show an edit that does not exist.
    #[tokio::test]
    async fn a_declined_write_is_not_pushed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));
        client.set_caps(ClientCaps {
            read_text_file: false,
            write_text_file: true,
        });
        fake_client(
            client.clone(),
            vec![Ok(
                json!({ "outcome": { "outcome": "selected", "optionId": "reject" } }),
            )],
        );

        let gate = gate_over(dir.path(), client.clone());
        let out = gate
            .dispatch("write_file", json!({ "path": "no.txt", "content": "x" }))
            .await;

        assert!(out.is_error);
        assert!(!dir.path().join("no.txt").exists());
        assert!(
            !client
                .sent_methods()
                .contains(&"fs/write_text_file".to_string()),
            "sent: {:?}",
            client.sent_methods()
        );
    }

    /// `written_path` is derived from the tool's capability, not a list of
    /// names: a read tool is never announced, and a multi-file write has no
    /// single path to announce.
    #[tokio::test]
    async fn only_single_path_writes_are_announced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));
        let gate = gate_over(dir.path(), client);

        assert!(gate
            .written_path("write_file", &json!({ "path": "a.txt" }))
            .is_some());
        assert!(gate
            .written_path("read_file", &json!({ "path": "a.txt" }))
            .is_none());
        // apply_patch can touch many files and takes no `path`.
        assert!(gate
            .written_path("apply_patch", &json!({ "patch": "..." }))
            .is_none());
    }

    /// Reads are not worth a prompt — if `read_file` asked for permission, the
    /// fake client above would never answer and this would hang.
    #[tokio::test]
    async fn reads_are_not_gated_but_do_come_from_the_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "on disk").expect("write");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));
        client.set_caps(ClientCaps {
            read_text_file: true,
            write_text_file: false,
        });
        fake_client(
            client.clone(),
            vec![Ok(json!({ "content": "unsaved buffer" }))],
        );

        let gate = gate_over(dir.path(), client);
        let out = gate.dispatch("read_file", json!({ "path": "a.txt" })).await;
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(
            out.content, "unsaved buffer",
            "the editor's buffer should win over the file on disk"
        );
    }

    /// A client that says nothing about `fs` must not be called for reads —
    /// the disk is the fallback, and it has to still work.
    #[tokio::test]
    async fn without_the_fs_capability_reads_come_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "on disk").expect("write");
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));

        let gate = gate_over(dir.path(), client);
        let out = gate.dispatch("read_file", json!({ "path": "a.txt" })).await;
        assert_eq!(out.content, "on disk");
    }

    #[tokio::test]
    async fn replies_route_to_the_waiting_request() {
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let client = Arc::new(Client::new(stdout));

        // Nothing is waiting on this id yet.
        assert!(!client.resolve("wm-999", Ok(json!({}))).await);

        let c = client.clone();
        let waiter = tokio::spawn(async move { c.request("fs/read_text_file", json!({})).await });
        // Give the request a moment to register itself, then answer it.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            if client
                .resolve("wm-1", Ok(json!({ "content": "hello" })))
                .await
            {
                break;
            }
        }
        let got = waiter.await.expect("task").expect("reply");
        assert_eq!(got.get("content").and_then(Value::as_str), Some("hello"));
    }
}
