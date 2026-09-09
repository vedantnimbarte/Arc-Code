//! `wingman serve` — the HTTP/SSE API. Full surface in `docs/HTTP-API.md`.
//!
//! One daemon serves an allowlist of repos so you can drive Wingman from a
//! phone, another machine, a Shortcut, or CI without opening a terminal.
//!
//! Three things worth knowing before reading the routes:
//!
//! 1. **Pilot state is read straight off disk.** Runs are already an
//!    append-only `tasks.jsonl` plus an atomic `state.json`, and control is
//!    already "append a line to `control.jsonl`" — so those routes are pure
//!    filesystem work with no coupling to a live orchestrator process.
//! 2. **Turns run as child processes.** `runtime::build_*` and every CLI
//!    command resolve the project from `std::env::current_dir()`, which is
//!    process-wide; concurrent turns in two repos would race it. Pilot
//!    already spawns its workers this way, the child's NDJSON events map onto
//!    SSE line-for-line, and a panicking turn cannot take the daemon with it.
//! 3. **The ceiling is not negotiable.** `[serve].max_permission_mode` bounds
//!    every request; a request may ask for less and never for more.

mod admin;
mod argv;
pub mod auth;
mod board;
mod child;
mod http;
mod notifications;
pub mod pairing;
mod pilot;
pub mod projects;
mod push;
mod routes;
mod sessions;
mod table;
mod timeline;
mod ui;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use wingman_config::{Config, PermissionMode};

use projects::Project;

/// Flags from `wingman serve`.
#[derive(Debug, Default)]
pub struct ServeOptions {
    /// Override `[serve].addr`.
    pub addr: Option<String>,
    /// Generate a token into the OS keyring, print it once, exit.
    pub init_token: bool,
    /// Print the resolved projects and ceiling, then exit.
    pub list: bool,
    /// Required to run with a `yolo` ceiling.
    pub allow_yolo: bool,
    /// Open pairing for this run: mint a single-use code, print the one-paste
    /// link, and accept one `POST /v1/pair/redeem`. Off unless asked, so a
    /// reachable daemon cannot be talked into minting a code.
    pub pair: bool,
}

/// Everything a request handler needs. Shared behind an `Arc` across
/// connections.
pub struct ServeState {
    pub cfg: Config,
    pub projects: Vec<Project>,
    pub token: Option<String>,
    /// Highest permission mode any request may obtain.
    pub ceiling: PermissionMode,
    pub started: Instant,
    /// Bounds concurrent agent turns across all projects.
    pub turns: Semaphore,
    /// The pending pairing code, when `--pair` opened one. `None` means
    /// `/v1/pair/redeem` has nothing to offer and says so.
    pub pairing: std::sync::Mutex<Option<pairing::Bootstrap>>,
}

impl ServeState {
    /// Clamp a requested mode to the ceiling. `None` means the request did
    /// not ask, and gets the ceiling.
    ///
    /// Returns `Err` with the offending mode when the request asked for more
    /// than the ceiling — a 403, not a silent downgrade, so a client never
    /// believes it got authority it did not get.
    pub fn effective_mode(
        &self,
        requested: Option<PermissionMode>,
    ) -> Result<PermissionMode, PermissionMode> {
        match requested {
            None => Ok(self.ceiling),
            Some(m) if rank(m) <= rank(self.ceiling) => Ok(m),
            Some(m) => Err(m),
        }
    }
}

/// Order the permission modes by how much authority they grant, so the
/// ceiling comparison is a number rather than a match arm per pair.
pub fn rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::ReadOnly => 0,
        PermissionMode::Plan => 1,
        PermissionMode::AutoEdit => 2,
        PermissionMode::Yolo => 3,
    }
}

pub async fn run(cfg: Config, opts: ServeOptions) -> Result<ExitCode> {
    if opts.init_token {
        return init_token();
    }

    let addr = opts.addr.clone().unwrap_or_else(|| cfg.serve.addr.clone());
    let token = auth::resolve_token(&cfg.serve);
    let ceiling = cfg.serve.max_permission_mode;
    let bind = auth::check_bind(&addr, token.as_deref(), ceiling, opts.allow_yolo)?;
    let projects = projects::resolve_all(&cfg.serve)?;

    if opts.list {
        println!("addr    {addr}");
        println!("ceiling {ceiling}");
        println!(
            "auth    {}",
            if token.is_some() {
                "bearer token"
            } else {
                "none (loopback only)"
            }
        );
        println!("projects:");
        for p in &projects {
            println!("  {:<16} {}", p.id, projects::display_root(&p.root));
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Minted before the listener so the code is on screen by the time the
    // port is answering, and refused up front when there is no token to hand
    // out — pairing into an unauthenticated server would return nothing.
    let bootstrap = match (opts.pair, token.is_some()) {
        (true, true) => Some(pairing::Bootstrap::issue()),
        (true, false) => {
            anyhow::bail!(
                "--pair needs a token to hand out, and this server has none                  (loopback with no `[serve].token`). Run `wingman serve --init-token` first."
            );
        }
        (false, _) => None,
    };
    if let Some(b) = &bootstrap {
        eprintln!(
            "
{}
",
            pairing::instructions(&bind, b.code())
        );
    }

    let state = Arc::new(ServeState {
        turns: Semaphore::new(cfg.serve.max_concurrent_turns.max(1)),
        cfg,
        projects,
        token,
        ceiling,
        started: Instant::now(),
        pairing: std::sync::Mutex::new(bootstrap),
    });

    // Put the allowlisted repos on the board once, so the panel opens onto a
    // board that can actually take a card. Without it a `serve`-only user sees
    // an empty registry and cannot add one, because registration otherwise
    // happens by running pilot from a terminal — the trip the panel exists to
    // avoid. Idempotent and best-effort; see `board::import_projects`.
    board::import_projects(&state);

    // Outbound push runs alongside the listener when configured, so a phone
    // learns a run finished without holding a connection open.
    if state.cfg.serve.push.url.is_some() {
        tokio::spawn(push::watcher(Arc::clone(&state)));
    }

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {addr}"))?;
    eprintln!(
        "wingman serve: listening on {addr} — {} project(s), ceiling {ceiling}, auth {}",
        state.projects.len(),
        if state.token.is_some() {
            "on"
        } else {
            "OFF (loopback)"
        }
    );
    for p in &state.projects {
        eprintln!("  {:<16} {}", p.id, projects::display_root(&p.root));
    }
    // Say which UI is being served. A binary built without `panel/dist/` answers
    // `/` with a placeholder, and finding that out from the browser rather
    // than the terminal wastes someone's afternoon.
    if ui::embedded() {
        eprintln!("  panel            http://{addr}/");
    } else {
        eprintln!("  panel            not built — run `npm run build` in panel/, then rebuild");
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (sock, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        continue;
                    }
                };
                let state = Arc::clone(&state);
                // One task per connection: a slow SSE reader must not block
                // the next request, and a handler panic is contained.
                tokio::spawn(async move {
                    if let Err(e) = routes::handle(state, sock).await {
                        tracing::debug!("connection from {peer} ended: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nwingman serve: stopped");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
}

/// Generate a token, store it in the OS keyring, print it exactly once.
fn init_token() -> Result<ExitCode> {
    let token = auth::generate_token();
    wingman_config::secrets::store(auth::KEYRING_ENTRY, &token)
        .context("storing the API token in the OS keyring")?;
    println!("{token}");
    eprintln!();
    eprintln!("Stored in the OS keyring. This is the only time it is printed.");
    eprintln!("Clients send it as:  Authorization: Bearer <token>");
    eprintln!("`wingman serve` picks it up with no further config.");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(ceiling: PermissionMode) -> ServeState {
        ServeState {
            cfg: Config::default(),
            projects: Vec::new(),
            token: None,
            ceiling,
            started: Instant::now(),
            pairing: std::sync::Mutex::new(None),
            turns: Semaphore::new(1),
        }
    }

    #[test]
    fn unspecified_mode_gets_the_ceiling() {
        let s = state_with(PermissionMode::AutoEdit);
        assert_eq!(s.effective_mode(None), Ok(PermissionMode::AutoEdit));
    }

    #[test]
    fn a_lower_request_is_honoured() {
        let s = state_with(PermissionMode::AutoEdit);
        assert_eq!(
            s.effective_mode(Some(PermissionMode::ReadOnly)),
            Ok(PermissionMode::ReadOnly)
        );
    }

    #[test]
    fn asking_above_the_ceiling_is_an_error_not_a_downgrade() {
        let s = state_with(PermissionMode::AutoEdit);
        assert_eq!(
            s.effective_mode(Some(PermissionMode::Yolo)),
            Err(PermissionMode::Yolo)
        );
    }
}
