//! Native provider drivers for TodeX 2.0.

mod acp;
pub(crate) mod antigravity;
mod claude;
mod cli_manager;
pub(crate) mod codex;
mod devin;
mod discovery;
mod doctor;
mod grok;
pub(crate) mod mcp_injection;
mod opencode;
mod pi;
pub(crate) mod process;
pub(crate) mod process_registry;
pub(crate) mod profile;
mod resident;
mod rpc;
mod supervisor;
pub(crate) mod types;

pub(crate) use cli_manager::{
    read_current_version, run_install, run_upgrade, CliManager, CliOperationAction,
    CliUpgradeOperation, CliVersionsResponse, ManagedCli,
};
pub(crate) use doctor::inspect_providers;
pub(crate) use grok::inspect_grok;
pub use supervisor::{
    CancelOutcome, ConversationPrompt, ConversationSupervisor, FollowUpAddOutcome,
    PromptContentRef, PromptSkillRef,
};
pub use types::{PermissionDecision, PermissionOutcome, PermissionPolicy};

#[cfg(test)]
mod control_tests;
#[cfg(test)]
pub(crate) mod golden_support;
