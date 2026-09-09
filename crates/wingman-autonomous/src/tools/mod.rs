//! Manager-only tools.
//!
//! These are the only tools registered on the manager's [`wingman_core::AgentLoop`].
//! Each is a thin Tool that translates JSON args from the model into a
//! [`crate::orchestrator::OrchestratorCommand`] and awaits the reply. Read-
//! only inspection tools (`list_dir`, `read_file`, `grep_tool`) come from
//! `wingman-tools::builtin` and are registered alongside these by
//! [`crate::manager::build_manager_registry`].

/// The manager's orchestration tools.
///
/// These are pollers and bookkeeping: the manager re-issues the same call each
/// tick until a worker changes the state it is asking about, so identical
/// repetition is normal operation rather than a loop. Named here so the
/// exemption cannot drift away from the set of tools it is about.
pub const ORCHESTRATION_TOOLS: &[&str] = &[
    "add_task",
    "assign_task",
    "reassign_task",
    "abort_task",
    "finalize_task",
    "message_agent",
    "run_acceptance",
];

mod abort_task;
mod add_task;
mod assign_task;
mod finalize_task;
mod message_agent;
mod reassign_task;
mod run_acceptance;

pub use abort_task::AbortTask;
pub use add_task::AddTask;
pub use assign_task::AssignTask;
pub use finalize_task::FinalizeTask;
pub use message_agent::MessageAgent;
pub use reassign_task::ReassignTask;
pub use run_acceptance::RunAcceptance;
