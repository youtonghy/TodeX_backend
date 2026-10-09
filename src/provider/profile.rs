//! What each provider can do and how TodeX adapts to it, declared once per
//! driver (`<driver>::PROFILE`).
//!
//! The supervisor, the catalogs and `/v2/providers` read these facts instead
//! of matching on `ProviderKind`, so adding or changing a provider means
//! editing its own profile. Capabilities that depend on the installed CLI
//! (fork support probed from ACP agents, Codex live controls) stay on the
//! driver and are overlaid by it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::conversation::ProviderKind;

use super::types::{ImageInputMode, PermissionConfigCapabilities, ProviderCapabilities};

/// How TodeX's own MCP servers (agent tools) reach the provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpInjection {
    /// The provider cannot load MCP servers TodeX supplies.
    None,
    /// Codex `config` overrides (`mcp_servers.<name>`).
    CodexConfig,
    /// ACP `mcpServers` on `session/new|load|resume`.
    AcpServers,
    /// Claude Code `--mcp-config` file plus `--allowedTools`.
    ClaudeArgs,
    /// Static entries in the provider's global config (Antigravity
    /// `mcp_config.json`, plus its approval hook) pointed at the
    /// conversation through the provider process's environment.
    GlobalConfigEnv,
}

/// How selected skills reach the provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillInjection {
    /// The provider loads them natively from the prompt's skill list.
    Native,
    /// Their instructions are inlined ahead of the prompt text.
    PromptText,
}

/// How attached workspace files are referenced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileAttachmentStyle {
    /// Only as typed prompt content.
    Native,
    /// Also as an `Attached file: @path` line in the prompt text.
    AtMention,
}

/// Process lifetime of a conversation's native runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModel {
    /// A process per turn.
    PerTurn,
    /// One process per conversation kept between turns, stopped after
    /// `idle` without a turn (never, when `None`), at most `max_sessions`
    /// at a time.
    Resident {
        idle: Option<Duration>,
        max_sessions: usize,
    },
}

impl ProcessModel {
    pub const fn idle_timeout(self) -> Option<Duration> {
        match self {
            Self::PerTurn => None,
            Self::Resident { idle, .. } => idle,
        }
    }

    pub const fn max_sessions(self) -> Option<usize> {
        match self {
            Self::PerTurn => None,
            Self::Resident { max_sessions, .. } => Some(max_sessions),
        }
    }
}

/// A provider's own configuration directory: `$env` when set, otherwise
/// `~/<home_relative>`.
#[derive(Clone, Copy, Debug)]
pub struct ConfigHome {
    pub env: Option<&'static str>,
    pub home_relative: &'static str,
}

impl ConfigHome {
    pub fn resolve(&self, home: &Path) -> PathBuf {
        self.env
            .and_then(std::env::var_os)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(self.home_relative))
    }
}

/// A user-level config file, relative to the home or the provider's
/// [`ConfigHome`].
#[derive(Clone, Copy, Debug)]
pub enum UserConfigFile {
    Home(&'static str),
    ConfigHome(&'static str),
}

/// Where the read-only skill/MCP catalogs come from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogSource {
    /// Scanned from the provider's documented directories and files.
    Filesystem,
    /// Reported by the CLI itself (`grok inspect`); its MCP servers are only
    /// invoked natively inside provider sessions.
    NativeInspect,
}

#[derive(Clone, Copy, Debug)]
pub struct CatalogProfile {
    pub source: CatalogSource,
    pub config_home: ConfigHome,
    /// Project skill directory, relative to the workspace. User skills live
    /// in `<config home>/skills`.
    pub project_skills: &'static str,
    /// MCP config files TodeX may list; empty when the provider keeps its
    /// servers elsewhere.
    pub mcp_user_files: &'static [UserConfigFile],
    pub mcp_project_files: &'static [&'static str],
    pub mcp_user_source: &'static str,
    pub mcp_project_source: &'static str,
}

impl CatalogProfile {
    /// A catalog without MCP config files.
    pub const fn skills_only(config_home: ConfigHome, project_skills: &'static str) -> Self {
        Self {
            source: CatalogSource::Filesystem,
            config_home,
            project_skills,
            mcp_user_files: &[],
            mcp_project_files: &[],
            mcp_user_source: "",
            mcp_project_source: "",
        }
    }
}

/// Static facts about one provider.
#[derive(Clone, Copy, Debug)]
pub struct ProviderProfile {
    pub kind: ProviderKind,
    pub display_name: &'static str,
    pub permission_config: PermissionConfigCapabilities,
    /// Defaults; a driver that probes its CLI overlays the live answer.
    pub native_fork: bool,
    pub native_compact: bool,
    pub native_resume: bool,
    pub cancel: bool,
    pub permissions: bool,
    pub tool_events: bool,
    pub native_skills: bool,
    pub native_mcp: bool,
    pub model_selection: bool,
    /// Whether prompts may carry images without a per-profile check.
    pub image_input: bool,
    pub image_input_mode: ImageInputMode,
    pub mcp_injection: McpInjection,
    pub skill_injection: SkillInjection,
    pub file_attachments: FileAttachmentStyle,
    /// Conversations must name one of the configured profiles
    /// (`providerProfile`), each launching its own executable.
    pub profile_required: bool,
    /// Startup recovery always replays the whole journal: resident runtimes
    /// and session-scoped dialogs outlive turns, so the journal tail cannot
    /// show whether one was left open.
    pub recovery_full_scan: bool,
    pub process_model: ProcessModel,
    /// How long discovered model/command catalogs are reused.
    pub discovery_cache_ttl: Option<Duration>,
    pub catalog: CatalogProfile,
}

impl ProviderProfile {
    /// Whether TodeX can hand this provider its own MCP servers.
    pub fn managed_mcp(&self) -> bool {
        self.mcp_injection != McpInjection::None
    }

    /// Whether typed prompt images may be sent at all; profile-dependent
    /// providers check the selected profile when the turn starts.
    pub fn accepts_typed_images(&self) -> bool {
        self.image_input || self.image_input_mode == ImageInputMode::Profile
    }

    /// The static capability block of `/v2/providers`.
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            permission_config: self.permission_config,
            native_fork: self.native_fork,
            native_compact: self.native_compact,
            native_resume: self.native_resume,
            cancel: self.cancel,
            permissions: self.permissions,
            tool_events: self.tool_events,
            native_skills: self.native_skills,
            native_mcp: self.native_mcp,
            managed_mcp: self.managed_mcp(),
            model_selection: self.model_selection,
            image_input: self.image_input,
            image_input_mode: self.image_input_mode,
        }
    }
}

/// The profile of `kind`.
pub fn profile(kind: ProviderKind) -> &'static ProviderProfile {
    let profile = match kind {
        ProviderKind::Acp => &super::acp::PROFILE,
        ProviderKind::Codex => &super::codex::PROFILE,
        ProviderKind::Pi => &super::pi::PROFILE,
        ProviderKind::ClaudeCode => &super::claude::PROFILE,
        ProviderKind::GrokBuild => &super::grok::PROFILE,
        ProviderKind::Devin => &super::devin::PROFILE,
        ProviderKind::Opencode => &super::opencode::PROFILE,
        ProviderKind::Antigravity => &super::antigravity::PROFILE,
    };
    debug_assert_eq!(profile.kind, kind);
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_its_own_profile() {
        for kind in ProviderKind::ALL {
            assert_eq!(profile(kind).kind, kind);
        }
    }
}
