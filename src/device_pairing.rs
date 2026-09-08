//! Device enrollment is separate from normal authenticated API access. Only a
//! local owner-private decision file can approve a request. A matching short
//! code authenticates the ephemeral transcript; the credential is returned
//! once, encrypted for the initiating client's ephemeral private key.
use crate::error::AppError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};

type Result<T> = std::result::Result<T, AppError>;
const TTL: Duration = Duration::from_secs(300);
const MAX_ACTIVE: usize = 16;
const MAX_RECORDS: usize = 64;
const MAX_LOCAL_FILE: u64 = 4096;
const DIRECTORY: &str = "device-pairing";
const TRANSCRIPT_DOMAIN: &[u8] = b"todex.device-pairing.v1/transcript\0";
const WRAP_INFO: &[u8] = b"todex.device-pairing.v1/wrap-key";
const POLL_INFO: &[u8] = b"todex.device-pairing.v1/poll-proof";
const CANCEL_INFO: &[u8] = b"todex.device-pairing.v1/cancel-proof";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateDevicePairingRequest {
    pub client_public_key: String,
    pub device_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateDevicePairingResponse {
    pub request_id: String,
    pub server_public_key: String,
    pub expires_at: u64,
    pub poll_interval_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DevicePairingProofRequest {
    pub request_id: String,
    pub proof: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevicePairingPollResponse {
    pub status: &'static str,
    pub expires_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ciphertext: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DevicePairingRequestSummary {
    pub request_id: String,
    pub verification_code: String,
    pub device_name: String,
    pub expires_at: u64,
    pub peer_address: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalDecision {
    request_id: String,
    approved: bool,
}

struct PairingMaterial {
    transcript: Vec<u8>,
    wrap_key: [u8; 32],
    poll_proof: [u8; 32],
    cancel_proof: [u8; 32],
    verification_code: String,
}

impl PairingMaterial {
    fn derive(
        request_id: &str,
        client_public: &[u8; 32],
        server_public: &[u8; 32],
        shared: &[u8; 32],
    ) -> Result<Self> {
        if bool::from(shared.ct_eq(&[0; 32])) {
            return Err(invalid("invalid client public key"));
        }
        let transcript = [
            TRANSCRIPT_DOMAIN,
            request_id.as_bytes(),
            &[0],
            client_public,
            server_public,
        ]
        .concat();
        let salt = Sha256::digest(&transcript);
        let hkdf = Hkdf::<Sha256>::new(Some(&salt), shared);
        let expand = |info: &[u8]| -> Result<[u8; 32]> {
            let mut key = [0; 32];
            hkdf.expand(info, &mut key)
                .map_err(|_| invalid("unable to derive pairing key"))?;
            Ok(key)
        };
        let short = salt[..5]
            .iter()
            .map(|value| format!("{value:02X}"))
            .collect::<String>();
        Ok(Self {
            transcript,
            wrap_key: expand(WRAP_INFO)?,
            poll_proof: expand(POLL_INFO)?,
            cancel_proof: expand(CANCEL_INFO)?,
            verification_code: format!("{}-{}", &short[..5], &short[5..]),
        })
    }

    fn wrap(&self, token: &str, nonce: &[u8; 24]) -> Result<String> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Credential<'a> {
            auth_token: &'a str,
        }
        let plaintext = serde_json::to_vec(&Credential { auth_token: token })?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&self.wrap_key));
        let encrypted = cipher
            .encrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: &plaintext,
                    aad: &self.transcript,
                },
            )
            .map_err(|_| invalid("unable to encrypt pairing response"))?;
        Ok(URL_SAFE_NO_PAD.encode(encrypted))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestStatus {
    Pending,
    Approved,
    Rejected,
}
struct PendingRequest {
    material: PairingMaterial,
    expires: Instant,
    expires_at: u64,
    status: RequestStatus,
}
struct RegistryState {
    directory: PathBuf,
    requests: HashMap<String, PendingRequest>,
    creates: VecDeque<(Instant, IpAddr)>,
    lookups: VecDeque<(Instant, IpAddr)>,
}
impl Drop for RegistryState {
    fn drop(&mut self) {
        for id in self.requests.keys() {
            remove_local_files(&self.directory, id);
        }
    }
}

#[derive(Clone)]
pub(crate) struct DevicePairingRegistry {
    inner: Arc<Mutex<RegistryState>>,
    token: Option<Arc<String>>,
}
impl DevicePairingRegistry {
    pub(crate) fn new(data_dir: &Path, token: Option<String>) -> Result<Self> {
        let directory = prepare_directory(data_dir)?;
        // Another daemon can share this data directory, or a second start can
        // fail after constructing AppState. Never delete its live requests.
        // Crash leftovers disappear only after their recorded TTL has elapsed.
        for entry in fs::read_dir(&directory)?.take(MAX_RECORDS * 4) {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name
                .strip_prefix("request-")
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            if validate_id(id).is_err() || !entry.file_type()?.is_file() {
                continue;
            }
            if read_private_json::<DevicePairingRequestSummary>(&entry.path())?
                .is_some_and(|summary| summary.request_id == id && summary.expires_at <= unix_ms())
            {
                remove_local_files(&directory, id);
            }
        }
        Ok(Self {
            token: token.map(Arc::new),
            inner: Arc::new(Mutex::new(RegistryState {
                directory,
                requests: HashMap::new(),
                creates: VecDeque::new(),
                lookups: VecDeque::new(),
            })),
        })
    }

    pub(crate) fn create(
        &self,
        peer: IpAddr,
        request: CreateDevicePairingRequest,
    ) -> Result<CreateDevicePairingResponse> {
        if self.token.is_none() {
            return Err(AppError::Conflict(
                "Authentication is disabled; connect without device pairing".to_owned(),
            ));
        }
        if request.device_name.chars().count() > 80
            || request.device_name.chars().any(char::is_control)
        {
            return Err(invalid(
                "deviceName must contain at most 80 printable characters",
            ));
        }
        let client_public = decode_32(&request.client_public_key)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| invalid("pairing registry is unavailable"))?;
        state.prune()?;
        rate_limit(&mut state.creates, peer, Duration::from_secs(60), 24, 4)?;
        if state.requests.len() >= MAX_RECORDS
            || state
                .requests
                .values()
                .filter(|request| request.status != RequestStatus::Rejected)
                .count()
                >= MAX_ACTIVE
        {
            return Err(AppError::ResourceExhausted(
                "Too many pending device pairing requests".to_owned(),
            ));
        }
        let server_secret = StaticSecret::random_from_rng(OsRng);
        let server_public = PublicKey::from(&server_secret).to_bytes();
        let shared = server_secret
            .diffie_hellman(&PublicKey::from(client_public))
            .to_bytes();
        let request_id = Uuid::new_v4().to_string();
        let material =
            PairingMaterial::derive(&request_id, &client_public, &server_public, &shared)?;
        let expires_at = unix_ms().saturating_add(TTL.as_millis() as u64);
        let summary = DevicePairingRequestSummary {
            request_id: request_id.clone(),
            verification_code: material.verification_code.clone(),
            device_name: if request.device_name.trim().is_empty() {
                "Unknown device".to_owned()
            } else {
                request.device_name.trim().to_owned()
            },
            expires_at,
            peer_address: peer.to_string(),
        };
        write_new_json(
            &state.directory,
            &summary_path(&state.directory, &request_id),
            &summary,
        )?;
        state.requests.insert(
            request_id.clone(),
            PendingRequest {
                material,
                expires: Instant::now() + TTL,
                expires_at,
                status: RequestStatus::Pending,
            },
        );
        Ok(CreateDevicePairingResponse {
            request_id,
            server_public_key: URL_SAFE_NO_PAD.encode(server_public),
            expires_at,
            poll_interval_ms: 1000,
        })
    }

    pub(crate) fn poll(
        &self,
        peer: IpAddr,
        request: DevicePairingProofRequest,
    ) -> Result<DevicePairingPollResponse> {
        validate_id(&request.request_id)?;
        let proof = decode_32(&request.proof)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| invalid("pairing registry is unavailable"))?;
        rate_limit(&mut state.lookups, peer, Duration::from_secs(1), 128, 32)?;
        state.prune()?;
        let Some(pending) = state.requests.get(&request.request_id) else {
            return Ok(poll_state("expired", unix_ms()));
        };
        if !bool::from(pending.material.poll_proof.ct_eq(&proof)) {
            return Err(AppError::Unauthenticated);
        }
        if pending.status == RequestStatus::Pending {
            return Ok(poll_state("pending", pending.expires_at));
        }
        // Removal and encryption happen under one lock: even concurrent valid
        // polls cannot receive the credential twice. A lost reply requires a
        // fresh pairing; the server never retries credential delivery.
        let pending = state
            .requests
            .remove(&request.request_id)
            .expect("request exists while locked");
        remove_local_files(&state.directory, &request.request_id);
        if pending.status == RequestStatus::Rejected {
            return Ok(poll_state("rejected", pending.expires_at));
        }
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut nonce);
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| invalid("authentication is disabled"))?;
        let ciphertext = pending.material.wrap(token, &nonce)?;
        Ok(DevicePairingPollResponse {
            status: "approved",
            expires_at: pending.expires_at,
            nonce: Some(URL_SAFE_NO_PAD.encode(nonce)),
            ciphertext: Some(ciphertext),
        })
    }

    pub(crate) fn cancel(&self, peer: IpAddr, request: DevicePairingProofRequest) -> Result<()> {
        validate_id(&request.request_id)?;
        let proof = decode_32(&request.proof)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| invalid("pairing registry is unavailable"))?;
        rate_limit(&mut state.lookups, peer, Duration::from_secs(1), 128, 32)?;
        state.prune()?;
        let Some(pending) = state.requests.get(&request.request_id) else {
            return Ok(());
        };
        if !bool::from(pending.material.cancel_proof.ct_eq(&proof)) {
            return Err(AppError::Unauthenticated);
        }
        state.requests.remove(&request.request_id);
        remove_local_files(&state.directory, &request.request_id);
        Ok(())
    }
}

impl RegistryState {
    fn prune(&mut self) -> Result<()> {
        let now = Instant::now();
        let expired = self
            .requests
            .iter()
            .filter(|(_, request)| request.expires <= now)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            self.requests.remove(&id);
            remove_local_files(&self.directory, &id);
        }
        for (id, request) in &mut self.requests {
            if request.status != RequestStatus::Pending {
                continue;
            }
            let path = decision_path(&self.directory, id);
            let Some(decision) = read_private_json::<LocalDecision>(&path)? else {
                continue;
            };
            if decision.request_id != *id {
                return Err(invalid("local pairing decision does not match its request"));
            }
            request.status = if decision.approved {
                RequestStatus::Approved
            } else {
                RequestStatus::Rejected
            };
            remove_local_files(&self.directory, id);
        }
        Ok(())
    }
}

fn poll_state(status: &'static str, expires_at: u64) -> DevicePairingPollResponse {
    DevicePairingPollResponse {
        status,
        expires_at,
        nonce: None,
        ciphertext: None,
    }
}
fn rate_limit(
    events: &mut VecDeque<(Instant, IpAddr)>,
    peer: IpAddr,
    window: Duration,
    global: usize,
    per_peer: usize,
) -> Result<()> {
    let now = Instant::now();
    while events
        .front()
        .is_some_and(|(instant, _)| now.duration_since(*instant) >= window)
    {
        events.pop_front();
    }
    if events.len() >= global || events.iter().filter(|(_, ip)| *ip == peer).count() >= per_peer {
        return Err(AppError::ResourceExhausted(
            "Device pairing rate limit reached; retry later".to_owned(),
        ));
    }
    events.push_back((now, peer));
    Ok(())
}
fn decode_32(value: &str) -> Result<[u8; 32]> {
    if value.len() != 43 {
        return Err(invalid("invalid pairing key or proof"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| invalid("invalid pairing key or proof"))?;
    bytes
        .try_into()
        .map_err(|_| invalid("invalid pairing key or proof"))
}
fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn validate_id(id: &str) -> Result<()> {
    if Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
        Ok(())
    } else {
        Err(invalid("invalid pairing request id"))
    }
}
fn summary_path(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("request-{id}.json"))
}
fn decision_path(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("decision-{id}.json"))
}
fn remove_local_files(directory: &Path, id: &str) {
    let _ = fs::remove_file(summary_path(directory, id));
    let _ = fs::remove_file(decision_path(directory, id));
}

pub(crate) fn list_device_pairing_requests(
    data_dir: &Path,
) -> Result<Vec<DevicePairingRequestSummary>> {
    let directory = data_dir.join(DIRECTORY);
    if !directory.exists() {
        return Ok(vec![]);
    }
    validate_private_directory(&directory)?;
    let mut requests = vec![];
    for entry in fs::read_dir(&directory)?.take(MAX_RECORDS * 4) {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("request-")
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if validate_id(id).is_err() || decision_path(&directory, id).exists() {
            continue;
        }
        if let Some(summary) = read_private_json::<DevicePairingRequestSummary>(&entry.path())? {
            if summary.request_id == id && summary.expires_at > unix_ms() {
                requests.push(summary);
            }
        }
    }
    requests.sort_by(|a, b| {
        a.expires_at
            .cmp(&b.expires_at)
            .then(a.request_id.cmp(&b.request_id))
    });
    Ok(requests)
}

pub(crate) fn decide_device_pairing(
    data_dir: &Path,
    request_id: &str,
    approved: bool,
) -> Result<()> {
    validate_id(request_id)?;
    let directory = data_dir.join(DIRECTORY);
    validate_private_directory(&directory)?;
    let summary =
        read_private_json::<DevicePairingRequestSummary>(&summary_path(&directory, request_id))?
            .ok_or_else(|| AppError::NotFound("pairing request no longer exists".to_owned()))?;
    if summary.request_id != request_id || summary.expires_at <= unix_ms() {
        return Err(AppError::Conflict("pairing request expired".to_owned()));
    }
    write_new_json(
        &directory,
        &decision_path(&directory, request_id),
        &LocalDecision {
            request_id: request_id.to_owned(),
            approved,
        },
    )
}

fn prepare_directory(data_dir: &Path) -> Result<PathBuf> {
    let directory = fs::canonicalize(data_dir)?.join(DIRECTORY);
    let builder = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        }
        #[cfg(not(unix))]
        {
            fs::DirBuilder::new()
        }
    };
    match builder.create(&directory) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    #[cfg(windows)]
    {
        use crate::transport_crypto::pairing_browser::{current_windows_sid, secure_windows_path};
        secure_windows_path(&directory, &current_windows_sid()?, true)?;
    }
    validate_private_directory(&directory)?;
    Ok(directory)
}
fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(
            "pairing control directory must not be a symbolic link",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid(
                "pairing control directory must be owned and accessible only by the current user",
            ));
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(AppError::Unsupported(
        "private device pairing control is unavailable on this platform".to_owned(),
    ));
    #[cfg(any(unix, windows))]
    Ok(())
}
fn write_new_json<T: Serialize>(directory: &Path, path: &Path, value: &T) -> Result<()> {
    validate_private_directory(directory)?;
    let temporary = directory.join(format!(".pairing-{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        #[cfg(windows)]
        {
            use crate::transport_crypto::pairing_browser::{
                current_windows_sid, secure_windows_path,
            };
            secure_windows_path(&temporary, &current_windows_sid()?, false)?;
        }
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() as u64 > MAX_LOCAL_FILE {
            return Err(invalid("pairing control record is too large"));
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        // Atomic publication without replacing an existing decision.
        fs::hard_link(&temporary, path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::Conflict("pairing request already has a decision".to_owned())
            } else {
                error.into()
            }
        })?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
fn read_private_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_LOCAL_FILE {
        return Err(invalid("invalid local pairing control record"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid("local pairing control record is not private"));
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_LOCAL_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LOCAL_FILE {
        return Err(invalid("local pairing control record is too large"));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("todex-device-pairing-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn registry(&self) -> DevicePairingRegistry {
            DevicePairingRegistry::new(&self.root, Some("synthetic-approved-token".to_owned()))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn peer() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }
    fn begin(registry: &DevicePairingRegistry) -> (CreateDevicePairingResponse, PairingMaterial) {
        let client_secret = StaticSecret::from([7; 32]);
        let client_public = PublicKey::from(&client_secret).to_bytes();
        let response = registry
            .create(
                peer(),
                CreateDevicePairingRequest {
                    client_public_key: URL_SAFE_NO_PAD.encode(client_public),
                    device_name: "Synthetic device".to_owned(),
                },
            )
            .unwrap();
        let server_public = decode_32(&response.server_public_key).unwrap();
        let shared = client_secret
            .diffie_hellman(&PublicKey::from(server_public))
            .to_bytes();
        let material = PairingMaterial::derive(
            &response.request_id,
            &client_public,
            &server_public,
            &shared,
        )
        .unwrap();
        (response, material)
    }
    fn proof(id: &str, value: &[u8; 32]) -> DevicePairingProofRequest {
        DevicePairingProofRequest {
            request_id: id.to_owned(),
            proof: URL_SAFE_NO_PAD.encode(value),
        }
    }
    fn decrypt(material: &PairingMaterial, response: &DevicePairingPollResponse) -> Value {
        let nonce = URL_SAFE_NO_PAD
            .decode(response.nonce.as_ref().unwrap())
            .unwrap();
        let ciphertext = URL_SAFE_NO_PAD
            .decode(response.ciphertext.as_ref().unwrap())
            .unwrap();
        let plaintext = XChaCha20Poly1305::new(Key::from_slice(&material.wrap_key))
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &material.transcript,
                },
            )
            .unwrap();
        serde_json::from_slice(&plaintext).unwrap()
    }
    fn vector() -> Value {
        let id = "11111111-2222-4333-8444-555555555555";
        let client_secret = StaticSecret::from([7; 32]);
        let server_secret = StaticSecret::from([9; 32]);
        let client_public = PublicKey::from(&client_secret).to_bytes();
        let server_public = PublicKey::from(&server_secret).to_bytes();
        let shared = server_secret
            .diffie_hellman(&PublicKey::from(client_public))
            .to_bytes();
        let material =
            PairingMaterial::derive(id, &client_public, &server_public, &shared).unwrap();
        json!({
            "requestId": id, "expiresAt": 2000000300000_u64,
            "clientSecret": URL_SAFE_NO_PAD.encode([7; 32]), "serverSecret": URL_SAFE_NO_PAD.encode([9; 32]),
            "clientPublicKey": URL_SAFE_NO_PAD.encode(client_public), "serverPublicKey": URL_SAFE_NO_PAD.encode(server_public),
            "transcript": URL_SAFE_NO_PAD.encode(&material.transcript), "verificationCode": material.verification_code,
            "wrapKey": URL_SAFE_NO_PAD.encode(material.wrap_key), "pollProof": URL_SAFE_NO_PAD.encode(material.poll_proof), "cancelProof": URL_SAFE_NO_PAD.encode(material.cancel_proof),
            "nonce": URL_SAFE_NO_PAD.encode([11; 24]), "authToken": "synthetic-device-pairing-token",
            "ciphertext": material.wrap("synthetic-device-pairing-token", &[11; 24]).unwrap(),
        })
    }
    #[test]
    fn device_pairing_cross_language_vector_matches() {
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/device-pairing-v1.json")).unwrap();
        assert_eq!(vector(), expected);
    }

    #[test]
    fn local_approval_delivers_wrapped_credential_once_and_never_persists_secrets() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let (created, material) = begin(&registry);
        let summaries = list_device_pairing_requests(&fixture.root).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].verification_code, material.verification_code);
        let file = fs::read_to_string(summary_path(
            &fixture.root.join(DIRECTORY),
            &created.request_id,
        ))
        .unwrap();
        for excluded in [
            "authToken",
            "synthetic-approved-token",
            "pollProof",
            "cancelProof",
            "privateKey",
            "clientSecret",
        ] {
            assert!(!file.contains(excluded));
        }
        assert_eq!(
            registry
                .poll(peer(), proof(&created.request_id, &material.poll_proof))
                .unwrap()
                .status,
            "pending"
        );
        decide_device_pairing(&fixture.root, &created.request_id, true).unwrap();
        assert!(list_device_pairing_requests(&fixture.root)
            .unwrap()
            .is_empty());
        assert!(decide_device_pairing(&fixture.root, &created.request_id, false).is_err());
        let response = registry
            .poll(peer(), proof(&created.request_id, &material.poll_proof))
            .unwrap();
        assert_eq!(response.status, "approved");
        assert!(!serde_json::to_string(&response)
            .unwrap()
            .contains("synthetic-approved-token"));
        assert_eq!(
            decrypt(&material, &response)["authToken"],
            "synthetic-approved-token"
        );
        assert_eq!(
            registry
                .poll(peer(), proof(&created.request_id, &material.poll_proof))
                .unwrap()
                .status,
            "expired"
        );
    }

    #[test]
    fn proofs_bind_the_request_and_operation_and_rejection_cancellation_ttl_are_final() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let (created, material) = begin(&registry);
        assert!(matches!(
            registry.poll(peer(), proof(&created.request_id, &material.cancel_proof)),
            Err(AppError::Unauthenticated)
        ));
        assert!(matches!(
            registry.cancel(peer(), proof(&created.request_id, &material.poll_proof)),
            Err(AppError::Unauthenticated)
        ));
        let (other, other_material) = begin(&registry);
        assert!(matches!(
            registry.poll(peer(), proof(&other.request_id, &material.poll_proof)),
            Err(AppError::Unauthenticated)
        ));
        decide_device_pairing(&fixture.root, &created.request_id, false).unwrap();
        assert_eq!(
            registry
                .poll(peer(), proof(&created.request_id, &material.poll_proof))
                .unwrap()
                .status,
            "rejected"
        );
        registry
            .cancel(
                peer(),
                proof(&other.request_id, &other_material.cancel_proof),
            )
            .unwrap();
        assert_eq!(
            registry
                .poll(peer(), proof(&other.request_id, &other_material.poll_proof))
                .unwrap()
                .status,
            "expired"
        );
        assert!(decide_device_pairing(&fixture.root, &other.request_id, true).is_err());
        let (expired, material) = begin(&registry);
        registry
            .inner
            .lock()
            .unwrap()
            .requests
            .get_mut(&expired.request_id)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            registry
                .poll(peer(), proof(&expired.request_id, &material.poll_proof))
                .unwrap()
                .status,
            "expired"
        );
        assert!(decide_device_pairing(&fixture.root, &expired.request_id, true).is_err());
        assert!(list_device_pairing_requests(&fixture.root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn concurrent_claims_can_only_receive_one_envelope() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let (created, material) = begin(&registry);
        decide_device_pairing(&fixture.root, &created.request_id, true).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let registry = registry.clone();
                let id = created.request_id.clone();
                let value = material.poll_proof;
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.poll(peer(), proof(&id, &value)).unwrap().status
                })
            })
            .collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|status| **status == "approved")
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|status| **status == "expired")
                .count(),
            7
        );
    }

    #[test]
    fn unauthenticated_creation_has_rate_capacity_and_key_validation_limits() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        assert!(registry
            .create(
                peer(),
                CreateDevicePairingRequest {
                    client_public_key: URL_SAFE_NO_PAD.encode([0; 32]),
                    device_name: "Device".into()
                }
            )
            .is_err());
        for _ in 0..3 {
            begin(&registry);
        }
        let key = URL_SAFE_NO_PAD.encode(PublicKey::from(&StaticSecret::from([7; 32])).to_bytes());
        assert!(matches!(
            registry.create(
                peer(),
                CreateDevicePairingRequest {
                    client_public_key: key.clone(),
                    device_name: "Device".into()
                }
            ),
            Err(AppError::ResourceExhausted(_))
        ));
        for index in 1..14 {
            registry
                .create(
                    format!("192.0.2.{index}").parse().unwrap(),
                    CreateDevicePairingRequest {
                        client_public_key: key.clone(),
                        device_name: "Device".into(),
                    },
                )
                .unwrap();
        }
        assert_eq!(registry.inner.lock().unwrap().requests.len(), MAX_ACTIVE);
        assert!(matches!(
            registry.create(
                "192.0.2.99".parse().unwrap(),
                CreateDevicePairingRequest {
                    client_public_key: key,
                    device_name: "Device".into()
                }
            ),
            Err(AppError::ResourceExhausted(_))
        ));
    }

    #[test]
    fn starting_another_registry_never_deletes_live_requests_or_decisions() {
        let fixture = Fixture::new();
        let first = fixture.registry();
        let (created, material) = begin(&first);
        let second = fixture.registry();
        assert_eq!(
            list_device_pairing_requests(&fixture.root).unwrap().len(),
            1
        );
        decide_device_pairing(&fixture.root, &created.request_id, true).unwrap();
        let third = fixture.registry();
        assert!(decision_path(&fixture.root.join(DIRECTORY), &created.request_id).exists());
        drop(second);
        drop(third);
        assert_eq!(
            first
                .poll(peer(), proof(&created.request_id, &material.poll_proof))
                .unwrap()
                .status,
            "approved"
        );
    }

    #[test]
    fn auth_disabled_server_does_not_offer_pairing() {
        let fixture = Fixture::new();
        let registry = DevicePairingRegistry::new(&fixture.root, None).unwrap();
        assert!(matches!(
            registry.create(
                peer(),
                CreateDevicePairingRequest {
                    client_public_key: String::new(),
                    device_name: String::new()
                }
            ),
            Err(AppError::Conflict(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_control_files_are_owner_only_and_reject_symbolic_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let (created, _) = begin(&registry);
        let directory = fixture.root.join(DIRECTORY);
        let path = summary_path(&directory, &created.request_id);
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let outside = fixture.root.join("outside");
        fs::write(&outside, "synthetic unrelated file").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(list_device_pairing_requests(&fixture.root).is_err());
        assert!(decide_device_pairing(&fixture.root, &created.request_id, true).is_err());
        assert_eq!(
            fs::read_to_string(outside).unwrap(),
            "synthetic unrelated file"
        );
    }
}
