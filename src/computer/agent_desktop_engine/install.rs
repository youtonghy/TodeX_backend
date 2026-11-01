//! The agent-desktop CLI the optional engine runs: the release pinned here,
//! downloaded on first use into `$DATA_DIR/agent-desktop/engine/<version>/`
//! and verified against the SHA-256 recorded here (from the release's
//! `checksums.txt`). `TODEX_AGENT_DESKTOP_PATH` points at another build for
//! development.

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::super::ComputerError;

pub(crate) const AGENT_DESKTOP_VERSION: &str = "1.0.2";
const DOWNLOAD_BASE: &str = "https://github.com/vercel-labs/agent-desktop/releases/download";
pub(crate) const PATH_OVERRIDE_ENV: &str = "TODEX_AGENT_DESKTOP_PATH";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// The archives are a few MB; anything far larger is not the release.
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// One platform's release archive: its target triple, SHA-256, and the
/// files it holds (the CLI first; helpers must sit beside it).
struct Package {
    target: &'static str,
    sha256: &'static str,
    files: &'static [&'static str],
}

const PACKAGES: [Package; 6] = [
    Package {
        target: "aarch64-apple-darwin",
        sha256: "7bee385196d7b478de80cf05ca4304aaa38f0eba593b3406ad3c247c1773be19",
        files: &["agent-desktop", "agent-desktop-macos-helper"],
    },
    Package {
        target: "x86_64-apple-darwin",
        sha256: "8edda6df17eafc042395502a823dbb8415bc8ac14835a6731e4d8becc9cb4121",
        files: &["agent-desktop", "agent-desktop-macos-helper"],
    },
    Package {
        target: "x86_64-unknown-linux-gnu",
        sha256: "ad661b7a3fd8f990a769243f4889a240ca0e2c8dab3ef5bddd6019086c71e0b4",
        files: &["agent-desktop", "agent-desktop-linux-clipboard-owner"],
    },
    Package {
        target: "aarch64-unknown-linux-gnu",
        sha256: "864cb782e0afc8bf0df19622016b24bdbd1a83fc7749f784ee82aa49a7f845eb",
        files: &["agent-desktop", "agent-desktop-linux-clipboard-owner"],
    },
    Package {
        target: "x86_64-pc-windows-msvc",
        sha256: "2186c76e7df197c0e67365e45d36d869f74714c56bb8a73817e2a66f3e18e222",
        files: &["agent-desktop.exe"],
    },
    Package {
        target: "aarch64-pc-windows-msvc",
        sha256: "a0ecd6e17e60a0bcfbb45533a72f2d3a7a73508769588273ef08b0477d2b9687",
        files: &["agent-desktop.exe"],
    },
];

fn package_for(os: &str, arch: &str) -> Option<&'static Package> {
    let target = match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return None,
    };
    PACKAGES.iter().find(|package| package.target == target)
}

fn current_package() -> Option<&'static Package> {
    package_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// What the settings screens show about the engine's install.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallState {
    pub version: &'static str,
    pub installed: bool,
    pub downloading: bool,
    /// 0..=1 while downloading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// [`PATH_OVERRIDE_ENV`] is in use.
    pub overridden: bool,
}

#[derive(Clone)]
pub(crate) struct Installer {
    root: PathBuf,
    state: Arc<Mutex<InstallState>>,
    running: Arc<tokio::sync::Mutex<()>>,
}

impl Installer {
    pub(crate) fn new(root: PathBuf) -> Self {
        let installer = Self {
            root,
            state: Arc::new(Mutex::new(InstallState {
                version: AGENT_DESKTOP_VERSION,
                ..InstallState::default()
            })),
            running: Arc::new(tokio::sync::Mutex::new(())),
        };
        installer.refresh();
        installer
    }

    pub(crate) fn state(&self) -> InstallState {
        self.refresh();
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InstallState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refresh(&self) {
        let overridden = std::env::var_os(PATH_OVERRIDE_ENV).is_some();
        let installed = self.executable().is_some();
        let mut state = self.lock();
        state.overridden = overridden;
        state.installed = installed;
    }

    fn version_dir(&self) -> PathBuf {
        self.root.join(AGENT_DESKTOP_VERSION)
    }

    /// The CLI to run, if one is available.
    pub(crate) fn executable(&self) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(PATH_OVERRIDE_ENV) {
            return Some(PathBuf::from(path)).filter(|path| path.is_file());
        }
        let package = current_package()?;
        let dir = self.version_dir();
        package
            .files
            .iter()
            .all(|file| dir.join(file).is_file())
            .then(|| dir.join(package.files[0]))
    }

    /// Downloads and installs the pinned release in the background unless
    /// it is there or already downloading.
    pub(crate) fn start_install(&self) {
        if cfg!(test) || self.executable().is_some() || self.state().downloading {
            return;
        }
        let installer = self.clone();
        tokio::spawn(async move {
            if let Err(error) = installer.install().await {
                tracing::warn!(%error, "agent-desktop engine: install failed");
            }
        });
    }

    /// Downloads, verifies and unpacks the pinned release (once at a
    /// time), then removes other versions. Progress and failures land in
    /// [`Self::state`].
    pub(crate) async fn install(&self) -> Result<PathBuf, ComputerError> {
        let _one = self.running.lock().await;
        if let Some(path) = self.executable() {
            return Ok(path);
        }
        let package = current_package().ok_or_else(|| {
            ComputerError::new(
                "UNSUPPORTED",
                format!(
                    "agent-desktop has no build for {}-{}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
        })?;
        {
            let mut state = self.lock();
            state.downloading = true;
            state.progress = Some(0.0);
            state.error = None;
        }
        let result = self.download_and_unpack(package).await;
        {
            let mut state = self.lock();
            state.downloading = false;
            state.progress = None;
            state.error = result.as_ref().err().map(|error| error.message.clone());
        }
        let path = result?;
        self.remove_other_versions().await;
        self.refresh();
        Ok(path)
    }

    async fn download_and_unpack(&self, package: &Package) -> Result<PathBuf, ComputerError> {
        let failed = |message: String| ComputerError::new("ENGINE_INSTALL_FAILED", message);
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|error| failed(format!("cannot create {}: {error}", self.root.display())))?;
        let url = format!(
            "{DOWNLOAD_BASE}/v{AGENT_DESKTOP_VERSION}/agent-desktop-v{AGENT_DESKTOP_VERSION}-{}.tar.gz",
            package.target
        );
        let response = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|error| failed(format!("cannot download agent-desktop: {error}")))?
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| failed(format!("cannot download agent-desktop: {error}")))?;
        let total = response.content_length();
        let archive = self
            .root
            .join(format!("{AGENT_DESKTOP_VERSION}.tar.gz.partial"));
        let mut file = tokio::fs::File::create(&archive)
            .await
            .map_err(|error| failed(format!("cannot write {}: {error}", archive.display())))?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk =
                chunk.map_err(|error| failed(format!("agent-desktop download failed: {error}")))?;
            received += chunk.len() as u64;
            if received > MAX_ARCHIVE_BYTES {
                drop(file);
                let _ = tokio::fs::remove_file(&archive).await;
                return Err(failed("the agent-desktop download is too large".to_owned()));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| failed(format!("cannot write the download: {error}")))?;
            if let Some(total) = total.filter(|total| *total > 0) {
                self.lock().progress = Some(received as f64 / total as f64);
            }
        }
        file.flush()
            .await
            .map_err(|error| failed(format!("cannot write the download: {error}")))?;
        drop(file);
        let digest = format!("{:x}", hasher.finalize());
        if !digest.eq_ignore_ascii_case(package.sha256) {
            let _ = tokio::fs::remove_file(&archive).await;
            return Err(failed(format!(
                "the agent-desktop download is corrupt (SHA-256 {digest}); try again"
            )));
        }
        let staging = self.root.join(format!("{AGENT_DESKTOP_VERSION}.partial"));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        let unpacked = {
            let (archive, staging) = (archive.clone(), staging.clone());
            let files = package.files;
            tokio::task::spawn_blocking(move || unpack(&archive, &staging, files))
                .await
                .map_err(|error| failed(error.to_string()))?
        };
        let _ = tokio::fs::remove_file(&archive).await;
        unpacked.map_err(failed)?;
        let target = self.version_dir();
        let _ = tokio::fs::remove_dir_all(&target).await;
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| failed(format!("cannot install agent-desktop: {error}")))?;
        Ok(target.join(package.files[0]))
    }

    /// Versions other than the pinned one (after a daemon update).
    async fn remove_other_versions(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name() == AGENT_DESKTOP_VERSION {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_file(&path).await
            };
            if let Err(error) = removed {
                tracing::warn!(%error, path = %path.display(), "cannot remove an old agent-desktop");
            }
        }
    }
}

/// Extracts exactly `files` (regular files at the archive's top level) into
/// `destination`; anything else in the archive is ignored, so no entry can
/// write outside it or plant a link.
fn unpack(archive: &Path, destination: &Path, files: &[&str]) -> Result<(), String> {
    std::fs::create_dir_all(destination)
        .map_err(|error| format!("cannot unpack agent-desktop: {error}"))?;
    let file = std::fs::File::open(archive)
        .map_err(|error| format!("cannot open the download: {error}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = tar
        .entries()
        .map_err(|error| format!("the agent-desktop package is not a tar.gz: {error}"))?;
    let mut found = Vec::new();
    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("cannot read the agent-desktop package: {error}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| format!("cannot read the agent-desktop package: {error}"))?
            .into_owned();
        let Some(name) = path.to_str().map(|name| name.trim_start_matches("./")) else {
            continue;
        };
        let Some(wanted) = files.iter().find(|file| **file == name) else {
            continue;
        };
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot unpack {name}: {error}"))?;
        let out = destination.join(wanted);
        std::fs::write(&out, &bytes).map_err(|error| format!("cannot unpack {name}: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("cannot unpack {name}: {error}"))?;
        }
        found.push(*wanted);
    }
    match files.iter().find(|file| !found.contains(file)) {
        Some(missing) => Err(format!("the agent-desktop package has no {missing}")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn every_supported_host_has_a_package() {
        for (os, arch) in [
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            let package = package_for(os, arch).expect("package");
            assert_eq!(package.sha256.len(), 64);
        }
        assert!(package_for("freebsd", "x86_64").is_none());
    }

    #[tokio::test]
    #[ignore = "downloads the pinned release from GitHub"]
    async fn the_pinned_release_downloads_and_verifies() {
        let root = std::env::temp_dir().join(format!("todex-ad-{}", uuid::Uuid::new_v4()));
        let installer = Installer::new(root.join("engine"));
        let path = installer.install().await.unwrap();
        assert!(path.is_file());
        assert!(installer.state().installed);
        let output = std::process::Command::new(&path)
            .arg("version")
            .output()
            .unwrap();
        let version: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(version["data"]["version"], AGENT_DESKTOP_VERSION);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unpacking_takes_only_the_expected_files() {
        let dir = std::env::temp_dir().join(format!("todex-ad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("a.tar.gz");
        std::fs::write(
            &archive,
            archive_with(&[("agent-desktop", b"cli"), ("helper", b"h"), ("extra", b"x")]),
        )
        .unwrap();
        let out = dir.join("out");
        unpack(&archive, &out, &["agent-desktop", "helper"]).unwrap();
        assert_eq!(std::fs::read(out.join("agent-desktop")).unwrap(), b"cli");
        assert!(out.join("helper").is_file());
        assert!(!out.join("extra").exists());

        let missing = unpack(&archive, &dir.join("out2"), &["agent-desktop", "absent"]);
        assert!(missing.unwrap_err().contains("absent"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
