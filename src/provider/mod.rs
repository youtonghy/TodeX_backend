//! Native provider drivers for TodeX 2.0.

mod acp;
mod claude;
mod cli_manager;
mod codex;
mod devin;
mod doctor;
mod grok;
mod opencode;
mod pi;
pub(crate) mod process;
mod supervisor;
pub(crate) mod types;

pub(crate) use cli_manager::{
    read_current_version, run_upgrade, CliManager, CliUpgradeOperation, CliVersionsResponse,
    ManagedCli,
};
pub(crate) use doctor::inspect_providers;
pub(crate) use grok::inspect_grok;
pub use supervisor::{
    ConversationPrompt, ConversationSupervisor, PromptContentRef, PromptSkillRef,
};
pub use types::PermissionDecision;

#[cfg(test)]
mod control_tests;
