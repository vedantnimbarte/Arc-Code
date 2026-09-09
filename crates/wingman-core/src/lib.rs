//! wingman core types.
//!
//! These types are the contract every provider speaks. Modeled after
//! Anthropic's Messages API because it's the most expressive (tool_use +
//! tool_result blocks, explicit cache_control). Other providers translate
//! into and out of this shape in `wingman-providers`.

pub mod agent;
pub mod checkpoint;
pub mod context_log;
pub mod error;
pub mod loopguard;
pub mod message;
pub mod pricing;
pub mod provider;
pub mod spill;
pub mod steer;
pub mod stream;
pub mod tokens;
pub mod tool;
pub mod usage;

pub use agent::{
    AgentConfig, AgentEvent, AgentLoop, AgentStop, GateReport, LearningHook, NoopLearningHook,
    ToolDispatcher, ToolOutcome, TurnGate,
};
pub use context_log::{ContextFact, ContextSink};
pub use error::{Result, WingmanError};
pub use loopguard::{LoopGuard, Verdict as LoopVerdict};
pub use message::{ContentBlock, Message, Role};
pub use pricing::{price_for, Price};
pub use provider::{
    complete_text, CacheBreakpoint, CacheKind, CompletionRequest, Provider, ProviderCapabilities,
    ReasoningEffort,
};
pub use spill::SpillStore;
pub use steer::SteerInbox;
pub use stream::{ProviderEventStream, StopReason, StreamEvent};
pub use tokens::{
    estimate_history_tokens, estimate_tokens, CompactPlan, Compactor, ToolOutputBudget,
    ToolResultPruner,
};
pub use tool::{canonical_args, ToolSpec};

/// Install the process-wide rustls crypto provider (ring) exactly once.
///
/// Our reqwest deps use the `rustls-no-provider` feature (so we stay on ring —
/// no aws-lc-rs/OpenSSL, preserving the static-binary distribution while
/// unifying every crate on reqwest 0.13). reqwest then reads the *process
/// default* `CryptoProvider`; without one installed, building an HTTPS client
/// fails. Every reqwest client builder in the workspace calls this first. It is
/// idempotent — the first install wins and later calls are ignored.
pub fn ensure_tls_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Ignore the Err: it only means a provider was already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
pub use usage::Usage;
