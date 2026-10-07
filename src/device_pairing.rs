//! Device enrollment is separate from normal authenticated API access. Only a
//! local owner-private decision file can approve a request. A matching short
//! code authenticates the ephemeral transcript; on approval the client's
//! long-term Ed25519 device key is registered and the assigned device id is
//! returned once, encrypted for the initiating client's ephemeral private key.
//!
//! Pairing v3 (commit, then reveal): `create` carries only a commitment to the
//! client's ephemeral key and nonce, and the server answers with its own
//! per-request key. The client then reveals the key and nonce; only then is
//! the transcript fixed and the verification code shown. A man in the middle
//! can no longer grind its own key against the 40-bit code, because its key
//! is committed before it sees the server key.
//!
//! Pairing also delivers the daemon's transport public key (the static key
//! of the v2 handshake). It is bound into the transcript, so the verification
//! code authenticates it, and it is repeated inside the encrypted credential;
//! clients pin it only when both agree.
use crate::config::PairingEncryption;
use crate::devices::DeviceRegistry;
use crate::error::AppError;
use crate::transport_crypto::PairingKeyStore;
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
use zeroize::{Zeroize, Zeroizing};

type Result<T> = std::result::Result<T, AppError>;
const TTL: Duration = Duration::from_secs(300);
const MAX_ACTIVE: usize = 16;
const MAX_RECORDS: usize = 64;
const MAX_LOCAL_FILE: u64 = 4096;
const DIRECTORY: &str = "device-pairing";
const COMMIT_LABEL: &[u8] = b"todex.device-pairing.v3/commit";
const TRANSCRIPT_DOMAIN: &[u8] = b"todex.device-pairing.v3/transcript\0";
const WRAP_INFO: &[u8] = b"todex.device-pairing.v3/wrap-key";
const POLL_INFO: &[u8] = b"todex.device-pairing.v3/poll-proof";
const CANCEL_INFO: &[u8] = b"todex.device-pairing.v3/cancel-proof";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateDevicePairingRequest {
    /// `SHA256(LP("todex.device-pairing.v3/commit") || client_public ||
    /// client_nonce)`; the key and nonce follow in `reveal`.
    #[serde(default)]
    pub client_commitment: Option<String>,
    /// Pairing v2 sent the ephemeral key here directly; such clients get
    /// `426 PROTOCOL_UPGRADE_REQUIRED`.
    #[serde(default)]
    pub client_public_key: Option<String>,
    /// Must be the JSON integer `1`: the client validates and pins the
    /// transport key this response delivers. Anything else (including a
    /// missing field from a client that still pins keys manually) gets
    /// `426 PROTOCOL_UPGRADE_REQUIRED`.
    #[serde(default)]
    pub transport_binding: Option<serde_json::Value>,
    pub device_name: String,
    /// Long-term Ed25519 device identity key registered on approval.
    pub device_public_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RevealDevicePairingRequest {
    pub request_id: String,
    /// Ephemeral X25519 key that encrypts the approval payload.
    pub client_public_key: String,
    /// 32 random bytes committed together with the key.
    pub client_nonce: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RevealDevicePairingResponse {
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateDevicePairingResponse {
    pub request_id: String,
    pub server_public_key: String,
    pub expires_at: u64,
    pub poll_interval_ms: u64,
    /// The server's configured `pairing_encryption`.
    pub transport_protocol: PairingEncryption,
    /// Static public key the v2 handshake uses right now for that protocol,
    /// base64url without padding; empty for `none`.
    pub transport_public_key: String,
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
    /// Fingerprint of the transport key bound into this request's code
    /// (`None` for `pairing_encryption = "none"`), shown next to the code.
    #[serde(default)]
    pub transport_fingerprint: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalDecision {
    request_id: String,
    approved: bool,
}

/// The transport key a request delivers. Captured at `create`, bound into
/// the transcript at `reveal` and repeated in the credential.
#[derive(Clone, PartialEq, Eq)]
struct TransportBinding {
    protocol: PairingEncryption,
    public_key: Vec<u8>,
}

/// `u32_be(len(x)) || x`.
fn length_prefixed(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + value.len());
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
}

struct PairingMaterial {
    transcript: Vec<u8>,
    wrap_key: [u8; 32],
    poll_proof: [u8; 32],
    cancel_proof: [u8; 32],
    verification_code: String,
}

impl Drop for PairingMaterial {
    fn drop(&mut self) {
        self.wrap_key.zeroize();
        self.poll_proof.zeroize();
        self.cancel_proof.zeroize();
    }
}

fn commitment(client_public: &[u8; 32], client_nonce: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((COMMIT_LABEL.len() as u32).to_be_bytes());
    hasher.update(COMMIT_LABEL);
    hasher.update(client_public);
    hasher.update(client_nonce);
    hasher.finalize().into()
}

impl PairingMaterial {
    fn derive(
        request_id: &str,
        client_public: &[u8; 32],
        server_public: &[u8; 32],
        device_public: &[u8; 32],
        client_nonce: &[u8; 32],
        transport: &TransportBinding,
        shared: &[u8; 32],
    ) -> Result<Self> {
        if bool::from(shared.ct_eq(&[0; 32])) {
            return Err(invalid("invalid client public key"));
        }
        // The enrolled device key and the delivered transport key are part
        // of the transcript, so the short verification code also binds the
        // identity being approved and the key the client will pin.
        let transcript = [
            TRANSCRIPT_DOMAIN,
            request_id.as_bytes(),
            &[0],
            client_public,
            server_public,
            &[0],
            device_public,
            client_nonce,
            &length_prefixed(transport.protocol.as_str().as_bytes()),
            &length_prefixed(&transport.public_key),
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

    fn wrap(
        &self,
        device_id: &str,
        transport: &TransportBinding,
        nonce: &[u8; 24],
    ) -> Result<String> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Credential<'a> {
            device_id: &'a str,
            transport_protocol: PairingEncryption,
            transport_public_key: String,
        }
        let plaintext = serde_json::to_vec(&Credential {
            device_id,
            transport_protocol: transport.protocol,
            transport_public_key: URL_SAFE_NO_PAD.encode(&transport.public_key),
        })?;
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
/// Before `reveal` only the commitment and the server's per-request secret
/// exist; the proofs and the verification code need the revealed key.
enum Stage {
    Committed {
        server_secret: StaticSecret,
        commitment: [u8; 32],
    },
    Revealed(PairingMaterial),
}

struct PendingRequest {
    stage: Stage,
    server_public: [u8; 32],
    transport: TransportBinding,
    peer: IpAddr,
    device_name: String,
    device_public_key: [u8; 32],
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
    enabled: bool,
    devices: DeviceRegistry,
    /// The handshake's key store and configured protocol: pairing delivers
    /// exactly the key the transport uses.
    transport_keys: PairingKeyStore,
    transport_protocol: PairingEncryption,
}
impl DevicePairingRegistry {
    pub(crate) fn new(
        data_dir: &Path,
        enabled: bool,
        devices: DeviceRegistry,
        transport_keys: PairingKeyStore,
        transport_protocol: PairingEncryption,
    ) -> Result<Self> {
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
            enabled,
            devices,
            transport_keys,
            transport_protocol,
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
        if !self.enabled {
            return Err(AppError::Conflict(
                "Authentication is disabled; connect without device pairing".to_owned(),
            ));
        }
        if request.client_public_key.is_some() {
            return Err(AppError::ProtocolUpgradeRequired(
                "device pairing v2 is retired; update this client to pairing v3 (clientCommitment)"
                    .to_owned(),
            ));
        }
        if request.transport_binding != Some(serde_json::Value::from(1)) {
            return Err(AppError::ProtocolUpgradeRequired(
                "this client does not take the transport key from device pairing; update it (transportBinding: 1)"
                    .to_owned(),
            ));
        }
        if request.device_name.chars().count() > 80
            || request.device_name.chars().any(char::is_control)
        {
            return Err(invalid(
                "deviceName must contain at most 80 printable characters",
            ));
        }
        let commitment = decode_32(
            request
                .client_commitment
                .as_deref()
                .ok_or_else(|| invalid("missing clientCommitment"))?,
        )?;
        let device_public = crate::devices::parse_public_key(&request.device_public_key)?;
        let transport = self.transport_binding()?;
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
        let request_id = Uuid::new_v4().to_string();
        let expires_at = unix_ms().saturating_add(TTL.as_millis() as u64);
        let device_name = if request.device_name.trim().is_empty() {
            "Unknown device".to_owned()
        } else {
            request.device_name.trim().to_owned()
        };
        state.requests.insert(
            request_id.clone(),
            PendingRequest {
                stage: Stage::Committed {
                    server_secret,
                    commitment,
                },
                server_public,
                transport: transport.clone(),
                peer,
                device_name,
                device_public_key: device_public,
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
            transport_protocol: transport.protocol,
            transport_public_key: URL_SAFE_NO_PAD.encode(&transport.public_key),
        })
    }

    /// The protocol and key the handshake would use right now.
    fn transport_binding(&self) -> Result<TransportBinding> {
        let keys = self.transport_keys.current()?;
        Ok(TransportBinding {
            protocol: self.transport_protocol,
            public_key: keys.transport_public_key(self.transport_protocol).to_vec(),
        })
    }

    /// Opens the commitment, fixes the transcript and only then publishes the
    /// verification code to the local approver. Single use: a second reveal,
    /// or one that does not match the commitment, fails (the latter also
    /// discards the request).
    pub(crate) fn reveal(
        &self,
        peer: IpAddr,
        request: RevealDevicePairingRequest,
    ) -> Result<RevealDevicePairingResponse> {
        validate_id(&request.request_id)?;
        let client_public = decode_32(&request.client_public_key)?;
        let client_nonce = decode_32(&request.client_nonce)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| invalid("pairing registry is unavailable"))?;
        rate_limit(&mut state.lookups, peer, Duration::from_secs(1), 128, 32)?;
        state.prune()?;
        let directory = state.directory.clone();
        let Some(pending) = state.requests.get_mut(&request.request_id) else {
            return Err(AppError::NotFound(
                "pairing request no longer exists".to_owned(),
            ));
        };
        let Stage::Committed {
            server_secret,
            commitment: expected,
        } = &pending.stage
        else {
            return Err(AppError::Conflict(
                "pairing request was already revealed".to_owned(),
            ));
        };
        if !bool::from(commitment(&client_public, &client_nonce).ct_eq(expected)) {
            state.requests.remove(&request.request_id);
            return Err(AppError::Unauthenticated);
        }
        let shared = Zeroizing::new(
            server_secret
                .diffie_hellman(&PublicKey::from(client_public))
                .to_bytes(),
        );
        let material = match PairingMaterial::derive(
            &request.request_id,
            &client_public,
            &pending.server_public,
            &pending.device_public_key,
            &client_nonce,
            &pending.transport,
            &shared,
        ) {
            Ok(material) => material,
            Err(error) => {
                state.requests.remove(&request.request_id);
                return Err(error);
            }
        };
        let summary = DevicePairingRequestSummary {
            request_id: request.request_id.clone(),
            verification_code: material.verification_code.clone(),
            device_name: pending.device_name.clone(),
            expires_at: pending.expires_at,
            peer_address: pending.peer.to_string(),
            transport_fingerprint: crate::transport_crypto::transport_fingerprint(
                &pending.transport.public_key,
            ),
        };
        // The summary file is what the local approver (TUI) lists, so the
        // code becomes visible only now.
        if let Err(error) = write_new_json(
            &directory,
            &summary_path(&directory, &request.request_id),
            &summary,
        ) {
            state.requests.remove(&request.request_id);
            return Err(error);
        }
        pending.stage = Stage::Revealed(material);
        Ok(RevealDevicePairingResponse { status: "pending" })
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
        if !pending
            .material()
            .is_some_and(|material| bool::from(material.poll_proof.ct_eq(&proof)))
        {
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
        // The keys were reset after this request was created: its credential
        // would pin a key the handshake no longer accepts. Nothing is
        // registered; the client starts a fresh pairing.
        if self.transport_binding()? != pending.transport {
            return Ok(poll_state("expired", pending.expires_at));
        }
        // Approval commits the long-term device key to the registry before the
        // id is delivered; a registration failure must not leak a device id
        // that cannot authenticate.
        let record = self
            .devices
            .register(&pending.device_name, &pending.device_public_key)?;
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut nonce);
        let Some(material) = pending.material() else {
            return Err(AppError::Unauthenticated);
        };
        let ciphertext = material.wrap(&record.device_id, &pending.transport, &nonce)?;
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
        if !pending
            .material()
            .is_some_and(|material| bool::from(material.cancel_proof.ct_eq(&proof)))
        {
            return Err(AppError::Unauthenticated);
        }
        state.requests.remove(&request.request_id);
        remove_local_files(&state.directory, &request.request_id);
        Ok(())
    }
}

impl PendingRequest {
    fn material(&self) -> Option<&PairingMaterial> {
        match &self.stage {
            Stage::Revealed(material) => Some(material),
            Stage::Committed { .. } => None,
        }
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
        keys: PairingKeyStore,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("todex-device-pairing-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let keys = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(PairingKeyStore::load(&root))
                .unwrap();
            Self { root, keys }
        }
        fn registry(&self) -> DevicePairingRegistry {
            self.registry_with(true, PairingEncryption::MlKem768)
        }
        fn registry_with(
            &self,
            enabled: bool,
            protocol: PairingEncryption,
        ) -> DevicePairingRegistry {
            DevicePairingRegistry::new(
                &self.root,
                enabled,
                DeviceRegistry::load(&self.root).unwrap(),
                self.keys.clone(),
                protocol,
            )
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
    fn device_key() -> [u8; 32] {
        ed25519_dalek::SigningKey::from_bytes(&[21; 32])
            .verifying_key()
            .to_bytes()
    }
    const CLIENT_NONCE: [u8; 32] = [5; 32];
    fn client_public() -> [u8; 32] {
        PublicKey::from(&StaticSecret::from([7; 32])).to_bytes()
    }
    fn create_request(commitment: Option<[u8; 32]>) -> CreateDevicePairingRequest {
        CreateDevicePairingRequest {
            client_commitment: commitment.map(|value| URL_SAFE_NO_PAD.encode(value)),
            client_public_key: None,
            transport_binding: Some(json!(1)),
            device_name: "Synthetic device".to_owned(),
            device_public_key: URL_SAFE_NO_PAD.encode(device_key()),
        }
    }
    fn reveal_request(
        id: &str,
        client_public: [u8; 32],
        nonce: [u8; 32],
    ) -> RevealDevicePairingRequest {
        RevealDevicePairingRequest {
            request_id: id.to_owned(),
            client_public_key: URL_SAFE_NO_PAD.encode(client_public),
            client_nonce: URL_SAFE_NO_PAD.encode(nonce),
        }
    }
    /// Commit only; the request is invisible to the local approver.
    fn commit(registry: &DevicePairingRegistry) -> CreateDevicePairingResponse {
        registry
            .create(
                peer(),
                create_request(Some(commitment(&client_public(), &CLIENT_NONCE))),
            )
            .unwrap()
    }
    fn begin(registry: &DevicePairingRegistry) -> (CreateDevicePairingResponse, PairingMaterial) {
        let client_secret = StaticSecret::from([7; 32]);
        let response = commit(registry);
        assert_eq!(
            registry
                .reveal(
                    peer(),
                    reveal_request(&response.request_id, client_public(), CLIENT_NONCE),
                )
                .unwrap()
                .status,
            "pending"
        );
        let server_public = decode_32(&response.server_public_key).unwrap();
        let shared = client_secret
            .diffie_hellman(&PublicKey::from(server_public))
            .to_bytes();
        let material = PairingMaterial::derive(
            &response.request_id,
            &client_public(),
            &server_public,
            &device_key(),
            &CLIENT_NONCE,
            &response_binding(&response),
            &shared,
        )
        .unwrap();
        (response, material)
    }
    /// The binding a client takes from the create response.
    fn response_binding(response: &CreateDevicePairingResponse) -> TransportBinding {
        TransportBinding {
            protocol: response.transport_protocol,
            public_key: URL_SAFE_NO_PAD
                .decode(&response.transport_public_key)
                .unwrap(),
        }
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
    fn fixture_bytes(value: &Value) -> Vec<u8> {
        let text = value.as_str().unwrap();
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }
    fn fixture_32(value: &Value) -> [u8; 32] {
        fixture_bytes(value).try_into().unwrap()
    }

    /// `pairingV3` in the shared transport v2 vectors
    /// (tests/fixtures/transport-v2.json, generated by TodeX_protocol).
    #[test]
    fn device_pairing_v3_cross_language_vector_matches() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/transport-v2.json")).unwrap();
        let vector = &fixture["pairingV3"];
        let request_id = vector["requestId"].as_str().unwrap();
        let client_secret = StaticSecret::from(fixture_32(&vector["clientSecretKey"]));
        let server_secret = StaticSecret::from(fixture_32(&vector["serverSecretKey"]));
        let client_public = PublicKey::from(&client_secret).to_bytes();
        let server_public = PublicKey::from(&server_secret).to_bytes();
        assert_eq!(client_public, fixture_32(&vector["clientPublicKey"]));
        assert_eq!(server_public, fixture_32(&vector["serverPublicKey"]));
        let device_public =
            ed25519_dalek::SigningKey::from_bytes(&fixture_32(&vector["deviceSeed"]))
                .verifying_key()
                .to_bytes();
        assert_eq!(device_public, fixture_32(&vector["devicePublicKey"]));
        let client_nonce = fixture_32(&vector["clientNonce"]);
        let expected_commitment = commitment(&client_public, &client_nonce);
        assert_eq!(expected_commitment, fixture_32(&vector["commitment"]));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(expected_commitment),
            vector["commitmentBase64Url"]
        );
        let shared = server_secret
            .diffie_hellman(&PublicKey::from(client_public))
            .to_bytes();
        assert_eq!(shared, fixture_32(&vector["shared"]));
        let binding = |case: &Value| TransportBinding {
            protocol: PairingEncryption::parse(case["transportProtocol"].as_str().unwrap())
                .filter(|protocol| protocol.as_str() == case["transportProtocol"])
                .unwrap(),
            public_key: URL_SAFE_NO_PAD
                .decode(case["transportPublicKey"].as_str().unwrap())
                .unwrap(),
        };
        let derive = |transport: &TransportBinding| {
            PairingMaterial::derive(
                request_id,
                &client_public,
                &server_public,
                &device_public,
                &client_nonce,
                transport,
                &shared,
            )
            .unwrap()
        };
        let transport = binding(vector);
        // The bound key is the ML-KEM server static key of the protocol
        // vectors, i.e. what the handshake would use.
        assert_eq!(transport.protocol, PairingEncryption::MlKem768);
        let ml_kem_static = fixture["protocols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["protocol"] == "ml-kem-768")
            .unwrap();
        assert_eq!(
            transport.public_key,
            fixture_bytes(&ml_kem_static["server"]["publicKey"])
        );
        let material = derive(&transport);
        assert_eq!(material.transcript, fixture_bytes(&vector["transcript"]));
        assert_eq!(
            Sha256::digest(&material.transcript).as_slice(),
            fixture_bytes(&vector["transcriptHash"])
        );
        assert_eq!(material.verification_code, vector["verificationCode"]);
        assert_eq!(material.wrap_key, fixture_32(&vector["wrapKey"]));
        assert_eq!(material.poll_proof, fixture_32(&vector["pollProof"]));
        assert_eq!(material.cancel_proof, fixture_32(&vector["cancelProof"]));
        assert_eq!(
            crate::transport_crypto::transport_fingerprint(&transport.public_key).unwrap(),
            vector["fingerprint"]
        );

        // The credential: XChaCha20-Poly1305 under the wrap key with the full
        // transcript as AAD; the plaintext repeats the transport key.
        let credential = &vector["credential"];
        let nonce: [u8; 24] = fixture_bytes(&credential["nonce"]).try_into().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(nonce), credential["nonceBase64Url"]);
        let wrapped = material.wrap("dev_vector", &transport, &nonce).unwrap();
        assert_eq!(wrapped, credential["ciphertextBase64Url"]);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&wrapped).unwrap(),
            fixture_bytes(&credential["ciphertext"])
        );
        let opened = XChaCha20Poly1305::new(Key::from_slice(&fixture_32(&vector["wrapKey"])))
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &fixture_bytes(&credential["ciphertext"]),
                    aad: &fixture_bytes(&vector["transcript"]),
                },
            )
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&opened).unwrap(),
            credential["plaintext"]
        );

        // A substituted transport key changes the code the user compares.
        for case in [&vector["tampered"], &vector["noneCase"]] {
            let other = derive(&binding(case));
            assert_eq!(
                Sha256::digest(&other.transcript).as_slice(),
                fixture_bytes(&case["transcriptHash"])
            );
            assert_eq!(other.verification_code, case["verificationCode"]);
            assert_ne!(other.verification_code, material.verification_code);
        }
        let none = binding(&vector["noneCase"]);
        assert_eq!(none.protocol, PairingEncryption::None);
        assert!(none.public_key.is_empty());
        assert_eq!(
            crate::transport_crypto::transport_fingerprint(&none.public_key),
            None
        );
        assert_eq!(vector["noneCase"]["fingerprint"], "none");
    }

    #[test]
    fn reveal_is_single_use_bound_to_the_commitment_and_gates_the_code() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        // v2 clients send the key in `create`.
        let mut legacy = create_request(None);
        legacy.client_public_key = Some(URL_SAFE_NO_PAD.encode(client_public()));
        assert!(matches!(
            registry.create(peer(), legacy),
            Err(AppError::ProtocolUpgradeRequired(_))
        ));
        // Clients that do not take the transport key from pairing.
        for binding in [
            None,
            Some(json!(0)),
            Some(json!(2)),
            Some(json!("1")),
            Some(json!(1.0)),
        ] {
            let mut request = create_request(Some([1; 32]));
            request.transport_binding = binding;
            assert!(matches!(
                registry.create(peer(), request),
                Err(AppError::ProtocolUpgradeRequired(_))
            ));
        }
        assert!(matches!(
            registry.create(peer(), create_request(None)),
            Err(AppError::InvalidRequest(_))
        ));

        let created = commit(&registry);
        // Nothing for the approver to see, and no proof works, before reveal.
        assert!(list_device_pairing_requests(&fixture.root)
            .unwrap()
            .is_empty());
        assert!(decide_device_pairing(&fixture.root, &created.request_id, true).is_err());
        assert!(matches!(
            registry.poll(peer(), proof(&created.request_id, &[0; 32])),
            Err(AppError::Unauthenticated)
        ));
        registry
            .reveal(
                peer(),
                reveal_request(&created.request_id, client_public(), CLIENT_NONCE),
            )
            .unwrap();
        let listed = list_device_pairing_requests(&fixture.root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].request_id, created.request_id);
        assert!(matches!(
            registry.reveal(
                peer(),
                reveal_request(&created.request_id, client_public(), CLIENT_NONCE),
            ),
            Err(AppError::Conflict(_))
        ));

        // A reveal that does not open the commitment discards the request.
        let other = commit(&registry);
        assert!(matches!(
            registry.reveal(
                peer(),
                reveal_request(&other.request_id, client_public(), [6; 32])
            ),
            Err(AppError::Unauthenticated)
        ));
        assert!(matches!(
            registry.reveal(
                peer(),
                reveal_request(&other.request_id, client_public(), CLIENT_NONCE),
            ),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn local_approval_delivers_wrapped_credential_once_and_never_persists_secrets() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let (created, material) = begin(&registry);
        let summaries = list_device_pairing_requests(&fixture.root).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].verification_code, material.verification_code);
        assert_eq!(
            summaries[0].transport_fingerprint,
            crate::transport_crypto::transport_fingerprint(
                &URL_SAFE_NO_PAD
                    .decode(&created.transport_public_key)
                    .unwrap()
            )
        );
        let file = fs::read_to_string(summary_path(
            &fixture.root.join(DIRECTORY),
            &created.request_id,
        ))
        .unwrap();
        for excluded in [
            "authToken",
            "pollProof",
            "cancelProof",
            "privateKey",
            "clientSecret",
            "devicePublicKey",
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
            .contains("authToken"));
        let expected_id = crate::devices::device_id_for(&device_key());
        // The credential repeats the delivered transport key, which is the
        // handshake's current ML-KEM static key.
        let handshake_key = fixture.keys.current().unwrap();
        assert_eq!(created.transport_protocol, PairingEncryption::MlKem768);
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(&created.transport_public_key)
                .unwrap(),
            handshake_key.static_public(crate::transport_crypto::EncryptionProtocol::MlKem768)
        );
        assert_eq!(
            decrypt(&material, &response),
            json!({
                "deviceId": expected_id,
                "transportProtocol": "ml-kem-768",
                "transportPublicKey": created.transport_public_key,
            })
        );
        // Approval committed the device to the shared registry.
        let record = crate::devices::list_devices(&fixture.root)
            .unwrap()
            .into_iter()
            .find(|device| device.device_id == expected_id)
            .expect("approved device is registered");
        assert_eq!(record.name, "Synthetic device");
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
        // A low-order key commits fine but fails at reveal.
        let created = registry
            .create(
                peer(),
                create_request(Some(commitment(&[0; 32], &CLIENT_NONCE))),
            )
            .unwrap();
        assert!(registry
            .reveal(
                peer(),
                reveal_request(&created.request_id, [0; 32], CLIENT_NONCE)
            )
            .is_err());
        // An invalid Ed25519 key is rejected before rate limits are consumed.
        let mut bad_device = create_request(Some([1; 32]));
        bad_device.device_public_key = "not-a-key".into();
        assert!(registry.create(peer(), bad_device).is_err());
        for _ in 0..3 {
            begin(&registry);
        }
        assert!(matches!(
            registry.create(peer(), create_request(Some([1; 32]))),
            Err(AppError::ResourceExhausted(_))
        ));
        for index in 1..14 {
            registry
                .create(
                    format!("192.0.2.{index}").parse().unwrap(),
                    create_request(Some([1; 32])),
                )
                .unwrap();
        }
        assert_eq!(registry.inner.lock().unwrap().requests.len(), MAX_ACTIVE);
        assert!(matches!(
            registry.create("192.0.2.99".parse().unwrap(), create_request(Some([1; 32]))),
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
    fn plaintext_server_delivers_an_empty_key_bound_as_a_zero_length_field() {
        let fixture = Fixture::new();
        let registry = fixture.registry_with(true, PairingEncryption::None);
        let (created, material) = begin(&registry);
        assert_eq!(created.transport_protocol, PairingEncryption::None);
        assert_eq!(created.transport_public_key, "");
        assert!(material
            .transcript
            .ends_with(&[0, 0, 0, 4, b'n', b'o', b'n', b'e', 0, 0, 0, 0]));
        decide_device_pairing(&fixture.root, &created.request_id, true).unwrap();
        let response = registry
            .poll(peer(), proof(&created.request_id, &material.poll_proof))
            .unwrap();
        assert_eq!(
            decrypt(&material, &response)["transportProtocol"],
            json!("none")
        );
        assert_eq!(
            decrypt(&material, &response)["transportPublicKey"],
            json!("")
        );
    }

    #[test]
    fn a_key_reset_switches_new_pairings_and_voids_pending_credentials() {
        let fixture = Fixture::new();
        let registry = fixture.registry_with(true, PairingEncryption::X25519);
        let (before, material) = begin(&registry);
        decide_device_pairing(&fixture.root, &before.request_id, true).unwrap();
        let reset = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(crate::transport_crypto::PairingKeys::reset(&fixture.root))
            .unwrap();
        let reset_key =
            URL_SAFE_NO_PAD.encode(reset.transport_public_key(PairingEncryption::X25519));
        assert_ne!(before.transport_public_key, reset_key);
        // An approved request created before the reset would pin the old
        // key: it is dropped without registering the device.
        assert_eq!(
            registry
                .poll(peer(), proof(&before.request_id, &material.poll_proof))
                .unwrap()
                .status,
            "expired"
        );
        assert!(crate::devices::list_devices(&fixture.root)
            .unwrap()
            .is_empty());
        let (after, _) = begin(&registry);
        assert_eq!(after.transport_public_key, reset_key);
    }

    #[test]
    fn auth_disabled_server_does_not_offer_pairing() {
        let fixture = Fixture::new();
        let registry = fixture.registry_with(false, PairingEncryption::MlKem768);
        assert!(matches!(
            registry.create(
                peer(),
                CreateDevicePairingRequest {
                    client_commitment: None,
                    client_public_key: None,
                    transport_binding: None,
                    device_name: String::new(),
                    device_public_key: String::new(),
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
