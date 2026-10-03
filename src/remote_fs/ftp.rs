//! FTP and explicit FTPS (AUTH TLS, rustls with the webpki root store) in
//! passive, binary mode.

use std::{future::Future, sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::body::Bytes;
use suppaftp::{
    list::{File as ListFile, ListParser},
    tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream},
    tokio_rustls::{
        rustls::{self, ClientConfig, RootCertStore},
        TlsConnector,
    },
    types::FileType as TransferType,
    FtpError, Mode, Status,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use zeroize::Zeroizing;

use super::{
    finish_listing, join_path, listable_name, permission_string, system_time_millis, too_large,
    EntryKind, Listing, RemoteEntry, RemoteFs, RemoteStat, STREAM_CHUNK_BYTES,
};
use crate::{
    error::AppError,
    ssh::{FtpProtocol, FtpSite},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound for one command, or one read/write step of a transfer.
const OP_TIMEOUT: Duration = Duration::from_secs(60);
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct FtpFs {
    stream: AsyncRustlsFtpStream,
    /// The server answers `MLSD`/`MLST` (RFC 3659).
    machine_listing: bool,
    home: String,
}

async fn timed<T>(
    what: &str,
    duration: Duration,
    future: impl Future<Output = Result<T, FtpError>>,
) -> Result<T, AppError> {
    match tokio::time::timeout(duration, future).await {
        Ok(result) => result.map_err(|error| map_error(what, error)),
        Err(_) => Err(AppError::RemoteUnreachable(format!(
            "{what}: FTP timed out"
        ))),
    }
}

impl FtpFs {
    /// Connects and logs in; anonymous when the site has no user. The
    /// password is dropped before this returns.
    pub(crate) async fn connect(
        site: &FtpSite,
        password: Option<Zeroizing<String>>,
    ) -> Result<Self, AppError> {
        let host = site.host.trim_matches(['[', ']']);
        let address = (host, site.port);
        let mut stream = timed(
            &site.host,
            CONNECT_TIMEOUT,
            AsyncRustlsFtpStream::connect(address),
        )
        .await?;
        if site.protocol == FtpProtocol::Ftps {
            let connector = AsyncRustlsConnector::from(TlsConnector::from(tls_config()?));
            stream = timed(
                &site.host,
                CONNECT_TIMEOUT,
                stream.into_secure(connector, host),
            )
            .await?;
        }
        stream.set_mode(Mode::Passive);
        // Connect data channels to the control host, never to the address a
        // PASV reply names: a hostile server could aim it at internal hosts.
        stream.set_passive_nat_workaround(true);
        let (user, password) = match (&site.user, password) {
            (Some(user), password) => (
                user.clone(),
                password.unwrap_or_else(|| Zeroizing::new(String::new())),
            ),
            (None, password) => (
                "anonymous".to_owned(),
                password.unwrap_or_else(|| Zeroizing::new("anonymous@".to_owned())),
            ),
        };
        let login = timed(
            &site.host,
            CONNECT_TIMEOUT,
            stream.login(user.as_str(), password.as_str()),
        )
        .await;
        drop(password);
        if let Err(error) = login {
            let _ = tokio::time::timeout(QUIT_TIMEOUT, stream.quit()).await;
            return Err(match error {
                AppError::RemoteFailed(_) | AppError::RemotePermissionDenied(_) => {
                    AppError::RemoteAuthFailed(format!(
                        "{}: login failed; check the user name and password",
                        site.host
                    ))
                }
                other => other,
            });
        }
        let mut fs = Self {
            stream,
            machine_listing: false,
            home: "/".to_owned(),
        };
        // On failure, dropping the stream closes the sockets.
        fs.finish_setup(site).await?;
        Ok(fs)
    }

    async fn finish_setup(&mut self, site: &FtpSite) -> Result<(), AppError> {
        timed(
            "TYPE",
            OP_TIMEOUT,
            self.stream.transfer_type(TransferType::Binary),
        )
        .await?;
        // FEAT is optional; servers without it get LIST parsing.
        if let Ok(Ok(features)) = tokio::time::timeout(OP_TIMEOUT, self.stream.feat()).await {
            self.machine_listing = features.keys().any(|key| key.eq_ignore_ascii_case("MLST"));
        }
        if let Some(directory) = &site.initial_directory {
            timed(directory, OP_TIMEOUT, self.stream.cwd(directory.as_str())).await?;
        }
        let pwd = timed("PWD", OP_TIMEOUT, self.stream.pwd()).await?;
        self.home = match super::normalize_path(&pwd) {
            Ok(path) => path,
            Err(_) => "/".to_owned(),
        };
        Ok(())
    }

    fn entry_from(&self, dir: &str, file: &ListFile, permissions: bool) -> Option<RemoteEntry> {
        let name = file.name().to_owned();
        if !listable_name(&name) {
            return None;
        }
        let kind = if file.is_directory() {
            EntryKind::Directory
        } else if file.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        Some(RemoteEntry {
            path: join_path(dir, &name),
            name,
            kind,
            size_bytes: (kind == EntryKind::File).then_some(file.size() as u64),
            modified_at: system_time_millis(file.modified()),
            permissions: permissions.then(|| permission_string(list_mode(file))),
        })
    }

    async fn stat_fallback(&mut self, path: &str) -> Result<Option<RemoteStat>, AppError> {
        match timed(path, OP_TIMEOUT, self.stream.size(path)).await {
            Ok(size) => {
                return Ok(Some(RemoteStat {
                    kind: EntryKind::File,
                    size: size as u64,
                }))
            }
            Err(AppError::RemoteUnreachable(message)) => {
                return Err(AppError::RemoteUnreachable(message))
            }
            Err(_) => {}
        }
        match timed(path, OP_TIMEOUT, self.stream.cwd(path)).await {
            Ok(()) => Ok(Some(RemoteStat {
                kind: EntryKind::Directory,
                size: 0,
            })),
            Err(AppError::RemoteUnreachable(message)) => Err(AppError::RemoteUnreachable(message)),
            Err(_) => Ok(None),
        }
    }
}

fn tls_config() -> Result<Arc<ClientConfig>, AppError> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|error| AppError::RemoteFailed(format!("TLS setup failed: {error}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Mode bits reconstructed from a parsed listing line.
fn list_mode(file: &ListFile) -> u32 {
    use suppaftp::list::PosixPexQuery::{Group, Others, Owner};
    let mut mode = 0;
    for (shift, who) in [(6, Owner), (3, Group), (0, Others)] {
        let bits = u32::from(file.can_read(who)) << 2
            | u32::from(file.can_write(who)) << 1
            | u32::from(file.can_execute(who));
        mode |= bits << shift;
    }
    mode
}

/// Parses `MLSD` or `LIST` output. Returns the entry and whether its
/// permissions are real (MLSD only has them with `UNIX.mode`).
fn parse_line(line: &str, machine: bool) -> Option<(ListFile, bool)> {
    if machine {
        let facts = line.split_once(' ').map(|(facts, _)| facts)?.to_lowercase();
        if facts.contains("type=cdir") || facts.contains("type=pdir") {
            return None;
        }
        let file = ListParser::parse_mlsd(line).ok()?;
        return Some((file, facts.contains("unix.mode=")));
    }
    if let Ok(file) = ListParser::parse_posix(line) {
        return Some((file, true));
    }
    ListParser::parse_dos(line).ok().map(|file| (file, false))
}

fn map_error(what: &str, error: FtpError) -> AppError {
    match error {
        FtpError::UnexpectedResponse(response) => {
            let message = String::from_utf8_lossy(&response.body).trim().to_owned();
            match response.status {
                Status::NotLoggedIn => AppError::RemoteAuthFailed(format!("{what}: {message}")),
                Status::FileUnavailable => AppError::NotFound(format!("{what}: {message}")),
                Status::NotAvailable | Status::RequestedActionNotTaken
                    if message.to_lowercase().contains("permission") =>
                {
                    AppError::RemotePermissionDenied(format!("{what}: {message}"))
                }
                Status::BadFilename => AppError::InvalidRequest(format!("{what}: {message}")),
                _ => AppError::RemoteFailed(format!("{what}: {message}")),
            }
        }
        FtpError::ConnectionError(error) => AppError::RemoteUnreachable(format!("{what}: {error}")),
        FtpError::SecureError(error) => {
            AppError::RemoteUnreachable(format!("{what}: TLS error: {error}"))
        }
        other => AppError::RemoteFailed(format!("{what}: {other}")),
    }
}

#[async_trait]
impl RemoteFs for FtpFs {
    async fn home_dir(&mut self) -> Result<String, AppError> {
        Ok(self.home.clone())
    }

    async fn list(&mut self, path: &str) -> Result<Listing, AppError> {
        let lines = if self.machine_listing {
            timed(path, OP_TIMEOUT, self.stream.mlsd(Some(path))).await?
        } else {
            timed(path, OP_TIMEOUT, self.stream.list(Some(path))).await?
        };
        let entries = lines
            .iter()
            .filter_map(|line| parse_line(line, self.machine_listing))
            .filter_map(|(file, permissions)| self.entry_from(path, &file, permissions))
            .collect();
        Ok(finish_listing(entries))
    }

    async fn stat(&mut self, path: &str) -> Result<Option<RemoteStat>, AppError> {
        if path == "/" {
            return Ok(Some(RemoteStat {
                kind: EntryKind::Directory,
                size: 0,
            }));
        }
        if self.machine_listing {
            match timed(path, OP_TIMEOUT, self.stream.mlst(Some(path))).await {
                Ok(line) => {
                    if let Ok(file) = ListParser::parse_mlst(&line) {
                        return Ok(Some(RemoteStat {
                            kind: if file.is_directory() {
                                EntryKind::Directory
                            } else {
                                EntryKind::File
                            },
                            size: file.size() as u64,
                        }));
                    }
                }
                Err(AppError::NotFound(_)) => return Ok(None),
                Err(AppError::RemoteUnreachable(message)) => {
                    return Err(AppError::RemoteUnreachable(message))
                }
                Err(_) => {}
            }
        }
        self.stat_fallback(path).await
    }

    async fn read(&mut self, path: &str, max: u64) -> Result<Vec<u8>, AppError> {
        let transfer = timed(path, OP_TIMEOUT, self.stream.retr_as_stream(path)).await?;
        let mut limited = transfer.take(max + 1);
        let mut bytes = Vec::new();
        let mut chunk = vec![0_u8; STREAM_CHUNK_BYTES];
        loop {
            let read = timed(path, OP_TIMEOUT, async {
                limited
                    .read(&mut chunk)
                    .await
                    .map_err(FtpError::ConnectionError)
            })
            .await?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let transfer = limited.into_inner();
        if bytes.len() as u64 > max {
            let _ = timed(path, OP_TIMEOUT, self.stream.abort(transfer)).await;
            return Err(too_large(max));
        }
        timed(path, OP_TIMEOUT, transfer.finish()).await?;
        Ok(bytes)
    }

    /// FTP has no portable atomic replace; the file is overwritten in place.
    async fn replace(&mut self, path: &str, data: &[u8]) -> Result<(), AppError> {
        self.write_at(path, 0, data).await
    }

    async fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), AppError> {
        let mut transfer = if offset == 0 {
            timed(path, OP_TIMEOUT, self.stream.put_with_stream(path)).await?
        } else {
            timed(path, OP_TIMEOUT, self.stream.append_with_stream(path)).await?
        };
        for chunk in data.chunks(STREAM_CHUNK_BYTES) {
            timed(path, OP_TIMEOUT, async {
                transfer
                    .write_all(chunk)
                    .await
                    .map_err(FtpError::ConnectionError)
            })
            .await?;
        }
        timed(path, OP_TIMEOUT, transfer.finish()).await
    }

    async fn download(
        &mut self,
        path: &str,
        limit: u64,
        sink: &mpsc::Sender<std::io::Result<Bytes>>,
    ) -> Result<(), AppError> {
        let transfer = timed(path, OP_TIMEOUT, self.stream.retr_as_stream(path)).await?;
        let mut limited = transfer.take(limit);
        let mut chunk = vec![0_u8; STREAM_CHUNK_BYTES];
        let mut sent = 0_u64;
        let mut receiver_gone = false;
        loop {
            let read = timed(path, OP_TIMEOUT, async {
                limited
                    .read(&mut chunk)
                    .await
                    .map_err(FtpError::ConnectionError)
            })
            .await?;
            if read == 0 {
                break;
            }
            sent += read as u64;
            if !super::send_chunk(sink, Bytes::copy_from_slice(&chunk[..read])).await {
                receiver_gone = true;
                break;
            }
        }
        let transfer = limited.into_inner();
        if receiver_gone {
            return timed(path, OP_TIMEOUT, self.stream.abort(transfer)).await;
        }
        match timed(path, OP_TIMEOUT, transfer.finish()).await {
            // Everything requested arrived; a late transfer complaint (for
            // example a file that grew meanwhile) does not matter here.
            Err(AppError::RemoteFailed(_)) if sent == limit => Ok(()),
            result => result,
        }
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<(), AppError> {
        timed(from, OP_TIMEOUT, self.stream.rename(from, to)).await
    }

    async fn remove(&mut self, path: &str) -> Result<(), AppError> {
        match self.stat(path).await? {
            Some(stat) if stat.kind == EntryKind::Directory => {
                timed(path, OP_TIMEOUT, self.stream.rmdir(path))
                    .await
                    .map_err(|error| match error {
                        // 550 for an existing directory: not empty or locked.
                        AppError::NotFound(_) | AppError::RemoteFailed(_) => {
                            AppError::Conflict(format!("{path} is not empty or cannot be removed"))
                        }
                        other => other,
                    })
            }
            Some(_) => timed(path, OP_TIMEOUT, self.stream.rm(path)).await,
            None => Err(AppError::NotFound(path.to_owned())),
        }
    }

    async fn mkdir(&mut self, path: &str) -> Result<(), AppError> {
        timed(path, OP_TIMEOUT, self.stream.mkdir(path)).await
    }

    async fn close(&mut self) {
        let _ = tokio::time::timeout(QUIT_TIMEOUT, self.stream.quit()).await;
    }
}
