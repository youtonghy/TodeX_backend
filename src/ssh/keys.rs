//! SSH key inventory, import and generation in the backend user's `~/.ssh`.
//!
//! Keys are parsed in-process with the RustCrypto `ssh-key` crate, so
//! passphrases never appear in a child's argv. Only OpenSSH-format private
//! keys are fully understood; legacy PEM keys (PKCS#1/PKCS#8/SEC1) are listed
//! with whatever their `.pub` provides and cannot be imported. Responses carry
//! public material only.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use ssh_key::{public::KeyData, Algorithm, EcdsaCurve, HashAlg, LineEnding, PrivateKey, PublicKey};
use zeroize::Zeroize;

use super::SshService;
use crate::{
    error::AppError,
    external_command::{self, prepare_captured, CommandLimits},
    secure_fs,
};

/// Key files are a few KiB at most; anything larger is not a key.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024;
const MAX_NAME_LEN: usize = 64;
const MAX_COMMENT_LEN: usize = 256;
const MAX_PASSPHRASE_LEN: usize = 1024;
const AGENT_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(5),
    output_limit: 256 * 1024,
};
/// Files OpenSSH gives a meaning other than "key"; never listed or created.
const RESERVED_PREFIXES: &[&str] = &["config", "known_hosts", "authorized_keys", "environment"];

const OPENSSH_HEADER: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
/// Legacy PEM private keys: PKCS#1 RSA/DSA, SEC1 EC, PKCS#8 (plain/encrypted).
const PEM_HEADERS: &[(&str, &str)] = &[
    ("-----BEGIN RSA PRIVATE KEY-----", "ssh-rsa"),
    ("-----BEGIN DSA PRIVATE KEY-----", "ssh-dss"),
    ("-----BEGIN EC PRIVATE KEY-----", "ecdsa"),
    ("-----BEGIN PRIVATE KEY-----", "unknown"),
    ("-----BEGIN ENCRYPTED PRIVATE KEY-----", "unknown"),
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SshKeyView {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key_path: Option<String>,
    pub algorithm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits: Option<u32>,
    /// Absent only for legacy PEM keys without a `.pub`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
    pub loaded_in_agent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub used_by: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SshKeyList {
    pub ssh_directory: String,
    pub agent_available: bool,
    pub keys: Vec<SshKeyView>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct KeyImportRequest {
    pub name: String,
    pub private_key: String,
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum KeyKind {
    #[default]
    Ed25519,
    Rsa,
    Ecdsa,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct KeyGenerateRequest {
    pub name: String,
    #[serde(default)]
    pub algorithm: KeyKind,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
}

// Secrets are wiped on drop and redacted from `Debug`, so request structs can
// never leak key material or passphrases into logs.
impl Drop for KeyImportRequest {
    fn drop(&mut self) {
        self.private_key.zeroize();
        self.passphrase.zeroize();
    }
}

impl Drop for KeyGenerateRequest {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

impl fmt::Debug for KeyImportRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyImportRequest")
            .field("name", &self.name)
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for KeyGenerateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyGenerateRequest")
            .field("name", &self.name)
            .field("algorithm", &self.algorithm)
            .field("comment", &self.comment)
            .finish_non_exhaustive()
    }
}

impl SshService {
    fn ssh_dir(&self) -> Result<PathBuf, AppError> {
        self.inner
            .home
            .as_ref()
            .map(|home| home.join(".ssh"))
            .ok_or_else(|| AppError::Unsupported("home directory is unknown".to_owned()))
    }

    pub async fn list_keys(&self) -> Result<SshKeyList, AppError> {
        let dir = self.ssh_dir()?;
        let scan_dir = dir.clone();
        let scan = async {
            tokio::task::spawn_blocking(move || scan_keys(&scan_dir))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))?
                .map_err(AppError::from)
        };
        let (scanned, agent, hosts) =
            tokio::join!(scan, self.agent_fingerprints(), self.list_hosts());
        let mut keys = scanned?;
        let identity_users = match hosts {
            Ok(hosts) => {
                let home = self.inner.home.clone().unwrap_or_default();
                let mut users: HashMap<PathBuf, Vec<String>> = HashMap::new();
                for host in hosts {
                    for file in host.resolved.iter().flat_map(|r| &r.identity_files) {
                        users
                            .entry(expand_home(file, &home))
                            .or_default()
                            .push(host.alias.clone());
                    }
                }
                users
            }
            Err(error) => {
                tracing::warn!(%error, "ssh hosts unavailable; key usage left empty");
                HashMap::new()
            }
        };
        for key in &mut keys {
            key.loaded_in_agent = match (&agent, &key.fingerprint) {
                (Some(loaded), Some(fingerprint)) => loaded.contains(fingerprint),
                _ => false,
            };
            let mut used_by: Vec<String> = [&key.private_key_path, &key.public_key_path]
                .into_iter()
                .flatten()
                .filter_map(|path| identity_users.get(Path::new(path)))
                .flatten()
                .cloned()
                .collect();
            used_by.sort();
            used_by.dedup();
            key.used_by = used_by;
        }
        Ok(SshKeyList {
            ssh_directory: dir.display().to_string(),
            agent_available: agent.is_some(),
            keys,
        })
    }

    /// SHA256 fingerprints of the identities in the user's ssh-agent, or
    /// `None` when no agent is reachable.
    async fn agent_fingerprints(&self) -> Option<HashSet<String>> {
        // Windows' OpenSSH agent listens on a fixed named pipe instead.
        if cfg!(unix) && std::env::var_os("SSH_AUTH_SOCK").is_none_or(|sock| sock.is_empty()) {
            return None;
        }
        let mut command = external_command::secure_command(ssh_add_program(&self.inner.ssh_bin));
        command.arg("-L");
        prepare_captured(&mut command, false);
        match external_command::run(command, None, AGENT_LIMITS).await {
            // Exit 1 with "The agent has no identities." is a reachable agent.
            Ok(output) if output.status.success() || output.status.code() == Some(1) => Some(
                agent_fingerprints_from(&String::from_utf8_lossy(&output.stdout)),
            ),
            Ok(_) => None,
            Err(error) => {
                tracing::debug!(%error, "ssh-add -L failed");
                None
            }
        }
    }

    pub async fn import_key(&self, request: KeyImportRequest) -> Result<SshKeyView, AppError> {
        valid_key_name(&request.name)?;
        let dir = self.ssh_dir()?;
        let name = request.name.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let (private, public) = prepare_import(&request)?;
            write_key_pair(&dir, &request.name, &private, &public)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        self.key_view(&name).await
    }

    pub async fn generate_key(&self, request: KeyGenerateRequest) -> Result<SshKeyView, AppError> {
        valid_key_name(&request.name)?;
        let comment = request.comment.as_deref().map(str::trim).unwrap_or("");
        if comment.len() > MAX_COMMENT_LEN || comment.chars().any(char::is_control) {
            return Err(AppError::InvalidRequest(format!(
                "comment must be at most {MAX_COMMENT_LEN} printable characters"
            )));
        }
        let comment = comment.to_owned();
        let passphrase = checked_passphrase(request.passphrase.as_deref())?.map(str::to_owned);
        let dir = self.ssh_dir()?;
        let name = request.name.clone();
        let kind = request.algorithm;
        drop(request);
        let file_name = name.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let mut passphrase = passphrase;
            let result = generate_pair(kind, &comment, passphrase.as_deref())
                .and_then(|(private, public)| write_key_pair(&dir, &file_name, &private, &public));
            passphrase.zeroize();
            result
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        self.key_view(&name).await
    }

    async fn key_view(&self, name: &str) -> Result<SshKeyView, AppError> {
        self.list_keys()
            .await?
            .keys
            .into_iter()
            .find(|key| key.name == name)
            .ok_or_else(|| AppError::NotFound(format!("ssh key {name}")))
    }
}

fn valid_key_name(name: &str) -> Result<(), AppError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with(['.', '-'])
        && !name.ends_with(".pub")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !is_reserved(name);
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "key name must be 1-{MAX_NAME_LEN} of A-Z a-z 0-9 . _ -, must not start with . or -, \
             end with .pub, or name an OpenSSH file"
        )))
    }
}

fn is_reserved(name: &str) -> bool {
    RESERVED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// An empty passphrase means "none"; oversized ones are rejected.
fn checked_passphrase(passphrase: Option<&str>) -> Result<Option<&str>, AppError> {
    match passphrase {
        Some(passphrase) if passphrase.len() > MAX_PASSPHRASE_LEN => Err(AppError::InvalidRequest(
            format!("passphrase must be at most {MAX_PASSPHRASE_LEN} bytes"),
        )),
        Some(passphrase) if !passphrase.is_empty() => Ok(Some(passphrase)),
        _ => Ok(None),
    }
}

/// `ssh-add` from `PATH` for a bare `ssh_bin`, otherwise its sibling file.
fn ssh_add_program(ssh_bin: &str) -> PathBuf {
    let ssh = Path::new(ssh_bin);
    match ssh.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            let mut name = std::ffi::OsString::from("ssh-add");
            if let Some(extension) = ssh.extension() {
                name.push(".");
                name.push(extension);
            }
            parent.join(name)
        }
        _ => PathBuf::from("ssh-add"),
    }
}

/// Expands the `~` and `%d` forms `ssh -G` prints for `IdentityFile`.
fn expand_home(file: &str, home: &Path) -> PathBuf {
    for prefix in ["~/", "%d/", "~\\", "%d\\"] {
        if let Some(rest) = file.strip_prefix(prefix) {
            return home.join(rest);
        }
    }
    if file == "~" || file == "%d" {
        return home.to_path_buf();
    }
    PathBuf::from(file)
}

fn agent_fingerprints_from(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .filter_map(|line| PublicKey::from_openssh(line.trim()).ok())
        .map(|key| sha256_fingerprint(key.key_data()))
        .collect()
}

fn sha256_fingerprint(key: &KeyData) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

fn key_bits(key: &KeyData) -> Option<u32> {
    if let Some(rsa) = key.rsa() {
        return mpint_bits(&rsa.n);
    }
    if let Some(dsa) = key.dsa() {
        return mpint_bits(&dsa.p);
    }
    if let Some(ecdsa) = key.ecdsa() {
        return Some(match ecdsa.curve() {
            EcdsaCurve::NistP256 => 256,
            EcdsaCurve::NistP384 => 384,
            EcdsaCurve::NistP521 => 521,
        });
    }
    (key.is_ed25519() || key.is_sk_ed25519() || key.is_sk_ecdsa_p256()).then_some(256)
}

fn mpint_bits(value: &ssh_key::Mpint) -> Option<u32> {
    let bytes = value.as_positive_bytes()?;
    let first = bytes.first()?;
    Some((bytes.len() as u32 - 1) * 8 + (8 - first.leading_zeros()))
}

fn encoding_error(error: ssh_key::Error) -> AppError {
    AppError::Anyhow(anyhow::anyhow!("could not encode ssh key: {error}"))
}

/// What a file's content says about it being a private key.
enum PrivateContent {
    OpenSsh(Box<PrivateKey>),
    /// OpenSSH armor whose body does not parse (corrupt or unsupported).
    OpenSshInvalid,
    Pem {
        algorithm: &'static str,
        encrypted: bool,
    },
}

fn classify_private(text: &str) -> Option<PrivateContent> {
    let text = text.trim();
    if text.starts_with(OPENSSH_HEADER) {
        return Some(match PrivateKey::from_openssh(text) {
            Ok(key) => PrivateContent::OpenSsh(Box::new(key)),
            Err(_) => PrivateContent::OpenSshInvalid,
        });
    }
    PEM_HEADERS
        .iter()
        .find(|(header, _)| text.starts_with(header))
        .map(|(header, algorithm)| PrivateContent::Pem {
            algorithm,
            encrypted: header.contains("ENCRYPTED") || text.contains("Proc-Type: 4,ENCRYPTED"),
        })
}

/// Reads a candidate key file: regular files (or symlinks to files inside
/// `ssh_dir`) of at most [`MAX_KEY_FILE_BYTES`], as UTF-8.
fn read_candidate(ssh_dir: &Path, path: &Path) -> io::Result<Option<zeroize::Zeroizing<String>>> {
    let target = if fs::symlink_metadata(path)?.file_type().is_symlink() {
        let resolved = fs::canonicalize(path)?;
        if !resolved.starts_with(ssh_dir) {
            return Ok(None);
        }
        resolved
    } else {
        path.to_path_buf()
    };
    let metadata = fs::metadata(&target)?;
    if !metadata.is_file() || metadata.len() > MAX_KEY_FILE_BYTES {
        return Ok(None);
    }
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    fs::File::open(&target)?
        .take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return Ok(None);
    }
    Ok(std::str::from_utf8(&bytes)
        .ok()
        .map(|text| zeroize::Zeroizing::new(text.to_owned())))
}

#[derive(Default)]
struct ScannedPair {
    private: Option<(PathBuf, PrivateContent)>,
    public: Option<(PathBuf, PublicKey)>,
}

/// Lists key pairs in `dir` (non-recursive). Agent state and host usage are
/// filled in by the caller.
fn scan_keys(dir: &Path) -> io::Result<Vec<SshKeyView>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let canonical_dir = fs::canonicalize(dir)?;
    let mut pairs: BTreeMap<String, ScannedPair> = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if is_reserved(&file_name) {
            continue;
        }
        let path = entry.path();
        let text = match read_candidate(&canonical_dir, &path) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "skipping unreadable ssh file");
                continue;
            }
        };
        if let Some(stem) = file_name
            .strip_suffix(".pub")
            .filter(|stem| !stem.is_empty())
        {
            let line = text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'));
            if let Some(key) = line.and_then(|line| PublicKey::from_openssh(line).ok()) {
                pairs.entry(stem.to_owned()).or_default().public = Some((path, key));
            }
            continue;
        }
        if let Some(content) = classify_private(&text) {
            pairs.entry(file_name).or_default().private = Some((path, content));
        }
    }
    Ok(pairs
        .into_iter()
        .map(|(name, pair)| key_view(name, pair))
        .collect())
}

fn key_view(name: String, pair: ScannedPair) -> SshKeyView {
    let file_public = pair.public.as_ref().map(|(_, key)| key);
    let (public, encrypted, algorithm_hint) = match pair.private.as_ref().map(|(_, c)| c) {
        Some(PrivateContent::OpenSsh(private)) => {
            // OpenSSH keeps the public half in clear even for encrypted keys;
            // the comment is only readable when unencrypted.
            let mut public = private.public_key().clone();
            if public.comment().is_empty() {
                if let Some(file_public) = file_public {
                    public.set_comment(file_public.comment());
                }
            }
            (Some(public), Some(private.is_encrypted()), None)
        }
        Some(PrivateContent::OpenSshInvalid) => (file_public.cloned(), None, None),
        Some(PrivateContent::Pem {
            algorithm,
            encrypted,
        }) => (file_public.cloned(), Some(*encrypted), Some(*algorithm)),
        None => (file_public.cloned(), None, None),
    };
    let algorithm = match (&public, algorithm_hint) {
        (Some(public), _) => public.algorithm().as_str().to_owned(),
        (None, Some(hint)) => hint.to_owned(),
        (None, None) => "unknown".to_owned(),
    };
    SshKeyView {
        name,
        private_key_path: pair
            .private
            .as_ref()
            .map(|(path, _)| path.display().to_string()),
        public_key_path: pair
            .public
            .as_ref()
            .map(|(path, _)| path.display().to_string()),
        algorithm,
        bits: public.as_ref().and_then(|key| key_bits(key.key_data())),
        fingerprint: public
            .as_ref()
            .map(|key| sha256_fingerprint(key.key_data())),
        comment: public
            .as_ref()
            .map(|key| key.comment().to_owned())
            .filter(|comment| !comment.is_empty()),
        encrypted,
        loaded_in_agent: false,
        public_key: public.as_ref().and_then(|key| key.to_openssh().ok()),
        used_by: Vec::new(),
    }
}

/// Validates an import and returns the canonical private key file (LF line
/// endings, still encrypted if it was) and the public key line.
fn prepare_import(
    request: &KeyImportRequest,
) -> Result<(zeroize::Zeroizing<String>, String), AppError> {
    let invalid = |message: &str| AppError::InvalidRequest(message.to_owned());
    if request.private_key.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(invalid("privateKey is too large"));
    }
    let normalized = zeroize::Zeroizing::new(request.private_key.replace("\r\n", "\n"));
    let private = match classify_private(&normalized) {
        Some(PrivateContent::OpenSsh(private)) => private,
        Some(PrivateContent::Pem { .. }) => {
            return Err(invalid(
                "only OpenSSH-format private keys can be imported; convert a PEM key with \
                 `ssh-keygen -p -f <file>` first",
            ))
        }
        _ => return Err(invalid("privateKey is not a valid OpenSSH private key")),
    };
    let mut comment = private.comment().to_owned();
    if private.is_encrypted() {
        if let Some(passphrase) = checked_passphrase(request.passphrase.as_deref())? {
            // Only checks the passphrase and recovers the comment; the file
            // is written still encrypted.
            let decrypted = private
                .decrypt(passphrase)
                .map_err(|_| invalid("passphrase does not decrypt privateKey"))?;
            comment = decrypted.comment().to_owned();
        }
    }
    let mut public = private.public_key().clone();
    match request
        .public_key
        .as_deref()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        Some(line) => {
            let supplied = PublicKey::from_openssh(line)
                .map_err(|_| invalid("publicKey is not a valid OpenSSH public key"))?;
            if supplied.key_data() != public.key_data() {
                return Err(invalid("publicKey does not match privateKey"));
            }
            public = supplied;
        }
        None => public.set_comment(comment),
    }
    let private_file = private.to_openssh(LineEnding::LF).map_err(encoding_error)?;
    let public_line = public.to_openssh().map_err(encoding_error)?;
    Ok((private_file, public_line))
}

fn generate_pair(
    kind: KeyKind,
    comment: &str,
    passphrase: Option<&str>,
) -> Result<(zeroize::Zeroizing<String>, String), AppError> {
    let algorithm = match kind {
        KeyKind::Ed25519 => Algorithm::Ed25519,
        // ssh-key generates 4096-bit RSA keys.
        KeyKind::Rsa => Algorithm::Rsa { hash: None },
        KeyKind::Ecdsa => Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
    };
    let mut private = PrivateKey::random(&mut OsRng, algorithm)
        .map_err(|error| AppError::Anyhow(anyhow::anyhow!("key generation failed: {error}")))?;
    private.set_comment(comment);
    let public_line = private.public_key().to_openssh().map_err(encoding_error)?;
    if let Some(passphrase) = passphrase {
        private = private
            .encrypt(&mut OsRng, passphrase)
            .map_err(|error| AppError::Anyhow(anyhow::anyhow!("key encryption failed: {error}")))?;
    }
    let private_file = private.to_openssh(LineEnding::LF).map_err(encoding_error)?;
    Ok((private_file, public_line))
}

/// Writes `<name>` (0600) and `<name>.pub` (0644) without replacing either;
/// creates `dir` owner-only if missing. On failure nothing new is left behind.
fn write_key_pair(dir: &Path, name: &str, private: &str, public: &str) -> Result<(), AppError> {
    if fs::symlink_metadata(dir).is_err() {
        secure_fs::ensure_owner_only_dir(dir)?;
    }
    let private_path = dir.join(name);
    let public_path = dir.join(format!("{name}.pub"));
    let exists = |path: &Path| AppError::Conflict(format!("{} already exists", path.display()));
    if fs::symlink_metadata(&public_path).is_ok() {
        return Err(exists(&public_path));
    }
    secure_fs::create_owner_only(&private_path, private.as_bytes()).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            exists(&private_path)
        } else {
            error.into()
        }
    })?;
    if let Err(error) = create_public_file(&public_path, format!("{public}\n").as_bytes()) {
        if let Err(remove_error) = fs::remove_file(&private_path) {
            tracing::warn!(
                path = %private_path.display(),
                error = %remove_error,
                "could not remove private key after failed public key write"
            );
        }
        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            exists(&public_path)
        } else {
            error.into()
        });
    }
    Ok(())
}

/// Like [`secure_fs::create_owner_only`] but world-readable (0644 before
/// umask), as OpenSSH writes `.pub` files.
fn create_public_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options.open(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn resolves_ssh_add_next_to_ssh() {
        assert_eq!(ssh_add_program("ssh"), PathBuf::from("ssh-add"));
        assert_eq!(
            ssh_add_program("/opt/ssh/bin/ssh"),
            PathBuf::from("/opt/ssh/bin/ssh-add")
        );
        assert_eq!(
            ssh_add_program("tools/ssh.exe"),
            PathBuf::from("tools/ssh-add.exe")
        );
    }

    #[test]
    fn expands_identity_file_home_forms() {
        let home = Path::new("/home/u");
        assert_eq!(
            expand_home("~/.ssh/id_ed25519", home),
            home.join(".ssh/id_ed25519")
        );
        assert_eq!(expand_home("%d/.ssh/k", home), home.join(".ssh/k"));
        assert_eq!(expand_home("/etc/k", home), PathBuf::from("/etc/k"));
    }

    #[test]
    fn validates_key_names() {
        for name in ["id_ed25519", "work.key", "A-1_b"] {
            assert!(valid_key_name(name).is_ok(), "{name}");
        }
        let long = "a".repeat(65);
        for name in [
            "",
            ".hidden",
            "-x",
            "a/b",
            "../x",
            "x.pub",
            "config",
            "known_hosts2",
            "authorized_keys",
            "sp ace",
            &long,
        ] {
            assert!(
                matches!(valid_key_name(name), Err(AppError::InvalidRequest(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn parses_agent_listing_and_key_sizes() {
        let (_, line) = generate_pair(KeyKind::Ed25519, "agent", None).unwrap();
        let key = PublicKey::from_openssh(&line).unwrap();
        let listed = agent_fingerprints_from(&format!("{line}\nnot a key\n"));
        assert_eq!(listed.len(), 1);
        assert!(listed.contains(&sha256_fingerprint(key.key_data())));
        assert_eq!(key_bits(key.key_data()), Some(256));
        let mpint = ssh_key::Mpint::from_positive_bytes(&[0x80, 0, 0]).unwrap();
        assert_eq!(mpint_bits(&mpint), Some(24));
        let mpint = ssh_key::Mpint::from_positive_bytes(&[0x01, 0]).unwrap();
        assert_eq!(mpint_bits(&mpint), Some(9));
    }

    #[test]
    fn request_debug_redacts_secrets() {
        let request: KeyImportRequest = serde_json::from_value(serde_json::json!({
            "name": "k",
            "privateKey": "-----BEGIN OPENSSH PRIVATE KEY-----SECRET",
            "passphrase": "hunter2",
        }))
        .unwrap();
        let debug = format!("{request:?}");
        assert!(
            !debug.contains("SECRET") && !debug.contains("hunter2"),
            "{debug}"
        );
        let unknown = serde_json::from_value::<KeyGenerateRequest>(serde_json::json!({
            "name": "k", "bits": 2048
        }));
        assert!(unknown.is_err());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ssh::tests::{fixture, Fixture};
    use std::os::unix::fs::PermissionsExt;

    fn ssh_dir(fixture: &Fixture) -> PathBuf {
        fixture.root.join("home/.ssh")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn generate(name: &str, algorithm: KeyKind, passphrase: Option<&str>) -> KeyGenerateRequest {
        KeyGenerateRequest {
            name: name.into(),
            algorithm,
            comment: Some(format!("{name}@test")),
            passphrase: passphrase.map(str::to_owned),
        }
    }

    fn import(name: &str, private_key: &str) -> KeyImportRequest {
        KeyImportRequest {
            name: name.into(),
            private_key: private_key.into(),
            public_key: None,
            passphrase: None,
        }
    }

    #[tokio::test]
    async fn generates_and_lists_keys_without_secrets() {
        let fixture = fixture("Host web\n").await;
        let service = &fixture.service;
        let ed = service
            .generate_key(generate("id_ed25519", KeyKind::Ed25519, None))
            .await
            .unwrap();
        assert_eq!(ed.algorithm, "ssh-ed25519");
        assert_eq!(ed.bits, Some(256));
        assert_eq!(ed.encrypted, Some(false));
        assert_eq!(ed.comment.as_deref(), Some("id_ed25519@test"));
        assert_eq!(ed.used_by, ["web"]);
        assert!(ed.fingerprint.as_deref().unwrap().starts_with("SHA256:"));

        let ec = service
            .generate_key(generate("work", KeyKind::Ecdsa, Some("s3cret-pass")))
            .await
            .unwrap();
        assert_eq!(ec.algorithm, "ecdsa-sha2-nistp256");
        assert_eq!(ec.bits, Some(256));
        assert_eq!(ec.encrypted, Some(true));
        assert_eq!(ec.comment.as_deref(), Some("work@test"), "taken from .pub");
        assert!(ec.used_by.is_empty());

        let dir = ssh_dir(&fixture);
        assert_eq!(mode(&dir.join("work")), 0o600);
        assert_eq!(mode(&dir.join("work.pub")), 0o644);
        let stored = fs::read_to_string(dir.join("work")).unwrap();
        let decrypted = PrivateKey::from_openssh(&stored)
            .unwrap()
            .decrypt("s3cret-pass")
            .unwrap();
        assert_eq!(
            sha256_fingerprint(decrypted.public_key().key_data()),
            ec.fingerprint.clone().unwrap()
        );

        assert!(matches!(
            service
                .generate_key(generate("work", KeyKind::Ed25519, None))
                .await
                .unwrap_err(),
            AppError::Conflict(_)
        ));
        assert_eq!(fs::read_to_string(dir.join("work")).unwrap(), stored);

        let list = service.list_keys().await.unwrap();
        assert!(!list.agent_available, "no ssh-add next to the fake ssh");
        let names: Vec<&str> = list.keys.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(names, ["id_ed25519", "work"]);
        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("PRIVATE KEY") && !json.contains("s3cret-pass"));
    }

    #[tokio::test]
    async fn scans_pairs_orphans_and_legacy_keys_by_content() {
        let fixture = fixture("").await;
        let dir = ssh_dir(&fixture);
        let (solo, _) = generate_pair(KeyKind::Ed25519, "solo@host", None).unwrap();
        fs::write(dir.join("solo"), solo.as_bytes()).unwrap();
        let (_, lonely) = generate_pair(KeyKind::Ecdsa, "lonely@host", None).unwrap();
        fs::write(dir.join("lonely.pub"), format!("{lonely}\n")).unwrap();
        let (locked, _) = generate_pair(KeyKind::Ed25519, "hidden", Some("pw")).unwrap();
        fs::write(dir.join("locked"), locked.as_bytes()).unwrap();
        fs::write(
            dir.join("legacy"),
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nAAAA\n-----END RSA PRIVATE KEY-----\n",
        )
        .unwrap();
        // Not keys: plain text named like a key, OpenSSH files, oversized
        // files, directories, and symlinks leaving ~/.ssh.
        fs::write(dir.join("id_rsa"), "hello").unwrap();
        fs::write(dir.join("known_hosts"), "x").unwrap();
        let mut big = solo.to_string();
        big.push_str(&" ".repeat(70 * 1024));
        fs::write(dir.join("big"), big).unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        let outside = fixture.root.join("outside");
        fs::write(&outside, solo.as_bytes()).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
        std::os::unix::fs::symlink(dir.join("solo"), dir.join("inner")).unwrap();

        let list = fixture.service.list_keys().await.unwrap();
        let names: Vec<&str> = list.keys.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(names, ["inner", "legacy", "locked", "lonely", "solo"]);
        let by_name = |name: &str| list.keys.iter().find(|key| key.name == name).unwrap();

        let solo_view = by_name("solo");
        assert!(solo_view.public_key_path.is_none());
        assert_eq!(solo_view.comment.as_deref(), Some("solo@host"));
        assert!(solo_view
            .public_key
            .as_deref()
            .unwrap()
            .starts_with("ssh-ed25519 "));
        assert_eq!(by_name("inner").fingerprint, solo_view.fingerprint);

        let lonely_view = by_name("lonely");
        assert!(lonely_view.private_key_path.is_none());
        assert_eq!(lonely_view.encrypted, None);
        assert_eq!(lonely_view.algorithm, "ecdsa-sha2-nistp256");

        let locked_view = by_name("locked");
        assert_eq!(locked_view.encrypted, Some(true));
        assert!(locked_view.fingerprint.is_some(), "public half is in clear");
        assert_eq!(locked_view.comment, None);

        let legacy = by_name("legacy");
        assert_eq!(legacy.algorithm, "ssh-rsa");
        assert_eq!(legacy.encrypted, Some(true));
        assert_eq!(legacy.fingerprint, None);
    }

    #[tokio::test]
    async fn import_validates_and_never_overwrites() {
        let fixture = fixture("").await;
        let service = &fixture.service;
        let dir = ssh_dir(&fixture);
        let (private, public) = generate_pair(KeyKind::Ed25519, "me@laptop", None).unwrap();

        for request in [
            import("../escape", &private),
            import("ok", "not a key"),
            import(
                "ok",
                "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n",
            ),
        ] {
            assert!(matches!(
                service.import_key(request).await.unwrap_err(),
                AppError::InvalidRequest(_)
            ));
        }
        let (_, other_public) = generate_pair(KeyKind::Ed25519, "", None).unwrap();
        let mut mismatched = import("ok", &private);
        mismatched.public_key = Some(other_public);
        assert!(matches!(
            service.import_key(mismatched).await.unwrap_err(),
            AppError::InvalidRequest(_)
        ));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "only config");

        let crlf = private.replace('\n', "\r\n");
        let key = service.import_key(import("laptop", &crlf)).await.unwrap();
        assert_eq!(key.comment.as_deref(), Some("me@laptop"));
        assert_eq!(key.public_key.as_deref(), Some(public.as_str()));
        assert_eq!(fs::read_to_string(dir.join("laptop")).unwrap(), *private);
        assert_eq!(mode(&dir.join("laptop")), 0o600);
        assert_eq!(mode(&dir.join("laptop.pub")), 0o644);
        assert!(matches!(
            service
                .import_key(import("laptop", &private))
                .await
                .unwrap_err(),
            AppError::Conflict(_)
        ));

        // An existing .pub blocks the import without leaving a private file.
        fs::write(dir.join("taken.pub"), "x").unwrap();
        assert!(matches!(
            service
                .import_key(import("taken", &private))
                .await
                .unwrap_err(),
            AppError::Conflict(_)
        ));
        assert!(!dir.join("taken").exists());

        let (locked, _) = generate_pair(KeyKind::Ed25519, "locked@host", Some("pw")).unwrap();
        let mut wrong = import("locked", &locked);
        wrong.passphrase = Some("nope".into());
        assert!(matches!(
            service.import_key(wrong).await.unwrap_err(),
            AppError::InvalidRequest(_)
        ));
        let mut right = import("locked", &locked);
        right.passphrase = Some("pw".into());
        let key = service.import_key(right).await.unwrap();
        assert_eq!(key.encrypted, Some(true));
        assert_eq!(key.comment.as_deref(), Some("locked@host"));
        assert_eq!(fs::read_to_string(dir.join("locked")).unwrap(), *locked);
    }

    /// Seconds in dev builds thanks to the `rsa`/`num-bigint-dig` opt-level
    /// override in Cargo.toml.
    #[tokio::test]
    async fn generates_rsa_4096() {
        let fixture = fixture("").await;
        let key = fixture
            .service
            .generate_key(generate("id_rsa", KeyKind::Rsa, None))
            .await
            .unwrap();
        assert_eq!(key.algorithm, "ssh-rsa");
        assert_eq!(key.bits, Some(4096));
    }

    #[tokio::test]
    async fn creates_missing_ssh_dir_owner_only() {
        let fixture = fixture("").await;
        let dir = ssh_dir(&fixture);
        fs::remove_dir_all(&dir).unwrap();
        assert!(fixture.service.list_keys().await.unwrap().keys.is_empty());
        fixture
            .service
            .generate_key(generate("fresh", KeyKind::Ed25519, None))
            .await
            .unwrap();
        assert_eq!(mode(&dir), 0o700);
    }
}
