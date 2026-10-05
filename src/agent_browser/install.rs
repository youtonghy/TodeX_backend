//! The Chromium the agent browser runs: Chrome for Testing at a version
//! pinned by this daemon release, downloaded on first use into
//! `$DATA_DIR/agent-browser/chromium/<version>/` and verified against the
//! SHA-256 recorded here. `TODEX_AGENT_BROWSER_PATH` points at another
//! Chromium for development.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::BrowserError;

/// Chrome for Testing stable at the time of this release.
pub(crate) const CHROMIUM_VERSION: &str = "154.0.8037.92";
const DOWNLOAD_BASE: &str = "https://storage.googleapis.com/chrome-for-testing-public";
pub(crate) const PATH_OVERRIDE_ENV: &str = "TODEX_AGENT_BROWSER_PATH";

/// One platform's package: path under the version directory to the
/// executable, archive name and its SHA-256.
struct Package {
    platform: &'static str,
    archive: &'static str,
    sha256: &'static str,
    executable: &'static str,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const PACKAGE: Option<Package> = Some(Package {
    platform: "mac-arm64",
    archive: "chrome-mac-arm64.zip",
    sha256: "b62e904b6571c5ff5108ed7812cf93ac6d1c4027f10ae47ac34d8e229ed88001",
    executable:
        "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
});
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PACKAGE: Option<Package> = Some(Package {
    platform: "linux64",
    archive: "chrome-linux64.zip",
    sha256: "ff43322f335e436b2f4dcdfeeec5db032299e335a7e8c1c618b326e100ce8732",
    executable: "chrome-linux64/chrome",
});
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const PACKAGE: Option<Package> = Some(Package {
    platform: "win64",
    archive: "chrome-win64.zip",
    sha256: WIN64_SHA256,
    executable: "chrome-win64/chrome.exe",
});
#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "x86_64")
)))]
const PACKAGE: Option<Package> = None;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const WIN64_SHA256: &str = "b897ef3601c947ac0620c784556dec719ac602b0159ce105927acf645ee0f598";

/// What the settings screens show about the browser.
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
    /// A development override is in use.
    pub overridden: bool,
}

#[derive(Clone)]
pub(crate) struct Installer {
    root: PathBuf,
    state: Arc<Mutex<InstallState>>,
    running: Arc<tokio::sync::Mutex<()>>,
}

impl Installer {
    pub(crate) fn new(data_dir: &Path) -> Self {
        let installer = Self {
            root: data_dir.join("agent-browser").join("chromium"),
            state: Arc::new(Mutex::new(InstallState {
                version: CHROMIUM_VERSION,
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
        self.root.join(CHROMIUM_VERSION)
    }

    /// The Chromium to launch, if one is available.
    pub(crate) fn executable(&self) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(PATH_OVERRIDE_ENV) {
            return Some(PathBuf::from(path)).filter(|path| path.is_file());
        }
        let package = PACKAGE.as_ref()?;
        Some(self.version_dir().join(package.executable)).filter(|path| path.is_file())
    }

    /// Downloads, verifies and unpacks the pinned Chromium (once at a time),
    /// then removes other versions. Progress and failures land in
    /// [`Self::state`].
    pub(crate) async fn install(&self) -> Result<PathBuf, BrowserError> {
        let _one = self.running.lock().await;
        if let Some(path) = self.executable() {
            return Ok(path);
        }
        let package = PACKAGE.as_ref().ok_or_else(|| {
            BrowserError::new(
                "UNSUPPORTED",
                format!(
                    "no Chromium build for {}-{}",
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

    async fn download_and_unpack(&self, package: &Package) -> Result<PathBuf, BrowserError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|error| {
                BrowserError::failed(format!("cannot create {}: {error}", self.root.display()))
            })?;
        let url = format!(
            "{DOWNLOAD_BASE}/{CHROMIUM_VERSION}/{}/{}",
            package.platform, package.archive
        );
        let archive = self.root.join(format!("{CHROMIUM_VERSION}.zip.partial"));
        let response = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| BrowserError::failed(format!("cannot download Chromium: {error}")))?;
        let total = response.content_length();
        let mut file = tokio::fs::File::create(&archive).await.map_err(|error| {
            BrowserError::failed(format!("cannot write {}: {error}", archive.display()))
        })?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|error| {
                BrowserError::failed(format!("Chromium download failed: {error}"))
            })?;
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|error| {
                BrowserError::failed(format!("cannot write the download: {error}"))
            })?;
            received += chunk.len() as u64;
            if let Some(total) = total.filter(|total| *total > 0) {
                self.lock().progress = Some(received as f64 / total as f64);
            }
        }
        file.flush()
            .await
            .map_err(|error| BrowserError::failed(format!("cannot write the download: {error}")))?;
        drop(file);
        let digest = format!("{:x}", hasher.finalize());
        if !digest.eq_ignore_ascii_case(package.sha256) {
            let _ = tokio::fs::remove_file(&archive).await;
            return Err(BrowserError::failed(format!(
                "the Chromium download is corrupt (SHA-256 {digest}); try again"
            )));
        }
        let staging = self.root.join(format!("{CHROMIUM_VERSION}.partial"));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        let unpacked = {
            let (archive, staging) = (archive.clone(), staging.clone());
            tokio::task::spawn_blocking(move || unpack(&archive, &staging))
                .await
                .map_err(|error| BrowserError::failed(error.to_string()))?
        };
        let _ = tokio::fs::remove_file(&archive).await;
        unpacked?;
        let target = self.version_dir();
        let _ = tokio::fs::remove_dir_all(&target).await;
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| BrowserError::failed(format!("cannot install Chromium: {error}")))?;
        let executable = target.join(package.executable);
        if !executable.is_file() {
            return Err(BrowserError::failed(format!(
                "the Chromium package has no {}",
                package.executable
            )));
        }
        Ok(executable)
    }

    /// Older pinned versions (after a daemon update). Failures are logged:
    /// a running old Chromium may hold files open on Windows.
    async fn remove_other_versions(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name() == CHROMIUM_VERSION {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_file(&path).await
            };
            if let Err(error) = removed {
                tracing::warn!(%error, path = %path.display(), "cannot remove an old Chromium");
            }
        }
    }
}

/// macOS keeps the app bundle's symlinks and signature with `ditto`; the
/// Linux and Windows packages have neither and unpack in process.
fn unpack(archive: &Path, destination: &Path) -> Result<(), BrowserError> {
    std::fs::create_dir_all(destination)
        .map_err(|error| BrowserError::failed(format!("cannot unpack Chromium: {error}")))?;
    if cfg!(target_os = "macos") {
        let status = std::process::Command::new("ditto")
            .arg("-x")
            .arg("-k")
            .arg(archive)
            .arg(destination)
            .status()
            .map_err(|error| BrowserError::failed(format!("cannot run ditto: {error}")))?;
        if !status.success() {
            return Err(BrowserError::failed(format!("ditto failed: {status}")));
        }
        return Ok(());
    }
    let file = std::fs::File::open(archive)
        .map_err(|error| BrowserError::failed(format!("cannot open the download: {error}")))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| {
        BrowserError::failed(format!("the Chromium package is not a zip: {error}"))
    })?;
    zip.extract(destination)
        .map_err(|error| BrowserError::failed(format!("cannot unpack Chromium: {error}")))
}
