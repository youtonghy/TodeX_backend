//! The Chromium the agent browser runs: Chrome for Testing at a version
//! pinned by this daemon release, downloaded on first use into
//! `$DATA_DIR/agent-browser/chromium/<version>/` and verified against the
//! SHA-256 recorded here. `TODEX_AGENT_BROWSER_PATH` points at another
//! Chromium for development.

use std::{
    fs::TryLockError,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::BrowserError;

/// Chrome for Testing stable at the time of this release.
pub(crate) const CHROMIUM_VERSION: &str = "155.0.8059.39";
const DOWNLOAD_BASE: &str = "https://storage.googleapis.com/chrome-for-testing-public";
pub(crate) const PATH_OVERRIDE_ENV: &str = "TODEX_AGENT_BROWSER_PATH";
/// Held (exclusive file lock) by whichever process, daemon or
/// `browser install`, is downloading; lives in the chromium root.
const LOCK_FILE: &str = ".install.lock";
/// The archives are ~200 MB, so no total timeout: a stalled connection is
/// caught by the connect and per-read timeouts instead.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a process waiting for another's install checks for the result.
const BUSY_POLL: Duration = Duration::from_millis(500);

/// One platform's package: path under the version directory to the
/// executable, archive name and its SHA-256.
struct Package {
    platform: &'static str,
    archive: &'static str,
    sha256: &'static str,
    executable: &'static str,
}

const PACKAGES: [Package; 5] = [
    Package {
        platform: "mac-arm64",
        archive: "chrome-mac-arm64.zip",
        sha256: "529a71bd61aaa2ef266a4d4bd300ae9572ba6a3468a8d55c3023ffeffb5b6b4e",
        executable: "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    },
    Package {
        platform: "mac-x64",
        archive: "chrome-mac-x64.zip",
        sha256: "9e4da7961d4a426078f02e89ecfa569a7926ada2dc88aa53c0bd9ed0d46c3bb7",
        executable: "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    },
    Package {
        platform: "linux64",
        archive: "chrome-linux64.zip",
        sha256: "55672d1f392fd3e7b7a08621b6e804e6bcb39d40cf155504abb74b3a021ea8ea",
        executable: "chrome-linux64/chrome",
    },
    Package {
        platform: "linux-arm64",
        archive: "chrome-linux-arm64.zip",
        sha256: "b9d44e5d183260ca941a4c4d8d21c8a437d81ef778a489047ef98968b0475dc5",
        executable: "chrome-linux-arm64/chrome",
    },
    Package {
        platform: "win64",
        archive: "chrome-win64.zip",
        sha256: "59ab2a6e99bde9c0bc180414988394f9f5355a0d3764c704151ee8ecea235c7e",
        executable: "chrome-win64/chrome.exe",
    },
];

/// The package for an `std::env::consts` OS / architecture pair.
fn package_for(os: &str, arch: &str) -> Option<&'static Package> {
    let platform = match (os, arch) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("linux", "x86_64") => "linux64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win64",
        _ => return None,
    };
    PACKAGES.iter().find(|package| package.platform == platform)
}

fn current_package() -> Option<&'static Package> {
    package_for(std::env::consts::OS, std::env::consts::ARCH)
}

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
        self.installed_executable(current_package()?)
    }

    fn installed_executable(&self, package: &Package) -> Option<PathBuf> {
        Some(self.version_dir().join(package.executable)).filter(|path| path.is_file())
    }

    /// Downloads, verifies and unpacks the pinned Chromium (once at a time,
    /// across processes too), then removes other versions. Progress and
    /// failures land in [`Self::state`].
    pub(crate) async fn install(&self) -> Result<PathBuf, BrowserError> {
        if let Some(path) = self.executable() {
            return Ok(path);
        }
        let package = current_package().ok_or_else(|| {
            BrowserError::new(
                "UNSUPPORTED",
                format!(
                    "no Chromium build for {}-{}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
        })?;
        self.install_with(package, || self.download_and_unpack(package))
            .await
    }

    /// [`Self::install`] with the download-and-unpack step injectable, so
    /// tests can exercise the locking without a network.
    async fn install_with<F, Fut>(
        &self,
        package: &Package,
        download: F,
    ) -> Result<PathBuf, BrowserError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<PathBuf, BrowserError>>,
    {
        let _one = self.running.lock().await;
        if let Some(path) = self.installed_executable(package) {
            return Ok(path);
        }
        let downloading = DownloadingGuard::start(&self.state);
        let result = self.install_locked(package, download).await;
        downloading.finish(result.as_ref().err().map(|error| error.message.clone()));
        let path = result?;
        self.refresh();
        Ok(path)
    }

    async fn install_locked<F, Fut>(
        &self,
        package: &Package,
        download: F,
    ) -> Result<PathBuf, BrowserError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<PathBuf, BrowserError>>,
    {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|error| {
                BrowserError::failed(format!("cannot create {}: {error}", self.root.display()))
            })?;
        // Held until the end: the download, unpacking, rename and cleanup
        // all touch files another process's install would also touch.
        let _lock = match self.acquire_lock(package).await? {
            Acquired::Lock(lock) => lock,
            Acquired::Installed(path) => return Ok(path),
        };
        // The process that held the lock before may have finished.
        if let Some(path) = self.installed_executable(package) {
            return Ok(path);
        }
        let path = download().await?;
        self.remove_other_versions().await;
        Ok(path)
    }

    /// Takes the cross-process install lock. While another process holds
    /// it, waits for that process's result instead.
    async fn acquire_lock(&self, package: &Package) -> Result<Acquired, BrowserError> {
        let path = self.root.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                BrowserError::failed(format!("cannot open {}: {error}", path.display()))
            })?;
        let mut announced = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Acquired::Lock(file)),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(BrowserError::failed(format!(
                        "cannot lock {}: {error}",
                        path.display()
                    )))
                }
            }
            if let Some(path) = self.installed_executable(package) {
                return Ok(Acquired::Installed(path));
            }
            if !announced {
                tracing::info!("agent browser: Chromium is installing in another process; waiting");
                announced = true;
            }
            tokio::time::sleep(BUSY_POLL).await;
        }
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
        let response = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|error| BrowserError::failed(format!("cannot download Chromium: {error}")))?
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

    /// Older pinned versions (after a daemon update); call with the install
    /// lock held. Failures are logged:
    /// a running old Chromium may hold files open on Windows.
    async fn remove_other_versions(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            // The lock file is what keeps other processes out; their
            // `*.partial` files are safe to remove only because the caller
            // holds that lock.
            if entry.file_name() == CHROMIUM_VERSION || entry.file_name() == LOCK_FILE {
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

/// The result of waiting for the install lock.
enum Acquired {
    Lock(std::fs::File),
    /// Another process finished installing while this one waited.
    Installed(PathBuf),
}

/// Marks the install as running and, however the install ends (return,
/// error, panic or a dropped future), clears it so the next
/// `start_install` can retry.
struct DownloadingGuard<'a> {
    state: &'a Mutex<InstallState>,
}

impl<'a> DownloadingGuard<'a> {
    fn start(state: &'a Mutex<InstallState>) -> Self {
        let guard = Self { state };
        let mut state = guard.lock();
        state.downloading = true;
        state.progress = Some(0.0);
        state.error = None;
        drop(state);
        guard
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InstallState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records how the install ended (the flags reset on drop).
    fn finish(self, error: Option<String>) {
        self.lock().error = error;
    }
}

impl Drop for DownloadingGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.lock();
        state.downloading = false;
        state.progress = None;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;
    use uuid::Uuid;

    use super::*;

    /// A package whose "executable" is a plain file the fake download writes.
    const FAKE: Package = Package {
        platform: "fake",
        archive: "fake.zip",
        sha256: "",
        executable: "chrome",
    };

    fn temp_data_dir() -> PathBuf {
        std::env::temp_dir().join(format!("todex-install-{}", Uuid::new_v4()))
    }

    #[test]
    fn every_package_has_a_sha256_and_matching_paths() {
        for package in &PACKAGES {
            assert_eq!(package.sha256.len(), 64, "{}", package.platform);
            assert!(
                package.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}",
                package.platform
            );
            assert_eq!(package.archive, format!("chrome-{}.zip", package.platform));
            assert!(
                package
                    .executable
                    .starts_with(&format!("chrome-{}/", package.platform)),
                "{}",
                package.platform
            );
        }
        let mut hashes = PACKAGES.iter().map(|p| p.sha256).collect::<Vec<_>>();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), PACKAGES.len());
    }

    #[test]
    fn platforms_map_to_packages() {
        for (os, arch, platform) in [
            ("macos", "aarch64", "mac-arm64"),
            ("macos", "x86_64", "mac-x64"),
            ("linux", "x86_64", "linux64"),
            ("linux", "aarch64", "linux-arm64"),
            ("windows", "x86_64", "win64"),
        ] {
            assert_eq!(package_for(os, arch).unwrap().platform, platform);
        }
        assert!(package_for("windows", "aarch64").is_none());
        assert!(package_for("freebsd", "x86_64").is_none());
    }

    #[test]
    fn the_downloading_flag_resets_however_the_install_ends() {
        let state = Mutex::new(InstallState::default());
        let started = || state.lock().unwrap().downloading;

        let guard = DownloadingGuard::start(&state);
        assert!(started());
        guard.finish(Some("boom".to_owned()));
        assert!(!started());
        assert_eq!(state.lock().unwrap().error.as_deref(), Some("boom"));

        // A cancelled install (dropped future) must not leave it stuck.
        drop(DownloadingGuard::start(&state));
        assert!(!started());
        assert!(state.lock().unwrap().error.is_none());

        // Nor a panic, which also poisons nothing the next install needs.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = DownloadingGuard::start(&state);
            panic!("download task panicked");
        }));
        assert!(panicked.is_err());
        assert!(!started());
        assert!(state.lock().unwrap().progress.is_none());
    }

    #[tokio::test]
    async fn a_second_installer_waits_for_the_first_and_leaves_its_partials() {
        let data_dir = temp_data_dir();
        let first = Installer::new(&data_dir);
        let second = Installer::new(&data_dir);
        let partial = first.root.join(format!("{CHROMIUM_VERSION}.zip.partial"));
        let downloaded = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let second_downloads = Arc::new(AtomicUsize::new(0));

        let first_task = {
            let (partial, downloaded, release) =
                (partial.clone(), downloaded.clone(), release.clone());
            let first = first.clone();
            tokio::spawn(async move {
                first
                    .install_with(&FAKE, || async {
                        std::fs::write(&partial, b"half").unwrap();
                        downloaded.notify_one();
                        release.notified().await;
                        let executable = first.version_dir().join(FAKE.executable);
                        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
                        std::fs::write(&executable, b"chrome").unwrap();
                        std::fs::remove_file(&partial).unwrap();
                        Ok(executable)
                    })
                    .await
            })
        };
        downloaded.notified().await;

        let second_task = {
            let second_downloads = second_downloads.clone();
            let second = second.clone();
            tokio::spawn(async move {
                second
                    .install_with(&FAKE, || async {
                        second_downloads.fetch_add(1, Ordering::SeqCst);
                        Err(BrowserError::failed("must not download"))
                    })
                    .await
            })
        };
        tokio::time::sleep(BUSY_POLL * 3).await;
        assert!(!second_task.is_finished(), "waits while the lock is held");
        assert!(second.state().downloading);
        assert_eq!(std::fs::read(&partial).unwrap(), b"half");

        release.notify_one();
        let path = first_task.await.unwrap().unwrap();
        assert_eq!(second_task.await.unwrap().unwrap(), path);
        assert_eq!(second_downloads.load(Ordering::SeqCst), 0);
        assert!(!second.state().downloading);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[tokio::test]
    async fn a_failed_install_can_be_retried() {
        let data_dir = temp_data_dir();
        let installer = Installer::new(&data_dir);
        let error = installer
            .install_with(&FAKE, || async { Err(BrowserError::failed("offline")) })
            .await
            .unwrap_err();
        assert_eq!(error.message, "offline");
        let state = installer.state();
        assert!(!state.downloading);
        assert_eq!(state.error.as_deref(), Some("offline"));
        // The lock was released with the failed attempt.
        let file = std::fs::File::open(installer.root.join(LOCK_FILE)).unwrap();
        file.try_lock().unwrap();
        drop(file);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[tokio::test]
    async fn cleanup_keeps_the_lock_file_and_current_version() {
        let data_dir = temp_data_dir();
        let installer = Installer::new(&data_dir);
        std::fs::create_dir_all(installer.version_dir()).unwrap();
        std::fs::create_dir_all(installer.root.join("1.0.0.0")).unwrap();
        std::fs::write(installer.root.join("1.0.0.0.zip.partial"), b"x").unwrap();
        std::fs::write(installer.root.join(LOCK_FILE), b"").unwrap();
        installer.remove_other_versions().await;
        assert!(installer.version_dir().is_dir());
        assert!(installer.root.join(LOCK_FILE).is_file());
        assert!(!installer.root.join("1.0.0.0").exists());
        assert!(!installer.root.join("1.0.0.0.zip.partial").exists());
        std::fs::remove_dir_all(&data_dir).unwrap();
    }
}
