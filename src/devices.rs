//! Persistent registry of paired devices.
//!
//! The daemon registers a device when its pairing request is approved. The TUI
//! lists and revokes devices through the same file; the daemon notices file
//! changes by mtime so revocation applies without a restart. The file is the
//! source of truth: deleting it locks out every paired device.
use crate::error::AppError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, AppError>;

const FILE_NAME: &str = "devices.json";
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_DEVICES: usize = 256;
/// `lastSeenAt` updates are batched; auth verifies should not rewrite the file
/// on every request.
const SEEN_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeviceRecord {
    pub device_id: String,
    pub name: String,
    /// Ed25519 public key, base64url without padding.
    pub public_key: String,
    pub paired_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistryFile {
    version: u8,
    devices: BTreeMap<String, DeviceRecord>,
}

/// `dev_` + base64url of the first 12 bytes of the key fingerprint. Derived
/// from the public key, so re-pairing the same key updates the same record.
pub(crate) fn device_id_for(public_key: &[u8; 32]) -> String {
    let fingerprint = Sha256::digest(public_key);
    format!("dev_{}", URL_SAFE_NO_PAD.encode(&fingerprint[..12]))
}

pub(crate) fn parse_public_key(value: &str) -> Result<[u8; 32]> {
    if value.len() != 43 {
        return Err(invalid("invalid device public key"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| invalid("invalid device public key"))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("invalid device public key"))?;
    ed25519_dalek::VerifyingKey::from_bytes(&key)
        .map_err(|_| invalid("device public key is not a valid Ed25519 key"))?;
    Ok(key)
}

struct Inner {
    path: PathBuf,
    devices: BTreeMap<String, DeviceRecord>,
    mtime: Option<SystemTime>,
    dirty: bool,
    last_flush: Instant,
}

/// Daemon-side registry handle. Reads reload the file when another process
/// (the TUI) changed it; writes publish atomically.
#[derive(Clone)]
pub(crate) struct DeviceRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl DeviceRegistry {
    pub(crate) fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(FILE_NAME);
        let (devices, mtime) = read_records(&path)?.unwrap_or_default();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                devices,
                mtime,
                dirty: false,
                last_flush: Instant::now(),
            })),
        })
    }

    pub(crate) fn register(&self, name: &str, public_key: &[u8; 32]) -> Result<DeviceRecord> {
        let record = DeviceRecord {
            device_id: device_id_for(public_key),
            name: name.to_owned(),
            public_key: URL_SAFE_NO_PAD.encode(public_key),
            paired_at: unix_ms(),
            last_seen_at: None,
        };
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        if inner.devices.len() >= MAX_DEVICES && !inner.devices.contains_key(&record.device_id) {
            return Err(AppError::ResourceExhausted(
                "device registry is full; revoke a device first".to_owned(),
            ));
        }
        inner
            .devices
            .insert(record.device_id.clone(), record.clone());
        inner.write()?;
        Ok(record)
    }

    /// Returns the current record for `device_id`, reloading the file when the
    /// TUI (or another writer) changed it since the last read.
    pub(crate) fn get(&self, device_id: &str) -> Result<Option<DeviceRecord>> {
        let mut inner = self.lock()?;
        inner.reload_if_changed()?;
        Ok(inner.devices.get(device_id).cloned())
    }

    /// Record request activity. The timestamp is kept in memory and flushed at
    /// most once per SEEN_FLUSH_INTERVAL to avoid a disk write per request.
    pub(crate) fn touch(&self, device_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(record) = inner.devices.get_mut(device_id) {
                record.last_seen_at = Some(unix_ms());
                inner.dirty = true;
                if inner.last_flush.elapsed() >= SEEN_FLUSH_INTERVAL {
                    let _ = inner.write();
                }
            }
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| invalid("device registry is unavailable"))
    }
}

impl Inner {
    fn reload_if_changed(&mut self) -> Result<()> {
        let mtime = fs::metadata(&self.path)
            .and_then(|metadata| metadata.modified())
            .ok();
        if mtime != self.mtime {
            // An external writer (TUI revocation) wins over unflushed
            // last_seen updates; those timestamps are best-effort anyway.
            let (devices, mtime) = read_records(&self.path)?.unwrap_or_default();
            self.devices = devices;
            self.mtime = mtime;
            self.dirty = false;
            return Ok(());
        }
        if self.dirty {
            self.write()?;
        }
        Ok(())
    }

    fn write(&mut self) -> Result<()> {
        write_records(
            &self.path,
            &RegistryFile {
                version: 1,
                devices: self.devices.clone(),
            },
        )?;
        self.mtime = fs::metadata(&self.path)
            .and_then(|metadata| metadata.modified())
            .ok();
        self.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }
}

// File-level operations used by the TUI, which runs in a separate process from
// the daemon and reaches the registry through the shared file.
pub(crate) fn list_devices(data_dir: &Path) -> Result<Vec<DeviceRecord>> {
    let (devices, _) = read_records(&data_dir.join(FILE_NAME))?.unwrap_or_default();
    Ok(devices.into_values().collect())
}

pub(crate) fn revoke_device(data_dir: &Path, device_id: &str) -> Result<bool> {
    let path = data_dir.join(FILE_NAME);
    let (mut devices, _) = read_records(&path)?.unwrap_or_default();
    if devices.remove(device_id).is_none() {
        return Ok(false);
    }
    write_records(
        &path,
        &RegistryFile {
            version: 1,
            devices,
        },
    )?;
    Ok(true)
}

pub(crate) fn revoke_all_devices(data_dir: &Path) -> Result<()> {
    write_records(
        &data_dir.join(FILE_NAME),
        &RegistryFile {
            version: 1,
            devices: BTreeMap::new(),
        },
    )
}

type RecordsSnapshot = (BTreeMap<String, DeviceRecord>, Option<SystemTime>);

fn read_records(path: &Path) -> Result<Option<RecordsSnapshot>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_FILE_BYTES {
        return Err(invalid("invalid device registry file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid("device registry file is not private"));
        }
    }
    let bytes = fs::read(path)?;
    let file: RegistryFile = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(&format!("device registry file is unreadable: {error}")))?;
    if file.version != 1 || file.devices.len() > MAX_DEVICES {
        return Err(invalid("unsupported device registry file"));
    }
    for (id, record) in &file.devices {
        let public_key = parse_public_key(&record.public_key)
            .map_err(|_| invalid("device registry contains an invalid record"))?;
        if record.device_id != *id || device_id_for(&public_key) != *id {
            return Err(invalid("device registry contains an invalid record"));
        }
    }
    Ok(Some((file.devices, metadata.modified().ok())))
}

fn write_records(path: &Path, file: &RegistryFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(".devices-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut handle = options.open(&temporary)?;
        let bytes = serde_json::to_vec(file)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid("device registry file is too large"));
        }
        handle.write_all(&bytes)?;
        handle.sync_all()?;
        drop(handle);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> [u8; 32] {
        // Deterministic but valid Ed25519 public keys: derive them from fixed
        // signing keys instead of hand-picking points.
        let signing = ed25519_dalek::SigningKey::from_bytes(&[byte; 32]);
        signing.verifying_key().to_bytes()
    }

    fn fixture() -> (PathBuf, DeviceRegistry) {
        let root =
            std::env::temp_dir().join(format!("todex-devices-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let registry = DeviceRegistry::load(&root).unwrap();
        (root, registry)
    }

    #[test]
    fn register_list_revoke_roundtrip_is_stable_across_handles() {
        let (root, registry) = fixture();
        let public = key(7);
        let record = registry.register("Phone", &public).unwrap();
        assert!(record.device_id.starts_with("dev_"));
        assert_eq!(list_devices(&root).unwrap().len(), 1);

        // A second handle (the TUI process) sees the same file.
        let other = DeviceRegistry::load(&root).unwrap();
        assert_eq!(other.get(&record.device_id).unwrap().unwrap().name, "Phone");

        // Re-registering the same key updates the record in place.
        let updated = registry.register("Phone 2", &public).unwrap();
        assert_eq!(updated.device_id, record.device_id);
        assert_eq!(list_devices(&root).unwrap().len(), 1);

        // File-level revocation is observed by the live registry via mtime.
        assert!(revoke_device(&root, &record.device_id).unwrap());
        assert!(registry.get(&record.device_id).unwrap().is_none());
        assert!(!revoke_device(&root, &record.device_id).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn registry_file_rejects_foreign_keys_and_unknown_ids() {
        let (root, registry) = fixture();
        registry.register("Phone", &key(7)).unwrap();
        let path = root.join(FILE_NAME);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("private"));

        // A stored key that no longer matches its device id fingerprint is
        // rejected outright.
        let tampered = raw.replacen(
            &URL_SAFE_NO_PAD.encode(key(7)),
            &URL_SAFE_NO_PAD.encode(key(9)),
            1,
        );
        fs::write(&path, &tampered).unwrap();
        assert!(registry.get("dev_anything").is_err());
        fs::write(&path, "{\"version\":9,\"devices\":{}}").unwrap();
        assert!(registry.get("dev_anything").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unix_registry_files_are_owner_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (root, registry) = fixture();
            registry.register("Phone", &key(7)).unwrap();
            let mode = fs::metadata(root.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
            let _ = fs::remove_dir_all(&root);
        }
    }
}
