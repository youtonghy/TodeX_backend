//! API keys for the external API listener (`[api]`, see docs/API.md).
//!
//! A key is `tdx_<id>_<secret>`: `id` is 16 hex digits, `secret` 32 random
//! bytes in base64url. `api-keys.json` keeps only a hash of the secret and
//! the public key of the history recipient derived from it
//! ([`history_crypto::api_key_recipient_seed`]), so the file never holds
//! anything that authenticates or decrypts. Like `devices.json` the file is
//! owner-only, replaced atomically and reloaded when its [`FileStamp`]
//! changes, so the CLI and the TUI manage keys while the daemon runs. Every
//! read-modify-write, in any process, holds the cross-process
//! `api-keys.json.lock`, so a revocation is never overwritten.
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::history_crypto::{self, RecipientPublicKey};
use crate::history_keys::OwnerRecipients;
use crate::secure_fs::{self, FileStamp};

type Result<T> = std::result::Result<T, AppError>;

const FILE_NAME: &str = "api-keys.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Active (unrevoked) keys.
pub(crate) const MAX_ACTIVE_KEYS: usize = 256;
/// Revoked keys stay listed for audit; the oldest go once this many exist.
const MAX_RECORDS: usize = 1024;
const KEY_PREFIX: &str = "tdx_";
const ID_HEX_LEN: usize = 16;
const SECRET_LEN: usize = 32;
const SECRET_TEXT_LEN: usize = 43;
const MAX_NAME_CHARS: usize = 100;
const MAX_SCOPE_WORKSPACES: usize = 64;
/// Owner id prefix of conversations created with an API key.
pub(crate) const OWNER_PREFIX: &str = "apikey:";
const AUTH_HASH_LABEL: &[u8] = b"todex-apikey-v1/auth\0";
const USE_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ApprovalPolicy {
    /// Permission requests go to the caller as events.
    #[default]
    Ask,
    /// Answered `allow_once` at once (device-bound requests excepted).
    AutoApprove,
    /// Answered `reject_once` at once.
    Reject,
}

impl ApprovalPolicy {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "ask" => Some(Self::Ask),
            "auto-approve" => Some(Self::AutoApprove),
            "reject" => Some(Self::Reject),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoApprove => "auto-approve",
            Self::Reject => "reject",
        }
    }
}

/// What a key may reach. `None` means every agent / every workspace root.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApiKeyScopes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<ProviderKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<PathBuf>>,
}

impl ApiKeyScopes {
    pub(crate) fn allows_agent(&self, agent: ProviderKind) -> bool {
        self.agents
            .as_ref()
            .is_none_or(|agents| agents.contains(&agent))
    }

    /// Whether the (canonical) `workspace` is inside the scope.
    pub(crate) fn allows_workspace(&self, workspace: &Path) -> bool {
        match &self.workspaces {
            None => true,
            Some(scopes) => scopes.iter().any(|scope| {
                let scope = fs::canonicalize(scope).unwrap_or_else(|_| scope.clone());
                workspace.starts_with(scope)
            }),
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(agents) = &self.agents {
            if agents.is_empty() {
                return Err(invalid("scopes.agents must name at least one agent"));
            }
        }
        if let Some(workspaces) = &self.workspaces {
            if workspaces.is_empty() || workspaces.len() > MAX_SCOPE_WORKSPACES {
                return Err(invalid(&format!(
                    "scopes.workspaces must list 1 to {MAX_SCOPE_WORKSPACES} paths"
                )));
            }
            if workspaces.iter().any(|path| !path.is_absolute()) {
                return Err(invalid("scopes.workspaces must be absolute paths"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApiKeyRecord {
    pub id: String,
    pub name: String,
    /// base64url SHA-256 of the auth label and the secret.
    pub secret_hash: String,
    /// base64url X-Wing public key of the key's history recipient.
    pub history_public_key: String,
    #[serde(default)]
    pub scopes: ApiKeyScopes,
    #[serde(default)]
    pub approval: ApprovalPolicy,
    /// Unix milliseconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<u64>,
}

impl ApiKeyRecord {
    pub(crate) fn owner_id(&self) -> String {
        format!("{OWNER_PREFIX}{}", self.id)
    }

    pub(crate) fn prefix(&self) -> String {
        format!("{KEY_PREFIX}{}", self.id)
    }

    pub(crate) fn is_active(&self, now: u64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expires| now < expires)
    }

    pub(crate) fn status(&self, now: u64) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else if !self.is_active(now) {
            "expired"
        } else {
            "active"
        }
    }

    /// The public view: everything but the hashes and keys.
    pub(crate) fn summary(&self) -> serde_json::Value {
        let now = unix_ms();
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "prefix": self.prefix(),
            "status": self.status(now),
            "scopes": self.scopes,
            "approval": self.approval,
            "createdAt": self.created_at,
            "expiresAt": self.expires_at,
            "lastUsedAt": self.last_used_at,
            "revokedAt": self.revoked_at,
        })
    }
}

/// A new key's settings.
#[derive(Clone, Debug, Default)]
pub(crate) struct NewApiKey {
    pub name: String,
    pub scopes: ApiKeyScopes,
    pub approval: ApprovalPolicy,
    pub expires_at: Option<u64>,
}

/// A partial update; `None` fields stay as they are.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApiKeyUpdate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub scopes: Option<ApiKeyScopes>,
    #[serde(default)]
    pub approval: Option<ApprovalPolicy>,
    /// `Some(None)` clears the expiry.
    #[serde(default, with = "double_option")]
    pub expires_at: Option<Option<u64>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<u64>>, D::Error> {
        Option::<u64>::deserialize(deserializer).map(Some)
    }
}

/// A key that authenticated a request, with the history recipient seed its
/// secret derives (zeroized on drop). Lives only as long as the request.
pub(crate) struct AuthenticatedKey {
    pub record: ApiKeyRecord,
    pub seed: Zeroizing<[u8; 32]>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KeysFile {
    version: u8,
    keys: BTreeMap<String, ApiKeyRecord>,
}

struct Inner {
    path: PathBuf,
    keys: BTreeMap<String, ApiKeyRecord>,
    stamp: Option<FileStamp>,
    dirty: bool,
    last_flush: Instant,
}

/// Handle on `api-keys.json`. Every read reloads the file when another
/// process changed it; writes publish atomically.
#[derive(Clone)]
pub(crate) struct ApiKeyStore {
    inner: Arc<Mutex<Inner>>,
}

impl ApiKeyStore {
    pub(crate) fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(FILE_NAME);
        let (keys, stamp) = read_records(&path)?.unwrap_or_default();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                keys,
                stamp,
                dirty: false,
                last_flush: Instant::now(),
            })),
        })
    }

    /// Creates a key and returns its record and the full key text, which is
    /// never shown again.
    pub(crate) fn create(&self, new: NewApiKey) -> Result<(ApiKeyRecord, Zeroizing<String>)> {
        let name = validate_name(&new.name)?;
        new.scopes.validate()?;
        let mut id_bytes = [0u8; ID_HEX_LEN / 2];
        OsRng.fill_bytes(&mut id_bytes);
        let id = hex(&id_bytes);
        let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
        OsRng.fill_bytes(secret.as_mut());
        let seed = history_crypto::api_key_recipient_seed(secret.as_ref());
        let recipient = history_crypto::recipient_from_seed(&seed);
        let now = unix_ms();
        if new.expires_at.is_some_and(|expires| expires <= now) {
            return Err(invalid("expiresAt must be in the future"));
        }
        let record = ApiKeyRecord {
            id: id.clone(),
            name,
            secret_hash: secret_hash(secret.as_ref()),
            history_public_key: URL_SAFE_NO_PAD.encode(recipient.to_bytes()),
            scopes: new.scopes,
            approval: new.approval,
            created_at: now,
            expires_at: new.expires_at,
            last_used_at: None,
            revoked_at: None,
        };
        let mut inner = self.lock()?;
        let _file_lock = lock_file(&inner.path)?;
        inner.reload_if_changed()?;
        let active = inner.keys.values().filter(|key| key.is_active(now)).count();
        if active >= MAX_ACTIVE_KEYS {
            return Err(AppError::ResourceExhausted(
                "too many active API keys; revoke one first".to_owned(),
            ));
        }
        inner.keys.insert(id.clone(), record.clone());
        inner.prune_revoked();
        inner.write()?;
        let text = Zeroizing::new(format!(
            "{KEY_PREFIX}{id}_{}",
            URL_SAFE_NO_PAD.encode(secret.as_ref())
        ));
        Ok((record, text))
    }

    pub(crate) fn list(&self) -> Result<Vec<ApiKeyRecord>> {
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        inner.flush_if_due();
        let mut keys = inner.keys.values().cloned().collect::<Vec<_>>();
        keys.sort_by_key(|key| key.created_at);
        Ok(keys)
    }

    pub(crate) fn get(&self, id: &str) -> Result<Option<ApiKeyRecord>> {
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        inner.flush_if_due();
        Ok(inner.keys.get(id).cloned())
    }

    pub(crate) fn update(&self, id: &str, update: ApiKeyUpdate) -> Result<ApiKeyRecord> {
        let mut inner = self.lock()?;
        let _file_lock = lock_file(&inner.path)?;
        inner.reload_if_changed()?;
        let record = inner
            .keys
            .get_mut(id)
            .ok_or_else(|| AppError::NotFound("api key".to_owned()))?;
        if record.revoked_at.is_some() {
            return Err(AppError::Conflict("the API key is revoked".to_owned()));
        }
        let mut next = record.clone();
        if let Some(name) = update.name {
            next.name = validate_name(&name)?;
        }
        if let Some(scopes) = update.scopes {
            scopes.validate()?;
            next.scopes = scopes;
        }
        if let Some(approval) = update.approval {
            next.approval = approval;
        }
        if let Some(expires_at) = update.expires_at {
            next.expires_at = expires_at;
        }
        *record = next.clone();
        inner.write()?;
        Ok(next)
    }

    /// Marks the key revoked. `Ok(false)` when it does not exist or was
    /// already revoked.
    pub(crate) fn revoke(&self, id: &str) -> Result<bool> {
        let mut inner = self.lock()?;
        let _file_lock = lock_file(&inner.path)?;
        inner.reload_if_changed()?;
        let Some(record) = inner.keys.get_mut(id) else {
            return Ok(false);
        };
        if record.revoked_at.is_some() {
            return Ok(false);
        }
        record.revoked_at = Some(unix_ms());
        inner.write()?;
        Ok(true)
    }

    /// The active key `token` names, if its secret matches. Every failure is
    /// the same `Unauthenticated`.
    pub(crate) fn authenticate(&self, token: &str) -> Result<AuthenticatedKey> {
        let (id, secret) = parse_token(token).ok_or(AppError::Unauthenticated)?;
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        let now = unix_ms();
        let record = inner.keys.get_mut(id).ok_or(AppError::Unauthenticated)?;
        let expected = URL_SAFE_NO_PAD
            .decode(&record.secret_hash)
            .map_err(|_| AppError::Unauthenticated)?;
        let presented = Sha256::new()
            .chain_update(AUTH_HASH_LABEL)
            .chain_update(secret.as_slice())
            .finalize();
        if !bool::from(presented.as_slice().ct_eq(&expected)) || !record.is_active(now) {
            return Err(AppError::Unauthenticated);
        }
        record.last_used_at = Some(now);
        let record = record.clone();
        inner.dirty = true;
        inner.flush_if_due();
        Ok(AuthenticatedKey {
            record,
            seed: history_crypto::api_key_recipient_seed(secret.as_slice()),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| invalid("API key store is unavailable"))
    }
}

impl OwnerRecipients for ApiKeyStore {
    fn recipient_for_owner(&self, owner_id: &str) -> Option<RecipientPublicKey> {
        let id = owner_id.strip_prefix(OWNER_PREFIX)?;
        let record = self.get(id).ok().flatten()?;
        if !record.is_active(unix_ms()) {
            return None;
        }
        RecipientPublicKey::from_base64url(&record.history_public_key).ok()
    }
}

impl Inner {
    /// Reloads the file when another process changed it. Returns whether
    /// it did; an external writer (CLI, TUI) wins over unflushed lastUsedAt
    /// updates, which are best-effort.
    fn reload_if_changed(&mut self) -> Result<bool> {
        let stamp = FileStamp::read(&self.path).ok();
        if stamp == self.stamp {
            return Ok(false);
        }
        let (keys, stamp) = read_records(&self.path)?.unwrap_or_default();
        self.keys = keys;
        self.stamp = stamp;
        self.dirty = false;
        Ok(true)
    }

    /// Writes pending lastUsedAt updates once the flush interval passed,
    /// under the file lock and only when the file did not change meanwhile.
    /// A failure is logged and retried after another interval.
    fn flush_if_due(&mut self) {
        if !self.dirty || self.last_flush.elapsed() < USE_FLUSH_INTERVAL {
            return;
        }
        let result = lock_file(&self.path).and_then(|_file_lock| {
            if self.reload_if_changed()? {
                Ok(())
            } else {
                self.write()
            }
        });
        if let Err(error) = result {
            self.last_flush = Instant::now();
            tracing::warn!(error = %error, "failed to record API key use");
        }
    }

    /// Drops the oldest revoked records beyond [`MAX_RECORDS`].
    fn prune_revoked(&mut self) {
        while self.keys.len() > MAX_RECORDS {
            let Some(oldest) = self
                .keys
                .values()
                .filter(|key| key.revoked_at.is_some())
                .min_by_key(|key| key.revoked_at)
                .map(|key| key.id.clone())
            else {
                return;
            };
            self.keys.remove(&oldest);
        }
    }

    /// Publishes the in-memory keys. Callers hold [`lock_file`] and reloaded
    /// under it, so no external change is overwritten.
    fn write(&mut self) -> Result<()> {
        let stamp = write_records(
            &self.path,
            &KeysFile {
                version: 1,
                keys: self.keys.clone(),
            },
        )?;
        self.stamp = Some(stamp);
        self.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }
}

/// `(id, secret)` of a well-formed `tdx_<id>_<secret>`.
fn parse_token(token: &str) -> Option<(&str, Zeroizing<Vec<u8>>)> {
    let rest = token.trim().strip_prefix(KEY_PREFIX)?;
    let (id, secret) = rest.split_at_checked(ID_HEX_LEN)?;
    let secret = secret.strip_prefix('_')?;
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || secret.len() != SECRET_TEXT_LEN
    {
        return None;
    }
    let secret = Zeroizing::new(URL_SAFE_NO_PAD.decode(secret).ok()?);
    (secret.len() == SECRET_LEN).then_some((id, secret))
}

fn secret_hash(secret: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(
        Sha256::new()
            .chain_update(AUTH_HASH_LABEL)
            .chain_update(secret)
            .finalize(),
    )
}

fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(invalid(&format!(
            "API key name must be 1 to {MAX_NAME_CHARS} printable characters"
        )));
    }
    Ok(name.to_owned())
}

/// Takes the cross-process lock that serializes read-modify-write cycles of
/// the key file at `path`.
fn lock_file(path: &Path) -> Result<fs::File> {
    secure_fs::lock_exclusive(&secure_fs::lock_path(path), secure_fs::STATE_LOCK_TIMEOUT)
        .map_err(|error| invalid(&format!("API key file is locked: {error}")))
}

type RecordsSnapshot = (BTreeMap<String, ApiKeyRecord>, Option<FileStamp>);

fn read_records(path: &Path) -> Result<Option<RecordsSnapshot>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_FILE_BYTES {
        return Err(invalid("invalid API key file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid("API key file is not private"));
        }
    }
    let bytes = fs::read(path)?;
    let file: KeysFile = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(&format!("API key file is unreadable: {error}")))?;
    if file.version != 1 || file.keys.len() > MAX_RECORDS {
        return Err(invalid("unsupported API key file"));
    }
    for (id, record) in &file.keys {
        if record.id != *id
            || id.len() != ID_HEX_LEN
            || RecipientPublicKey::from_base64url(&record.history_public_key).is_err()
        {
            return Err(invalid("API key file contains an invalid record"));
        }
    }
    Ok(Some((file.keys, Some(FileStamp::of(&metadata)))))
}

/// Atomically replaces the key file and returns the [`FileStamp`] of the
/// version written, read from the temporary file before the rename (which
/// keeps inode, length and modification time), so a newer external version
/// at the path is never mistaken for this one.
fn write_records(path: &Path, file: &KeysFile) -> Result<FileStamp> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(".api-keys-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<FileStamp> {
        let mut handle = options.open(&temporary)?;
        let bytes = serde_json::to_vec(file)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid("API key file is too large"));
        }
        handle.write_all(&bytes)?;
        handle.sync_all()?;
        drop(handle);
        let stamp = FileStamp::read(&temporary)?;
        secure_fs::replace_file(&temporary, path)?;
        Ok(stamp)
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture() -> (PathBuf, ApiKeyStore) {
        let root =
            std::env::temp_dir().join(format!("todex-api-keys-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = ApiKeyStore::load(&root).unwrap();
        (root, store)
    }

    fn named(name: &str) -> NewApiKey {
        NewApiKey {
            name: name.to_owned(),
            ..NewApiKey::default()
        }
    }

    #[test]
    fn created_keys_authenticate_and_derive_their_recipient() {
        let (root, store) = fixture();
        let (record, text) = store.create(named("CI")).unwrap();
        assert!(text.starts_with(&format!("tdx_{}_", record.id)));
        let authenticated = store.authenticate(&text).unwrap();
        assert_eq!(authenticated.record.id, record.id);
        assert_eq!(
            URL_SAFE_NO_PAD
                .encode(history_crypto::recipient_from_seed(&authenticated.seed).to_bytes()),
            record.history_public_key
        );
        let owner = record.owner_id();
        assert_eq!(
            store.recipient_for_owner(&owner).unwrap().to_bytes(),
            history_crypto::recipient_from_seed(&authenticated.seed).to_bytes()
        );
        assert!(store.recipient_for_owner("local").is_none());

        // The file holds neither the secret nor the derived seed.
        let raw = fs::read_to_string(root.join(FILE_NAME)).unwrap();
        let secret = text.rsplit('_').next().unwrap();
        assert!(!raw.contains(secret));
        assert!(!raw.contains(&URL_SAFE_NO_PAD.encode(*authenticated.seed)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wrong_malformed_revoked_and_expired_keys_are_rejected() {
        let (root, store) = fixture();
        let (record, text) = store.create(named("CI")).unwrap();
        let mut wrong = text.to_string();
        let last = wrong.pop().unwrap();
        wrong.push(if last == 'A' { 'B' } else { 'A' });
        for token in [
            wrong.as_str(),
            "",
            "tdx_",
            "Bearer x",
            &text[..text.len() - 1],
        ] {
            assert!(matches!(
                store.authenticate(token),
                Err(AppError::Unauthenticated)
            ));
        }

        // Revocation by another handle (the CLI) applies through the file.
        let other = ApiKeyStore::load(&root).unwrap();
        assert!(other.revoke(&record.id).unwrap());
        assert!(!other.revoke(&record.id).unwrap());
        assert!(matches!(
            store.authenticate(&text),
            Err(AppError::Unauthenticated)
        ));
        assert!(store.recipient_for_owner(&record.owner_id()).is_none());
        assert_eq!(
            store.get(&record.id).unwrap().unwrap().status(unix_ms()),
            "revoked"
        );

        let (expiring, text) = store
            .create(NewApiKey {
                expires_at: Some(unix_ms() + 60_000),
                ..named("soon")
            })
            .unwrap();
        assert!(store.authenticate(&text).is_ok());
        store
            .update(
                &expiring.id,
                ApiKeyUpdate {
                    expires_at: Some(Some(1)),
                    ..ApiKeyUpdate::default()
                },
            )
            .unwrap();
        assert!(matches!(
            store.authenticate(&text),
            Err(AppError::Unauthenticated)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    /// A revocation by another process within one mtime tick (simulated by
    /// restoring the modification time) is seen, and the daemon's pending
    /// lastUsedAt flush does not write the key back as active.
    #[test]
    fn a_same_mtime_external_revocation_is_seen_and_not_written_back() {
        let (root, store) = fixture();
        let (record, text) = store.create(named("CI")).unwrap();
        store.authenticate(&text).unwrap();
        assert!(store.inner.lock().unwrap().dirty);
        let path = root.join(FILE_NAME);
        let original = fs::metadata(&path).unwrap().modified().unwrap();

        let cli = ApiKeyStore::load(&root).unwrap();
        assert!(cli.revoke(&record.id).unwrap());
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(original)
            .unwrap();
        // The daemon's lastUsedAt flush is due.
        store.inner.lock().unwrap().last_flush =
            Instant::now().checked_sub(USE_FLUSH_INTERVAL).unwrap();

        assert!(matches!(
            store.authenticate(&text),
            Err(AppError::Unauthenticated)
        ));
        assert_eq!(
            store.get(&record.id).unwrap().unwrap().status(unix_ms()),
            "revoked"
        );
        let on_disk = ApiKeyStore::load(&root)
            .unwrap()
            .get(&record.id)
            .unwrap()
            .unwrap();
        assert!(on_disk.revoked_at.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scopes_and_names_are_validated() {
        let (root, store) = fixture();
        assert!(store.create(named("  ")).is_err());
        assert!(store
            .create(NewApiKey {
                scopes: ApiKeyScopes {
                    agents: Some(Vec::new()),
                    workspaces: None,
                },
                ..named("x")
            })
            .is_err());
        assert!(store
            .create(NewApiKey {
                scopes: ApiKeyScopes {
                    agents: None,
                    workspaces: Some(vec!["relative".into()]),
                },
                ..named("x")
            })
            .is_err());
        let scopes = ApiKeyScopes {
            agents: Some(vec![ProviderKind::Codex]),
            workspaces: Some(vec![root.join("a")]),
        };
        assert!(scopes.allows_agent(ProviderKind::Codex));
        assert!(!scopes.allows_agent(ProviderKind::ClaudeCode));
        assert!(scopes.allows_workspace(&root.join("a").join("b")));
        assert!(!scopes.allows_workspace(&root.join("ab")));
        assert!(ApiKeyScopes::default().allows_workspace(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unix_key_files_are_owner_only_and_checked() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (root, store) = fixture();
            store.create(named("CI")).unwrap();
            let path = root.join(FILE_NAME);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(ApiKeyStore::load(&root).is_err());
            let _ = fs::remove_dir_all(&root);
        }
    }
}
