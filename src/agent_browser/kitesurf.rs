//! The experimental Kitesurf engine (github.com/cloudflare/kitesurf): a
//! headless browser that runs as a Worker on a local `workerd`. Both are
//! pinned by this daemon release and downloaded on first use into
//! `$DATA_DIR/agent-browser/kitesurf/<version>/`, each verified against the
//! SHA-256 recorded here:
//!
//! - `workerd-<WORKERD_VERSION>`: Cloudflare's own release binary;
//! - `engine/`: the Kitesurf Worker bundle (`dist/`) and its fonts
//!   (`public/`), built by this repository's `kitesurf-engine` workflow.
//!
//! `TODEX_AGENT_BROWSER_KITESURF_DIR` (a directory with `dist/` and
//! `public/`) and `TODEX_WORKERD_PATH` point at local copies instead.
//!
//! The engine listens on a Unix socket in an owner-only directory, so only
//! this user can drive it; there is no Windows build yet.

use std::{
    fs::TryLockError,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::{
    install::{
        DownloadingGuard, InstallState, BUSY_POLL, CONNECT_TIMEOUT, LOCK_FILE, READ_TIMEOUT,
    },
    BrowserError,
};

/// cloudflare/kitesurf commit date and short SHA of the bundled engine.
pub(crate) const KITESURF_VERSION: &str = "2026.11.13-4a29038";
const ENGINE_SHA256: &str = "297f2ea82e47d93c27b742cd188c9508375a355bf77ae1d8f3ba9d900b675501";
const WORKERD_VERSION: &str = "1.20260911.1";

pub(crate) const DIR_OVERRIDE_ENV: &str = "TODEX_AGENT_BROWSER_KITESURF_DIR";
pub(crate) const WORKERD_OVERRIDE_ENV: &str = "TODEX_WORKERD_PATH";

/// Upper bounds on what is downloaded and unpacked.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;

struct WorkerdPackage {
    os: &'static str,
    arch: &'static str,
    asset: &'static str,
    /// Of the `.gz` asset.
    sha256: &'static str,
}

const WORKERD_PACKAGES: &[WorkerdPackage] = &[
    WorkerdPackage {
        os: "macos",
        arch: "aarch64",
        asset: "workerd-darwin-arm64.gz",
        sha256: "640d57e2205be322e1dec6cbc7458ce3d5d31c7a340f7b19ae066e73f2a63343",
    },
    WorkerdPackage {
        os: "macos",
        arch: "x86_64",
        asset: "workerd-darwin-64.gz",
        sha256: "91b510ae03aa9730022f44c9fc8aeb77af5ee295306bb5eef22ab8a63e0e633d",
    },
    WorkerdPackage {
        os: "linux",
        arch: "x86_64",
        asset: "workerd-linux-64.gz",
        sha256: "5de6114f7982dafd1ceb11fe48df690392b5554454786d355b52844b9b3d7d4e",
    },
    WorkerdPackage {
        os: "linux",
        arch: "aarch64",
        asset: "workerd-linux-arm64.gz",
        sha256: "d5297c6afc907de0e47c9a8db742b557efc6c97d0e9e7fab129aa5ae4aa1836a",
    },
];

fn workerd_package() -> Option<&'static WorkerdPackage> {
    WORKERD_PACKAGES.iter().find(|package| {
        package.os == std::env::consts::OS && package.arch == std::env::consts::ARCH
    })
}

/// Why Kitesurf cannot run on this machine, if it cannot.
pub(crate) fn unsupported_reason() -> Option<String> {
    if cfg!(windows) {
        return Some("Kitesurf is not available on Windows yet.".to_owned());
    }
    let overridden = std::env::var_os(WORKERD_OVERRIDE_ENV).is_some();
    (!overridden && workerd_package().is_none()).then(|| {
        format!(
            "Kitesurf has no build for {}-{}.",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

/// `engines.kitesurf` in `GET /v2/agent-desktop`.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KitesurfStatus {
    #[serde(flatten)]
    pub install: InstallState,
    /// This OS and architecture can run it.
    pub supported: bool,
    /// It can run here (supported and installed).
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What a launch needs.
pub(crate) struct Paths {
    pub workerd: PathBuf,
    /// Holds `dist/` and `public/`.
    pub engine: PathBuf,
}

#[derive(Clone)]
pub(crate) struct KitesurfInstaller {
    root: PathBuf,
    state: Arc<Mutex<InstallState>>,
    running: Arc<tokio::sync::Mutex<()>>,
}

impl KitesurfInstaller {
    pub(crate) fn new(data_dir: &Path) -> Self {
        let installer = Self {
            root: data_dir.join("agent-browser").join("kitesurf"),
            state: Arc::new(Mutex::new(InstallState {
                version: KITESURF_VERSION,
                ..InstallState::default()
            })),
            running: Arc::new(tokio::sync::Mutex::new(())),
        };
        installer.refresh();
        installer
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InstallState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refresh(&self) {
        let overridden = std::env::var_os(DIR_OVERRIDE_ENV).is_some()
            || std::env::var_os(WORKERD_OVERRIDE_ENV).is_some();
        let installed = self.paths().is_some();
        let mut state = self.lock();
        state.overridden = overridden;
        state.installed = installed;
    }

    pub(crate) fn state(&self) -> InstallState {
        self.refresh();
        self.lock().clone()
    }

    pub(crate) fn status(&self) -> KitesurfStatus {
        let install = self.state();
        let unsupported = unsupported_reason();
        let supported = unsupported.is_none();
        let reason = unsupported.or_else(|| {
            (!install.installed).then(|| {
                if install.downloading {
                    "Kitesurf is downloading.".to_owned()
                } else {
                    "Kitesurf is not installed yet.".to_owned()
                }
            })
        });
        KitesurfStatus {
            supported,
            available: reason.is_none(),
            reason,
            install,
        }
    }

    fn version_dir(&self) -> PathBuf {
        self.root.join(KITESURF_VERSION)
    }

    fn installed_workerd(&self) -> PathBuf {
        self.version_dir()
            .join(format!("workerd-{WORKERD_VERSION}"))
    }

    fn installed_engine(&self) -> PathBuf {
        self.version_dir().join("engine")
    }

    /// The `workerd` and engine to launch, when both are present.
    pub(crate) fn paths(&self) -> Option<Paths> {
        let workerd = match std::env::var_os(WORKERD_OVERRIDE_ENV) {
            Some(path) => PathBuf::from(path),
            None => self.installed_workerd(),
        };
        let engine = match std::env::var_os(DIR_OVERRIDE_ENV) {
            Some(path) => PathBuf::from(path),
            None => self.installed_engine(),
        };
        (workerd.is_file() && is_engine_dir(&engine)).then_some(Paths { workerd, engine })
    }

    /// Downloads whatever is missing (once at a time, across processes
    /// too), then removes other versions. Progress and failures land in
    /// [`Self::state`].
    pub(crate) async fn install(&self) -> Result<Paths, BrowserError> {
        if let Some(reason) = unsupported_reason() {
            return Err(BrowserError::new("UNSUPPORTED", reason));
        }
        if let Some(paths) = self.paths() {
            return Ok(paths);
        }
        let _one = self.running.lock().await;
        if let Some(paths) = self.paths() {
            return Ok(paths);
        }
        let downloading = DownloadingGuard::start(&self.state);
        let result = self.install_locked().await;
        downloading.finish(result.as_ref().err().map(|error| error.message.clone()));
        let paths = result?;
        self.refresh();
        Ok(paths)
    }

    async fn install_locked(&self) -> Result<Paths, BrowserError> {
        tokio::fs::create_dir_all(self.version_dir())
            .await
            .map_err(|error| {
                BrowserError::failed(format!("cannot create {}: {error}", self.root.display()))
            })?;
        let _lock = self.acquire_lock().await?;
        let needs_engine = std::env::var_os(DIR_OVERRIDE_ENV).is_none()
            && !is_engine_dir(&self.installed_engine());
        let needs_workerd =
            std::env::var_os(WORKERD_OVERRIDE_ENV).is_none() && !self.installed_workerd().is_file();
        if needs_engine {
            self.install_engine(if needs_workerd { 0.0..0.4 } else { 0.0..1.0 })
                .await?;
        }
        if needs_workerd {
            self.install_workerd(if needs_engine { 0.4..1.0 } else { 0.0..1.0 })
                .await?;
        }
        self.remove_other_versions().await;
        self.paths()
            .ok_or_else(|| BrowserError::failed("the Kitesurf install is incomplete"))
    }

    async fn acquire_lock(&self) -> Result<std::fs::File, BrowserError> {
        let path = self.root.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                BrowserError::failed(format!("cannot open {}: {error}", path.display()))
            })?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => tokio::time::sleep(BUSY_POLL).await,
                Err(TryLockError::Error(error)) => {
                    return Err(BrowserError::failed(format!(
                        "cannot lock {}: {error}",
                        path.display()
                    )))
                }
            }
        }
    }

    async fn install_engine(&self, share: std::ops::Range<f64>) -> Result<(), BrowserError> {
        if ENGINE_SHA256.is_empty() {
            return Err(BrowserError::new(
                "UNSUPPORTED",
                format!("no Kitesurf engine is pinned for this build; set {DIR_OVERRIDE_ENV}"),
            ));
        }
        let name = format!("kitesurf-engine-{KITESURF_VERSION}");
        let url = format!(
            "https://github.com/{}/releases/download/{name}/{name}.tar.gz",
            crate::update::REPOSITORY
        );
        let archive = self.root.join(format!("{name}.tar.gz.partial"));
        self.download(&url, &archive, ENGINE_SHA256, share).await?;
        let staging = self.version_dir().join("engine.partial");
        let _ = tokio::fs::remove_dir_all(&staging).await;
        let unpacked = {
            let (archive, staging) = (archive.clone(), staging.clone());
            tokio::task::spawn_blocking(move || unpack_engine(&archive, &staging))
                .await
                .map_err(|error| BrowserError::failed(error.to_string()))?
        };
        let _ = tokio::fs::remove_file(&archive).await;
        unpacked?;
        let target = self.installed_engine();
        let _ = tokio::fs::remove_dir_all(&target).await;
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| BrowserError::failed(format!("cannot install Kitesurf: {error}")))
    }

    async fn install_workerd(&self, share: std::ops::Range<f64>) -> Result<(), BrowserError> {
        let package = workerd_package().ok_or_else(|| {
            BrowserError::new(
                "UNSUPPORTED",
                unsupported_reason().unwrap_or_else(|| "no workerd for this machine".to_owned()),
            )
        })?;
        let url = format!(
            "https://github.com/cloudflare/workerd/releases/download/v{WORKERD_VERSION}/{}",
            package.asset
        );
        let archive = self.root.join(format!("{}.partial", package.asset));
        self.download(&url, &archive, package.sha256, share).await?;
        let staging = self.version_dir().join("workerd.partial");
        let unpacked = {
            let (archive, staging) = (archive.clone(), staging.clone());
            tokio::task::spawn_blocking(move || gunzip_executable(&archive, &staging))
                .await
                .map_err(|error| BrowserError::failed(error.to_string()))?
        };
        let _ = tokio::fs::remove_file(&archive).await;
        unpacked?;
        tokio::fs::rename(&staging, self.installed_workerd())
            .await
            .map_err(|error| BrowserError::failed(format!("cannot install workerd: {error}")))
    }

    /// Streams `url` to `to`, checking its SHA-256; `share` is the part of
    /// the progress bar this download fills.
    async fn download(
        &self,
        url: &str,
        to: &Path,
        sha256: &str,
        share: std::ops::Range<f64>,
    ) -> Result<(), BrowserError> {
        let response = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|error| BrowserError::failed(format!("cannot download Kitesurf: {error}")))?
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| BrowserError::failed(format!("cannot download Kitesurf: {error}")))?;
        let total = response.content_length();
        let mut file = tokio::fs::File::create(to).await.map_err(|error| {
            BrowserError::failed(format!("cannot write {}: {error}", to.display()))
        })?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|error| {
                BrowserError::failed(format!("Kitesurf download failed: {error}"))
            })?;
            received += chunk.len() as u64;
            if received > MAX_DOWNLOAD_BYTES {
                drop(file);
                let _ = tokio::fs::remove_file(to).await;
                return Err(BrowserError::failed("the Kitesurf download is too large"));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|error| {
                BrowserError::failed(format!("cannot write the download: {error}"))
            })?;
            if let Some(total) = total.filter(|total| *total > 0) {
                let done = (received as f64 / total as f64).min(1.0);
                self.lock().progress = Some(share.start + (share.end - share.start) * done);
            }
        }
        file.flush()
            .await
            .map_err(|error| BrowserError::failed(format!("cannot write the download: {error}")))?;
        drop(file);
        let digest = format!("{:x}", hasher.finalize());
        if !digest.eq_ignore_ascii_case(sha256) {
            let _ = tokio::fs::remove_file(to).await;
            return Err(BrowserError::failed(format!(
                "the Kitesurf download is corrupt (SHA-256 {digest}); try again"
            )));
        }
        Ok(())
    }

    /// Other pinned versions (after a daemon update); call with the install
    /// lock held.
    async fn remove_other_versions(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            if name == KITESURF_VERSION || name == LOCK_FILE {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_file(&path).await
            };
            if let Err(error) = removed {
                tracing::warn!(%error, path = %path.display(), "cannot remove an old Kitesurf");
            }
        }
    }
}

fn is_engine_dir(dir: &Path) -> bool {
    dir.join("dist").join("index.js").is_file()
        && dir.join("dist").join("engine-mf.json").is_file()
        && dir.join("public").is_dir()
}

/// Unpacks the engine bundle (`kitesurf-engine-<version>/...`) into `to`,
/// dropping that top directory. Only plain files and directories with
/// relative, `..`-free paths are accepted.
fn unpack_engine(archive: &Path, to: &Path) -> Result<(), BrowserError> {
    let fail = |error: String| BrowserError::failed(format!("cannot unpack Kitesurf: {error}"));
    std::fs::create_dir_all(to).map_err(|error| fail(error.to_string()))?;
    let file = std::fs::File::open(archive).map_err(|error| fail(error.to_string()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut written = 0u64;
    for entry in tar.entries().map_err(|error| fail(error.to_string()))? {
        let mut entry = entry.map_err(|error| fail(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| fail(error.to_string()))?
            .into_owned();
        let mut parts = path.components();
        match parts.next() {
            Some(Component::Normal(_)) => {}
            _ => return Err(fail(format!("unexpected entry {}", path.display()))),
        }
        let relative: PathBuf = parts.as_path().to_path_buf();
        if relative.as_os_str().is_empty() {
            continue;
        }
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(fail(format!("unsafe entry {}", path.display())));
        }
        let destination = to.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|error| fail(error.to_string()))?;
        } else if kind.is_file() {
            written += entry.size();
            if written > MAX_UNPACKED_BYTES {
                return Err(fail("the archive is too large".to_owned()));
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|error| fail(error.to_string()))?;
            }
            let mut out =
                std::fs::File::create(&destination).map_err(|error| fail(error.to_string()))?;
            std::io::copy(&mut entry, &mut out).map_err(|error| fail(error.to_string()))?;
        } else {
            return Err(fail(format!("unsupported entry {}", path.display())));
        }
    }
    if !is_engine_dir(to) {
        return Err(fail("the archive has no dist/ and public/".to_owned()));
    }
    Ok(())
}

fn gunzip_executable(archive: &Path, to: &Path) -> Result<(), BrowserError> {
    let fail =
        |error: std::io::Error| BrowserError::failed(format!("cannot unpack workerd: {error}"));
    let file = std::fs::File::open(archive).map_err(fail)?;
    let mut decoder = flate2::read::GzDecoder::new(file).take(MAX_UNPACKED_BYTES);
    let mut out = std::fs::File::create(to).map_err(fail)?;
    std::io::copy(&mut decoder, &mut out).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755)).map_err(fail)?;
    }
    Ok(())
}

// Only Unix launches Kitesurf; unit tests still build these on Windows.
#[cfg_attr(not(unix), allow(dead_code))]
/// The `workerd` config serving the engine in `engine` on `socket`. Module
/// files are embedded by paths relative to `config_dir`, where the config is
/// written (workerd resolves `embed` relative to the config file).
///
/// Page requests may reach public addresses, this machine (local dev
/// servers) and the local network; link-local and cloud metadata addresses
/// never. Which local-network hosts a tab reaches is the request guard's
/// call (`policy::guard_request`: only hosts the person allowed), and
/// Kitesurf's own private-network check still keeps a public page from
/// reaching this machine or the local network.
pub(crate) fn workerd_config(
    engine: &Path,
    config_dir: &Path,
    socket: &Path,
) -> Result<String, BrowserError> {
    let fail = |error: String| BrowserError::failed(format!("cannot prepare Kitesurf: {error}"));
    let engine = engine
        .canonicalize()
        .map_err(|error| fail(error.to_string()))?;
    let config_dir = config_dir
        .canonicalize()
        .map_err(|error| fail(error.to_string()))?;
    let dist = engine.join("dist");
    let options: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dist.join("engine-mf.json")).map_err(|error| fail(error.to_string()))?,
    )
    .map_err(|error| fail(error.to_string()))?;
    let quote = |text: &str| serde_json::to_string(text).unwrap_or_default();
    let relative = |path: &Path| -> Result<String, BrowserError> {
        relative_path(&config_dir, path)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .ok_or_else(|| fail(format!("no relative path to {}", path.display())))
    };

    let mut files = Vec::new();
    collect_files(&dist, &mut files).map_err(|error| fail(error.to_string()))?;
    files.sort();
    let mut modules = Vec::new();
    for file in &files {
        let name = file
            .strip_prefix(&dist)
            .map_err(|error| fail(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        if name == "engine-mf.json" || name.ends_with(".map") {
            continue;
        }
        let Some(kind) = module_kind(&name) else {
            continue;
        };
        let line = format!(
            "    (name = {}, {kind} = embed {}),",
            quote(&name),
            quote(&relative(file)?)
        );
        if name == "index.js" {
            modules.insert(0, line);
        } else {
            modules.push(line);
        }
    }
    if !modules
        .first()
        .is_some_and(|line| line.contains("\"index.js\""))
    {
        return Err(fail("the engine has no index.js".to_owned()));
    }

    let date = options["compatibilityDate"]
        .as_str()
        .unwrap_or("2026-09-11");
    let flags = options["compatibilityFlags"]
        .as_array()
        .map(|flags| {
            flags
                .iter()
                .filter_map(|flag| flag.as_str())
                .map(quote)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let mut vars = Vec::new();
    if let Some(bindings) = options["bindings"].as_object() {
        for (name, value) in bindings {
            if let Some(value) = value.as_str() {
                let value = match name.as_str() {
                    "KITESURF_ENV" => "production",
                    "KITESURF_VERSION" => KITESURF_VERSION,
                    // Pages never reach private networks on their own.
                    "KITESURF_ALLOW_PRIVATE_NETWORK" => "",
                    _ => value,
                };
                vars.push(format!(
                    "    (name = {}, text = {}),",
                    quote(name),
                    quote(value)
                ));
            }
        }
    }
    let public = engine.join("public");
    let socket = format!("unix:{}", socket.display());
    Ok(format!(
        r#"using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "kitesurf", worker = .engine),
    (name = "fonts", disk = (path = {public}, writable = false)),
    (name = "net", network = (allow = ["public", "local", "private"], deny = ["169.254.0.0/16", "fe80::/10", "fd00:ec2::254/128"], tlsOptions = (trustBrowserCas = true{roots}))),
  ],
  sockets = [ (name = "cdp", address = {socket}, http = (), service = "kitesurf") ],
);

const engine :Workerd.Worker = (
  compatibilityDate = {date},
  compatibilityFlags = [{flags}],
  modules = [
{modules}
  ],
  bindings = [
    (name = "LOADER", workerLoader = ()),
    (name = "FONT_ASSETS", service = "fonts"),
{vars}
  ],
  globalOutbound = "net",
);
"#,
        public = quote(&public.to_string_lossy()),
        socket = quote(&socket),
        date = quote(date),
        modules = modules.join("\n"),
        vars = vars.join("\n"),
        roots = trusted_roots(&quote),
    ))
}

/// `, trustedCertificates = [...]` with this computer's trusted roots, or
/// nothing when it has none to add. workerd's own browser CA bundle lacks
/// some current roots (sites behind SSL.com's 2022 roots fail with "TLS
/// peer's certificate is not trusted"); the system's store has them.
#[cfg_attr(not(unix), allow(dead_code))]
fn trusted_roots(quote: &dyn Fn(&str) -> String) -> String {
    let roots = system_roots();
    if roots.is_empty() {
        return String::new();
    }
    let roots: Vec<String> = roots.iter().map(|pem| quote(pem)).collect();
    format!(", trustedCertificates = [{}]", roots.join(", "))
}

/// The OS's trusted root certificates as PEM, read once.
#[cfg_attr(not(unix), allow(dead_code))]
fn system_roots() -> &'static [String] {
    static ROOTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| {
        #[cfg(target_os = "macos")]
        let pem = std::process::Command::new("/usr/bin/security")
            .args([
                "find-certificate",
                "-a",
                "-p",
                "/System/Library/Keychains/SystemRootCertificates.keychain",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        let pem = [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/cert.pem",
        ]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
        pem_blocks(&pem)
    })
}

/// The certificates of a PEM bundle, one string each.
#[cfg_attr(not(unix), allow(dead_code))]
fn pem_blocks(pem: &str) -> Vec<String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut blocks = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(BEGIN) {
        let Some(length) = rest[start..].find(END) else {
            break;
        };
        let end = start + length + END.len();
        blocks.push(format!("{}\n", &rest[start..end]));
        rest = &rest[end..];
    }
    blocks
}

// Only Unix launches Kitesurf; unit tests still build these on Windows.
#[cfg_attr(not(unix), allow(dead_code))]
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

// Only Unix launches Kitesurf; unit tests still build these on Windows.
#[cfg_attr(not(unix), allow(dead_code))]
/// The module type of a `dist/` file, by Kitesurf's wrangler rules
/// (`*.glue.js`, `*.css`, `*.dat` as text) and wrangler's defaults.
fn module_kind(name: &str) -> Option<&'static str> {
    if name.ends_with(".glue.js") || name.ends_with(".css") || name.ends_with(".dat") {
        Some("text")
    } else if name.ends_with(".wasm") {
        Some("wasm")
    } else if name.ends_with(".js") || name.ends_with(".mjs") {
        Some("esModule")
    } else if name.ends_with(".bin") {
        Some("data")
    } else {
        None
    }
}

// Only Unix launches Kitesurf; unit tests still build these on Windows.
#[cfg_attr(not(unix), allow(dead_code))]
/// `to` relative to directory `from`; both absolute.
fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from: Vec<_> = from.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    if from.first() != to_parts.first() {
        return None;
    }
    let common = from
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut path = PathBuf::new();
    for _ in common..from.len() {
        path.push("..");
    }
    for part in &to_parts[common..] {
        path.push(part.as_os_str());
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_climb_to_the_common_root() {
        assert_eq!(
            relative_path(
                Path::new("/tmp/run/x"),
                Path::new("/data/engine/dist/index.js")
            ),
            Some(PathBuf::from("../../../data/engine/dist/index.js"))
        );
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/b/c")),
            Some(PathBuf::from("c"))
        );
    }

    #[test]
    fn module_kinds_follow_the_wrangler_rules() {
        assert_eq!(module_kind("index.js"), Some("esModule"));
        assert_eq!(module_kind("x-renderer_core.glue.js"), Some("text"));
        assert_eq!(module_kind("x.wasm"), Some("wasm"));
        assert_eq!(module_kind("x.css"), Some("text"));
        assert_eq!(module_kind("x.dat"), Some("text"));
        assert_eq!(module_kind("README.md"), None);
    }

    fn engine_fixture(root: &Path) -> PathBuf {
        let engine = root.join("engine");
        std::fs::create_dir_all(engine.join("dist")).unwrap();
        std::fs::create_dir_all(engine.join("public")).unwrap();
        std::fs::write(engine.join("dist/index.js"), "export default {}").unwrap();
        std::fs::write(engine.join("dist/a.wasm"), [0u8; 4]).unwrap();
        std::fs::write(engine.join("dist/index.js.map"), "{}").unwrap();
        std::fs::write(
            engine.join("dist/engine-mf.json"),
            r#"{"compatibilityDate":"2026-09-11","compatibilityFlags":["global_fetch_strictly_public"],
               "bindings":{"KITESURF_ENV":"local","KITESURF_ALLOW_PRIVATE_NETWORK":"1","OTHER":"x"}}"#,
        )
        .unwrap();
        engine
    }

    #[test]
    fn pem_bundles_split_into_certificates() {
        let bundle = "junk\n-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n# x\n-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\ncut";
        assert_eq!(
            pem_blocks(bundle),
            vec![
                "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".to_owned(),
                "-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\n".to_owned(),
            ]
        );
    }

    #[test]
    fn the_config_embeds_the_engine_and_keeps_metadata_addresses_closed() {
        let root = std::env::temp_dir().join(format!("todex-kitesurf-{}", uuid::Uuid::new_v4()));
        let engine = engine_fixture(&root);
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        let config = workerd_config(&engine, &run, &run.join("cdp.sock")).unwrap();
        assert!(
            config.contains(r#"(name = "index.js", esModule = embed "../engine/dist/index.js"),"#)
        );
        assert!(config.contains(r#"(name = "a.wasm", wasm = embed "../engine/dist/a.wasm"),"#));
        assert!(!config.contains(".map"));
        assert!(config.contains(r#"(name = "KITESURF_ENV", text = "production")"#));
        assert!(config.contains(r#"(name = "KITESURF_ALLOW_PRIVATE_NETWORK", text = "")"#));
        assert!(config.contains(r#"(name = "OTHER", text = "x")"#));
        assert!(config.contains(r#"allow = ["public", "local", "private"]"#));
        assert!(config.contains(r#"deny = ["169.254.0.0/16", "fe80::/10", "fd00:ec2::254/128"]"#));
        assert!(config.contains("trustBrowserCas = true"));
        assert!(config.contains("unix:"));
        let index = config.find("index.js").unwrap();
        assert!(index < config.find("a.wasm").unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn engine_archives_unpack_without_escaping() {
        let root = std::env::temp_dir().join(format!("todex-kitesurf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let build = |entries: &[(&str, &[u8])]| {
            let path = root.join(format!("{}.tar.gz", uuid::Uuid::new_v4()));
            let file = std::fs::File::create(&path).unwrap();
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::fast(),
            ));
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::Regular);
                // `append_data` refuses `..`; write the raw name.
                header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
                header.set_cksum();
                tar.append(&header, *data).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
            path
        };
        let good = build(&[
            ("kitesurf-engine-x/dist/index.js", b"x"),
            ("kitesurf-engine-x/dist/engine-mf.json", b"{}"),
            ("kitesurf-engine-x/public/f.woff2", b"f"),
        ]);
        let out = root.join("good");
        unpack_engine(&good, &out).unwrap();
        assert!(out.join("dist/index.js").is_file());
        let bad = build(&[("kitesurf-engine-x/../escape.txt", b"x")]);
        assert!(unpack_engine(&bad, &root.join("bad")).is_err());
        assert!(!root.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
