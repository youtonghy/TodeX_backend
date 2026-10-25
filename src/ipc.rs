//! The local IPC listener (`[ipc]`, docs/API.md "Local IPC"): a Unix socket
//! (a named pipe on Windows) that only the daemon's own user can open. It
//! serves the normal router plus the local admin API; its peer is
//! [`PeerAddr::Local`], which counts as loopback.
//!
//! Trust model: a peer that passes the check runs as the daemon's user, so it
//! could read and write the data directory directly. The listener grants it
//! nothing that local file access does not already give.
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Who sent a request: a TCP peer, or a same-user IPC client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PeerAddr {
    Tcp(SocketAddr),
    Local,
}

impl PeerAddr {
    /// IPC peers count as loopback.
    pub(crate) fn is_loopback(self) -> bool {
        match self {
            Self::Tcp(address) => address.ip().to_canonical().is_loopback(),
            Self::Local => true,
        }
    }

    /// The TCP peer's IP; `None` for an IPC client.
    pub(crate) fn ip(self) -> Option<std::net::IpAddr> {
        match self {
            Self::Tcp(address) => Some(address.ip()),
            Self::Local => None,
        }
    }
}

/// The `ConnectInfo` of IPC connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalPeer;

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, IpcListener>>
    for LocalPeer
{
    fn connect_info(_stream: axum::serve::IncomingStream<'_, IpcListener>) -> Self {
        LocalPeer
    }
}

/// `<hash>`: lowercase hex of SHA-256 over the canonical data directory
/// (UTF-8; on Windows without the `\\?\` verbatim prefix), cut to `len`.
fn data_dir_hash(canonical_data_dir: &Path, len: usize) -> String {
    let text = canonical_data_dir.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let digest = Sha256::digest(text.as_bytes());
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    hex[..len].to_owned()
}

/// Longest default socket path used as-is; `sun_path` holds 104 (macOS) or
/// 108 (Linux) bytes.
#[cfg(unix)]
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// `<data_dir>/agentd.sock`, or when that is longer than 100 bytes
/// `$XDG_RUNTIME_DIR/todex-agentd-<hash8>.sock` (an absolute
/// `XDG_RUNTIME_DIR`), else `/tmp/todex-<uid>/agentd-<hash8>.sock`.
#[cfg(unix)]
pub(crate) fn default_path_for(
    canonical_data_dir: &Path,
    xdg_runtime_dir: Option<std::ffi::OsString>,
    uid: u32,
) -> PathBuf {
    let path = canonical_data_dir.join("agentd.sock");
    if path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return path;
    }
    let hash = data_dir_hash(canonical_data_dir, 8);
    match xdg_runtime_dir.map(PathBuf::from) {
        Some(runtime) if runtime.is_absolute() => runtime.join(format!("todex-agentd-{hash}.sock")),
        _ => PathBuf::from(format!("/tmp/todex-{uid}")).join(format!("agentd-{hash}.sock")),
    }
}

/// `\\.\pipe\todex-agentd-<hash16>`.
#[cfg(windows)]
pub(crate) fn default_path_for(canonical_data_dir: &Path) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\todex-agentd-{}",
        data_dir_hash(canonical_data_dir, 16)
    ))
}

/// The path the listener binds: `[ipc] path` / `TODEX_AGENTD_IPC_PATH`, or
/// the default for the data directory.
pub(crate) fn resolve_path(config: &crate::config::Config) -> io::Result<PathBuf> {
    if let Some(path) = &config.ipc.path {
        return Ok(path.clone());
    }
    let data_dir = std::fs::canonicalize(&config.data_dir)?;
    #[cfg(unix)]
    {
        Ok(default_path_for(
            &data_dir,
            std::env::var_os("XDG_RUNTIME_DIR"),
            unsafe { libc::geteuid() },
        ))
    }
    #[cfg(windows)]
    {
        Ok(default_path_for(&data_dir))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = data_dir;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "local IPC is unavailable on this platform",
        ))
    }
}

#[cfg(windows)]
pub(crate) use pipe::IpcListener;
#[cfg(unix)]
pub(crate) use unix::IpcListener;

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tokio::net::{UnixListener, UnixStream};

    /// Logged once per listener: a peer of another user was refused.
    static FOREIGN_PEER_LOGGED: AtomicBool = AtomicBool::new(false);

    pub(crate) struct IpcListener {
        listener: UnixListener,
        euid: u32,
        /// Removes the socket file when the listener is dropped.
        _socket: SocketFile,
    }

    /// The bound socket file; dropped, it is unlinked if it is still the
    /// one this listener created.
    struct SocketFile {
        path: PathBuf,
        dev: u64,
        ino: u64,
    }

    impl Drop for SocketFile {
        fn drop(&mut self) {
            if fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| metadata.dev() == self.dev && metadata.ino() == self.ino)
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn invalid(message: String) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    /// The directory for the `/tmp/todex-<uid>` fallback, created 0700.
    fn ensure_private_tmp_dir(dir: &Path, euid: u32) -> io::Result<()> {
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(dir)?;
        if !metadata.is_dir() || metadata.uid() != euid || metadata.mode() & 0o077 != 0 {
            return Err(invalid(format!(
                "{} must be a directory owned and accessible only by uid {euid}",
                dir.display()
            )));
        }
        Ok(())
    }

    impl IpcListener {
        /// Binds `path` (mode 0600). Its directory must exist and not be
        /// group or world writable; a stale socket there is replaced, a live
        /// one or any other file is an error.
        pub(crate) fn bind(path: &Path) -> io::Result<Self> {
            let euid = unsafe { libc::geteuid() };
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| invalid(format!("{} has no directory", path.display())))?;
            if parent == Path::new(&format!("/tmp/todex-{euid}")) {
                ensure_private_tmp_dir(parent, euid)?;
            }
            let metadata = fs::metadata(parent)?;
            if !metadata.is_dir() {
                return Err(invalid(format!("{} is not a directory", parent.display())));
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(invalid(format!(
                    "{} is group or world writable",
                    parent.display()
                )));
            }
            match fs::symlink_metadata(path) {
                Ok(existing) if existing.file_type().is_socket() => {
                    if std::os::unix::net::UnixStream::connect(path).is_ok() {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("another process is listening on {}", path.display()),
                        ));
                    }
                    fs::remove_file(path)?;
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(path)?;
            let socket = fs::symlink_metadata(path).map(|metadata| SocketFile {
                path: path.to_path_buf(),
                dev: metadata.dev(),
                ino: metadata.ino(),
            })?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                listener,
                euid,
                _socket: socket,
            })
        }
    }

    impl axum::serve::Listener for IpcListener {
        type Io = UnixStream;
        type Addr = super::LocalPeer;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                let stream = match self.listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "IPC accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                match stream.peer_cred() {
                    Ok(credentials) if credentials.uid() == self.euid => {
                        return (stream, super::LocalPeer)
                    }
                    result => {
                        if !FOREIGN_PEER_LOGGED.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                peer_uid = ?result.ok().map(|credentials| credentials.uid()),
                                "refused an IPC connection from another user"
                            );
                        }
                    }
                }
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            Ok(super::LocalPeer)
        }
    }
}

#[cfg(windows)]
mod pipe {
    use std::ffi::c_void;
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    /// A DACL granting only SYSTEM and the current user, from SDDL.
    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    // The descriptor is immutable after creation and only read by
    // `CreateNamedPipeW`.
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        fn current_user_only() -> io::Result<Self> {
            let sid = crate::transport_crypto::pairing_browser::current_windows_sid()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(Self(descriptor))
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }

    pub(crate) struct IpcListener {
        name: String,
        security: SecurityDescriptor,
        next: NamedPipeServer,
    }

    impl IpcListener {
        fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
            create_instance(&self.name, &self.security, first)
        }

        /// Creates the first pipe instance; fails when another process
        /// already owns the name.
        pub(crate) fn bind(path: &Path) -> io::Result<Self> {
            let name = path.to_string_lossy().into_owned();
            let security = SecurityDescriptor::current_user_only()?;
            let next = create_instance(&name, &security, true)?;
            Ok(Self {
                name,
                security,
                next,
            })
        }
    }

    fn create_instance(
        name: &str,
        security: &SecurityDescriptor,
        first: bool,
    ) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0 .0,
            bInheritHandle: false.into(),
        };
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    &mut attributes as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        }
    }

    impl axum::serve::Listener for IpcListener {
        type Io = NamedPipeServer;
        type Addr = super::LocalPeer;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                if let Err(error) = self.next.connect().await {
                    tracing::warn!(%error, "IPC pipe connect failed");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    match self.create(false) {
                        Ok(server) => self.next = server,
                        Err(error) => tracing::warn!(%error, "IPC pipe instance failed"),
                    }
                    continue;
                }
                match self.create(false) {
                    Ok(server) => {
                        let connected = std::mem::replace(&mut self.next, server);
                        return (connected, super::LocalPeer);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "IPC pipe instance failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            Ok(super::LocalPeer)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::default_path_for;
    use std::path::{Path, PathBuf};

    #[test]
    fn short_data_dirs_keep_the_socket_inside_and_long_ones_fall_back() {
        assert_eq!(
            default_path_for(Path::new("/home/me/.todex-agent"), None, 1000),
            PathBuf::from("/home/me/.todex-agent/agentd.sock")
        );
        let long = PathBuf::from(format!("/home/me/{}", "d".repeat(100)));
        let hash = super::data_dir_hash(&long, 8);
        assert_eq!(hash.len(), 8);
        assert!(hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_eq!(
            default_path_for(&long, Some("/run/user/1000".into()), 1000),
            PathBuf::from(format!("/run/user/1000/todex-agentd-{hash}.sock"))
        );
        for runtime in [None, Some("relative/run".into())] {
            assert_eq!(
                default_path_for(&long, runtime, 1000),
                PathBuf::from(format!("/tmp/todex-1000/agentd-{hash}.sock"))
            );
        }
    }

    #[tokio::test]
    async fn bind_replaces_stale_sockets_and_refuses_live_ones_and_shared_dirs() {
        use std::os::unix::fs::PermissionsExt;
        // `/tmp`, not `temp_dir()`: macOS's per-user temp directory is too
        // long for a socket path (104 bytes).
        let dir = PathBuf::from(format!(
            "/tmp/todex-ipc-bind-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("agentd.sock");
        // A socket nobody listens on any more.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let listener = super::IpcListener::bind(&path).unwrap();
        let error = super::IpcListener::bind(&path).err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        drop(listener);
        assert!(!path.exists());

        std::fs::write(&path, b"not a socket").unwrap();
        assert!(super::IpcListener::bind(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = super::IpcListener::bind(&path).err().unwrap();
        assert!(error.to_string().contains("writable"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
