//! Native session history of Claude Code, Pi, Codex, Grok Build and Devin:
//! the agents' own sessions, outside TodeX's conversation store. Lists them
//! (with their disk usage where it is known), shows a bounded transcript,
//! deletes chosen sessions, and applies a TodeX-managed age / capacity
//! policy per provider (saved in `$data_dir/native-history-cleanup.json`,
//! run by the history cleanup scheduler in [`crate::history_cleanup`]).
//!
//! Claude Code and Pi sessions are files TodeX removes itself. Codex rollouts
//! and Grok Build session directories are read from disk but deleted only
//! through the provider's own CLI; Devin's sessions live in its SQLite
//! database and are both listed and deleted through its CLI ([`cli`]).
//!
//! Safety: a session a TodeX conversation (of any tenant) resumes, a running
//! session, and a session written in the last two minutes are never
//! deleted. Ids are validated and resolved through a fresh scan; every path
//! TodeX removes must lie inside the provider's root, and symlinks are never
//! followed or removed.

mod cli;
mod parse;
mod retention;

pub use cli::NativeClis;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{self, BufRead, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    agent_providers::AgentDirs,
    app_state::AppState,
    conversation::ProviderKind,
    error::AppError,
    history_cleanup::{CleanupPolicy, CleanupRun, CleanupTrigger, HistoryCleanup},
    provider::{
        plan_policy_deletions, CleanupReport, CleanupSkip, CleanupThresholds,
        ConversationSupervisor, NativeSessionRef, PolicyCandidate, PolicyRule,
    },
};

use parse::{SessionMeta, TranscriptMessage};

pub const NATIVE_HISTORY_CLEANUP_FILE: &str = "native-history-cleanup.json";
/// Bytes read from each end of a session file for its listing metadata.
const HEAD_TAIL_BYTES: u64 = 256 * 1024;
/// A session file written this recently counts as running.
const RECENT_WRITE: Duration = Duration::from_secs(120);
/// Largest `sessions/<pid>.json` Claude running marker read.
const MAX_MARKER_BYTES: u64 = 64 * 1024;
pub const DEFAULT_TRANSCRIPT_LIMIT: usize = 200;
pub const MAX_TRANSCRIPT_LIMIT: usize = 1000;
const MAX_SESSION_ID_CHARS: usize = 128;
/// Where Codex moves archived rollouts, under `$CODEX_HOME`.
const CODEX_ARCHIVED_DIR: &str = "archived_sessions";
/// Codex's append-only thread name index, under `$CODEX_HOME`.
const CODEX_SESSION_INDEX: &str = "session_index.jsonl";
/// Largest `session_index.jsonl` read for thread names.
const MAX_CODEX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
/// Largest Grok `summary.json` read.
const MAX_GROK_SUMMARY_BYTES: u64 = 4 * 1024 * 1024;
/// Devin's session database (with `-wal` / `-shm`) in its data directory.
const DEVIN_SESSION_DB: &str = "sessions.db";
const GROK_SUMMARY_FILE: &str = "summary.json";
const GROK_UPDATES_FILE: &str = "updates.jsonl";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NativeProvider {
    ClaudeCode,
    Pi,
    Codex,
    GrokBuild,
    Devin,
}

/// What a provider's native history supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// The transcript endpoint works.
    pub view: bool,
    /// Per-session byte sizes are known.
    pub sizes: bool,
}

impl NativeProvider {
    pub const ALL: [Self; 5] = [
        Self::ClaudeCode,
        Self::Pi,
        Self::Codex,
        Self::GrokBuild,
        Self::Devin,
    ];

    /// `claude-code`, `pi`, `codex`, `grok-build` or `devin`; anything else
    /// is `INVALID_REQUEST`.
    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.as_str() == value)
            .ok_or_else(|| {
                AppError::InvalidRequest(format!(
                    "native history supports claude-code, pi, codex, grok-build and devin, not {value}"
                ))
            })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::GrokBuild => "grok-build",
            Self::Devin => "devin",
        }
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Pi => "Pi",
            Self::Codex => "Codex",
            Self::GrokBuild => "Grok Build",
            Self::Devin => "Devin",
        }
    }

    const fn kind(self) -> ProviderKind {
        match self {
            Self::ClaudeCode => ProviderKind::ClaudeCode,
            Self::Pi => ProviderKind::Pi,
            Self::Codex => ProviderKind::Codex,
            Self::GrokBuild => ProviderKind::GrokBuild,
            Self::Devin => ProviderKind::Devin,
        }
    }

    pub const fn capabilities(self) -> Capabilities {
        match self {
            Self::Devin => Capabilities {
                view: false,
                sizes: false,
            },
            _ => Capabilities {
                view: true,
                sizes: true,
            },
        }
    }

    /// Sessions are deleted through the provider's CLI, not by TodeX.
    const fn cli_deletes(self) -> bool {
        matches!(self, Self::Codex | Self::GrokBuild | Self::Devin)
    }

    /// A policy for this provider is valid: a capacity limit needs
    /// per-session sizes.
    pub fn validate_policy(self, policy: &CleanupPolicy) -> Result<(), AppError> {
        if !self.capabilities().sizes && policy.max_total_bytes.is_some() {
            return Err(AppError::InvalidRequest(format!(
                "{} session sizes are unknown, so maxTotalBytes must be null",
                self.as_str()
            )));
        }
        Ok(())
    }
}

/// Where each provider keeps its sessions.
#[derive(Clone, Debug)]
pub struct NativeRoots {
    pub claude_dir: PathBuf,
    pub pi_session_dir: PathBuf,
    /// `$CODEX_HOME` (`~/.codex`).
    pub codex_home: PathBuf,
    /// `$GROK_HOME` (`~/.grok`).
    pub grok_home: PathBuf,
    /// `$XDG_DATA_HOME/devin/cli` (`~/.local/share/devin/cli`), where Devin
    /// keeps `sessions.db`.
    pub devin_dir: PathBuf,
}

impl NativeRoots {
    pub fn detect() -> Self {
        let dirs = AgentDirs::detect();
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".local")
                    .join("share")
            });
        Self {
            claude_dir: dirs.claude_dir().to_owned(),
            pi_session_dir: dirs.pi_session_dir(),
            codex_home: dirs.codex_home().to_owned(),
            grok_home: dirs.grok_home().to_owned(),
            devin_dir: data_home.join("devin").join("cli"),
        }
    }

    /// The directory sessions live in (reported as `root`).
    pub fn sessions_root(&self, provider: NativeProvider) -> PathBuf {
        match provider {
            NativeProvider::ClaudeCode => self.claude_dir.join("projects"),
            NativeProvider::Pi => self.pi_session_dir.clone(),
            NativeProvider::Codex => self.codex_home.join("sessions"),
            NativeProvider::GrokBuild => self.grok_home.join("sessions"),
            NativeProvider::Devin => self.devin_dir.clone(),
        }
    }

    /// The directory per-session side data lives in.
    fn side_root(&self, provider: NativeProvider) -> &Path {
        match provider {
            NativeProvider::ClaudeCode => &self.claude_dir,
            _ => &self.pi_session_dir,
        }
    }

    /// The provider's history store exists. Codex also keeps archived
    /// rollouts outside `sessions/`.
    fn store_exists(&self, provider: NativeProvider) -> bool {
        match provider {
            NativeProvider::Codex => {
                self.codex_home.join("sessions").is_dir()
                    || self.codex_home.join(CODEX_ARCHIVED_DIR).is_dir()
            }
            NativeProvider::Devin => self.devin_dir.join(DEVIN_SESSION_DB).is_file(),
            provider => self.sessions_root(provider).is_dir(),
        }
    }
}

/// Session ids are UUIDs (Grok and Devin may take client-chosen ids);
/// anything with characters outside `[A-Za-z0-9._-]`, a leading dot or
/// dash (an option to a provider CLI) or `..` is rejected before any lookup.
pub fn validate_session_id(id: &str) -> Result<(), AppError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_SESSION_ID_CHARS
        && !id.starts_with('.')
        && !id.starts_with('-')
        && !id.contains("..")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid native session id: {id:?}"
        )))
    }
}

/// One native session: its transcript file(s) — Claude may hold the same id
/// under two project directories, Codex in `sessions/` and
/// `archived_sessions/` — plus Claude's per-session side data. Grok's files
/// are its `summary.json` and `updates.jsonl` inside `dir`; Devin sessions
/// have no files TodeX knows of.
#[derive(Clone, Debug)]
struct NativeSession {
    id: String,
    files: Vec<PathBuf>,
    side: Vec<PathBuf>,
    /// Grok's session directory.
    dir: Option<PathBuf>,
    /// `None` when the provider does not expose per-session sizes.
    bytes: Option<u64>,
    modified: SystemTime,
    meta: SessionMeta,
    /// The provider reports the session open in a live process (Devin).
    locked: bool,
}

/// A provider's sessions plus what the store as a whole tells.
#[derive(Default)]
struct Listing {
    sessions: Vec<NativeSession>,
    available: bool,
    /// The store's total size for providers without per-session sizes.
    storage_bytes: u64,
}

impl Listing {
    fn total_bytes(&self, provider: NativeProvider) -> u64 {
        if provider.capabilities().sizes {
            self.sessions
                .iter()
                .filter_map(|session| session.bytes)
                .fold(0, u64::saturating_add)
        } else {
            self.storage_bytes
        }
    }
}

#[derive(Clone)]
struct CachedMeta {
    size: u64,
    modified: SystemTime,
    meta: Option<SessionMeta>,
}

type MetaCache = HashMap<(NativeProvider, PathBuf), CachedMeta>;

#[derive(Clone)]
pub struct NativeHistory {
    cache: Arc<std::sync::Mutex<MetaCache>>,
    /// Per-provider policies and last runs, keyed by provider id.
    pub(crate) cleanup: HistoryCleanup,
    /// Serializes deletions (manual and policy) and settings edits, one
    /// lock per provider so a slow CLI (Devin's `rm` can take seconds per
    /// session) never holds up another agent's cleanup.
    locks: Arc<[tokio::sync::Mutex<()>; 5]>,
    /// Fixed roots for tests; otherwise detected per call so environment
    /// changes after start still apply.
    pub(crate) roots_override: Option<NativeRoots>,
    /// The Codex, Grok and Devin CLIs.
    pub(crate) clis: NativeClis,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionView<'a> {
    id: &'a str,
    title: Option<&'a str>,
    cwd: Option<&'a str>,
    created_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
    bytes: Option<u64>,
    referenced_by: Vec<&'a str>,
    running: bool,
}

/// What keeps sessions from being deleted.
struct Protection {
    refs: HashMap<String, Vec<NativeSessionRef>>,
    running_native: HashSet<String>,
    now: SystemTime,
}

impl Protection {
    fn referenced(&self, id: &str) -> bool {
        self.refs.contains_key(id)
    }

    fn running(&self, session: &NativeSession) -> bool {
        session.locked
            || self.running_native.contains(&session.id)
            || self
                .refs
                .get(&session.id)
                .is_some_and(|refs| refs.iter().any(|reference| reference.active))
            || recently_written(session.modified, self.now)
    }

    fn skip(&self, session: &NativeSession) -> Option<AppError> {
        if self.referenced(&session.id) {
            Some(AppError::Conflict(
                "a TodeX conversation uses this session".to_owned(),
            ))
        } else if self.running(session) {
            Some(AppError::Conflict("session is running".to_owned()))
        } else {
            None
        }
    }
}

fn recently_written(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .map_or(true, |age| age < RECENT_WRITE)
}

impl NativeHistory {
    pub fn new(data_dir: &Path, history_cleanup: &HistoryCleanup, clis: NativeClis) -> Self {
        Self {
            cache: Arc::default(),
            cleanup: history_cleanup.sibling(data_dir.join(NATIVE_HISTORY_CLEANUP_FILE)),
            locks: Arc::default(),
            roots_override: None,
            clis,
        }
    }

    fn provider_lock(&self, provider: NativeProvider) -> &tokio::sync::Mutex<()> {
        let index = match provider {
            NativeProvider::ClaudeCode => 0,
            NativeProvider::Pi => 1,
            NativeProvider::Codex => 2,
            NativeProvider::GrokBuild => 3,
            NativeProvider::Devin => 4,
        };
        &self.locks[index]
    }

    pub fn roots(&self) -> NativeRoots {
        self.roots_override
            .clone()
            .unwrap_or_else(NativeRoots::detect)
    }

    /// The provider's sessions. A missing store (or, for Devin, a missing
    /// or failing CLI) is an empty, unavailable listing, not an error.
    async fn scan(
        &self,
        roots: &NativeRoots,
        provider: NativeProvider,
    ) -> Result<Listing, AppError> {
        if provider == NativeProvider::Devin {
            return Ok(self.scan_devin(roots).await);
        }
        let available = roots.store_exists(provider);
        let roots = roots.clone();
        let cache = self.cache.clone();
        let sessions = tokio::task::spawn_blocking(move || scan(&roots, provider, &cache))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))?
            .map_err(AppError::from)?;
        Ok(Listing {
            sessions,
            available,
            storage_bytes: 0,
        })
    }

    async fn scan_devin(&self, roots: &NativeRoots) -> Listing {
        let dir = roots.devin_dir.clone();
        let storage_bytes = [DEVIN_SESSION_DB, "sessions.db-wal", "sessions.db-shm"]
            .iter()
            .filter_map(|name| fs::symlink_metadata(dir.join(name)).ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .fold(0, u64::saturating_add);
        if !roots.store_exists(NativeProvider::Devin)
            || !crate::provider::process::executable_available(&self.clis.devin)
        {
            return Listing {
                storage_bytes,
                ..Listing::default()
            };
        }
        match cli::devin_sessions(&self.clis, &dir).await {
            Ok(items) => Listing {
                sessions: items.iter().filter_map(devin_session).collect(),
                available: true,
                storage_bytes,
            },
            Err(error) => {
                tracing::warn!(%error, "could not list Devin sessions");
                Listing {
                    storage_bytes,
                    ..Listing::default()
                }
            }
        }
    }

    async fn protection(
        &self,
        conversations: &ConversationSupervisor,
        roots: &NativeRoots,
        provider: NativeProvider,
    ) -> Result<Protection, AppError> {
        let mut refs: HashMap<String, Vec<NativeSessionRef>> = HashMap::new();
        for reference in conversations.native_session_refs(provider.kind()).await? {
            refs.entry(reference.native_session_id.clone())
                .or_default()
                .push(reference);
        }
        let running_native = match provider {
            NativeProvider::ClaudeCode => {
                let claude_dir = roots.claude_dir.clone();
                tokio::task::spawn_blocking(move || claude_running_sessions(&claude_dir))
                    .await
                    .map_err(|error| AppError::Anyhow(error.into()))?
            }
            _ => HashSet::new(),
        };
        Ok(Protection {
            refs,
            running_native,
            now: SystemTime::now(),
        })
    }

    /// `GET /v2/native-history/providers`.
    pub async fn providers(&self) -> Result<Value, AppError> {
        let roots = self.roots();
        let mut providers = Vec::new();
        for provider in NativeProvider::ALL {
            let root = roots.sessions_root(provider);
            let listing = self.scan(&roots, provider).await?;
            let native = match provider {
                NativeProvider::ClaudeCode => {
                    let path = retention::settings_path(&roots.claude_dir);
                    json!({
                        "cleanupPeriodDays": retention::read(&path).ok().flatten(),
                        "settingsPath": path,
                    })
                }
                _ => Value::Null,
            };
            providers.push(json!({
                "provider": provider.as_str(),
                "available": listing.available,
                "root": root,
                "sessionCount": listing.sessions.len(),
                "totalBytes": listing.total_bytes(provider),
                "measuredAt": Utc::now(),
                "capabilities": provider.capabilities(),
                "native": native,
            }));
        }
        Ok(json!({ "providers": providers }))
    }

    /// `GET /v2/native-history/{provider}/sessions`; `referencedBy` lists
    /// the tenant's own conversations only.
    pub async fn sessions(
        &self,
        conversations: &ConversationSupervisor,
        provider: NativeProvider,
        tenant_id: &str,
    ) -> Result<Value, AppError> {
        let roots = self.roots();
        let mut listing = self.scan(&roots, provider).await?;
        let protection = self.protection(conversations, &roots, provider).await?;
        let total_bytes = listing.total_bytes(provider);
        let sessions = &mut listing.sessions;
        sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.id.cmp(&b.id)));
        let views: Vec<SessionView> = sessions
            .iter()
            .map(|session| SessionView {
                id: &session.id,
                title: session.meta.title.as_deref(),
                cwd: session.meta.cwd.as_deref(),
                created_at: session.meta.created_at,
                updated_at: DateTime::<Utc>::from(session.modified),
                bytes: session.bytes,
                referenced_by: protection
                    .refs
                    .get(&session.id)
                    .into_iter()
                    .flatten()
                    .filter(|reference| reference.owner_id == tenant_id)
                    .map(|reference| reference.conversation_id.as_str())
                    .collect(),
                running: protection.running(session),
            })
            .collect();
        Ok(json!({
            "provider": provider.as_str(),
            "root": roots.sessions_root(provider),
            "totalBytes": total_bytes,
            "count": sessions.len(),
            "measuredAt": Utc::now(),
            "sessions": views,
        }))
    }

    /// `GET /v2/native-history/{provider}/sessions/{id}/transcript`: the last
    /// `limit` displayable messages, oldest first.
    pub async fn transcript(
        &self,
        provider: NativeProvider,
        id: &str,
        limit: usize,
    ) -> Result<Value, AppError> {
        validate_session_id(id)?;
        if !provider.capabilities().view {
            return Err(AppError::Unsupported(format!(
                "{} sessions have no transcript view",
                provider.as_str()
            )));
        }
        let roots = self.roots();
        let session = self
            .scan(&roots, provider)
            .await?
            .sessions
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| AppError::NotFound(format!("native session {id} not found")))?;
        let path = match &session.dir {
            // Grok: the ACP update log is the conversation.
            Some(dir) => Some(dir.join(GROK_UPDATES_FILE)).filter(|path| path.is_file()),
            // The newest file when one id has several.
            None => session
                .files
                .iter()
                .max_by_key(|path| modified(path))
                .cloned(),
        };
        let path =
            path.ok_or_else(|| AppError::NotFound(format!("native session {id} not found")))?;
        let (total, messages) =
            tokio::task::spawn_blocking(move || read_transcript(provider, &path, limit))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(json!({
            "provider": provider.as_str(),
            "id": session.id,
            "title": session.meta.title,
            "cwd": session.meta.cwd,
            "totalMessages": total,
            "truncated": total > messages.len(),
            "messages": messages,
        }))
    }

    /// `POST /v2/native-history/{provider}/cleanup`: deletes the named
    /// sessions; referenced, running and unknown ones are skipped. The ids
    /// must already be validated and deduplicated.
    pub async fn cleanup(
        &self,
        conversations: &ConversationSupervisor,
        provider: NativeProvider,
        ids: &[String],
    ) -> Result<CleanupReport, AppError> {
        let _guard = self.provider_lock(provider).lock().await;
        let roots = self.roots();
        let sessions = self.scan(&roots, provider).await?.sessions;
        let protection = self.protection(conversations, &roots, provider).await?;
        let mut report = CleanupReport::default();
        for id in ids {
            let Some(session) = sessions.iter().find(|session| &session.id == id) else {
                report.skipped.push(CleanupSkip::new(
                    id,
                    &AppError::NotFound(format!("native session {id} not found")),
                ));
                continue;
            };
            if let Some(error) = protection.skip(session) {
                report.skipped.push(CleanupSkip::new(id, &error));
                continue;
            }
            self.delete(&roots, provider, session, "manual", &mut report)
                .await;
        }
        Ok(report)
    }

    /// Applies policy thresholds to the provider's sessions (age rule, then
    /// capacity rule oldest first). Referenced and running sessions count
    /// toward the total but are never deleted.
    pub async fn apply_policy(
        &self,
        conversations: &ConversationSupervisor,
        provider: NativeProvider,
        thresholds: CleanupThresholds,
        now: DateTime<Utc>,
        trigger: CleanupTrigger,
    ) -> Result<CleanupReport, AppError> {
        let _guard = self.provider_lock(provider).lock().await;
        let roots = self.roots();
        let sessions = self.scan(&roots, provider).await?.sessions;
        let protection = self.protection(conversations, &roots, provider).await?;
        let mut thresholds = thresholds;
        if !provider.capabilities().sizes {
            // No sizes, no capacity rule (the API refuses to save one).
            thresholds.max_total_bytes = None;
        }
        let candidates: Vec<PolicyCandidate> = sessions
            .iter()
            .map(|session| PolicyCandidate {
                id: session.id.clone(),
                updated_at: DateTime::<Utc>::from(session.modified),
                bytes: session.bytes.unwrap_or(0),
                protected: protection.skip(session).is_some(),
            })
            .collect();
        let age_cutoff = thresholds
            .max_age_days
            .map(|days| now - chrono::Duration::days(i64::from(days)));
        let trigger = match trigger {
            CleanupTrigger::Auto => "policy-auto",
            CleanupTrigger::Manual => "policy-manual",
        };
        let mut report = CleanupReport::default();
        for (id, rule) in plan_policy_deletions(&candidates, thresholds, now) {
            let Some(session) = sessions.iter().find(|session| session.id == id) else {
                continue;
            };
            // The session must not have been written since the scan. Devin
            // has no files to check; `devin rm` refuses an open session.
            let current = if session.files.is_empty() {
                Some(session.modified)
            } else {
                session.files.iter().filter_map(|path| modified(path)).max()
            };
            let unchanged = current.is_some_and(|current| current <= session.modified);
            let still_old = match (rule, age_cutoff) {
                (PolicyRule::Age, Some(cutoff)) => {
                    current.is_some_and(|current| DateTime::<Utc>::from(current) < cutoff)
                }
                _ => true,
            };
            if !unchanged || !still_old {
                report.skipped.push(CleanupSkip::new(
                    &id,
                    &AppError::Conflict("session changed during cleanup".to_owned()),
                ));
                continue;
            }
            self.delete(&roots, provider, session, trigger, &mut report)
                .await;
        }
        Ok(report)
    }

    async fn delete(
        &self,
        roots: &NativeRoots,
        provider: NativeProvider,
        session: &NativeSession,
        trigger: &'static str,
        report: &mut CleanupReport,
    ) {
        let outcome = if provider.cli_deletes() {
            let spec = match provider {
                NativeProvider::Codex => self.clis.codex_delete(&roots.codex_home, &session.id),
                NativeProvider::GrokBuild => self.clis.grok_delete(&roots.grok_home, &session.id),
                _ => self.clis.devin_delete(&roots.devin_dir, &session.id),
            };
            cli::run_delete(provider, &spec).await
        } else {
            let sessions_root = roots.sessions_root(provider);
            let side_root = roots.side_root(provider).to_owned();
            let target = session.clone();
            tokio::task::spawn_blocking(move || remove_session(&sessions_root, &side_root, &target))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))
                .and_then(|result| result.map_err(AppError::from))
        };
        let freed = session.bytes.unwrap_or(0);
        match outcome {
            Ok(()) => {
                tracing::info!(
                    provider = provider.as_str(),
                    session_id = %session.id,
                    files = session.files.len(),
                    side_entries = session.side.len(),
                    freed_bytes = freed,
                    trigger,
                    "native history session deleted"
                );
                report.freed_bytes = report.freed_bytes.saturating_add(freed);
                report.deleted.push(session.id.clone());
            }
            Err(error) => {
                tracing::warn!(
                    provider = provider.as_str(),
                    session_id = %session.id,
                    error = %error,
                    "native history session could not be deleted"
                );
                report.skipped.push(CleanupSkip::new(&session.id, &error));
            }
        }
    }

    /// `GET /v2/native-history/{provider}/native-retention` (Claude only).
    pub async fn native_retention(&self, provider: NativeProvider) -> Result<Value, AppError> {
        let path = retention_path(&self.roots(), provider)?;
        let days = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || retention::read(&path))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??
        };
        Ok(json!({ "cleanupPeriodDays": days, "settingsPath": path }))
    }

    /// `PUT /v2/native-history/{provider}/native-retention` (Claude only).
    pub async fn set_native_retention(
        &self,
        provider: NativeProvider,
        body: &Value,
    ) -> Result<Value, AppError> {
        let path = retention_path(&self.roots(), provider)?;
        let days = retention::parse_request(body)?;
        let _guard = self.provider_lock(provider).lock().await;
        let saved = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || retention::write(&path, days))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??
        };
        tracing::info!(
            settings_path = %path.display(),
            cleanup_period_days = ?days,
            "Claude Code cleanupPeriodDays updated"
        );
        Ok(json!({ "cleanupPeriodDays": saved, "settingsPath": path }))
    }
}

fn retention_path(roots: &NativeRoots, provider: NativeProvider) -> Result<PathBuf, AppError> {
    match provider {
        NativeProvider::ClaudeCode => Ok(retention::settings_path(&roots.claude_dir)),
        other => Err(AppError::InvalidRequest(format!(
            "{} has no native retention setting",
            other.as_str()
        ))),
    }
}

/// Applies `policy` to the provider's sessions now and records the outcome
/// as its `lastRun`. The caller makes sure the policy has a threshold.
pub async fn run_policy(
    native: &NativeHistory,
    conversations: &ConversationSupervisor,
    provider: NativeProvider,
    policy: CleanupPolicy,
    trigger: CleanupTrigger,
) -> Result<(CleanupReport, CleanupRun), AppError> {
    let at = Utc::now();
    let outcome = native
        .apply_policy(conversations, provider, policy.thresholds(), at, trigger)
        .await;
    let run = match &outcome {
        Ok(report) => CleanupRun {
            at,
            trigger,
            deleted: report.deleted.len(),
            freed_bytes: report.freed_bytes,
            error: None,
        },
        Err(error) => CleanupRun {
            at,
            trigger,
            deleted: 0,
            freed_bytes: 0,
            error: Some(error.to_string()),
        },
    };
    if let Err(error) = native
        .cleanup
        .record_run(provider.as_str(), run.clone())
        .await
    {
        tracing::warn!(
            provider = provider.as_str(),
            error = %error,
            "could not record the native history cleanup run"
        );
    }
    outcome.map(|report| (report, run))
}

/// The scheduler's share of a pass: every enabled native-history policy.
pub(crate) async fn run_enabled(state: &AppState) {
    for (key, policy) in state.native_history.cleanup.enabled_policies().await {
        let Ok(provider) = NativeProvider::parse(&key) else {
            continue;
        };
        match run_policy(
            &state.native_history,
            &state.conversations,
            provider,
            policy,
            CleanupTrigger::Auto,
        )
        .await
        {
            Ok((report, _)) if !report.deleted.is_empty() || !report.skipped.is_empty() => {
                tracing::info!(
                    provider = provider.as_str(),
                    deleted = report.deleted.len(),
                    skipped = report.skipped.len(),
                    freed_bytes = report.freed_bytes,
                    "native history cleanup policy removed sessions"
                )
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                provider = provider.as_str(),
                error = %error,
                "native history cleanup policy run failed"
            ),
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok()?.modified().ok()
}

/// Entries of `dir` (empty when it is missing or unreadable).
fn entries(dir: &Path) -> Vec<fs::DirEntry> {
    fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// A real directory (not a symlink to one).
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The provider's sessions, grouped by id. Listing metadata comes from the
/// cache when a file's size and mtime are unchanged.
fn scan(
    roots: &NativeRoots,
    provider: NativeProvider,
    cache: &std::sync::Mutex<MetaCache>,
) -> io::Result<Vec<NativeSession>> {
    if provider == NativeProvider::GrokBuild {
        return Ok(scan_grok(&roots.sessions_root(provider)));
    }
    let root = roots.sessions_root(provider);
    // (id from the file name for Claude, path, size, mtime)
    let mut files: Vec<(Option<String>, PathBuf, u64, SystemTime)> = Vec::new();
    let mut push_file = |id: Option<String>, path: PathBuf| {
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.is_file() {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((id, path, metadata.len(), modified));
            }
        }
    };
    match provider {
        NativeProvider::ClaudeCode => {
            for project in entries(&root) {
                if !is_real_dir(&project.path()) {
                    continue;
                }
                for entry in entries(&project.path()) {
                    let name = entry.file_name();
                    let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".jsonl"))
                    else {
                        continue;
                    };
                    if uuid::Uuid::parse_str(stem).is_ok() {
                        push_file(Some(stem.to_owned()), entry.path());
                    }
                }
            }
        }
        NativeProvider::Pi => {
            let is_jsonl = |entry: &fs::DirEntry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".jsonl"))
            };
            for entry in entries(&root) {
                let path = entry.path();
                if is_real_dir(&path) {
                    for nested in entries(&path) {
                        if is_jsonl(&nested) {
                            push_file(None, nested.path());
                        }
                    }
                } else if is_jsonl(&entry) {
                    push_file(None, path);
                }
            }
        }
        NativeProvider::Codex => {
            // `sessions/YYYY/MM/DD/` and the flat `archived_sessions/`.
            for dir in [root.clone(), roots.codex_home.join(CODEX_ARCHIVED_DIR)] {
                for path in codex_rollouts(&dir) {
                    let id = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(codex_rollout_id);
                    if id.is_some() {
                        push_file(id, path);
                    }
                }
            }
        }
        NativeProvider::GrokBuild | NativeProvider::Devin => {}
    }

    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut seen = HashSet::new();
    let mut groups: BTreeMap<String, Vec<(PathBuf, u64, SystemTime, SessionMeta)>> =
        BTreeMap::new();
    for (file_id, path, size, modified) in files {
        let key = (provider, path.clone());
        seen.insert(path.clone());
        let meta = match cache.get(&key) {
            Some(hit) if hit.size == size && hit.modified == modified => hit.meta.clone(),
            _ => {
                let meta = match read_head_tail(&path, size) {
                    Ok((head, tail)) => parse::session_meta(provider, &head, &tail),
                    Err(error) => {
                        tracing::debug!(path = %path.display(), %error, "could not read native session");
                        // Claude and Codex files are sessions by name; Pi
                        // needs a header.
                        (provider != NativeProvider::Pi).then(SessionMeta::default)
                    }
                };
                cache.insert(
                    key,
                    CachedMeta {
                        size,
                        modified,
                        meta: meta.clone(),
                    },
                );
                meta
            }
        };
        let Some(meta) = meta else { continue };
        let Some(id) = file_id.or_else(|| meta.id.clone()) else {
            continue;
        };
        if validate_session_id(&id).is_err() {
            continue;
        }
        groups
            .entry(id)
            .or_default()
            .push((path, size, modified, meta));
    }
    cache.retain(|(cached_provider, path), _| *cached_provider != provider || seen.contains(path));
    drop(cache);

    let todos: Vec<String> = match provider {
        NativeProvider::ClaudeCode => entries(&roots.claude_dir.join("todos"))
            .into_iter()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        _ => Vec::new(),
    };
    let names = if provider == NativeProvider::Codex {
        codex_thread_names(&roots.codex_home.join(CODEX_SESSION_INDEX))
    } else {
        HashMap::new()
    };
    let mut sessions = Vec::with_capacity(groups.len());
    for (id, mut group) in groups {
        group.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        let modified = group[0].2;
        let mut meta = group[0].3.clone();
        if let Some(name) = names.get(&id) {
            meta.title = Some(name.clone());
        }
        let files: Vec<PathBuf> = group.iter().map(|(path, ..)| path.clone()).collect();
        let side = match provider {
            NativeProvider::ClaudeCode => {
                claude_side_entries(&roots.claude_dir, &id, &files, &todos)
            }
            _ => Vec::new(),
        };
        let bytes = group
            .iter()
            .map(|(_, size, ..)| *size)
            .chain(side.iter().map(|path| tree_bytes(path)))
            .fold(0, u64::saturating_add);
        sessions.push(NativeSession {
            id,
            files,
            side,
            dir: None,
            bytes: Some(bytes),
            modified,
            meta,
            locked: false,
        });
    }
    Ok(sessions)
}

/// The thread id at the end of a Codex rollout file name,
/// `rollout-<timestamp>-<uuid>.jsonl`.
fn codex_rollout_id(name: &str) -> Option<String> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let start = stem.len().checked_sub(36)?;
    let id = stem.get(start..)?;
    uuid::Uuid::parse_str(id).ok()?;
    Some(id.to_owned())
}

/// `rollout-*.jsonl` files up to four directory levels under `dir`
/// (`YYYY/MM/DD`), never through symlinks.
fn codex_rollouts(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        for entry in entries(dir) {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && depth > 0 {
                walk(&path, depth - 1, out);
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 3, &mut out);
    out
}

/// Thread names from Codex's append-only `session_index.jsonl`
/// (`{"id","thread_name","updated_at"}`): the last entry for an id wins,
/// an empty name clears it.
fn codex_thread_names(path: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(file) = fs::File::open(path) else {
        return names;
    };
    let reader = io::BufReader::new(file.take(MAX_CODEX_INDEX_BYTES));
    for line in reader.lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = entry
            .get("thread_name")
            .and_then(Value::as_str)
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|name| !name.is_empty());
        match name {
            Some(name) => {
                names.insert(id.to_owned(), parse::cap(&name, parse::TITLE_CHARS));
            }
            None => {
                names.remove(id);
            }
        }
    }
    names
}

/// Grok Build sessions: `<root>/<encoded-cwd>/<session-id>/summary.json`,
/// Grok's own on-disk index. The directory's whole size is the session's.
fn scan_grok(root: &Path) -> Vec<NativeSession> {
    let mut sessions = Vec::new();
    for group in entries(root) {
        if !is_real_dir(&group.path()) {
            continue;
        }
        for entry in entries(&group.path()) {
            let dir = entry.path();
            if !is_real_dir(&dir) {
                continue;
            }
            let summary_path = dir.join(GROK_SUMMARY_FILE);
            let Some(summary) = read_json_file(&summary_path, MAX_GROK_SUMMARY_BYTES) else {
                continue;
            };
            let Some(meta) = parse::grok_summary(&summary) else {
                continue;
            };
            let Some(id) = meta.id.clone() else { continue };
            if validate_session_id(&id).is_err() {
                continue;
            }
            let files: Vec<PathBuf> = [summary_path, dir.join(GROK_UPDATES_FILE)]
                .into_iter()
                .filter(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()))
                .collect();
            let modified = files
                .iter()
                .filter_map(|path| modified(path))
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            sessions.push(NativeSession {
                id,
                files,
                side: Vec::new(),
                bytes: Some(tree_bytes(&dir)),
                dir: Some(dir),
                modified,
                meta,
                locked: false,
            });
        }
    }
    sessions
}

fn read_json_file(path: &Path, max_bytes: u64) -> Option<Value> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// A Devin `session/list` entry; one without a valid id is dropped, one
/// without `updatedAt` counts as just written (so it is protected).
fn devin_session(item: &Value) -> Option<NativeSession> {
    let id = item.get("sessionId").and_then(Value::as_str)?.to_owned();
    validate_session_id(&id).ok()?;
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let time = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|at| at.with_timezone(&Utc))
    };
    let modified = time(item.get("updatedAt"))
        .map(SystemTime::from)
        .unwrap_or_else(SystemTime::now);
    let title = text(item.get("title"))
        .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|title| parse::cap(&title, parse::TITLE_CHARS));
    Some(NativeSession {
        meta: SessionMeta {
            id: Some(id.clone()),
            title,
            cwd: text(item.get("cwd")),
            created_at: time(item.pointer("/_meta/cognition.ai~1createdAt")),
        },
        locked: item
            .pointer("/_meta/cognition.ai~1isLocked")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        id,
        files: Vec::new(),
        side: Vec::new(),
        dir: None,
        bytes: None,
        modified,
    })
}

/// Claude's per-session side data that exists and is strictly keyed by the
/// session id: the transcript's sibling `<id>/` directory (subagents, tool
/// results), `session-env/<id>/`, `file-history/<id>/`, `debug/<id>.txt`
/// and `todos/<id>-*.json`. Symlinks are left out.
fn claude_side_entries(
    claude_dir: &Path,
    id: &str,
    transcripts: &[PathBuf],
    todos: &[String],
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = transcripts
        .iter()
        .filter_map(|path| path.parent().map(|parent| parent.join(id)))
        .collect();
    candidates.push(claude_dir.join("session-env").join(id));
    candidates.push(claude_dir.join("file-history").join(id));
    candidates.push(claude_dir.join("debug").join(format!("{id}.txt")));
    let prefix = format!("{id}-");
    candidates.extend(
        todos
            .iter()
            .filter(|name| name.starts_with(&prefix) && name.ends_with(".json"))
            .map(|name| claude_dir.join("todos").join(name)),
    );
    candidates
        .into_iter()
        .filter(|path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.file_type().is_symlink())
        })
        .collect()
}

/// Bytes under `path`, never following symlinks.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_dir() {
        entries(path)
            .iter()
            .map(|entry| tree_bytes(&entry.path()))
            .fold(0, u64::saturating_add)
    } else if metadata.is_file() {
        metadata.len()
    } else {
        0
    }
}

/// Whole lines from the first and last [`HEAD_TAIL_BYTES`] of a file; a
/// small file is returned whole as `head`. Partial lines at the cut points
/// are dropped.
fn read_head_tail(path: &Path, size: u64) -> io::Result<(Vec<String>, Vec<String>)> {
    let mut file = fs::File::open(path)?;
    let lines = |bytes: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    };
    if size <= 2 * HEAD_TAIL_BYTES {
        let mut bytes = Vec::new();
        file.by_ref()
            .take(2 * HEAD_TAIL_BYTES)
            .read_to_end(&mut bytes)?;
        return Ok((lines(&bytes), Vec::new()));
    }
    let mut head = vec![0; HEAD_TAIL_BYTES as usize];
    file.read_exact(&mut head)?;
    let head_end = head.iter().rposition(|byte| *byte == b'\n').unwrap_or(0);
    file.seek(SeekFrom::Start(size - HEAD_TAIL_BYTES))?;
    let mut tail = Vec::new();
    file.take(HEAD_TAIL_BYTES).read_to_end(&mut tail)?;
    let tail_start = tail
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(tail.len(), |index| index + 1);
    Ok((lines(&head[..head_end]), lines(&tail[tail_start..])))
}

/// Streams a session file; returns the number of displayable messages and
/// the last `limit` of them.
fn read_transcript(
    provider: NativeProvider,
    path: &Path,
    limit: usize,
) -> io::Result<(usize, Vec<TranscriptMessage>)> {
    let mut reader = io::BufReader::new(fs::File::open(path)?);
    let mut ring = std::collections::VecDeque::with_capacity(limit);
    let mut total = 0usize;
    let mut line = Vec::new();
    let mut scratch = Vec::new();
    let mut grok = (provider == NativeProvider::GrokBuild).then(parse::GrokTranscript::default);
    let mut keep = |scratch: &mut Vec<TranscriptMessage>| {
        for message in scratch.drain(..) {
            total += 1;
            if ring.len() == limit {
                ring.pop_front();
            }
            ring.push_back(message);
        }
    };
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match grok.as_mut() {
            Some(grok) => grok.push_record(&record, &mut scratch),
            None => parse::record_messages(provider, &record, &mut scratch),
        }
        keep(&mut scratch);
    }
    if let Some(grok) = grok.as_mut() {
        grok.flush(&mut scratch);
        keep(&mut scratch);
    }
    Ok((total, ring.into()))
}

/// Ids of Claude sessions a live process has open: `sessions/<pid>.json`
/// markers naming a session whose pid is still alive.
fn claude_running_sessions(claude_dir: &Path) -> HashSet<String> {
    let mut running = HashSet::new();
    for entry in entries(&claude_dir.join("sessions")) {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file.take(MAX_MARKER_BYTES).read_to_end(&mut bytes).is_err() {
            continue;
        }
        let Ok(marker) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let (Some(pid), Some(session_id)) = (
            marker.get("pid").and_then(Value::as_i64),
            marker.get("sessionId").and_then(Value::as_str),
        ) else {
            continue;
        };
        if pid_alive(pid) {
            running.insert(session_id.to_owned());
        }
    }
    running
}

fn pid_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        // No portable check: treat the session as running to be safe.
        true
    }
}

/// Removes a session's transcript files, then its side data. Each path must
/// resolve (through its canonical parent) inside its root; symlinks are
/// refused.
fn remove_session(
    sessions_root: &Path,
    side_root: &Path,
    session: &NativeSession,
) -> io::Result<()> {
    let sessions_root = fs::canonicalize(sessions_root)?;
    for path in &session.files {
        remove_inside(&sessions_root, path)?;
    }
    if session.side.is_empty() {
        return Ok(());
    }
    // The transcript is gone: side data is best effort from here on.
    let side_root = match fs::canonicalize(side_root) {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(%error, "could not resolve the native session side-data root");
            return Ok(());
        }
    };
    for path in &session.side {
        if let Err(error) = remove_inside(&side_root, path) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove native session side data"
            );
        }
    }
    Ok(())
}

fn remove_inside(root: &Path, path: &Path) -> io::Result<()> {
    let outside = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is outside the provider root", path.display()),
        )
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(outside());
    };
    let parent = fs::canonicalize(parent)?;
    if !parent.starts_with(root) {
        return Err(outside());
    }
    let target = parent.join(name);
    if target == root {
        return Err(outside());
    }
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symlink", path.display()),
        ));
    }
    if metadata.is_dir() {
        // `remove_dir_all` removes symlinks inside without following them.
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    }
}

#[cfg(test)]
mod tests;
