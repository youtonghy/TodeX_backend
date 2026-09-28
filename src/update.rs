//! Updates are installed only at launch, so no in-flight agent work is interrupted.
//! A running daemon checks periodically and restarts itself once Agents are idle.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "youtonghy/TodeX_backend";
const MAX_BINARY: usize = 256 * 1024 * 1024;
const RELAUNCHED_ENV: &str = "TODEX_UPDATE_RELAUNCHED";
/// Set by an idle daemon on the launch that replaces it. The replacement
/// inherits it, so it can tell that this version already failed to install.
const HANDOFF_ENV: &str = "TODEX_UPDATE_HANDOFF_VERSION";
/// Rollback copies are removed once they are older than this.
const BACKUP_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    enabled: bool,
    current_version: &'static str,
    latest_version: Option<String>,
    update_available: bool,
    installed: bool,
    #[serde(skip)]
    backup: Option<PathBuf>,
    #[serde(skip)]
    executable: Option<PathBuf>,
}

fn stable_version(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut version = [0; 3];
    for (index, part) in parts.into_iter().enumerate() {
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        version[index] = part.parse().ok()?;
    }
    Some(version)
}

fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Some("linux-x64-gnu"),
        ("macos", "aarch64") => Some("macos-arm64"),
        ("windows", "x86_64") => Some("windows-x64"),
        _ => None,
    }
}

fn eligible_version(version: &str) -> bool {
    stable_version(version).is_some_and(|value| value != [0, 0, 0])
}

pub fn enabled() -> bool {
    option_env!("TODEX_RELEASE_BUILD") == Some("1")
        && eligible_version(crate::version::APP_VERSION)
        && platform().is_some()
}

/// Automatic updates at launch and in a running daemon; `update` stays manual.
pub fn auto_update_enabled() -> bool {
    enabled() && std::env::var("TODEX_AUTO_UPDATE").as_deref() != Ok("0")
}

fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str> {
    let expected = format!(
        "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
        release.tag_name
    );
    let mut matches = release.assets.iter().filter(|asset| asset.name == name);
    let asset = matches
        .next()
        .context("release is missing required update asset")?;
    if matches.next().is_some() || asset.browser_download_url != expected {
        bail!("release asset URL or identity is invalid");
    }
    Ok(&asset.browser_download_url)
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        bail!("update response exceeds size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("update response exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn verify_checksum(manifest: &str, name: &str, bytes: &[u8]) -> Result<()> {
    let hashes: Vec<_> = manifest
        .lines()
        .filter_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name).then_some(hash)
        })
        .collect();
    let actual = format!("{:x}", Sha256::digest(bytes));
    if hashes.len() != 1 || hashes[0] != actual {
        bail!("update SHA-256 checksum mismatch or missing checksum");
    }
    Ok(())
}

struct UpdateLock(PathBuf);
impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn install(
    executable: &Path,
    bytes: &[u8],
    expected_version: Option<&str>,
) -> Result<PathBuf> {
    let lock_path = executable.with_extension("update-lock");
    fs::create_dir(&lock_path)
        .context("cannot acquire update lock (another update may be running)")?;
    let _lock = UpdateLock(lock_path.clone());
    let staged = lock_path.join(if cfg!(windows) { "new.exe" } else { "new" });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::set_permissions(&staged, fs::metadata(executable)?.permissions())?;
    if let Some(version) = expected_version {
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&staged)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("downloaded executable validation timed out")??;
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout).trim() != format!("todex-agentd {version}")
        {
            bail!("downloaded executable version validation failed");
        }
    }
    // Same `previous-<UTC timestamp>-<id>` form as install.sh, so pruning can
    // date the copy by name; hard links and renames keep the old mtime.
    let backup = executable.with_extension(format!(
        "previous-{}-{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    #[cfg(unix)]
    {
        // Preserve the old inode, then atomically replace the installed name.
        // A crash cannot leave the installed path missing on Unix.
        fs::hard_link(executable, &backup).context("cannot back up installed executable")?;
        if let Err(error) = fs::rename(&staged, executable) {
            let _ = fs::remove_file(&backup);
            return Err(error).context("cannot install new executable; original unchanged");
        }
    }
    #[cfg(not(unix))]
    {
        // Windows permits renaming the running image, but not overwriting it.
        fs::rename(executable, &backup).context("cannot back up installed executable")?;
        if let Err(error) = fs::rename(&staged, executable) {
            fs::rename(&backup, executable).context("update failed and backup restore failed")?;
            return Err(error).context("cannot install new executable; original restored");
        }
    }
    Ok(backup)
}

pub async fn run(check_only: bool) -> Result<UpdateStatus> {
    let mut status = UpdateStatus {
        enabled: enabled(),
        current_version: crate::version::APP_VERSION,
        latest_version: None,
        update_available: false,
        installed: false,
        backup: None,
        executable: None,
    };
    if !status.enabled {
        return Ok(status);
    }
    let client = reqwest::Client::builder()
        .user_agent(format!("todex-agentd/{}", status.current_version))
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .build()?;
    let metadata = tokio::time::timeout(
        Duration::from_secs(10),
        download(
            &client,
            &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
            2 * 1024 * 1024,
        ),
    )
    .await
    .context("release check timed out")??;
    let release: Release = serde_json::from_slice(&metadata)?;
    let version = release
        .tag_name
        .strip_prefix('v')
        .context("release tag must start with v")?;
    let latest = stable_version(version).context("release is not a stable version")?;
    if release.draft || release.prerelease {
        bail!("release is not published and stable");
    }
    status.latest_version = Some(version.to_owned());
    status.update_available = latest > stable_version(status.current_version).unwrap();
    if !status.update_available || check_only {
        return Ok(status);
    }
    let name = format!(
        "todex-agentd-v{version}-{}{}",
        platform().unwrap(),
        if cfg!(windows) { ".exe" } else { ".bin" }
    );
    let url = asset_url(&release, &name)?;
    let checksums_url = asset_url(&release, "SHA256SUMS")?;
    let manifest = download(&client, checksums_url, 1024 * 1024).await?;
    let bytes = download(&client, url, MAX_BINARY).await?;
    verify_checksum(std::str::from_utf8(&manifest)?, &name, &bytes)?;
    let executable = std::env::current_exe()?;
    let backup = install(&executable, &bytes, Some(version)).await?;
    eprintln!(
        "Installed backend {version}; previous executable: {}",
        backup.display()
    );
    status.executable = Some(executable);
    status.backup = Some(backup);
    status.installed = true;
    Ok(status)
}

fn restore_backup(backup: &Path, executable: &Path) -> Result<()> {
    fs::remove_file(executable).context("cannot remove failed update")?;
    fs::rename(backup, executable).context("cannot restore previous executable")
}

/// When the backup was made: from the timestamp in its name, or for the
/// older `previous-<uuid>` form, its modification time. `None` for files this
/// updater did not name, which are never removed.
fn backup_created_at(suffix: &str, modified: Option<SystemTime>) -> Option<SystemTime> {
    if let Some((stamp, id)) = suffix.split_once('-') {
        if stamp.len() == 14 && !id.is_empty() && stamp.bytes().all(|c| c.is_ascii_digit()) {
            let time = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d%H%M%S").ok()?;
            return Some(time.and_utc().into());
        }
    }
    uuid::Uuid::parse_str(suffix).ok().and(modified)
}

fn prune_backups_in(executable: &Path, now: SystemTime) -> Result<usize> {
    let stem = executable
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("executable name is not valid UTF-8")?;
    let prefix = format!("{stem}.previous-");
    let directory = executable
        .parent()
        .context("executable has no parent directory")?;
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
            continue;
        };
        // Symlinks and directories are never rollback copies.
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() && path != executable => metadata,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("Cannot inspect backend backup {}: {error}", path.display());
                continue;
            }
        };
        let Some(created) = backup_created_at(suffix, metadata.modified().ok()) else {
            continue;
        };
        if now
            .duration_since(created)
            .is_ok_and(|age| age > BACKUP_RETENTION)
        {
            match fs::remove_file(&path) {
                Ok(()) => {
                    eprintln!(
                        "Removed backend backup older than 30 days: {}",
                        path.display()
                    );
                    removed += 1;
                }
                Err(error) => {
                    eprintln!(
                        "Cannot remove old backend backup {}: {error}",
                        path.display()
                    )
                }
            }
        }
    }
    Ok(removed)
}

/// Removes rollback copies beside the executable once they exceed the
/// retention period. Failures are reported and retried at the next pass.
pub fn prune_backups() {
    if !enabled() {
        return;
    }
    let result = std::env::current_exe()
        .context("cannot resolve the current executable")
        .and_then(|executable| prune_backups_in(&executable, SystemTime::now()));
    if let Err(error) = result {
        eprintln!("Old backend backups were not pruned: {error:#}");
    }
}

/// Periodic check from a running daemon: the release to restart into, or
/// `None` when there is nothing new to install.
pub async fn check_for_daemon() -> Option<String> {
    prune_backups();
    if !auto_update_enabled() {
        return None;
    }
    let status = match run(true).await {
        Ok(status) => status,
        Err(error) => {
            eprintln!("Backend update check failed; retrying later: {error:#}");
            return None;
        }
    };
    let latest = status.latest_version.filter(|_| status.update_available)?;
    if std::env::var(HANDOFF_ENV).as_deref() == Ok(latest.as_str()) {
        // The restart meant to install this version came back unchanged
        // (see its startup log); restarting again would fail the same way.
        eprintln!(
            "Backend {latest} did not install on the last restart; staying on {} until a newer release or a manual restart",
            status.current_version
        );
        return None;
    }
    Some(latest)
}

/// Marks `command` as the fresh launch that replaces an idle daemon, so its
/// startup update runs even if this daemon was itself a relaunch.
pub fn prepare_handoff(command: &mut std::process::Command, version: &str) {
    command.env_remove(RELAUNCHED_ENV).env(HANDOFF_ENV, version);
}

/// Called only at an explicit service launch, never from a running server.
pub async fn before_start() -> Result<()> {
    prune_backups();
    if !auto_update_enabled() || std::env::var_os(RELAUNCHED_ENV).is_some() {
        return Ok(());
    }
    match run(false).await {
        Ok(status) if status.installed => {
            let mut command = std::process::Command::new(status.executable.as_deref().unwrap());
            command
                .args(std::env::args_os().skip(1))
                .env(RELAUNCHED_ENV, "1");
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let error = command.exec();
                restore_backup(
                    status.backup.as_deref().unwrap(),
                    status.executable.as_deref().unwrap(),
                )?;
                eprintln!("Updated executable could not start; original restored: {error}");
            }
            #[cfg(not(unix))]
            {
                match command.status() {
                    Ok(exit) => std::process::exit(exit.code().unwrap_or(1)),
                    Err(error) => {
                        restore_backup(
                            status.backup.as_deref().unwrap(),
                            status.executable.as_deref().unwrap(),
                        )?;
                        eprintln!("Updated executable could not start; original restored: {error}");
                    }
                }
            }
        }
        Ok(_) => {}
        Err(error) => eprintln!("Backend update skipped; continuing current version: {error:#}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_stable_numeric_versions_are_eligible() {
        for invalid in [
            "DEV0.0.0",
            "v1.2.3",
            "1.2.3-beta.1",
            "1.2.3+build",
            "01.2.3",
            "1.2",
            "1.2.3.4",
            "1.2.-3",
            "1.2.999999999999999999999999",
        ] {
            assert!(stable_version(invalid).is_none(), "{invalid}");
        }
        assert!(stable_version("1.10.0") > stable_version("1.9.9"));
        assert!(!eligible_version("0.0.0"));
        assert!(!eligible_version("DEV0.0.0"));
        assert!(eligible_version("0.0.1"));
    }

    #[test]
    fn checksum_rejects_missing_duplicate_and_corrupt_assets() {
        let manifest = format!("{:x}  binary\n", Sha256::digest(b"good"));
        assert!(verify_checksum(&manifest, "binary", b"good").is_ok());
        assert!(verify_checksum(&manifest, "binary", b"bad").is_err());
        assert!(verify_checksum(&manifest, "other", b"good").is_err());
        assert!(verify_checksum(&manifest.repeat(2), "binary", b"good").is_err());
    }

    #[test]
    fn asset_origin_and_name_must_match_release() {
        let mut release = Release {
            tag_name: "v1.2.3".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "binary".into(),
                browser_download_url: format!(
                    "https://github.com/{REPOSITORY}/releases/download/v1.2.3/binary"
                ),
            }],
        };
        assert!(asset_url(&release, "binary").is_ok());
        release.assets[0].browser_download_url = "https://evil.example/binary".into();
        assert!(asset_url(&release, "binary").is_err());
    }

    #[test]
    fn backup_age_comes_from_its_name_or_legacy_mtime() {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let stamped = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        // Built-in updater and install.sh (`<timestamp>-<pid>`) names.
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            backup_created_at(&format!("20260920T{id}"), Some(mtime)),
            None
        );
        assert_eq!(
            backup_created_at(&format!("20260921141320-{id}"), Some(mtime)),
            Some(stamped)
        );
        assert_eq!(
            backup_created_at("20260921141320-4242", None),
            Some(stamped)
        );
        // Earlier builds named copies `previous-<uuid>` only.
        assert_eq!(backup_created_at(&id.to_string(), Some(mtime)), Some(mtime));
        assert_eq!(backup_created_at("manual-copy", Some(mtime)), None);
        assert_eq!(backup_created_at("20269999999999-1", Some(mtime)), None);
    }

    #[test]
    fn pruning_removes_only_expired_rollback_copies() {
        let dir = std::env::temp_dir().join(format!("todex-prune-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let exe = dir.join("todex-agentd.exe");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let legacy_expired = dir.join(format!("todex-agentd.previous-{}", uuid::Uuid::new_v4()));
        let kept = [
            exe.clone(),
            dir.join("todex-agentd.previous-20260901000000-1"),
            dir.join("todex-agentd.previous-notes"),
            dir.join("other.previous-20200101000000-1"),
        ];
        let expired = [
            dir.join("todex-agentd.previous-20260801000000-1"),
            legacy_expired.clone(),
        ];
        for path in kept.iter().chain(&expired) {
            fs::write(path, b"binary").unwrap();
        }
        fs::create_dir(dir.join("todex-agentd.previous-20200101000000-dir")).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&legacy_expired)
            .unwrap()
            .set_modified(now - BACKUP_RETENTION - Duration::from_secs(1))
            .unwrap();

        assert_eq!(prune_backups_in(&exe, now).unwrap(), 2);
        assert!(kept.iter().all(|path| path.exists()));
        assert!(expired.iter().all(|path| !path.exists()));
        assert!(dir
            .join("todex-agentd.previous-20200101000000-dir")
            .is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn installation_preserves_backup_and_refuses_concurrent_update() {
        let dir = std::env::temp_dir().join(format!("todex-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let exe = dir.join("agent");
        fs::write(&exe, b"old").unwrap();
        let backup = install(&exe, b"new", None).await.unwrap();
        let suffix = backup.file_name().unwrap().to_str().unwrap();
        assert!(backup_created_at(suffix.strip_prefix("agent.previous-").unwrap(), None).is_some());
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(backup).unwrap(), b"old");
        assert!(install(&exe, b"not an executable", Some("1.2.3"))
            .await
            .is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        fs::create_dir(exe.with_extension("update-lock")).unwrap();
        assert!(install(&exe, b"bad", None).await.is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        fs::remove_dir_all(dir).unwrap();
    }
}
