//! Native session history of Claude Code and Pi: the agents' own session
//! files, outside TodeX's conversation store. Lists them with their disk
//! usage, shows a bounded transcript, deletes chosen sessions, and applies a
//! TodeX-managed age / capacity policy per provider (saved in
//! `$data_dir/native-history-cleanup.json`, run by the history cleanup
//! scheduler in [`crate::history_cleanup`]).
//!
//! Safety: a session a TodeX conversation (of any tenant) resumes, a running
//! session, and a session written in the last two minutes are never
//! deleted. Ids are validated and resolved through a fresh scan; every
//! removed path must lie inside the provider's root, and symlinks are never
//! followed or removed.

mod parse;
mod retention;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NativeProvider {
    ClaudeCode,
    Pi,
}

impl NativeProvider {
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::Pi];

    /// `claude-code` or `pi`; anything else is `INVALID_REQUEST`.
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "claude-code" => Ok(Self::ClaudeCode),
            "pi" => Ok(Self::Pi),
            other => Err(AppError::InvalidRequest(format!(
                "native history supports claude-code and pi, not {other}"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Pi => "pi",
        }
    }

    const fn kind(self) -> ProviderKind {
        match self {
            Self::ClaudeCode => ProviderKind::ClaudeCode,
            Self::Pi => ProviderKind::Pi,
        }
    }
}

/// Where each provider keeps its sessions.
#[derive(Clone, Debug)]
pub struct NativeRoots {
    pub claude_dir: PathBuf,
    pub pi_session_dir: PathBuf,
}

impl NativeRoots {
    pub fn detect() -> Self {
        let dirs = AgentDirs::detect();
        Self {
            claude_dir: dirs.claude_dir().to_owned(),
            pi_session_dir: dirs.pi_session_dir(),
        }
    }

    /// The directory session files live in (reported as `root`).
    pub fn sessions_root(&self, provider: NativeProvider) -> PathBuf {
        match provider {
            NativeProvider::ClaudeCode => self.claude_dir.join("projects"),
            NativeProvider::Pi => self.pi_session_dir.clone(),
        }
    }

    /// The directory per-session side data lives in.
    fn side_root(&self, provider: NativeProvider) -> &Path {
        match provider {
            NativeProvider::ClaudeCode => &self.claude_dir,
            NativeProvider::Pi => &self.pi_session_dir,
        }
    }
}

/// Claude and Pi session ids are UUIDs; anything with characters outside
/// `[A-Za-z0-9._-]`, a leading dot or `..` is rejected before any lookup.
pub fn validate_session_id(id: &str) -> Result<(), AppError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_SESSION_ID_CHARS
        && !id.starts_with('.')
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
/// under two project directories — plus Claude's per-session side data.
#[derive(Clone, Debug)]
struct NativeSession {
    id: String,
    files: Vec<PathBuf>,
    side: Vec<PathBuf>,
    bytes: u64,
    modified: SystemTime,
    meta: SessionMeta,
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
    /// Serializes deletions (manual and policy) and settings edits.
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Fixed roots for tests; otherwise detected per call so environment
    /// changes after start still apply.
    pub(crate) roots_override: Option<NativeRoots>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionView<'a> {
    id: &'a str,
    title: Option<&'a str>,
    cwd: Option<&'a str>,
    created_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
    bytes: u64,
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
        self.running_native.contains(&session.id)
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
    pub fn new(data_dir: &Path, history_cleanup: &HistoryCleanup) -> Self {
        Self {
            cache: Arc::default(),
            cleanup: history_cleanup.sibling(data_dir.join(NATIVE_HISTORY_CLEANUP_FILE)),
            lock: Arc::default(),
            roots_override: None,
        }
    }

    pub fn roots(&self) -> NativeRoots {
        self.roots_override
            .clone()
            .unwrap_or_else(NativeRoots::detect)
    }

    async fn scan(
        &self,
        roots: &NativeRoots,
        provider: NativeProvider,
    ) -> Result<Vec<NativeSession>, AppError> {
        let roots = roots.clone();
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || scan(&roots, provider, &cache))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))?
            .map_err(AppError::from)
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
            NativeProvider::Pi => HashSet::new(),
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
            let sessions = self.scan(&roots, provider).await?;
            let native = match provider {
                NativeProvider::ClaudeCode => {
                    let path = retention::settings_path(&roots.claude_dir);
                    json!({
                        "cleanupPeriodDays": retention::read(&path).ok().flatten(),
                        "settingsPath": path,
                    })
                }
                NativeProvider::Pi => Value::Null,
            };
            providers.push(json!({
                "provider": provider.as_str(),
                "available": root.is_dir(),
                "root": root,
                "sessionCount": sessions.len(),
                "totalBytes": total_bytes(&sessions),
                "measuredAt": Utc::now(),
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
        let mut sessions = self.scan(&roots, provider).await?;
        let protection = self.protection(conversations, &roots, provider).await?;
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
            "totalBytes": total_bytes(&sessions),
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
        let roots = self.roots();
        let session = self
            .scan(&roots, provider)
            .await?
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| AppError::NotFound(format!("native session {id} not found")))?;
        // The newest file when one id has several.
        let path = session
            .files
            .iter()
            .max_by_key(|path| modified(path))
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("native session {id} not found")))?;
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
        let _guard = self.lock.lock().await;
        let roots = self.roots();
        let sessions = self.scan(&roots, provider).await?;
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
        let _guard = self.lock.lock().await;
        let roots = self.roots();
        let sessions = self.scan(&roots, provider).await?;
        let protection = self.protection(conversations, &roots, provider).await?;
        let candidates: Vec<PolicyCandidate> = sessions
            .iter()
            .map(|session| PolicyCandidate {
                id: session.id.clone(),
                updated_at: DateTime::<Utc>::from(session.modified),
                bytes: session.bytes,
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
            // The session must not have been written since the scan.
            let current = session.files.iter().filter_map(|path| modified(path)).max();
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
        let sessions_root = roots.sessions_root(provider);
        let side_root = roots.side_root(provider).to_owned();
        let target = session.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            remove_session(&sessions_root, &side_root, &target)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))
        .and_then(|result| result.map_err(AppError::from));
        match outcome {
            Ok(()) => {
                tracing::info!(
                    provider = provider.as_str(),
                    session_id = %session.id,
                    files = session.files.len(),
                    side_entries = session.side.len(),
                    freed_bytes = session.bytes,
                    trigger,
                    "native history session deleted"
                );
                report.freed_bytes = report.freed_bytes.saturating_add(session.bytes);
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
        let _guard = self.lock.lock().await;
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
        NativeProvider::Pi => Err(AppError::InvalidRequest(
            "pi has no native retention setting".to_owned(),
        )),
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

fn total_bytes(sessions: &[NativeSession]) -> u64 {
    sessions
        .iter()
        .map(|session| session.bytes)
        .fold(0, u64::saturating_add)
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
                        // Claude files are sessions by name; Pi needs a header.
                        (provider == NativeProvider::ClaudeCode).then(SessionMeta::default)
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
        NativeProvider::Pi => Vec::new(),
    };
    let mut sessions = Vec::with_capacity(groups.len());
    for (id, mut group) in groups {
        group.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        let modified = group[0].2;
        let meta = group[0].3.clone();
        let files: Vec<PathBuf> = group.iter().map(|(path, ..)| path.clone()).collect();
        let side = match provider {
            NativeProvider::ClaudeCode => {
                claude_side_entries(&roots.claude_dir, &id, &files, &todos)
            }
            NativeProvider::Pi => Vec::new(),
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
            bytes,
            modified,
            meta,
        });
    }
    Ok(sessions)
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
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        scratch.clear();
        parse::record_messages(provider, &record, &mut scratch);
        for message in scratch.drain(..) {
            total += 1;
            if ring.len() == limit {
                ring.pop_front();
            }
            ring.push_back(message);
        }
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
