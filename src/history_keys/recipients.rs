//! `$DATA_DIR/history/recipients.json`: everyone who can read encrypted
//! history. History is always end-to-end encrypted; the file's retired
//! `mode` field is ignored on read and always written as `"e2e"` (so an
//! older daemon reading it keeps encrypting).
//!
//! With device auth off every connection is the same `local` device, so
//! `local` may hold several active recipients (one per client key pair);
//! with auth on a device's new key replaces its previous one.
//!
//! The TUI revokes devices from another process, so like `devices.json` the
//! file is reloaded whenever its stamp changes. When device auth is on, a
//! device recipient whose device is no longer in `devices.json` is revoked on
//! the next access even if the TUI hook did not run.
//!
//! Revoked devices are also blocked (`revokedDevices`): a blocked device may
//! only call `history.encryption.get` until another device restores it, so
//! re-pairing the same device identity does not bring history access back.
//! Changes made from another process are reported through
//! [`RecipientRegistry::take_external_changes`] so the daemon can push them.

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
    devices::DeviceRegistry, error::AppError, history_crypto::RecipientPublicKey, secure_fs,
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
/// Unreported external revocations kept in memory between polls. The
/// on-file block list itself is unbounded: dropping an entry would unblock
/// a revoked device (only `MAX_FILE_BYTES` limits it, and a write past that
/// fails instead of losing entries).
const MAX_PENDING_EXTERNAL: usize = 256;
/// `revokedBy` of devices blocked from the TUI (`devices.json` revocation).
pub(crate) const REVOKED_BY_TUI: &str = "tui";
/// `revokedBy` of devices blocked by the daemon itself because they vanished
/// from `devices.json` without the TUI hook running.
pub(crate) const REVOKED_BY_DAEMON: &str = "local";

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

/// A device that may no longer use `history.*` (except
/// `history.encryption.get`) until `history.device.restore`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RevokedDevice {
    pub device_id: String,
    pub revoked_at: DateTime<Utc>,
    /// The revoking device id, `"tui"` or `"local"` (the daemon).
    pub revoked_by: String,
}

/// The retired `mode` field of `recipients.json`: any stored value (`off`
/// included) is accepted and ignored; it is always written as `"e2e"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RetiredMode;

impl Serialize for RetiredMode {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(HISTORY_MODE)
    }
}

impl<'de> Deserialize<'de> for RetiredMode {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self)
    }
}

/// The only history mode (`history.encryption.get` / `.updated` `mode`).
pub(crate) const HISTORY_MODE: &str = "e2e";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecipientsFile {
    version: u8,
    #[serde(default)]
    mode: RetiredMode,
    epoch: u64,
    recipients: Vec<RecipientRecord>,
    grants: Vec<GrantRecord>,
    /// Absent in files written before device blocking; omitted while empty
    /// so such files stay readable by older daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    revoked_devices: Vec<RevokedDevice>,
}

impl Default for RecipientsFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            mode: RetiredMode,
            epoch: 0,
            recipients: Vec::new(),
            grants: Vec::new(),
            revoked_devices: Vec::new(),
        }
    }
}

/// A consistent view for `history.encryption.get`.
#[derive(Clone, Debug)]
pub(crate) struct RecipientsSnapshot {
    pub epoch: u64,
    pub recipients: Vec<RecipientRecord>,
    pub grants: Vec<GrantRecord>,
    pub revoked_devices: Vec<RevokedDevice>,
}

/// The registry state a successful write left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Written {
    pub epoch: u64,
}

/// The result of a mutation; `written` is set when it changed the file.
#[derive(Debug)]
pub(crate) struct Applied<T> {
    pub value: T,
    pub written: Option<Written>,
}

/// A device blocked by another process (the TUI) or by the daemon's own
/// reconciliation, i.e. not by a `history.*` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalRevocation {
    pub device_id: String,
    pub state: Written,
}

/// What a new DEK is wrapped for.
pub(crate) struct ActiveRecipients {
    pub epoch: u64,
    pub keys: Vec<RecipientPublicKey>,
}

struct Inner {
    path: PathBuf,
    file: RecipientsFile,
    stamp: Option<FileStamp>,
    /// Unreported [`ExternalRevocation`]s, oldest first, bounded.
    external: Vec<ExternalRevocation>,
}

/// Daemon-side handle. Every operation reloads a changed file, reconciles
/// against the device registry, and publishes changes atomically.
#[derive(Clone)]
pub(crate) struct RecipientRegistry {
    inner: Arc<Mutex<Inner>>,
    devices: Option<DeviceRegistry>,
    clock: Clock,
}

impl RecipientRegistry {
    pub(crate) fn load(
        data_dir: &Path,
        devices: Option<DeviceRegistry>,
        clock: Clock,
    ) -> Result<Self> {
        let path = data_dir.join(HISTORY_DIR).join(FILE_NAME);
        let stamp = file_stamp(&path);
        let loaded = read_file(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                file: loaded.unwrap_or_default(),
                stamp,
                external: Vec::new(),
            })),
            devices,
            clock,
        })
    }

    pub(crate) fn snapshot(&self) -> Result<RecipientsSnapshot> {
        self.update(|file, _| {
            Ok(RecipientsSnapshot {
                epoch: file.epoch,
                recipients: file.recipients.clone(),
                grants: file.grants.clone(),
                revoked_devices: file.revoked_devices.clone(),
            })
        })
    }

    /// Fails with `HISTORY_ACCESS_REVOKED` when `device_id` is blocked.
    pub(crate) fn ensure_access(&self, device_id: &str) -> Result<()> {
        self.update(|file, _| file.ensure_access(device_id))
    }

    /// Reloads a changed file, reconciles it and returns the device
    /// revocations not made through this handle's commands since the last
    /// call (see [`ExternalRevocation`]).
    pub(crate) fn take_external_changes(&self) -> Result<Vec<ExternalRevocation>> {
        self.update(|_, _| Ok(()))?;
        Ok(std::mem::take(&mut self.lock()?.external))
    }

    /// The current epoch.
    pub(crate) fn state(&self) -> Result<Written> {
        self.update(|file, _| Ok(Written { epoch: file.epoch }))
    }

    /// The epoch and the parsed public keys of every unrevoked recipient
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
                epoch: file.epoch,
                keys,
            })
        })
    }

    /// The newest active recipient id registered by `device_id` (with
    /// device auth off `local` may have several; see [`Self::device_owns`]).
    pub(crate) fn device_rid(&self, device_id: &str) -> Result<Option<String>> {
        self.update(|file, _| {
            Ok(file
                .active_device(device_id)
                .map(|record| record.rid.clone()))
        })
    }

    /// Whether `rid` is an active recipient registered by `device_id`.
    pub(crate) fn device_owns(&self, device_id: &str, rid: &str) -> Result<bool> {
        self.update(|file, _| {
            Ok(file.active(rid).is_some_and(|record| {
                record.kind == RecipientKind::Device
                    && record.device_id.as_deref() == Some(device_id)
            }))
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

    /// Registers `key` for `device_id`. The same key again is a no-op. With
    /// device auth on a new key revokes the device's previous one; with it
    /// off every client is the same `local` device, so each client's key is
    /// added alongside the others (revocation is by `rid`). A new key bumps
    /// the epoch.
    pub(crate) fn register_device(
        &self,
        device_id: &str,
        key: &RecipientPublicKey,
    ) -> Result<Applied<String>> {
        let rid = encode_id(&key.rid());
        let public_key = encode_id(&key.to_bytes());
        let replaces_previous = self.devices.is_some();
        self.apply(|file, now| {
            file.ensure_access(device_id)?;
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
            if replaces_previous {
                if let Some(previous) = file
                    .active_device(device_id)
                    .map(|record| record.rid.clone())
                {
                    file.revoke(&previous, now);
                }
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
    pub(crate) fn set_recovery(&self, key: &RecipientPublicKey) -> Result<Applied<String>> {
        let rid = encode_id(&key.rid());
        let public_key = encode_id(&key.to_bytes());
        self.apply(|file, now| {
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

    /// Revokes `rid` and returns its device id (`None` for the recovery
    /// recipient). Revoking a device's active recipient also blocks the
    /// device (by `revoked_by`) when device auth is on: without it every
    /// connection is the same `local` principal and nobody could restore it.
    /// Revoking an already revoked recipient changes nothing.
    pub(crate) fn revoke(&self, rid: &str, revoked_by: &str) -> Result<Applied<Option<String>>> {
        let blocks_devices = self.devices.is_some();
        self.apply(|file, now| {
            let Some(record) = file.recipients.iter().find(|record| record.rid == rid) else {
                return Err(AppError::NotFound(format!("history recipient {rid}")));
            };
            let device_id = record.device_id.clone();
            if file.revoke(rid, now) && blocks_devices {
                if let Some(device_id) = &device_id {
                    file.block(device_id, revoked_by, now);
                }
            }
            Ok(device_id)
        })
    }

    /// Lifts the block on `device_id` (`NOT_FOUND` when it is not blocked).
    /// Its revoked keys stay revoked: the device registers a new key and
    /// needs a new grant for older history.
    pub(crate) fn restore_device(&self, device_id: &str) -> Result<Applied<()>> {
        self.apply(|file, _| {
            let before = file.revoked_devices.len();
            file.revoked_devices
                .retain(|entry| entry.device_id != device_id);
            if file.revoked_devices.len() == before {
                return Err(AppError::NotFound(format!(
                    "device {device_id} is not blocked from history access"
                )));
            }
            Ok(())
        })
    }

    /// Opens a grant for `device_id`'s active recipient, or returns the one
    /// already pending for it.
    pub(crate) fn request_grant(&self, device_id: &str) -> Result<Applied<RequestedGrant>> {
        self.apply(|file, now| {
            file.ensure_access(device_id)?;
            let rid = file
                .active_device(device_id)
                .map(|record| record.rid.clone())
                .ok_or_else(not_registered)?;
            if let Some(grant) = file
                .grants
                .iter()
                .find(|grant| grant.rid == rid && grant.status == GrantStatus::Pending)
            {
                return Ok(RequestedGrant {
                    grant_id: grant.grant_id.clone(),
                    rid,
                });
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
                rid: rid.clone(),
                device_id: device_id.to_owned(),
                requested_at: now,
                status: GrantStatus::Pending,
                updated_at: None,
            });
            Ok(RequestedGrant { grant_id, rid })
        })
    }

    /// Dismisses a pending grant; settled grants are left as they are.
    pub(crate) fn dismiss_grant(&self, grant_id: &str) -> Result<Applied<GrantStatus>> {
        self.set_grant_status(grant_id, GrantStatus::Dismissed)
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

    pub(crate) fn complete_grant(&self, grant_id: &str) -> Result<Applied<GrantStatus>> {
        self.set_grant_status(grant_id, GrantStatus::Fulfilled)
    }

    fn set_grant_status(
        &self,
        grant_id: &str,
        status: GrantStatus,
    ) -> Result<Applied<GrantStatus>> {
        self.apply(|file, now| {
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

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut RecipientsFile, DateTime<Utc>) -> Result<T>,
    ) -> Result<T> {
        self.apply(operation).map(|applied| applied.value)
    }

    /// Runs `operation` on the current file and publishes any change. The
    /// operation works on a copy, so a failed operation changes nothing.
    /// `written` reports whether the operation (or reconciliation) wrote.
    fn apply<T>(
        &self,
        operation: impl FnOnce(&mut RecipientsFile, DateTime<Utc>) -> Result<T>,
    ) -> Result<Applied<T>> {
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        let now = (self.clock)();
        let mut file = inner.file.clone();
        let reconciled = self.reconcile_devices(&mut file, now)?;
        let value = operation(&mut file, now)?;
        let mut written = None;
        if file != inner.file {
            file.prune()?;
            let state = Written { epoch: file.epoch };
            inner.write(file)?;
            inner.report(reconciled, state);
            written = Some(state);
        }
        Ok(Applied { value, written })
    }

    /// Keeps the file consistent with device state: recipients of blocked
    /// devices are revoked, and with device auth on, devices missing from
    /// `devices.json` lose their recipient and are blocked (the TUI hook did
    /// not run). Returns the devices this newly blocked.
    fn reconcile_devices(
        &self,
        file: &mut RecipientsFile,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>> {
        let blocked_rids = file
            .recipients
            .iter()
            .filter(|record| {
                record.is_active()
                    && record
                        .device_id
                        .as_deref()
                        .is_some_and(|device_id| file.is_blocked(device_id))
            })
            .map(|record| record.rid.clone())
            .collect::<Vec<_>>();
        for rid in blocked_rids {
            tracing::warn!(rid = %rid, "revoking active history recipient of a blocked device");
            file.revoke(&rid, now);
        }
        let Some(devices) = &self.devices else {
            return Ok(Vec::new());
        };
        let mut orphaned = Vec::new();
        for record in file.recipients.iter().filter(|record| record.is_active()) {
            if let Some(device_id) = &record.device_id {
                if devices.get(device_id)?.is_none() {
                    orphaned.push((record.rid.clone(), device_id.clone()));
                }
            }
        }
        let mut blocked = Vec::new();
        for (rid, device_id) in orphaned {
            tracing::info!(rid = %rid, "revoking history recipient of an unpaired device");
            file.revoke(&rid, now);
            if file.block(&device_id, REVOKED_BY_DAEMON, now) {
                blocked.push(device_id);
            }
        }
        Ok(blocked)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| invalid("history recipient registry is unavailable"))
    }
}

impl Inner {
    /// Reloads a file changed by another process and records the devices
    /// that change blocked.
    fn reload_if_changed(&mut self) -> Result<()> {
        let stamp = file_stamp(&self.path);
        if stamp != self.stamp {
            let loaded = read_file(&self.path)?;
            let loaded = loaded.unwrap_or_default();
            let blocked = loaded
                .revoked_devices
                .iter()
                .filter(|entry| !self.file.is_blocked(&entry.device_id))
                .map(|entry| entry.device_id.clone())
                .collect::<Vec<_>>();
            let state = Written {
                epoch: loaded.epoch,
            };
            self.file = loaded;
            self.stamp = stamp;
            self.report(blocked, state);
        }
        Ok(())
    }

    fn report(&mut self, device_ids: Vec<String>, state: Written) {
        self.external.extend(
            device_ids
                .into_iter()
                .map(|device_id| ExternalRevocation { device_id, state }),
        );
        if self.external.len() > MAX_PENDING_EXTERNAL {
            let excess = self.external.len() - MAX_PENDING_EXTERNAL;
            self.external.drain(..excess);
        }
    }

    fn write(&mut self, file: RecipientsFile) -> Result<()> {
        write_file(&self.path, &file)?;
        self.file = file;
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

    /// The newest active recipient of `device_id`.
    fn active_device(&self, device_id: &str) -> Option<&RecipientRecord> {
        self.recipients.iter().rev().find(|record| {
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

    fn is_blocked(&self, device_id: &str) -> bool {
        self.revoked_devices
            .iter()
            .any(|entry| entry.device_id == device_id)
    }

    fn ensure_access(&self, device_id: &str) -> Result<()> {
        if self.is_blocked(device_id) {
            return Err(AppError::HistoryAccessRevoked);
        }
        Ok(())
    }

    /// Adds `device_id` to the block list; `false` when already blocked.
    /// Entries are never pruned (see [`MAX_PENDING_EXTERNAL`]).
    fn block(&mut self, device_id: &str, revoked_by: &str, now: DateTime<Utc>) -> bool {
        if self.is_blocked(device_id) {
            return false;
        }
        self.revoked_devices.push(RevokedDevice {
            device_id: device_id.to_owned(),
            revoked_at: now,
            revoked_by: revoked_by.to_owned(),
        });
        true
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
        // Several active recipients may share a device id: with device auth
        // off every client is `local` (see [`RecipientRegistry::register_device`]).
        let mut active_devices = 0;
        let mut active_recovery = 0;
        for record in &self.recipients {
            let key =
                RecipientPublicKey::from_base64url(&record.public_key).map_err(|_| corrupt())?;
            if encode_id(&key.rid()) != record.rid || !rids.insert(record.rid.as_str()) {
                return Err(corrupt());
            }
            match (record.kind, record.device_id.as_deref()) {
                (RecipientKind::Device, Some(device_id)) if !device_id.is_empty() => {
                    active_devices += usize::from(record.is_active());
                }
                (RecipientKind::Recovery, None) => {
                    active_recovery += usize::from(record.is_active());
                }
                _ => return Err(corrupt()),
            }
        }
        if active_recovery > 1 || active_devices > MAX_ACTIVE_RECIPIENTS {
            return Err(corrupt());
        }
        let mut blocked = std::collections::HashSet::new();
        if self.revoked_devices.iter().any(|entry| {
            entry.device_id.is_empty()
                || entry.revoked_by.is_empty()
                || !blocked.insert(entry.device_id.as_str())
        }) {
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

/// Which devices [`revoke_device_recipients`] revokes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum DeviceRevocation<'a> {
    One(&'a str),
    /// Every device: these paired ids plus every device with an active
    /// recipient.
    All(&'a [String]),
}

/// File-level hook for device revocation (`devices::revoke_device` and
/// `revoke_all_devices`, used by the TUI from its own process): revokes the
/// device recipients of the selected devices and blocks those devices
/// (`revokedBy: "tui"`) in one atomic write. Returns how many recipients
/// were revoked. A missing registry file changes nothing: no device has
/// history keys yet. The daemon notices the change by the file stamp.
pub(crate) fn revoke_device_recipients(
    data_dir: &Path,
    devices: DeviceRevocation<'_>,
) -> Result<usize> {
    let path = data_dir.join(HISTORY_DIR).join(FILE_NAME);
    let Some(mut file) = read_file(&path)? else {
        return Ok(0);
    };
    let now = Utc::now();
    let selected = |device_id: &str| match devices {
        DeviceRevocation::One(target) => device_id == target,
        DeviceRevocation::All(_) => true,
    };
    let targets = file
        .recipients
        .iter()
        .filter(|record| {
            record.is_active()
                && record.kind == RecipientKind::Device
                && record.device_id.as_deref().is_some_and(selected)
        })
        .map(|record| (record.rid.clone(), record.device_id.clone()))
        .collect::<Vec<_>>();
    let mut blocked = match devices {
        DeviceRevocation::One(device_id) => vec![device_id.to_owned()],
        DeviceRevocation::All(paired) => paired.to_vec(),
    };
    for (rid, device_id) in &targets {
        file.revoke(rid, now);
        blocked.extend(device_id.clone());
    }
    let mut changed = !targets.is_empty();
    for device_id in &blocked {
        changed |= file.block(device_id, REVOKED_BY_TUI, now);
    }
    if changed {
        write_file(&path, &file)?;
    }
    Ok(targets.len())
}

/// `history.grant.request`'s result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestedGrant {
    pub grant_id: String,
    /// The requesting device's recipient.
    pub rid: String,
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

    /// Device auth off: every client is the `local` device.
    fn registry(root: &Path) -> RecipientRegistry {
        RecipientRegistry::load(root, None, system_clock()).unwrap()
    }

    #[test]
    fn register_replace_and_revoke_bump_the_epoch() {
        let root = temp_dir("registry");
        let (devices, a) = paired(&root, 1);
        let (_, b) = paired(&root, 2);
        let registry = RecipientRegistry::load(&root, Some(devices), system_clock()).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.epoch, 0);
        assert!(!root.join(HISTORY_DIR).join(FILE_NAME).exists());

        let first = registry.register_device(&a, &recipient(1)).unwrap().value;
        assert_eq!(registry.snapshot().unwrap().epoch, 1);
        // Same key again: idempotent, no epoch change.
        assert_eq!(
            registry.register_device(&a, &recipient(1)).unwrap().value,
            first
        );
        assert_eq!(registry.snapshot().unwrap().epoch, 1);
        // Another device cannot claim the key.
        assert_eq!(
            registry
                .register_device(&b, &recipient(1))
                .unwrap_err()
                .code(),
            "CONFLICT"
        );

        // With device auth a new key replaces the device's old one.
        let second = registry.register_device(&a, &recipient(2)).unwrap().value;
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.epoch, 3);
        assert_eq!(registry.device_rid(&a).unwrap(), Some(second.clone()));
        assert!(!registry.is_active(&first).unwrap());
        // A revoked key cannot come back.
        assert_eq!(
            registry
                .register_device(&a, &recipient(1))
                .unwrap_err()
                .code(),
            "CONFLICT"
        );

        assert_eq!(
            registry.revoke(&second, &b).unwrap().value.as_deref(),
            Some(a.as_str())
        );
        assert!(registry.revoke(&second, &b).unwrap().written.is_none());
        assert_eq!(registry.snapshot().unwrap().epoch, 4);
        assert_eq!(
            registry.revoke("missing", &b).unwrap_err().code(),
            "NOT_FOUND"
        );
        assert_eq!(registry.device_rid(&a).unwrap(), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn anonymous_clients_keep_their_own_keys() {
        let root = temp_dir("anonymous");
        let registry = registry(&root);
        // Two clients of the same `local` device each register a key.
        let first = registry
            .register_device("local", &recipient(1))
            .unwrap()
            .value;
        let second = registry
            .register_device("local", &recipient(2))
            .unwrap()
            .value;
        assert!(registry.is_active(&first).unwrap());
        assert!(registry.is_active(&second).unwrap());
        assert_eq!(registry.snapshot().unwrap().epoch, 2);
        assert_eq!(registry.active_recipients().unwrap().keys.len(), 2);
        assert!(registry.device_owns("local", &first).unwrap());
        assert!(registry.device_owns("local", &second).unwrap());
        assert!(!registry.device_owns("dev_other", &first).unwrap());
        // The newest key is the device's default rid.
        assert_eq!(registry.device_rid("local").unwrap(), Some(second.clone()));
        // The file with two active `local` recipients reloads.
        let reloaded = self::registry(&root);
        assert_eq!(reloaded.active_recipients().unwrap().keys.len(), 2);
        // Revocation is by rid and blocks nobody (auth off).
        registry.revoke(&second, "local").unwrap();
        assert!(registry.is_active(&first).unwrap());
        assert!(!registry.is_active(&second).unwrap());
        assert_eq!(registry.device_rid("local").unwrap(), Some(first));
        registry.ensure_access("local").unwrap();
        // The retired mode is written as "e2e" for older daemons.
        let raw = std::fs::read_to_string(root.join(HISTORY_DIR).join(FILE_NAME)).unwrap();
        assert!(raw.contains("\"mode\": \"e2e\""), "{raw}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_retired_off_mode_on_file_is_ignored() {
        let root = temp_dir("retired-mode");
        let registry = registry(&root);
        registry.register_device("local", &recipient(1)).unwrap();
        let path = root.join(HISTORY_DIR).join(FILE_NAME);
        let raw = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"e2e\"", "\"off\"");
        write_private_file(&path, raw.as_bytes()).unwrap();
        let reloaded = self::registry(&root);
        assert_eq!(reloaded.active_recipients().unwrap().keys.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_is_single() {
        let root = temp_dir("recovery");
        let registry = registry(&root);
        let recovery = registry.set_recovery(&recipient(9)).unwrap().value;
        assert_eq!(
            registry.set_recovery(&recipient(9)).unwrap().value,
            recovery
        );
        let replacement = registry.set_recovery(&recipient(10)).unwrap().value;
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
        let active = registry.active_recipients().unwrap();
        assert_eq!(active.keys.len(), 2);
        assert_eq!(active.epoch, 4);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn grants_follow_their_recipient() {
        let root = temp_dir("grants");
        let registry = registry(&root);
        assert_eq!(
            registry.request_grant("dev_b").unwrap_err().code(),
            "INVALID_REQUEST"
        );
        registry.register_device("dev_a", &recipient(1)).unwrap();
        let rid_b = registry
            .register_device("dev_b", &recipient(2))
            .unwrap()
            .value;
        let grant = registry.request_grant("dev_b").unwrap().value.grant_id;
        assert!(grant.starts_with("grt_"));
        assert_eq!(
            registry.request_grant("dev_b").unwrap().value.grant_id,
            grant
        );
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

        let second = registry.request_grant("dev_b").unwrap().value.grant_id;
        assert_ne!(second, grant);
        registry.complete_grant(&second).unwrap();
        let third = registry.request_grant("dev_b").unwrap().value.grant_id;
        // Revoking the requesting key settles its pending grant.
        registry.revoke(&rid_b, "local").unwrap();
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
        let registry = registry(&root);
        let a = registry
            .register_device("dev_a", &recipient(1))
            .unwrap()
            .value;
        let b = registry
            .register_device("dev_b", &recipient(2))
            .unwrap()
            .value;
        let recovery = registry.set_recovery(&recipient(9)).unwrap().value;
        // The TUI process revokes through the file; the daemon reloads it.
        assert_eq!(
            revoke_device_recipients(&root, DeviceRevocation::One("dev_a")).unwrap(),
            1
        );
        assert!(!registry.is_active(&a).unwrap());
        assert!(registry.is_active(&b).unwrap());
        assert_eq!(registry.snapshot().unwrap().epoch, 4);
        assert_eq!(
            revoke_device_recipients(&root, DeviceRevocation::All(&[])).unwrap(),
            1
        );
        assert!(!registry.is_active(&b).unwrap());
        // Recovery is not a device and survives a revoke-all.
        assert!(registry.is_active(&recovery).unwrap());
        assert_eq!(
            revoke_device_recipients(&root, DeviceRevocation::All(&[])).unwrap(),
            0
        );
        let empty = temp_dir("device-hook-empty");
        assert_eq!(
            revoke_device_recipients(&empty, DeviceRevocation::All(&[])).unwrap(),
            0
        );
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
        let registry =
            RecipientRegistry::load(&root, Some(devices.clone()), system_clock()).unwrap();
        registry.ensure_device(&device.device_id).unwrap();
        let rid = registry
            .register_device(&device.device_id, &recipient(1))
            .unwrap()
            .value;
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

    fn paired(root: &Path, byte: u8) -> (DeviceRegistry, String) {
        let devices = DeviceRegistry::load(root).unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&[byte; 32]);
        let device_id = devices
            .register("Device", &signing.verifying_key().to_bytes())
            .unwrap()
            .device_id;
        (devices, device_id)
    }

    #[test]
    fn revoking_a_device_recipient_blocks_until_restored() {
        let root = temp_dir("block");
        let (devices, a) = paired(&root, 1);
        let (_, b) = paired(&root, 2);
        let registry = RecipientRegistry::load(&root, Some(devices), system_clock()).unwrap();
        let rid_a = registry.register_device(&a, &recipient(1)).unwrap().value;
        let rid_b = registry.register_device(&b, &recipient(2)).unwrap().value;
        let recovery = registry.set_recovery(&recipient(9)).unwrap().value;

        // Recovery revocation blocks nobody.
        let revoked = registry.revoke(&recovery, &a).unwrap();
        assert_eq!(revoked.value, None);
        assert!(registry.snapshot().unwrap().revoked_devices.is_empty());

        let revoked = registry.revoke(&rid_b, &a).unwrap();
        assert_eq!(revoked.value.as_deref(), Some(b.as_str()));
        let written = revoked.written.unwrap();
        assert_eq!(written.epoch, registry.snapshot().unwrap().epoch);
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.revoked_devices.len(), 1);
        assert_eq!(snapshot.revoked_devices[0].device_id, b);
        assert_eq!(snapshot.revoked_devices[0].revoked_by, a);
        let code = |error: AppError| error.code();
        assert_eq!(
            code(registry.ensure_access(&b).unwrap_err()),
            "HISTORY_ACCESS_REVOKED"
        );
        assert_eq!(
            code(registry.register_device(&b, &recipient(3)).unwrap_err()),
            "HISTORY_ACCESS_REVOKED"
        );
        assert_eq!(
            code(registry.request_grant(&b).unwrap_err()),
            "HISTORY_ACCESS_REVOKED"
        );
        registry.ensure_access(&a).unwrap();
        // Command-originated blocks are not "external".
        assert!(registry.take_external_changes().unwrap().is_empty());

        assert_eq!(
            code(registry.restore_device("dev_unknown").unwrap_err()),
            "NOT_FOUND"
        );
        let epoch = registry.snapshot().unwrap().epoch;
        let restored = registry.restore_device(&b).unwrap();
        assert_eq!(restored.written.unwrap().epoch, epoch);
        registry.ensure_access(&b).unwrap();
        // The revoked key stays refused; a fresh key works and is wrapped for.
        assert_eq!(
            code(registry.register_device(&b, &recipient(2)).unwrap_err()),
            "CONFLICT"
        );
        let fresh = registry.register_device(&b, &recipient(3)).unwrap().value;
        assert_ne!(fresh, rid_b);
        assert_eq!(registry.active_recipients().unwrap().keys.len(), 2);
        assert!(registry.is_active(&rid_a).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_blocked_device_never_keeps_an_active_recipient() {
        let root = temp_dir("block-normalize");
        let registry = registry(&root);
        let rid = registry
            .register_device("dev_a", &recipient(1))
            .unwrap()
            .value;
        registry.register_device("dev_b", &recipient(2)).unwrap();
        // A hand-edited file blocks dev_a but leaves its recipient active.
        let path = root.join(HISTORY_DIR).join(FILE_NAME);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["revokedDevices"] = serde_json::json!([
            { "deviceId": "dev_a", "revokedAt": Utc::now(), "revokedBy": "dev_b" }
        ]);
        secure_fs::write_owner_only_atomic(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(!registry.is_active(&rid).unwrap());
        assert_eq!(registry.active_recipients().unwrap().keys.len(), 1);
        assert_eq!(registry.device_rid("dev_a").unwrap(), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn external_revocations_are_reported_once() {
        let root = temp_dir("external");
        let (devices, a) = paired(&root, 1);
        let (_, b) = paired(&root, 2);
        let registry =
            RecipientRegistry::load(&root, Some(devices.clone()), system_clock()).unwrap();
        registry.register_device(&a, &recipient(1)).unwrap();
        registry.register_device(&b, &recipient(2)).unwrap();
        assert!(registry.take_external_changes().unwrap().is_empty());

        // The TUI revokes `a` from its own process (devices.json + hook).
        assert!(crate::devices::revoke_device(&root, &a).unwrap());
        let changes = registry.take_external_changes().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].device_id, a);
        assert_eq!(changes[0].state.epoch, registry.snapshot().unwrap().epoch);
        assert!(registry.take_external_changes().unwrap().is_empty());
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.revoked_devices[0].revoked_by, REVOKED_BY_TUI);

        // `b` vanishes from devices.json without the hook: the daemon
        // revokes and blocks it itself and reports that too.
        let path = root.join("devices.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["devices"].as_object_mut().unwrap().remove(&b);
        secure_fs::write_owner_only_atomic(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        let changes = registry.take_external_changes().unwrap();
        assert_eq!(
            changes
                .iter()
                .map(|c| c.device_id.as_str())
                .collect::<Vec<_>>(),
            vec![b.as_str()]
        );
        let snapshot = registry.snapshot().unwrap();
        let entry = snapshot
            .revoked_devices
            .iter()
            .find(|entry| entry.device_id == b)
            .unwrap();
        assert_eq!(entry.revoked_by, REVOKED_BY_DAEMON);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_block_list_never_drops_entries() {
        let root = temp_dir("block-unbounded");
        let registry = registry(&root);
        registry.register_device("dev_a", &recipient(1)).unwrap();
        let ids = (0..300).map(|n| format!("dev_{n}")).collect::<Vec<_>>();
        revoke_device_recipients(&root, DeviceRevocation::All(&ids)).unwrap();
        revoke_device_recipients(&root, DeviceRevocation::One("dev_late")).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.revoked_devices.len(), 302);
        for device_id in ids.iter().map(String::as_str).chain(["dev_a", "dev_late"]) {
            assert_eq!(
                registry.ensure_access(device_id).unwrap_err().code(),
                "HISTORY_ACCESS_REVOKED"
            );
        }
        // A reload validates and keeps every entry.
        let reloaded = self::registry(&root);
        assert_eq!(reloaded.snapshot().unwrap().revoked_devices.len(), 302);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn files_without_revoked_devices_still_load() {
        let root = temp_dir("compat");
        let registry = registry(&root);
        registry.register_device("dev_a", &recipient(1)).unwrap();
        let path = root.join(HISTORY_DIR).join(FILE_NAME);
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // An empty list is not written, so older daemons can still read it.
        assert!(raw.get("revokedDevices").is_none());
        let reloaded = self::registry(&root);
        assert!(reloaded.snapshot().unwrap().revoked_devices.is_empty());
        assert!(reloaded.device_rid("dev_a").unwrap().is_some());

        assert_eq!(
            revoke_device_recipients(&root, DeviceRevocation::One("dev_a")).unwrap(),
            1
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["revokedDevices"][0]["deviceId"], "dev_a");
        assert_eq!(raw["revokedDevices"][0]["revokedBy"], "tui");
        assert!(raw["revokedDevices"][0]["revokedAt"].is_string());
        assert_eq!(raw["version"], 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn registry_file_is_private_and_validated() {
        let root = temp_dir("file");
        let registry = registry(&root);
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
        assert_eq!(value["mode"], "e2e");
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
