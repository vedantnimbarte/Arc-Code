//! Request dispatch: authenticate, route, let the handler write its own
//! response.
//!
//! Handlers take the socket rather than returning a response value, which is
//! what lets a streaming route and a JSON route share one signature — an SSE
//! handler simply keeps writing after the headers go out.

use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpStream;

use super::http::{self, Request};
use super::pairing;
use super::projects::Project;
use super::{
    admin, auth, board, notifications, pilot, projects, push, sessions, table, timeline, ui,
    ServeState,
};

/// Handle one connection start to finish.
pub async fn handle(state: Arc<ServeState>, mut sock: TcpStream) -> std::io::Result<()> {
    let Some(req) = http::read_request(&mut sock).await? else {
        return Ok(()); // unparseable or hung up; nothing useful to say back
    };

    // One line per request at debug. Never the headers: the token lives
    // there, and a log that leaks the credential is worse than no log.
    tracing::debug!(target: "serve", "{} {}", req.method, req.path);

    // Health is unauthenticated on purpose: a load balancer, a phone
    // shortcut, or a `curl` sanity check should be able to ask "is it up"
    // without holding the token. It reports nothing but liveness.
    if req.segments().as_slice() == ["v1", "health"] {
        return health(&state, &mut sock).await;
    }

    // The web panel's static shell, also unauthenticated — a browser must load
    // the page before it can present a credential. It is three embedded files
    // with no project data in them; everything the panel *shows* comes from
    // `/v1`, which is gated below exactly as before. See `serve::ui`.
    if ui::is_shell(&req.method, &req.segments()) {
        return ui::serve(&req, &mut sock).await;
    }

    // Sign-in and sign-out sit ahead of the gate for the same chicken-and-egg
    // reason: sign-in *is* the authentication (it checks the token itself
    // before setting anything), and sign-out must work for a browser holding a
    // cookie the server has stopped accepting.
    match (req.method.as_str(), req.segments().as_slice()) {
        // Redeeming a pairing code *is* the authentication, so it sits ahead
        // of the gate for the same reason sign-in does. It is the only
        // unauthenticated route that can return a credential; see
        // `serve::pairing` for why that is safe.
        ("POST", ["v1", "pair", "redeem"]) => {
            return pairing::redeem(&state, &req, &mut sock).await
        }
        ("POST", ["v1", "ui", "session"]) => return ui::sign_in(&state, &req, &mut sock).await,
        ("DELETE", ["v1", "ui", "session"]) => return ui::sign_out(&mut sock).await,
        _ => {}
    }

    if !auth::authorized(state.token.as_deref(), auth::presented(|n| req.header(n))) {
        return http::write_err(&mut sock, 401, "unauthorized").await;
    }

    dispatch(&state, &req, &mut sock).await
}

async fn dispatch(
    state: &Arc<ServeState>,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    match (req.method.as_str(), req.segments().as_slice()) {
        ("GET", ["v1", "projects"]) => {
            let list: Vec<_> = state.projects.iter().map(projects::describe).collect();
            http::write_json(sock, 200, &json!({ "projects": list })).await
        }
        ("GET", ["v1", "schema"]) => http::write_json(sock, 200, &schema(state)).await,
        ("GET", ["v1", "config"]) => admin::get_config(state, sock).await,
        ("GET", ["v1", "config", "schema"]) => admin::get_config_schema(sock).await,
        ("GET", ["v1", "events"]) => events(state, sock).await,
        // Global rather than project-scoped: the inbox lives in the global
        // dir because the processes that write to it span projects.
        ("GET", ["v1", "notifications"]) => notifications::list(sock).await,
        ("POST", ["v1", "notifications", id, "reply"]) => notifications::reply(id, req, sock).await,
        ("PATCH", ["v1", "config"]) => admin::patch_config(req, sock).await,

        // The board is global — one store spanning every project — so it sits
        // beside `/v1/config` rather than under `/v1/projects/{p}`.
        (_, ["v1", "board", rest @ ..]) => board::route(state, req, rest, sock).await,

        // Everything below operates on one repo. Resolve it once here so no
        // handler can forget the allowlist check.
        (_, ["v1", "projects", p, rest @ ..]) => match projects::find(&state.projects, p) {
            Some(project) => project_route(state, req, project, rest, sock).await,
            None => http::write_err(sock, 404, "unknown project").await,
        },

        _ => http::write_err(sock, 404, "no such route (see GET /v1/schema)").await,
    }
}

/// Routes scoped to a resolved project.
async fn project_route(
    state: &Arc<ServeState>,
    req: &Request,
    project: &Project,
    rest: &[&str],
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    match (req.method.as_str(), rest) {
        ("GET", ["pilot", "runs"]) => pilot::list_runs(project, sock).await,
        ("POST", ["pilot", "runs"]) => pilot::start_run(state, project, req, sock).await,
        ("GET", ["pilot", "runs", run]) => pilot::get_run(project, run, sock).await,
        ("GET", ["pilot", "runs", run, "events"]) => {
            pilot::get_events(project, run, req, sock).await
        }
        ("GET", ["pilot", "runs", run, "stream"]) => pilot::stream(project, run, req, sock).await,
        ("GET", ["pilot", "runs", run, "log"]) => pilot::get_log(project, run, req, sock).await,
        ("GET", ["pilot", "runs", run, "dashboard"]) => {
            pilot::get_dashboard(project, run, req, sock).await
        }
        ("POST", ["pilot", "runs", run, action]) => {
            pilot::control(project, run, action, req, sock).await
        }
        ("POST", ["pilot", "goals"]) => pilot::add_goal(state, project, req, sock).await,

        ("POST", ["sessions"]) => sessions::create(sock).await,
        ("GET", ["sessions"]) => sessions::list(project, sock).await,
        ("GET", ["sessions", id]) => sessions::get(project, id, sock).await,
        ("DELETE", ["sessions", id]) => sessions::delete(project, id, sock).await,
        ("POST", ["sessions", id, "turns"]) => {
            sessions::turn(state, project, Some(id), req, sock).await
        }
        ("POST", ["turns"]) => sessions::turn(state, project, None, req, sock).await,

        // Ahead of the table, which owns `cost` itself: a native handler is
        // the only way to answer "per day", since the CLI's own cost report
        // reads an untimestamped machine-wide total.
        ("GET", ["cost", "timeline"]) => timeline::get(project, req, sock).await,

        ("POST", ["exec"]) => admin::exec(state, project, req, sock).await,

        // The long tail: everything else the CLI can do, as table data.
        _ => match table::find(&req.method, rest) {
            Some(route) => table::run(state, project, route, req, sock).await,
            None => http::write_err(sock, 404, "no such route (see GET /v1/schema)").await,
        },
    }
}

async fn health(state: &Arc<ServeState>, sock: &mut TcpStream) -> std::io::Result<()> {
    http::write_json(
        sock,
        200,
        &json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": state.started.elapsed().as_secs(),
            // Whether a credential is needed at all, so the panel can skip its
            // sign-in screen on a loopback server with no token rather than
            // demanding a secret that does not exist. This leaks nothing: an
            // unauthenticated caller learns it is unauthenticated, which the
            // next 401 would have told it anyway.
            "auth_required": state.token.is_some(),
        }),
    )
    .await
}

/// `GET /v1/events` — SSE firehose of run transitions across every project.
///
/// Same detector outbound push uses, so what a client sees on the stream and
/// what lands in a webhook cannot disagree.
async fn events(state: &Arc<ServeState>, sock: &mut TcpStream) -> std::io::Result<()> {
    let mut seen = push::Seen::default();
    // Prime quietly: a client connecting now wants what happens next, not a
    // replay of every run on disk.
    let _ = push::poll(&state.projects, &mut seen, false);

    let mut sse = http::Sse::start(sock).await?;
    let mut last_write = std::time::Instant::now();
    loop {
        for t in push::poll(&state.projects, &mut seen, true) {
            sse.send(t.event, &t.to_json()).await?;
            last_write = std::time::Instant::now();
        }
        if last_write.elapsed() >= http::SSE_KEEPALIVE {
            sse.keepalive().await?;
            last_write = std::time::Instant::now();
        }
        tokio::time::sleep(push::POLL).await;
    }
}

/// The machine-readable route list. Generated from what dispatch actually
/// serves, so a client can discover the surface instead of pinning to a doc
/// that drifts.
fn schema(state: &Arc<ServeState>) -> serde_json::Value {
    let mut doc = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "ceiling": state.ceiling.to_string(),
        "routes": [
            { "method": "GET", "path": "/v1/health", "auth": false,
              "returns": "liveness, version, uptime, whether a credential is required" },
            { "method": "POST", "path": "/v1/ui/session", "auth": false,
              "body": { "token": "string" },
              "returns": "sets the web panel's HttpOnly cookie; 401 on a wrong token" },
            { "method": "DELETE", "path": "/v1/ui/session", "auth": false,
              "returns": "clears the panel cookie; never gated, so a stale cookie can always be dropped" },
            { "method": "GET", "path": "/v1/schema", "auth": true,
              "returns": "this document" },
            { "method": "GET", "path": "/v1/projects", "auth": true,
              "returns": "allowlisted projects with branch and index state" },
            { "method": "GET", "path": "/v1/events", "auth": true,
              "returns": "text/event-stream of run transitions across every project" },
            { "method": "GET", "path": "/v1/notifications", "auth": true,
              "returns": "open actionable cards — the inbox the desktop popup reads" },
            { "method": "POST", "path": "/v1/notifications/{id}/reply", "auth": true,
              "body": { "action": "string?", "text": "string?" },
              "returns": "{answered, via} — via is \"control\" when the button carries a run command, else \"reply\"; both fields absent dismisses" },

            { "method": "GET", "path": "/v1/projects/{project}/pilot/runs", "auth": true,
              "returns": "run summaries, most recent first" },
            { "method": "POST", "path": "/v1/projects/{project}/pilot/runs", "auth": true,
              "body": { "goal": "string", "yes": "bool", "plan_only": "bool",
                        "model": "string?", "tier": "string?", "max_usd": "number?" },
              "returns": "{run_id} — started detached" },
            { "method": "GET", "path": "/v1/projects/{project}/pilot/runs/{run}", "auth": true,
              "returns": "full RunState snapshot" },
            { "method": "GET", "path": "/v1/projects/{project}/pilot/runs/{run}/events", "auth": true,
              "params": { "tail": "how many recent events (default 50, max 1000)" } },
            { "method": "GET", "path": "/v1/projects/{project}/pilot/runs/{run}/stream", "auth": true,
              "params": { "tail": "replay depth before live events (default 20)" },
              "returns": "text/event-stream; closes with an 'end' event" },
            { "method": "GET", "path": "/v1/projects/{project}/pilot/runs/{run}/log", "auth": true,
              "params": { "tail": "how many trailing lines (default 500, max 5000)" },
              "returns": "the orchestrator's own stdout with {text, total_lines, shown_lines}" },
            { "method": "GET", "path": "/v1/projects/{project}/pilot/runs/{run}/dashboard", "auth": true,
              "params": { "width": "columns (default 100)" },
              "returns": "text/plain ASCII dashboard" },
            { "method": "POST", "path": "/v1/projects/{project}/pilot/runs/{run}/approve", "auth": true },
            { "method": "POST", "path": "/v1/projects/{project}/pilot/runs/{run}/veto", "auth": true },
            { "method": "POST", "path": "/v1/projects/{project}/pilot/runs/{run}/abort", "auth": true,
              "body": { "task": "string? — omit to abort the whole run" } },
            { "method": "POST", "path": "/v1/projects/{project}/pilot/runs/{run}/retry", "auth": true,
              "body": { "task": "string — required" } },
            { "method": "POST", "path": "/v1/projects/{project}/pilot/goals", "auth": true,
              "body": { "text": "string", "author": "string?" },
              "returns": "queues an intake file for the discovery daemon" },
        ],
    });
    if let Some(routes) = doc["routes"].as_array_mut() {
        routes.extend(board::schema());
        routes.extend(sessions::schema());
        routes.extend(timeline::schema());
        routes.extend(table::schema());
        routes.extend(admin::schema());
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;
    use wingman_config::{Config, PermissionMode};

    /// Bind an ephemeral port, serve exactly one request, and return the raw
    /// response. Exercises the real accept → parse → auth → dispatch path
    /// rather than calling the handler directly, so a mistake in the wiring
    /// shows up here instead of at runtime.
    async fn round_trip(token: Option<&str>, request: &str) -> String {
        round_trip_for(Vec::new(), token, request).await
    }

    /// As above, with an allowlist so project-scoped routes resolve.
    async fn round_trip_for(projects: Vec<Project>, token: Option<&str>, request: &str) -> String {
        let state = Arc::new(ServeState {
            cfg: Config::default(),
            projects,
            token: token.map(str::to_string),
            ceiling: PermissionMode::AutoEdit,
            started: Instant::now(),
            pairing: std::sync::Mutex::new(None),
            turns: Semaphore::new(1),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle(state, sock).await.unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        // A reset *after* a complete response is a normal way for a
        // `Connection: close` exchange to end on Windows, so it is only a
        // failure when nothing arrived.
        if let Err(e) = client.read_to_end(&mut buf).await {
            assert!(!buf.is_empty(), "read failed with no response at all: {e}");
        }
        server.await.unwrap();
        String::from_utf8_lossy(&buf).to_string()
    }

    #[tokio::test]
    async fn health_needs_no_token() {
        let resp = round_trip(
            Some("sekrit"),
            "GET /v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"ok\":true"), "{resp}");
    }

    /// The panel's whole sign-in loop, through the real accept → parse → auth
    /// path: a good token comes back as a cookie, and that cookie then opens a
    /// route that rejected the same request without it.
    #[tokio::test]
    async fn signing_in_returns_a_cookie_that_authenticates() {
        let body = r#"{"token":"sekrit"}"#;
        let resp = round_trip(
            Some("sekrit"),
            &format!(
                "POST /v1/ui/session HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("wingman_token=sekrit"), "{resp}");
        assert!(resp.contains("HttpOnly"), "{resp}");
        assert!(resp.contains("SameSite=Strict"), "{resp}");

        let with_cookie = round_trip(
            Some("sekrit"),
            "GET /v1/projects HTTP/1.1\r\nHost: x\r\nCookie: wingman_token=sekrit\r\n\
             Content-Length: 0\r\n\r\n",
        )
        .await;
        assert!(with_cookie.starts_with("HTTP/1.1 200"), "{with_cookie}");
    }

    #[tokio::test]
    async fn signing_in_with_the_wrong_token_sets_no_cookie() {
        let body = r#"{"token":"guess"}"#;
        let resp = round_trip(
            Some("sekrit"),
            &format!(
                "POST /v1/ui/session HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        assert!(!resp.contains("Set-Cookie"), "{resp}");
        // The reply must not confirm any part of the guess.
        assert!(!resp.contains("sekrit"), "{resp}");
    }

    /// Signing out has to work for a browser whose cookie the server has
    /// stopped accepting, so it is never gated.
    #[tokio::test]
    async fn signing_out_clears_the_cookie_without_a_valid_one() {
        let resp = round_trip(
            Some("sekrit"),
            "DELETE /v1/ui/session HTTP/1.1\r\nHost: x\r\nCookie: wingman_token=stale\r\n\
             Content-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert!(resp.contains("Max-Age=0"), "{resp}");
    }

    #[tokio::test]
    async fn health_reports_whether_a_credential_is_needed() {
        let with = round_trip(
            Some("sekrit"),
            "GET /v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(with.contains(r#""auth_required":true"#), "{with}");

        let without = round_trip(
            None,
            "GET /v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(without.contains(r#""auth_required":false"#), "{without}");
    }

    #[tokio::test]
    async fn other_routes_reject_a_missing_token() {
        let resp = round_trip(
            Some("sekrit"),
            "GET /v1/projects HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        // The body says nothing about why, so it cannot confirm a guess.
        assert!(resp.contains("unauthorized"), "{resp}");
        assert!(!resp.contains("sekrit"), "{resp}");
    }

    #[tokio::test]
    async fn a_good_token_reaches_the_route() {
        let resp = round_trip(
            Some("sekrit"),
            "GET /v1/projects HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sekrit\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"projects\""), "{resp}");
    }

    /// Seed a project containing one pilot run in `status`, with one task.
    /// Returns the temp dir (keep it alive), the allowlist, and the run dir.
    fn seed_run(
        status: &str,
        task_status: &str,
    ) -> (tempfile::TempDir, Vec<Project>, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let run_id = "2026-08-18-1042-abc123";
        let run_dir = root.join(".wingman").join("autonomous").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let state = json!({
            "run_id": run_id,
            "goal": "add SSE keepalives",
            "base_commit": "deadbeef",
            "integration_branch": "wingman/auto/x",
            "status": status,
            "tasks": [{
                "id": "t1",
                "role": "developer",
                "title": "write the keepalive",
                "status": task_status,
            }],
        });
        std::fs::write(
            run_dir.join("state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join("tasks.jsonl"),
            "{\"ev\":\"run.start\",\"t\":\"2026-08-18T10:42:00Z\",\"run_id\":\"2026-08-18-1042-abc123\",\"goal\":\"add SSE keepalives\",\"base_commit\":\"deadbeef\",\"integration_branch\":\"wingman/auto/x\"}\n",
        )
        .unwrap();
        let projects = vec![Project {
            id: "repo".into(),
            root,
        }];
        (dir, projects, run_dir)
    }

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n")
    }

    fn del(path: &str) -> String {
        format!("DELETE {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n")
    }

    fn post(path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn lists_runs_for_a_known_project() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(projects, None, &get("/v1/projects/repo/pilot/runs")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("2026-08-18-1042-abc123"), "{resp}");
        assert!(resp.contains("add SSE keepalives"), "{resp}");
    }

    #[tokio::test]
    async fn an_unlisted_project_is_404_even_with_a_valid_token() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(projects, None, &get("/v1/projects/other/pilot/runs")).await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        assert!(resp.contains("unknown project"), "{resp}");
    }

    #[tokio::test]
    async fn a_traversing_run_id_is_rejected_before_touching_disk() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp =
            round_trip_for(projects, None, &get("/v1/projects/repo/pilot/runs/..%2F..")).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    }

    #[tokio::test]
    async fn approving_a_run_that_is_not_gated_is_409() {
        let (_tmp, projects, run_dir) = seed_run("running", "in_progress");
        let resp = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/approve",
                "",
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 409"), "{resp}");
        // Nothing was written: a rejected command must not reach the run.
        assert!(!run_dir.join("control.jsonl").exists());
    }

    #[tokio::test]
    async fn approving_a_gated_run_writes_the_control_command() {
        let (_tmp, projects, run_dir) = seed_run("awaiting_approval", "pending");
        let resp = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/approve",
                "",
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 202"), "{resp}");
        let control = std::fs::read_to_string(run_dir.join("control.jsonl")).unwrap();
        assert_eq!(control.trim(), "{\"cmd\":\"approve\"}");
    }

    #[tokio::test]
    async fn retry_rejects_unknown_and_non_failed_tasks() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let unknown = round_trip_for(
            projects.clone(),
            None,
            &post(
                "/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/retry",
                "{\"task\":\"nope\"}",
            ),
        )
        .await;
        assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");

        let running = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/retry",
                "{\"task\":\"t1\"}",
            ),
        )
        .await;
        assert!(running.starts_with("HTTP/1.1 409"), "{running}");
    }

    #[tokio::test]
    async fn retry_accepts_a_failed_task() {
        let (_tmp, projects, run_dir) = seed_run("running", "failed");
        let resp = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/retry",
                "{\"task\":\"t1\"}",
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 202"), "{resp}");
        let control = std::fs::read_to_string(run_dir.join("control.jsonl")).unwrap();
        assert!(control.contains("retry_task"), "{control}");
        assert!(control.contains("t1"), "{control}");
    }

    #[tokio::test]
    async fn aborting_a_finished_run_is_409() {
        let (_tmp, projects, _) = seed_run("done", "done");
        let resp = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/abort",
                "",
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 409"), "{resp}");
    }

    /// The orchestrator's stdout is the only place a failed run explains
    /// itself, and it is returned with both counts so a truncated log cannot
    /// be presented as a whole one.
    #[tokio::test]
    async fn the_run_log_returns_its_tail_and_says_how_much_it_kept() {
        let (_tmp, projects, run_dir) = seed_run("failed", "failed");
        std::fs::write(
            run_dir.join("pilot.log"),
            "[pilot] planning\n[pilot] driving manager loop\nwingman: exceeded max_ticks\n",
        )
        .unwrap();

        let all = round_trip_for(
            projects.clone(),
            None,
            &get("/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/log"),
        )
        .await;
        assert!(all.starts_with("HTTP/1.1 200"), "{all}");
        assert!(all.contains(r#""total_lines":3"#), "{all}");
        assert!(all.contains(r#""shown_lines":3"#), "{all}");
        assert!(all.contains("exceeded max_ticks"), "{all}");

        let tail = round_trip_for(
            projects,
            None,
            &get("/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/log?tail=1"),
        )
        .await;
        assert!(tail.contains(r#""shown_lines":1"#), "{tail}");
        // The tail is the *end* of the log: the last line is what a run died
        // of, and a tail that returned the first line would be useless.
        assert!(tail.contains("exceeded max_ticks"), "{tail}");
        assert!(!tail.contains("planning"), "{tail}");
        // The total still reports the whole file, so the panel can say how
        // much it is not showing.
        assert!(tail.contains(r#""total_lines":3"#), "{tail}");
    }

    /// A run that was planned and never started has no log. That is an empty
    /// log, not a 500 the caller has to special-case.
    #[tokio::test]
    async fn a_run_with_no_log_yet_is_an_empty_log() {
        let (_tmp, projects, _) = seed_run("planning", "pending");
        let resp = round_trip_for(
            projects,
            None,
            &get("/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/log"),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains(r#""total_lines":0"#), "{resp}");
    }

    #[tokio::test]
    async fn dashboard_renders_as_plain_text() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(
            projects,
            None,
            &get("/v1/projects/repo/pilot/runs/2026-08-18-1042-abc123/dashboard?width=80"),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("text/plain"), "{resp}");
        assert!(resp.contains("write the keepalive"), "{resp}");
    }

    #[tokio::test]
    async fn a_goal_is_queued_as_an_intake_file() {
        let (tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/pilot/goals",
                "{\"text\":\"fix the flaky test\",\"author\":\"me\"}",
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 202"), "{resp}");
        let intake = tmp.path().join(".wingman").join("intake");
        let written: Vec<_> = std::fs::read_dir(&intake).unwrap().flatten().collect();
        assert_eq!(written.len(), 1);
        let body = std::fs::read_to_string(written[0].path()).unwrap();
        assert!(body.contains("fix the flaky test"), "{body}");
    }

    #[tokio::test]
    async fn a_turn_above_the_ceiling_is_refused_before_anything_spawns() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(
            projects,
            None,
            &post(
                "/v1/projects/repo/turns",
                "{\"prompt\":\"rm -rf everything\",\"mode\":\"yolo\"}",
            ),
        )
        .await;
        // The default ceiling in these tests is auto-edit.
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        assert!(resp.contains("exceeds"), "{resp}");
        // The accepted case is covered by `serve::tests::a_lower_request_is
        // _honoured` rather than here: a route test that gets past the mode
        // check spawns a real child, and under `cargo test` `current_exe()`
        // is the test binary.
    }

    #[tokio::test]
    async fn an_empty_prompt_is_rejected() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(
            projects,
            None,
            &post("/v1/projects/repo/turns", "{\"prompt\":\"   \"}"),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    }

    #[tokio::test]
    async fn a_traversing_session_id_cannot_read_a_file_outside_the_project() {
        let (_tmp, projects, _) = seed_run("running", "in_progress");
        let resp = round_trip_for(
            projects,
            None,
            &get("/v1/projects/repo/sessions/..%2F..%2Fsecrets"),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
    }

    #[tokio::test]
    async fn sessions_round_trip_through_the_transcript_on_disk() {
        let (tmp, projects, _) = seed_run("running", "in_progress");
        // A session is just a transcript file, so seeding one is enough for
        // list/get/delete to see it — exactly what a real turn would leave.
        let dir = tmp.path().join(".wingman").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = [
            r#"{"kind":"session_start","ts":"t","model":"m","provider":"p","system_hash":null}"#,
            r#"{"kind":"user","ts":"t","text":"why is the index stale?"}"#,
        ]
        .join("\n");
        std::fs::write(dir.join("20260818T104200000Z.jsonl"), transcript).unwrap();

        let listed =
            round_trip_for(projects.clone(), None, &get("/v1/projects/repo/sessions")).await;
        assert!(listed.contains("20260818T104200000Z"), "{listed}");
        assert!(listed.contains("why is the index stale?"), "{listed}");
        assert!(listed.contains("\"turns\":1"), "{listed}");

        let got = round_trip_for(
            projects.clone(),
            None,
            &get("/v1/projects/repo/sessions/20260818T104200000Z"),
        )
        .await;
        assert!(got.starts_with("HTTP/1.1 200"), "{got}");
        assert!(got.contains("session_start"), "{got}");

        let deleted = round_trip_for(
            projects,
            None,
            &del("/v1/projects/repo/sessions/20260818T104200000Z"),
        )
        .await;
        assert!(deleted.starts_with("HTTP/1.1 200"), "{deleted}");
        assert!(!dir.join("20260818T104200000Z.jsonl").exists());
    }

    #[tokio::test]
    async fn unknown_route_is_404_and_points_at_the_schema() {
        let resp = round_trip(
            None,
            "GET /v1/nope HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        assert!(resp.contains("/v1/schema"), "{resp}");
    }
}
