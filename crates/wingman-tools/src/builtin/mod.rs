//! Built-in tools shipped with wingman.
//!
//! Each tool has a stable name (used in the prompt and on the wire), a JSON
//! Schema describing its inputs, and an async `run` that produces a string
//! result. Tools that mutate state consult `ToolCtx` for permission.

mod apply_patch;
mod ask_user;
mod command_tool;
mod edit_file;
mod forget_memory;
mod glob_tool;
mod grep_tool;
mod invoke_skill;
mod list_dir;
mod lsp_tools;
mod present_plan;
mod update_tasks;

mod jobs_tools;
mod read_file;
mod read_session;
mod recall_memory;
mod recall_session;
mod run_plan;
mod run_shell;
mod save_memory;
mod semantic_search;
mod spawn_subagent;
mod task_complete;
mod tool_search;
mod web_fetch;
mod web_search;
mod write_file;

#[cfg(feature = "treesitter")]
mod edit_symbol;
#[cfg(feature = "treesitter")]
mod find_symbol;
#[cfg(feature = "treesitter")]
mod outline;
#[cfg(feature = "treesitter")]
mod who_calls;

pub use apply_patch::ApplyPatch;
pub use ask_user::AskUser;
pub use command_tool::CommandTool;
pub use edit_file::EditFile;
pub use forget_memory::ForgetMemory;
pub use glob_tool::Glob;
pub use grep_tool::Grep;
pub use invoke_skill::InvokeSkill;
pub use jobs_tools::{JobList, JobOutput, JobSend, JobStop};
pub use list_dir::ListDir;
pub use lsp_tools::{
    LspCodeAction, LspDefinition, LspDiagnostics, LspHover, LspReferences, LspRename,
};
pub use present_plan::PresentPlan;
pub use read_file::ReadFile;
pub use read_session::ReadSession;
pub use recall_memory::RecallMemory;
pub use recall_session::RecallSession;
pub use run_plan::RunPlan;
pub use run_shell::RunShell;
pub use save_memory::SaveMemory;
pub use semantic_search::SemanticSearch;
pub use spawn_subagent::{SpawnSubagent, SubagentRunner, SubagentSpec};
pub use task_complete::TaskComplete;
pub use tool_search::{is_meta_tool, ToolCall, ToolSearch, CALL_TOOL, SEARCH_TOOL};
pub use update_tasks::UpdateTasks;
pub use web_fetch::WebFetch;
pub use web_search::WebSearch;
pub use write_file::WriteFile;

#[cfg(feature = "treesitter")]
pub use edit_symbol::EditSymbol;
#[cfg(feature = "treesitter")]
pub use find_symbol::FindSymbol;
#[cfg(feature = "treesitter")]
pub use outline::Outline;
#[cfg(feature = "treesitter")]
pub use who_calls::WhoCalls;
