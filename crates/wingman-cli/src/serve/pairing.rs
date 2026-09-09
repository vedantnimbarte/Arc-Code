//! One-paste pairing: get a device onto the daemon without hand-carrying the
//! token.
//!
//! # What this is not
//!
//! It is not per-device authority. `serve/auth.rs` says per-token scopes are a
//! non-goal — run two servers with different ceilings if you need two
//! authorities — and nothing here changes that. Every device that pairs ends
//! up holding the same token with the same ceiling it would have had if you
//! had typed it in.
//!
//! # What it is
//!
//! Enrolment. Today, getting Wingman onto a phone means moving a 43-character
//! base64 secret onto that phone by hand, which in practice means pasting it
//! into a chat app or a notes file — putting the durable credential somewhere
//! it will outlive the moment.
//!
//! So: a short-lived, single-use code that can be exchanged *once* for the
//! real token, and is worthless a few minutes later. The two are separate the
//! way a Tailscale auth key is separate from the device identity it admits —
//! burning or expiring the code does not revoke anything already paired, and
//! revoking the token does not depend on remembering which codes were issued.
//!
//! # Why the code is safe to paste
//!
//! It is single-use and short-lived, so the window in which an intercepted
//! code is worth anything is minutes, and using it is *detectable*: the
//! intended device's redemption fails, loudly, because the code is already
//! spent. A leaked long-lived token has neither property.
//!
//! Pairing is opt-in per run (`wingman serve --pair`) and there is no standing
//! endpoint that mints codes: an attacker who reaches the daemon cannot ask it
//! to start pairing.

use std::time::{Duration, Instant};

use serde_json::json;

/// How long a code is worth anything. Long enough to walk to another device,
/// short enough that a code left on screen is not a credential.
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// A pending pairing code. At most one exists at a time: a second
/// `--pair` replaces the first, so an abandoned code cannot linger.
#[derive(Debug)]
pub struct Bootstrap {
    code: String,
    issued: Instant,
    /// Set once the code has been exchanged. A spent code is kept rather than
    /// dropped so a second attempt can be told it was *used*, not that it was
    /// wrong — that difference is the whole detection story.
    spent: bool,
}

impl Bootstrap {
    /// Mint a fresh code. Same generator and entropy as the API token itself:
    /// the code is short-lived, but for the minutes it lives it is a
    /// credential, and a guessable one would be a hole.
    pub fn issue() -> Self {
        Self {
            code: super::auth::generate_token(),
            issued: Instant::now(),
            spent: false,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn expired(&self) -> bool {
        self.issued.elapsed() >= CODE_TTL
    }

    /// Check `presented` against this code and consume it.
    ///
    /// Ordering matters: the constant-time comparison runs before the spent
    /// and expiry checks, so a wrong code and a spent one take the same path
    /// and neither the code nor its state leaks through timing.
    pub fn redeem(&mut self, presented: &str) -> Result<(), RedeemError> {
        use subtle::ConstantTimeEq;
        let matches: bool = self.code.len() == presented.len()
            && self.code.as_bytes().ct_eq(presented.as_bytes()).into();
        if !matches {
            return Err(RedeemError::NoMatch);
        }
        if self.spent {
            return Err(RedeemError::AlreadyUsed);
        }
        if self.expired() {
            return Err(RedeemError::Expired);
        }
        self.spent = true;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RedeemError {
    NoMatch,
    AlreadyUsed,
    Expired,
}

impl RedeemError {
    /// What the client is told.
    ///
    /// `AlreadyUsed` is reported distinctly and on purpose. It is the signal
    /// that someone else redeemed the code first, and collapsing it into a
    /// generic failure would hide precisely the event worth noticing.
    pub fn message(&self) -> &'static str {
        match self {
            RedeemError::NoMatch => "invalid pairing code",
            RedeemError::AlreadyUsed => {
                "this pairing code has already been used — if that was not you, \
                 rotate the token with `wingman serve --init-token`"
            }
            RedeemError::Expired => "this pairing code has expired — issue a new one",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            RedeemError::NoMatch => 401,
            // The code was real; it is the state that refuses.
            RedeemError::AlreadyUsed | RedeemError::Expired => 410,
        }
    }
}

/// The one-paste link handed to the other device.
///
/// A URL rather than a bare code so it carries where to connect as well as
/// the secret — the point is that the far end needs no other instructions.
pub fn setup_link(addr: &std::net::SocketAddr, code: &str) -> String {
    format!("wingman-pair://{addr}/?code={code}")
}

/// What `--pair` prints.
pub fn instructions(addr: &std::net::SocketAddr, code: &str) -> String {
    format!(
        "Pairing code issued — valid for {} minutes, single use.\n\n  {}\n\n\
         On the other device:\n  \
         curl -s -X POST http://{addr}/v1/pair/redeem \\\n    \
         -H 'content-type: application/json' \\\n    \
         -d '{{\"code\":\"{code}\"}}'\n\n\
         It returns the API token. Anything already paired is unaffected when \
         this code expires.",
        CODE_TTL.as_secs() / 60,
        setup_link(addr, code),
    )
}

/// `POST /v1/pair/redeem` — exchange a code for the API token.
///
/// Sits ahead of the auth gate, like sign-in does, for the same
/// chicken-and-egg reason: redeeming *is* the authentication. It is the only
/// unauthenticated route that returns a credential, which is why every refusal
/// path above is explicit.
pub async fn redeem(
    state: &std::sync::Arc<super::ServeState>,
    req: &super::http::Request,
    sock: &mut tokio::net::TcpStream,
) -> std::io::Result<()> {
    let Some(token) = state.token.as_deref() else {
        // No token configured means the daemon is loopback-only and open;
        // there is no credential to hand out and pretending otherwise would
        // give the client a token that authorises nothing.
        return super::http::write_err(sock, 409, "this server has no token to pair with").await;
    };

    let presented = serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_string))
        .unwrap_or_default();

    // The lock is taken and released inside this block, deliberately: a
    // `std::sync::MutexGuard` is not `Send`, and holding one across the
    // `.await` below would make the whole connection future non-`Send` and
    // unspawnable. Decide under the lock, write after it.
    let verdict = {
        let mut guard = state.pairing.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_mut().map(|bootstrap| bootstrap.redeem(&presented))
    };

    match verdict {
        None => super::http::write_err(sock, 404, "pairing is not open on this server").await,
        Some(Ok(())) => {
            tracing::info!(target: "wingman::serve", "pairing code redeemed");
            super::http::write_json(sock, 200, &json!({ "token": token })).await
        }
        Some(Err(e)) => {
            tracing::warn!(target: "wingman::serve", reason = ?e, "pairing redemption refused");
            super::http::write_err(sock, e.status(), e.message()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_code_redeems_once() {
        let mut b = Bootstrap::issue();
        let code = b.code().to_string();
        assert_eq!(b.redeem(&code), Ok(()));
    }

    /// The property the whole design rests on.
    #[test]
    fn a_code_cannot_be_redeemed_twice() {
        let mut b = Bootstrap::issue();
        let code = b.code().to_string();
        assert_eq!(b.redeem(&code), Ok(()));
        assert_eq!(b.redeem(&code), Err(RedeemError::AlreadyUsed));
    }

    #[test]
    fn a_wrong_code_is_refused_and_does_not_spend_the_real_one() {
        let mut b = Bootstrap::issue();
        let code = b.code().to_string();
        assert_eq!(b.redeem("not-the-code"), Err(RedeemError::NoMatch));
        assert_eq!(
            b.redeem(&code),
            Ok(()),
            "a failed guess must not burn the code — that would be a denial of service"
        );
    }

    #[test]
    fn an_expired_code_is_refused() {
        let mut b = Bootstrap::issue();
        let code = b.code().to_string();
        b.issued = Instant::now() - (CODE_TTL + Duration::from_secs(1));
        assert!(b.expired());
        assert_eq!(b.redeem(&code), Err(RedeemError::Expired));
    }

    /// "Already used" must be distinguishable from "wrong": it is how the
    /// intended device finds out someone else got there first.
    #[test]
    fn used_and_wrong_are_reported_differently() {
        assert_ne!(
            RedeemError::AlreadyUsed.message(),
            RedeemError::NoMatch.message()
        );
        assert_eq!(RedeemError::NoMatch.status(), 401);
        assert_eq!(RedeemError::AlreadyUsed.status(), 410);
    }

    #[test]
    fn codes_are_not_guessable_or_repeated() {
        let a = Bootstrap::issue();
        let b = Bootstrap::issue();
        assert_ne!(a.code(), b.code());
        assert!(a.code().len() >= 40, "code is {} chars", a.code().len());
    }

    #[test]
    fn the_link_carries_the_endpoint_and_the_code() {
        let addr: std::net::SocketAddr = "192.168.1.20:8787".parse().unwrap();
        let link = setup_link(&addr, "abc123");
        assert_eq!(link, "wingman-pair://192.168.1.20:8787/?code=abc123");
    }

    #[test]
    fn the_instructions_carry_the_address_and_the_code() {
        let addr: std::net::SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let text = instructions(&addr, "CODE");
        assert!(text.contains("127.0.0.1:8787"));
        assert!(text.contains("CODE"));
        assert!(text.contains("single use"));
    }
}
