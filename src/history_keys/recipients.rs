//! `$DATA_DIR/history/recipients.json`: the persisted history encryption
//! mode and everyone who can read encrypted history.
//!
//! Mode precedence: until the file exists the mode is `off` (nobody could
//! read encrypted history yet). The first write creates it with the
//! configured default (`history_encryption` / `TODEX_AGENTD_HISTORY_ENCRYPTION`)
//! when that write leaves at least one active device recipient, otherwise
//! `off`. From then on the file is authoritative; the configured default is
//! ignored and devices change the mode with `history.encryption.enable` /
//! `disable`.
//!
//! The TUI revokes devices from another process, so like `devices.json` the
//! file is reloaded whenever its stamp changes. When device auth is on, a
//! device recipient whose device is no longer in `devices.json` is revoked on
//! the next access even if the TUI hook did not run.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    encode_id, file_stamp, invalid, read_private_file, write_private_file, Clock, FileStamp,
    HISTORY_DIR,
};
use crate::{
    config::HistoryEncryption, devices::DeviceRegistry, error::AppError,
    history_crypto::RecipientPublicKey, secure_fs,
};

type Result<T> = std::result::Result<T, AppError>;

const FILE_NAME: &str = "recipients.json";
const FILE_VERSION: u8 = 1;
/// 256 recipients with 1216-byte keys stay far below this.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Recipients kept on file, revoked ones included. The oldest revoked
/// recipients are pruned beyond this.
const MAX_RECIPIENTS: usize = 256;
/// Active (unrevoked) recipients; every new DEK is wrapped for each of them.
const MAX_ACTIVE_RECIPIENTS: usize = 64;
const MAX_GRANTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RecipientKind {
    Device,
    Recovery,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecipientRecord {
    /// `SHA-256(publicKey)[0..16]`, base64url.
    pub rid: String,
    pub kind: RecipientKind,
    #[serde(default)]
    pub device_id: Option<String>,
    /// X-Wing public key, base64url.
    pub public_key: String,
    pub added_at: DateTime<Utc>,
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
}

impl RecipientRecord {
    fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GrantStatus {
    /// Waiting for an authorized device to re-wrap old keys.
    Pending,
    /// A device finished uploading wraps (`history.grant.fulfill` with
    /// `complete: true`).
    Fulfilled,
    Dismissed,
    /// The requesting recipient was revoked or replaced.
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GrantRecord {
    pub grant_id: String,
    pub rid: String,
    pub device_id: String,
    pub requested_at: DateTime<Utc>,
    pub status: GrantStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecipientsFile {
    version: u8,
    mode: HistoryEncryption,
    epoch: u64,
    recipients: Vec<RecipientRecord>,
    grants: Vec<GrantRecord>,
}

impl Default for RecipientsFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            mode: HistoryEncryption::Off,
            epoch: 0,
            recipients: Vec::new(),
            grants: Vec::new(),
        }
    }
}

/// A consistent view for `history.encryption.get`.
#[derive(Clone, Debug)]
pub(crate) struct RecipientsSnapshot {
    pub mode: HistoryEncryption,
    pub epoch: u64,
    pub recipients: Vec<RecipientRecord>,
    pub grants: Vec<GrantRecord>,
}

/// What a new DEK is wrapped for.
pub(crate) struct ActiveRecipients {
    pub mode: HistoryEncryption,
    pub epoch: u64,
    pub keys: Vec<RecipientPublicKey>,
}

struct Inner {
    path: PathBuf,
    file: RecipientsFile,
    exists: bool,
    stamp: Option<FileStamp>,
}

/// Daemon-side handle. Every operation reloads a changed file, reconciles
/// against the device registry, and publishes changes atomically.
#[derive(Clone)]
pub(crate) struct RecipientRegistry {
    inner: Arc<Mutex<Inner>>,
    default_mode: HistoryEncryption,
    devices: Option<DeviceRegistry>,
    clock: Clock,
}

impl RecipientRegistry {
    pub(crate) fn load(
        data_dir: &Path,
        default_mode: HistoryEncryption,
        devices: Option<DeviceRegistry>,
        clock: Clock,
    ) -> Result<Self> {
        let path = data_dir.join(HISTORY_DIR).join(FILE_NAME);
        let stamp = file_stamp(&path);
        let loaded = read_file(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                exists: loaded.is_some(),
                file: loaded.unwrap_or_default(),
                stamp,
            })),
            default_mode,
            devices,
            clock,
        })
    }

    pub(crate) fn snapshot(&self) -> Result<RecipientsSnapshot> {
        self.update(|file, _| {
            Ok(RecipientsSnapshot {
                mode: file.mode,
                epoch: file.epoch,
                recipients: file.recipients.clone(),
                grants: file.grants.clone(),
            })
        })
    }

    pub(crate) fn mode(&self) -> Result<HistoryEncryption> {
        self.update(|file, _| Ok(file.mode))
    }

    /// Mode, epoch and the parsed public keys of every unrevoked recipient
    /// (devices and recovery).
    pub(crate) fn active_recipients(&self) -> Result<ActiveRecipients> {
        self.update(|file, _| {
            let keys = file
                .recipients
                .iter()
                .filter(|record| record.is_active())
                .map(|record| RecipientPublicKey::from_base64url(&record.public_key))
                .collect::<Result<Vec<_>>>()?;
            Ok(ActiveRecipients {
                mode: file.mode,
                epoch: file.epoch,
                keys,
            })
        })
    }

    /// The active recipient id registered by `device_id`.
    pub(crate) fn device_rid(&self, device_id: &str) -> Result<Option<String>> {
        self.update(|file, _| {
            Ok(file
                .active_device(device_id)
                .map(|record| record.rid.clone()))
        })
    }

    pub(crate) fn is_active(&self, rid: &str) -> Result<bool> {
        self.update(|file, _| Ok(file.active(rid).is_some()))
    }

    /// Fails with `UNAUTHENTICATED` when device auth is on and `device_id`
    /// is no longer paired (a connection outliving its device's revocation).
    pub(crate) fn ensure_device(&self, device_id: &str) -> Result<()> {
        match &self.devices {
            Some(devices) if devices.get(device_id)?.is_none() => Err(AppError::Unauthenticated),
            _ => Ok(()),
        }
    }

    /// Registers `key` for `device_id`. The same key again is a no-op; a new
    /// key revokes the device's previous one. Either change bumps the epoch.
    pub(crate) fn register_device(
        &self,
        device_id: &str,
        key: &RecipientPublicKey,
    ) -> Result<String> {
        let rid = encode_id(&key.rid());
        let public_key = encode_id(&key.to_bytes());
        self.update(|file, now| {
            if let Some(existing) = file.recipients.iter().find(|record| record.rid == rid) {
                return match existing.kind {
                    _ if !existing.is_active() => Err(revoked_key()),
                    RecipientKind::Device if existing.device_id.as_deref() == Some(device_id) => {
                        Ok(rid.clone())
                    }
                    _ => Err(AppError::Conflict(
                        "history recipient key is already registered to another recipient"
                            .to_owned(),
                    )),
                };
            }
            if let Some(previous) = file
                .active_device(device_id)
                .map(|record| record.rid.clone())
            {
                file.revoke(&previous, now);
            }
            file.add(RecipientRecord {
                rid: rid.clone(),
                kind: RecipientKind::Device,
                device_id: Some(device_id.to_owned()),
                public_key,
                added_at: now,
                revoked_at: None,
            })?;
            Ok(rid.clone())
        })
    }

    /// Replaces the recovery recipient (at most one is active).
    pub(crate) fn set_recovery(&self, key: &RecipientPublicKey) -> Result<String> {
        let rid = encode_id(&key.rid());
        let public_key = encode_id(&key.to_bytes());
        self.update(|file, now| {
            if let Some(existing) = file.recipients.iter().find(|record| record.rid == rid) {
                return match existing.kind {
                    _ if !existing.is_active() => Err(revoked_key()),
                    RecipientKind::Recovery => Ok(rid.clone()),
                    RecipientKind::Device => Err(AppError::Conflict(
                        "history recipient key is already registered to a device".to_owned(),
                    )),
                };
            }
            if let Some(previous) = file.active_recovery().map(|record| record.rid.clone()) {
                file.revoke(&previous, now);
            }
            file.add(RecipientRecord {
                rid: rid.clone(),
                kind: RecipientKind::Recovery,
                device_id: None,
                public_key,
                added_at: now,
                revoked_at: None,
            })?;
            Ok(rid.clone())
        })
    }

    /// Revokes `rid`. `Ok(false)` when it was already revoked.
    pub(crate) fn revoke(&self, rid: &str) -> Result<bool> {
        self.update(|file, now| {
            if !file.recipients.iter().any(|record| record.rid == rid) {
                return Err(AppError::NotFound(format!("history recipient {rid}")));
            }
            Ok(file.revoke(rid, now))
        })
    }

    /// `e2e` requires at least one active device recipient.
    pub(crate) fn set_mode(&self, mode: HistoryEncryption) -> Result<()> {
        self.update(|file, _| {
            if mode == HistoryEncryption::E2e && !file.has_active_device() {
                return Err(invalid(
                    "history encryption requires a registered device recipient; call history.recipient.register first",
                ));
            }
            file.mode = mode;
            Ok(())
        })
    }

    /// Opens a grant for `device_id`'s active recipient, or returns the one
    /// already pending for it.
    pub(crate) fn request_grant(&self, device_id: &str) -> Result<String> {
        self.update(|file, now| {
            let rid = file
                .active_device(device_id)
                .map(|record| record.rid.clone())
                .ok_or_else(not_registered)?;
            if let Some(grant) = file
                .grants
                .iter()
                .find(|grant| grant.rid == rid && grant.status == GrantStatus::Pending)
            {
                return Ok(grant.grant_id.clone());
            }
            if file.grants.len() >= MAX_GRANTS {
                let Some(oldest) = file
                    .grants
                    .iter()
                    .enumerate()
                    .filter(|(_, grant)| grant.status != GrantStatus::Pending)
                    .min_by_key(|(_, grant)| grant.updated_at.unwrap_or(grant.requested_at))
                    .map(|(index, _)| index)
                else {
                    return Err(AppError::ResourceExhausted(
                        "too many pending history grants; dismiss one first".to_owned(),
                    ));
                };
                file.grants.remove(oldest);
            }
            let grant_id = format!("grt_{}", uuid::Uuid::new_v4().simple());
            file.grants.push(GrantRecord {
                grant_id: grant_id.clone(),
                rid,
                device_id: device_id.to_owned(),
                requested_at: now,
                status: GrantStatus::Pending,
                updated_at: None,
            });
            Ok(grant_id)
        })
    }

    /// Dismisses a pending grant; settled grants are left as they are.
    pub(crate) fn dismiss_grant(&self, grant_id: &str) -> Result<()> {
        self.set_grant_status(grant_id, GrantStatus::Dismissed)
            .map(|_| ())
    }

    /// The recipient a pending grant targets, for `history.grant.fulfill`.
    pub(crate) fn pending_grant_rid(&self, grant_id: &str) -> Result<String> {
        self.update(|file, _| {
            let grant = file.grant(grant_id)?;
            if grant.status != GrantStatus::Pending {
                return Err(AppError::Conflict(format!(
                    "history grant {grant_id} is no longer pending"
                )));
            }
            Ok(grant.rid.clone())
        })
    }

    pub(crate) fn complete_grant(&self, grant_id: &str) -> Result<()> {
        self.set_grant_status(grant_id, GrantStatus::Fulfilled)
            .map(|_| ())
    }

    fn set_grant_status(&self, grant_id: &str, status: GrantStatus) -> Result<GrantStatus> {
        self.update(|file, now| {
            let grant = file
                .grants
                .iter_mut()
                .find(|grant| grant.grant_id == grant_id)
                .ok_or_else(|| AppError::NotFound(format!("history grant {grant_id}")))?;
            if grant.status == GrantStatus::Pending {
                grant.status = status;
                grant.updated_at = Some(now);
            }
            Ok(grant.status)
        })
    }

    /// Runs `operation` on the current file and publishes any change. The
    /// operation works on a copy, so a failed operation changes nothing.
    fn update<T>(
        &self,
        operation: impl FnOnce(&mut RecipientsFile, DateTime<Utc>) -> Result<T>,
    ) -> Result<T> {
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        let now = (self.clock)();
        let mut file = inner.file.clone();
        self.reconcile_devices(&mut file, now)?;
        let value = operation(&mut file, now)?;
        if file != inner.file {
            if !inner.exists && self.default_mode == HistoryEncryption::E2e {
                // First write: seed the configured default (see module docs).
                if file.has_active_device() {
                    file.mode = HistoryEncryption::E2e;
                }
            }
            file.prune()?;
            inner.write(file)?;
        }
        Ok(value)
    }

    fn reconcile_devices(&self, file: &mut RecipientsFile, now: DateTime<Utc>) -> Result<()> {
        let Some(devices) = &self.devices else {
            return Ok(());
        };
        let mut orphaned = Vec::new();
        for record in file.recipients.iter().filter(|record| record.is_active()) {
            if let Some(device_id) = &record.device_id {
                if devices.get(device_id)?.is_none() {
                    orphaned.push(record.rid.clone());
                }
            }
        }
        for rid in orphaned {
            tracing::info!(rid = %rid, "revoking history recipient of an unpaired device");
            file.revoke(&rid, now);
        }
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| invalid("history recipient registry is unavailable"))
    }
}

impl Inner {
    fn reload_if_changed(&mut self) -> Result<()> {
        let stamp = file_stamp(&self.path);
        if stamp != self.stamp {
            let loaded = read_file(&self.path)?;
            self.exists = loaded.is_some();
            self.file = loaded.unwrap_or_default();
            self.stamp = stamp;
        }
        Ok(())
    }

    fn write(&mut self, file: RecipientsFile) -> Result<()> {
        write_file(&self.path, &file)?;
        self.file = file;
        self.exists = true;
        self.stamp = file_stamp(&self.path);
        Ok(())
    }
}

impl RecipientsFile {
    fn active(&self, rid: &str) -> Option<&RecipientRecord> {
        self.recipients
            .iter()
            .find(|record| record.rid == rid && record.is_active())
    }

    fn active_device(&self, device_id: &str) -> Option<&RecipientRecord> {
        self.recipients.iter().find(|record| {
            record.is_active()
                && record.kind == RecipientKind::Device
                && record.device_id.as_deref() == Some(device_id)
        })
    }

    fn active_recovery(&self) -> Option<&RecipientRecord> {
        self.recipients
            .iter()
            .find(|record| record.is_active() && record.kind == RecipientKind::Recovery)
    }

    fn has_active_device(&self) -> bool {
        self.recipients
            .iter()
            .any(|record| record.is_active() && record.kind == RecipientKind::Device)
    }

    fn grant(&self, grant_id: &str) -> Result<&GrantRecord> {
        self.grants
            .iter()
            .find(|grant| grant.grant_id == grant_id)
            .ok_or_else(|| AppError::NotFound(format!("history grant {grant_id}")))
    }

    fn add(&mut self, record: RecipientRecord) -> Result<()> {
        let active = self.recipients.iter().filter(|r| r.is_active()).count();
        if active >= MAX_ACTIVE_RECIPIENTS {
            return Err(AppError::ResourceExhausted(
                "too many history recipients; revoke one first".to_owned(),
            ));
        }
        self.recipients.push(record);
        self.epoch += 1;
        Ok(())
    }

    /// Marks `rid` revoked, settles its pending grants and bumps the epoch.
    /// `false` when it was not active.
    fn revoke(&mut self, rid: &str, now: DateTime<Utc>) -> bool {
        let Some(record) = self
            .recipients
            .iter_mut()
            .find(|record| record.rid == rid && record.is_active())
        else {
            return false;
        };
        record.revoked_at = Some(now);
        for grant in &mut self.grants {
            if grant.rid == rid && grant.status == GrantStatus::Pending {
                grant.status = GrantStatus::Revoked;
                grant.updated_at = Some(now);
            }
        }
        self.epoch += 1;
        true
    }

    /// Drops the oldest revoked recipients beyond the file cap. A pruned key
    /// could be registered again; with 256 slots that needs ~200 rotations.
    fn prune(&mut self) -> Result<()> {
        while self.recipients.len() > MAX_RECIPIENTS {
            let Some(oldest) = self
                .recipients
                .iter()
                .enumerate()
                .filter_map(|(index, record)| record.revoked_at.map(|at| (index, at)))
                .min_by_key(|(_, at)| *at)
                .map(|(index, _)| index)
            else {
                return Err(AppError::ResourceExhausted(
                    "history recipient registry is full".to_owned(),
                ));
            };
            self.recipients.remove(oldest);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let corrupt = || invalid("history recipient registry contains an invalid record");
        if self.version != FILE_VERSION
            || self.recipients.len() > MAX_RECIPIENTS
            || self.grants.len() > MAX_GRANTS
        {
            return Err(invalid("unsupported history recipient registry file"));
        }
        let mut rids = std::collections::HashSet::new();
        let mut active_devices = std::collections::HashSet::new();
        let mut active_recovery = 0;
        for record in &self.recipients {
            let key =
                RecipientPublicKey::from_base64url(&record.public_key).map_err(|_| corrupt())?;
            if encode_id(&key.rid()) != record.rid || !rids.insert(record.rid.as_str()) {
                return Err(corrupt());
            }
            match (record.kind, record.device_id.as_deref()) {
                (RecipientKind::Device, Some(device_id)) if !device_id.is_empty() => {
                    if record.is_active() && !active_devices.insert(device_id) {
                        return Err(corrupt());
                    }
                }
                (RecipientKind::Recovery, None) => {
                    active_recovery += usize::from(record.is_active());
                }
                _ => return Err(corrupt()),
            }
        }
        if active_recovery > 1 || active_devices.len() > MAX_ACTIVE_RECIPIENTS {
            return Err(corrupt());
        }
        Ok(())
    }
}

fn read_file(path: &Path) -> Result<Option<RecipientsFile>> {
    let Some(bytes) = read_private_file(path, MAX_FILE_BYTES, "history recipient registry")? else {
        return Ok(None);
    };
    let file: RecipientsFile = serde_json::from_slice(&bytes).map_err(|error| {
        invalid(&format!(
            "history recipient registry file is unreadable: {error}"
        ))
    })?;
    file.validate()?;
    Ok(Some(file))
}

fn write_file(path: &Path, file: &RecipientsFile) -> Result<()> {
    file.validate()?;
    if let Some(parent) = path.parent() {
        secure_fs::ensure_owner_only_dir(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(file)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("history recipient registry file is too large"));
    }
    write_private_file(path, &bytes)
}

/// File-level hook for device revocation (`devices::revoke_device` and
/// `revoke_all_devices`, used by the TUI from its own process): revokes the
/// recipients of `device_id`, or of every device when `None`. Returns how
/// many were revoked; a missing registry file revokes nothing.
pub(crate) fn revoke_device_recipients(data_dir: &Path, device_id: Option<&str>) -> Result<usize> {
    let path = data_dir.join(HISTORY_DIR).join(FILE_NAME);
    let Some(mut file) = read_file(&path)? else {
        return Ok(0);
    };
    let now = Utc::now();
    let targets = file
        .recipients
        .iter()
        .filter(|record| {
            record.is_active()
                && record.kind == RecipientKind::Device
                && device_id.is_none_or(|device_id| record.device_id.as_deref() == Some(device_id))
        })
        .map(|record| record.rid.clone())
        .collect::<Vec<_>>();
    for rid in &targets {
        file.revoke(rid, now);
    }
    if !targets.is_empty() {
        write_file(&path, &file)?;
    }
    Ok(targets.len())
}

fn revoked_key() -> AppError {
    AppError::Conflict("this history recipient key was revoked; generate a new key pair".to_owned())
}

fn not_registered() -> AppError {
    invalid("this device has no history recipient; call history.recipient.register first")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_keys::{system_clock, test_support::*};

    fn registry(root: &Path, default_mode: HistoryEncryption) -> RecipientRegistry {
        RecipientRegistry::load(root, default_mode, None, system_clock()).unwrap()
    }

    #[test]
    fn register_replace_and_revoke_bump_the_epoch() {
        let root = temp_dir("registry");
        let registry = registry(&root, HistoryEncryption::Off);
        let snapshot = registry.snapshot().unwrap();
        assert_eq!((snapshot.mode, snapshot.epoch), (HistoryEncryption::Off, 0));
        assert!(!root.join(HISTORY_DIR).join(FILE_NAME).exists());

        let first = registry.register_device("dev_a", &recipient(1)).unwrap();
        assert_eq!(registry.snapshot().unwrap().epoch, 1);
        // Same key again: idempotent, no epoch change.
        assert_eq!(
            registry.register_device("dev_a", &recipient(1)).unwrap(),
            first
        );
        assert_eq!(registry.snapshot().unwrap().epoch, 1);
        // Another device cannot claim the key.
        assert_eq!(
            registry
                .register_device("dev_b", &recipient(1))
                .unwrap_err()
                .code(),
            "CONFLICT"
        );

        // A new key replaces the device's old one: revoke + add.
        let second = registry.register_device("dev_a", &recipient(2)).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.epoch, 3);
        assert_eq!(registry.device_rid("dev_a").unwrap(), Some(second.clone()));
        assert!(!registry.is_active(&first).unwrap());
        // A revoked key cannot come back.
        assert_eq!(
            registry
                .register_device("dev_a", &recipient(1))
                .unwrap_err()
                .code(),
            "CONFLICT"
        );

        assert!(registry.revoke(&second).unwrap());
        assert!(!registry.revoke(&second).unwrap());
        assert_eq!(registry.snapshot().unwrap().epoch, 4);
        assert_eq!(registry.revoke("missing").unwrap_err().code(), "NOT_FOUND");
        assert_eq!(registry.device_rid("dev_a").unwrap(), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_is_single_and_enable_needs_a_device() {
        let root = temp_dir("recovery");
        let registry = registry(&root, HistoryEncryption::Off);
        let recovery = registry.set_recovery(&recipient(9)).unwrap();
        assert_eq!(registry.set_recovery(&recipient(9)).unwrap(), recovery);
        // Recovery alone cannot enable encryption.
        assert_eq!(
            registry
                .set_mode(HistoryEncryption::E2e)
                .unwrap_err()
                .code(),
            "INVALID_REQUEST"
        );
        let replacement = registry.set_recovery(&recipient(10)).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let active_recovery = snapshot
            .recipients
            .iter()
            .filter(|record| record.kind == RecipientKind::Recovery && record.is_active())
            .map(|record| record.rid.clone())
            .collect::<Vec<_>>();
        assert_eq!(active_recovery, vec![replacement]);
        assert_eq!(snapshot.epoch, 3);
        assert_eq!(
            registry
                .register_device("dev_a", &recipient(10))
                .unwrap_err()
                .code(),
            "CONFLICT"
        );

        registry.register_device("dev_a", &recipient(1)).unwrap();
        registry.set_mode(HistoryEncryption::E2e).unwrap();
        let active = registry.active_recipients().unwrap();
        assert_eq!(active.mode, HistoryEncryption::E2e);
        assert_eq!(active.keys.len(), 2);
        // Mode changes do not touch the epoch.
        assert_eq!(active.epoch, 4);
        registry.set_mode(HistoryEncryption::Off).unwrap();
        assert_eq!(registry.mode().unwrap(), HistoryEncryption::Off);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn configured_default_only_seeds_a_new_file() {
        let root = temp_dir("default");
        let registry = registry(&root, HistoryEncryption::E2e);
        assert_eq!(registry.mode().unwrap(), HistoryEncryption::Off);
        // The first write without a device keeps `off`.
        registry.set_recovery(&recipient(9)).unwrap();
        assert_eq!(registry.mode().unwrap(), HistoryEncryption::Off);
        registry.register_device("dev_a", &recipient(1)).unwrap();
        assert_eq!(registry.mode().unwrap(), HistoryEncryption::Off);

        let fresh = temp_dir("default-fresh");
        let registry =
            super::RecipientRegistry::load(&fresh, HistoryEncryption::E2e, None, system_clock())
                .unwrap();
        registry.register_device("dev_a", &recipient(1)).unwrap();
        assert_eq!(registry.mode().unwrap(), HistoryEncryption::E2e);
        // The persisted mode wins over the default afterwards.
        registry.set_mode(HistoryEncryption::Off).unwrap();
        let reloaded = self::registry(&fresh, HistoryEncryption::E2e);
        assert_eq!(reloaded.mode().unwrap(), HistoryEncryption::Off);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(fresh);
    }

    #[test]
    fn grants_follow_their_recipient() {
        let root = temp_dir("grants");
        let registry = registry(&root, HistoryEncryption::Off);
        assert_eq!(
            registry.request_grant("dev_b").unwrap_err().code(),
            "INVALID_REQUEST"
        );
        registry.register_device("dev_a", &recipient(1)).unwrap();
        let rid_b = registry.register_device("dev_b", &recipient(2)).unwrap();
        let grant = registry.request_grant("dev_b").unwrap();
        assert!(grant.starts_with("grt_"));
        assert_eq!(registry.request_grant("dev_b").unwrap(), grant);
        assert_eq!(registry.pending_grant_rid(&grant).unwrap(), rid_b);
        registry.dismiss_grant(&grant).unwrap();
        assert_eq!(
            registry.pending_grant_rid(&grant).unwrap_err().code(),
            "CONFLICT"
        );
        assert_eq!(
            registry.dismiss_grant("grt_missing").unwrap_err().code(),
            "NOT_FOUND"
        );

        let second = registry.request_grant("dev_b").unwrap();
        assert_ne!(second, grant);
        registry.complete_grant(&second).unwrap();
        let third = registry.request_grant("dev_b").unwrap();
        // Replacing the device key settles its pending grant.
        registry.register_device("dev_b", &recipient(3)).unwrap();
        let grants = registry.snapshot().unwrap().grants;
        let status = |id: &str| grants.iter().find(|g| g.grant_id == id).unwrap().status;
        assert_eq!(status(&grant), GrantStatus::Dismissed);
        assert_eq!(status(&second), GrantStatus::Fulfilled);
        assert_eq!(status(&third), GrantStatus::Revoked);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn device_revocation_reaches_the_registry() {
        let root = temp_dir("device-hook");
        let registry = registry(&root, HistoryEncryption::Off);
        let a = registry.register_device("dev_a", &recipient(1)).unwrap();
        let b = registry.register_device("dev_b", &recipient(2)).unwrap();
        let recovery = registry.set_recovery(&recipient(9)).unwrap();
        // The TUI process revokes through the file; the daemon reloads it.
        assert_eq!(revoke_device_recipients(&root, Some("dev_a")).unwrap(), 1);
        assert!(!registry.is_active(&a).unwrap());
        assert!(registry.is_active(&b).unwrap());
        assert_eq!(registry.snapshot().unwrap().epoch, 4);
        assert_eq!(revoke_device_recipients(&root, None).unwrap(), 1);
        assert!(!registry.is_active(&b).unwrap());
        // Recovery is not a device and survives a revoke-all.
        assert!(registry.is_active(&recovery).unwrap());
        assert_eq!(revoke_device_recipients(&root, None).unwrap(), 0);
        let empty = temp_dir("device-hook-empty");
        assert_eq!(revoke_device_recipients(&empty, None).unwrap(), 0);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn unpaired_devices_lose_their_recipient() {
        let root = temp_dir("reconcile");
        let devices = DeviceRegistry::load(&root).unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&[3; 32]);
        let device = devices
            .register("Phone", &signing.verifying_key().to_bytes())
            .unwrap();
        let registry = RecipientRegistry::load(
            &root,
            HistoryEncryption::Off,
            Some(devices.clone()),
            system_clock(),
        )
        .unwrap();
        registry.ensure_device(&device.device_id).unwrap();
        let rid = registry
            .register_device(&device.device_id, &recipient(1))
            .unwrap();
        // Bypass the hook: drop the device straight from devices.json.
        std::fs::remove_file(root.join("devices.json")).unwrap();
        assert!(!registry.is_active(&rid).unwrap());
        assert_eq!(
            registry
                .ensure_device(&device.device_id)
                .unwrap_err()
                .code(),
            "UNAUTHENTICATED"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn registry_file_is_private_and_validated() {
        let root = temp_dir("file");
        let registry = registry(&root, HistoryEncryption::Off);
        registry.register_device("dev_a", &recipient(1)).unwrap();
        let path = root.join(HISTORY_DIR).join(FILE_NAME);
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&path), 0o600);
            assert_eq!(mode_of(&root.join(HISTORY_DIR)), 0o700);
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["mode"], "off");
        assert_eq!(value["recipients"][0]["revokedAt"], serde_json::Value::Null);

        // A record whose rid does not match its key is rejected.
        let tampered = raw.replacen(&public_key_text(1), &public_key_text(2), 1);
        secure_fs::write_owner_only_atomic(&path, tampered.as_bytes()).unwrap();
        assert!(registry.snapshot().is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            secure_fs::write_owner_only_atomic(&path, raw.as_bytes()).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(registry.snapshot().is_err());
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
