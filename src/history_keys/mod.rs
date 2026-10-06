//! History key system: who may read encrypted conversation history and the
//! keys that make it readable (docs/history-encryption.md §3, §5.2, §7).
//!
//! - [`RecipientRegistry`] (`$DATA_DIR/history/recipients.json`): the
//!   persisted mode, recipient devices, the recovery key, the epoch and grants.
//! - [`KeyringStore`] (`conversations/<id>/keyring.json`): every segment key
//!   id of a conversation and its DEK wrapped for each recipient.
//! - [`DekManager`]: the in-memory DEKs used to encrypt new history.
//! - [`FingerprintKey`] (`$DATA_DIR/history/fingerprint.key`): the HMAC key
//!   behind `requestFingerprint` / `textMac`.
//!
//! The daemon only ever holds recipient public keys. The conversation store
//! wiring (encrypting appends with [`DekManager::current_key`]) is a later
//! step, so parts of this module are not called outside tests yet.
#![allow(dead_code)]

mod dek;
mod fingerprint;
mod keyring;
mod recipients;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};

use crate::{config::HistoryEncryption, devices::DeviceRegistry, error::AppError, secure_fs};

pub(crate) use dek::DekManager;
pub(crate) use fingerprint::FingerprintKey;
pub(crate) use keyring::KeyringStore;
pub(crate) use recipients::{
    revoke_device_recipients, GrantRecord, RecipientRegistry, RecipientsSnapshot,
};

type Result<T> = std::result::Result<T, AppError>;

/// `$DATA_DIR/history`, 0700.
pub(crate) const HISTORY_DIR: &str = "history";
/// Upper bound on items in one `history.*` request or page.
pub(crate) const MAX_BATCH: usize = 500;

/// Time source; tests inject a fixed clock to exercise DEK rotation.
pub(crate) type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub(crate) fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// The daemon's history key state, shared by every connection.
#[derive(Clone)]
pub(crate) struct HistoryKeys {
    data_dir: PathBuf,
    recipients: RecipientRegistry,
    keyrings: KeyringStore,
    deks: DekManager,
    fingerprint: Arc<Mutex<Option<Arc<FingerprintKey>>>>,
}

impl HistoryKeys {
    /// `default_mode` seeds `recipients.json` on its first write only (see
    /// [`RecipientRegistry`]). `devices` is the paired-device registry when
    /// device auth is on: recipients of devices missing from it are revoked.
    pub(crate) fn load(
        data_dir: &Path,
        default_mode: HistoryEncryption,
        devices: Option<DeviceRegistry>,
    ) -> Result<Self> {
        Self::load_with_clock(data_dir, default_mode, devices, system_clock())
    }

    pub(crate) fn load_with_clock(
        data_dir: &Path,
        default_mode: HistoryEncryption,
        devices: Option<DeviceRegistry>,
        clock: Clock,
    ) -> Result<Self> {
        let recipients = RecipientRegistry::load(data_dir, default_mode, devices, clock.clone())?;
        let keyrings = KeyringStore::new(data_dir);
        let deks = DekManager::new(recipients.clone(), keyrings.clone(), clock);
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            recipients,
            keyrings,
            deks,
            fingerprint: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn recipients(&self) -> &RecipientRegistry {
        &self.recipients
    }

    pub(crate) fn keyrings(&self) -> &KeyringStore {
        &self.keyrings
    }

    pub(crate) fn deks(&self) -> &DekManager {
        &self.deks
    }

    /// The fingerprint key, created on first use.
    pub(crate) fn fingerprint(&self) -> Result<Arc<FingerprintKey>> {
        let mut slot = self.fingerprint.lock().map_err(|_| {
            AppError::InvalidRequest("history fingerprint key is unavailable".into())
        })?;
        if let Some(key) = slot.as_ref() {
            return Ok(key.clone());
        }
        let key = Arc::new(FingerprintKey::load_or_create(&self.data_dir)?);
        *slot = Some(key.clone());
        Ok(key)
    }
}

pub(crate) fn encode_id(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Parses a base64url (no padding) id of exactly `N` bytes.
pub(crate) fn decode_id<const N: usize>(value: &str, what: &str) -> Result<[u8; N]> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid(&format!("invalid {what}")))
}

/// Conversation ids are UUID v4, as enforced by the conversation store; this
/// keeps keyring paths inside `conversations/`.
pub(crate) fn validate_conversation_id(id: &str) -> Result<()> {
    match uuid::Uuid::parse_str(id) {
        Ok(parsed) if parsed.get_version_num() == 4 => Ok(()),
        _ => Err(invalid("conversation id must be a UUID v4")),
    }
}

/// Identifies one version of a file for reload-on-change: modification time
/// plus length, so two writes within one mtime tick are still noticed when
/// their sizes differ.
pub(crate) type FileStamp = (Option<SystemTime>, u64);

fn file_stamp(path: &Path) -> Option<FileStamp> {
    fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.modified().ok(), metadata.len()))
}

/// Reads an owner-only regular file. `None` when it does not exist; an error
/// for symlinks, foreign owners, group/world permissions or oversize files.
fn read_private_file(path: &Path, max_bytes: u64, what: &str) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max_bytes {
        return Err(invalid(&format!("invalid {what} file")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid(&format!("{what} file is not private")));
        }
    }
    Ok(Some(fs::read(path)?))
}

/// Atomically replaces `path` (0600) and syncs its directory so the rename
/// is durable before the caller relies on it.
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    secure_fs::write_owner_only_atomic(path, bytes)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::history_crypto::RecipientPublicKey;

    pub(crate) fn temp_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "todex-history-keys-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// The recipient public key for a device seed (`seed[0] = byte`).
    pub(crate) fn recipient(byte: u8) -> RecipientPublicKey {
        RecipientPublicKey::from_bytes(&public_key_bytes(byte)).unwrap()
    }

    pub(crate) fn public_key_bytes(byte: u8) -> Vec<u8> {
        use x_wing::{Decapsulator, KeyExport};
        x_wing::DecapsulationKey::from(seed(byte))
            .encapsulation_key()
            .to_bytes()
            .to_vec()
    }

    pub(crate) fn public_key_text(byte: u8) -> String {
        encode_id(&public_key_bytes(byte))
    }

    pub(crate) fn seed(byte: u8) -> [u8; 32] {
        let mut seed = [7; 32];
        seed[0] = byte;
        seed
    }

    pub(crate) fn conversation_dir(data_dir: &Path) -> (String, PathBuf) {
        let id = uuid::Uuid::new_v4().to_string();
        let directory = data_dir.join("conversations").join(&id);
        fs::create_dir_all(&directory).unwrap();
        (id, directory)
    }

    pub(crate) fn mode_of(path: &Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            0
        }
    }
}
