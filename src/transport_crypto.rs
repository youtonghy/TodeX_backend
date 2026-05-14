use std::net::{IpAddr, UdpSocket};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use pqcrypto_mlkem::mlkem768;
use pqcrypto_traits::kem::{
    Ciphertext as MlKemCiphertext, PublicKey as MlKemPublicKey, SecretKey as MlKemSecretKey,
    SharedSecret as MlKemSharedSecret,
};
use qrcodegen::{QrCode, QrCodeEcc};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use crate::config::{Config, PairingEncryption};
use crate::error::AppError;

const PAIRING_VERSION: u8 = 1;
const PAIRING_QR_SEGMENT_DATA_LENGTH: usize = 160;
const PAIRING_KEYS_FILE: &str = "pairing_keys.json";
const WRAPPER_TYPE: &str = "todex.crypto.v1";
const AAD: &[u8] = b"todex-ws-transport-crypto-v1";

#[derive(Clone)]
pub struct PairingKeys {
    x25519_secret: Arc<X25519Secret>,
    x25519_public: [u8; 32],
    ml_kem_secret: Arc<mlkem768::SecretKey>,
    ml_kem_public: Vec<u8>,
}

impl PairingKeys {
    pub fn generate() -> Self {
        let x25519_secret = X25519Secret::random_from_rng(OsRng);
        let x25519_public = X25519PublicKey::from(&x25519_secret).to_bytes();
        let (ml_kem_public, ml_kem_secret) = mlkem768::keypair();

        Self {
            x25519_secret: Arc::new(x25519_secret),
            x25519_public,
            ml_kem_secret: Arc::new(ml_kem_secret),
            ml_kem_public: ml_kem_public.as_bytes().to_vec(),
        }
    }

    pub async fn load_or_generate(data_dir: &Path) -> Result<Self, AppError> {
        let path = data_dir.join(PAIRING_KEYS_FILE);
        match tokio::fs::read_to_string(&path).await {
            Ok(raw) => Self::from_persisted_json(&raw, "pairing keys file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let keys = Self::generate();
                keys.persist(&path).await?;
                Ok(keys)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn persist(&self, path: &Path) -> Result<(), AppError> {
        let persisted = PersistedPairingKeys {
            version: PAIRING_VERSION,
            x25519_secret: encode_b64(self.x25519_secret.to_bytes().as_slice()),
            ml_kem_public: encode_b64(&self.ml_kem_public),
            ml_kem_secret: encode_b64(self.ml_kem_secret.as_bytes()),
        };
        let json = serde_json::to_string_pretty(&persisted)?;
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        set_owner_only_permissions(&tmp_path).await?;
        tokio::fs::rename(&tmp_path, path).await?;
        Ok(())
    }

    fn from_persisted_json(raw: &str, label: &str) -> Result<Self, AppError> {
        let persisted: PersistedPairingKeys = serde_json::from_str(raw)
            .map_err(|error| AppError::InvalidRequest(format!("invalid {label}: {error}")))?;
        if persisted.version != PAIRING_VERSION {
            return Err(AppError::InvalidRequest(format!(
                "{label} version {} is not supported",
                persisted.version
            )));
        }
        let x25519_secret = X25519Secret::from(decode_fixed_32(
            &persisted.x25519_secret,
            "x25519 pairing secret",
        )?);
        let x25519_public = X25519PublicKey::from(&x25519_secret).to_bytes();
        let ml_kem_secret_bytes =
            decode_b64(&persisted.ml_kem_secret, "ml-kem-768 pairing secret")?;
        let ml_kem_secret =
            mlkem768::SecretKey::from_bytes(&ml_kem_secret_bytes).map_err(|_| {
                AppError::InvalidRequest("ml-kem-768 pairing secret is invalid".to_owned())
            })?;
        let ml_kem_public = decode_b64(&persisted.ml_kem_public, "ml-kem-768 pairing public key")?;
        mlkem768::PublicKey::from_bytes(&ml_kem_public).map_err(|_| {
            AppError::InvalidRequest("ml-kem-768 pairing public key is invalid".to_owned())
        })?;

        Ok(Self {
            x25519_secret: Arc::new(x25519_secret),
            x25519_public,
            ml_kem_secret: Arc::new(ml_kem_secret),
            ml_kem_public,
        })
    }

    #[cfg(test)]
    fn pairing_qr_text(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        render_qr_text(&self.pairing_qr_payload(config, port, preferred_encryption)?)
    }

    #[allow(dead_code)]
    pub(crate) fn pairing_qr_payload(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        self.pairing_link_json(config, port, preferred_encryption)
    }

    pub(crate) fn pairing_qr_payloads(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<Vec<String>, AppError> {
        let payload = self.pairing_link_json(config, port, preferred_encryption)?;
        if preferred_encryption != PairingEncryption::MlKem768 {
            return Ok(vec![payload]);
        }

        self.segment_pairing_qr_payload(&payload)
    }

    fn pairing_link_json(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        let advertise_host = pairing_advertise_host(&config.host);
        Ok(serde_json::to_string(&PairingLinkPayload {
            kind: "todex-pairing-link".to_owned(),
            version: PAIRING_VERSION,
            server_url: format!("http://{advertise_host}:{port}"),
            auth_token: config.security.auth_token.clone(),
            preferred_encryption: Some(preferred_encryption),
            protocol: self.pairing_protocol_for(preferred_encryption),
        })?)
    }

    fn pairing_protocol_for(&self, encryption: PairingEncryption) -> Option<PairingProtocol> {
        match encryption {
            PairingEncryption::None => None,
            PairingEncryption::X25519 => Some(PairingProtocol {
                id: EncryptionProtocol::X25519.as_str().to_owned(),
                public_key: encode_b64(&self.x25519_public),
            }),
            PairingEncryption::MlKem768 => Some(PairingProtocol {
                id: EncryptionProtocol::MlKem768.as_str().to_owned(),
                public_key: encode_b64(&self.ml_kem_public),
            }),
        }
    }

    fn segment_pairing_qr_payload(&self, payload: &str) -> Result<Vec<String>, AppError> {
        let encoded = encode_b64(payload.as_bytes());
        let checksum = encode_b64(&Sha256::digest(payload.as_bytes()));
        let chunks: Vec<&[u8]> = encoded
            .as_bytes()
            .chunks(PAIRING_QR_SEGMENT_DATA_LENGTH)
            .collect();
        let total = chunks.len() as u16;

        chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| {
                serde_json::to_string(&PairingQrChunkPayload {
                    kind: "todex-pairing-chunk".to_owned(),
                    version: PAIRING_VERSION,
                    checksum: checksum.clone(),
                    index: (index + 1) as u16,
                    total,
                    data: String::from_utf8(chunk.to_vec())
                        .expect("base64url chunk should always be valid UTF-8"),
                })
                .map_err(Into::into)
            })
            .collect()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedPairingKeys {
    version: u8,
    x25519_secret: String,
    ml_kem_public: String,
    ml_kem_secret: String,
}

fn pairing_advertise_host(config_host: &str) -> String {
    let host = config_host.trim();
    match host.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => {
            default_route_ipv4().unwrap_or_else(|| "127.0.0.1".to_owned())
        }
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(_) => host.to_owned(),
        Err(_) => host.to_owned(),
    }
}

fn default_route_ipv4() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip.to_string()),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncryptionProtocol {
    X25519,
    MlKem768,
}

impl EncryptionProtocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "x25519" => Some(Self::X25519),
            "ml-kem-768" | "mlkem768" => Some(Self::MlKem768),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::X25519 => "x25519",
            Self::MlKem768 => "ml-kem-768",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingProtocol {
    id: String,
    public_key: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingQrChunkPayload {
    kind: String,
    version: u8,
    checksum: String,
    index: u16,
    total: u16,
    data: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingLinkPayload {
    kind: String,
    version: u8,
    server_url: String,
    auth_token: Option<String>,
    preferred_encryption: Option<PairingEncryption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<PairingProtocol>,
}

#[derive(Clone)]
pub struct TransportCryptoSession {
    protocol: EncryptionProtocol,
    cipher: Arc<XChaCha20Poly1305>,
    send_counter: Arc<AtomicU64>,
}

impl TransportCryptoSession {
    pub fn from_headers_and_query(
        keys: &PairingKeys,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<Option<Self>, AppError> {
        let enc = query_value(query, "enc")
            .or_else(|| header_value(headers, "x-todex-encryption"))
            .filter(|value| !value.trim().is_empty());
        let Some(enc) = enc else {
            return Ok(None);
        };

        let protocol = EncryptionProtocol::parse(enc.as_str()).ok_or_else(|| {
            AppError::InvalidRequest(format!("unsupported encryption protocol: {enc}"))
        })?;

        let (shared, salt) = match protocol {
            EncryptionProtocol::X25519 => {
                let client_key = query_value(query, "client_key")
                    .or_else(|| header_value(headers, "x-todex-client-key"))
                    .ok_or_else(|| {
                        AppError::InvalidRequest("missing x25519 client public key".to_owned())
                    })?;
                let client_key = decode_fixed_32(&client_key, "x25519 client public key")?;
                let client_public = X25519PublicKey::from(client_key);
                let shared = keys.x25519_secret.diffie_hellman(&client_public);
                let salt = [keys.x25519_public.as_slice(), client_key.as_slice()].concat();
                (shared.as_bytes().to_vec(), salt)
            }
            EncryptionProtocol::MlKem768 => {
                let ciphertext = query_value(query, "ciphertext")
                    .or_else(|| header_value(headers, "x-todex-kem-ciphertext"))
                    .ok_or_else(|| {
                        AppError::InvalidRequest("missing ml-kem-768 ciphertext".to_owned())
                    })?;
                let ciphertext = decode_b64(&ciphertext, "ml-kem-768 ciphertext")?;
                let ciphertext = mlkem768::Ciphertext::from_bytes(&ciphertext).map_err(|_| {
                    AppError::InvalidRequest("invalid ml-kem-768 ciphertext".to_owned())
                })?;
                let shared = mlkem768::decapsulate(&ciphertext, &keys.ml_kem_secret);
                let salt = [keys.ml_kem_public.as_slice(), ciphertext.as_bytes()].concat();
                (shared.as_bytes().to_vec(), salt)
            }
        };

        let mut key = [0_u8; 32];
        let hk = Hkdf::<Sha256>::new(Some(&salt), &shared);
        hk.expand(protocol.as_str().as_bytes(), &mut key)
            .map_err(|_| AppError::InvalidRequest("failed to derive transport key".to_owned()))?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));

        Ok(Some(Self {
            protocol,
            cipher: Arc::new(cipher),
            send_counter: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub fn protocol(&self) -> EncryptionProtocol {
        self.protocol
    }

    pub fn encrypt_server_text(&self, plaintext: &str) -> Result<String, AppError> {
        self.encrypt_text(1, plaintext)
    }

    #[cfg(test)]
    pub fn encrypt_client_text_for_tests(&self, plaintext: &str) -> Result<String, AppError> {
        self.encrypt_text(2, plaintext)
    }

    pub fn decrypt_client_text(&self, text: &str) -> Result<String, AppError> {
        self.decrypt_text(2, text)
    }

    #[cfg(test)]
    pub fn decrypt_server_text_for_tests(&self, text: &str) -> Result<String, AppError> {
        self.decrypt_text(1, text)
    }

    fn encrypt_text(&self, direction: u8, plaintext: &str) -> Result<String, AppError> {
        let counter = self.send_counter.fetch_add(1, Ordering::Relaxed);
        let nonce = nonce_for(direction, counter);
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: AAD,
                },
            )
            .map_err(|_| {
                AppError::InvalidRequest("failed to encrypt websocket frame".to_owned())
            })?;
        Ok(json!({
            "type": WRAPPER_TYPE,
            "protocol": self.protocol.as_str(),
            "nonce": encode_b64(&nonce),
            "ciphertext": encode_b64(&ciphertext),
        })
        .to_string())
    }

    fn decrypt_text(&self, expected_direction: u8, text: &str) -> Result<String, AppError> {
        let wrapper: CryptoFrame = serde_json::from_str(text)
            .map_err(|err| AppError::InvalidRequest(format!("invalid encrypted frame: {err}")))?;
        if wrapper.frame_type != WRAPPER_TYPE {
            return Err(AppError::InvalidRequest(
                "encrypted websocket frame has an invalid type".to_owned(),
            ));
        }
        if wrapper.protocol != self.protocol.as_str() {
            return Err(AppError::InvalidRequest(
                "encrypted websocket frame protocol mismatch".to_owned(),
            ));
        }
        let nonce = decode_fixed_24(&wrapper.nonce, "encrypted frame nonce")?;
        if nonce[0] != expected_direction {
            return Err(AppError::InvalidRequest(
                "encrypted websocket frame direction mismatch".to_owned(),
            ));
        }
        let ciphertext = decode_b64(&wrapper.ciphertext, "encrypted frame ciphertext")?;
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: AAD,
                },
            )
            .map_err(|_| {
                AppError::InvalidRequest("failed to decrypt websocket frame".to_owned())
            })?;
        String::from_utf8(plaintext)
            .map_err(|_| AppError::InvalidRequest("encrypted frame was not UTF-8".to_owned()))
    }
}

#[derive(Debug, Deserialize)]
struct CryptoFrame {
    #[serde(rename = "type")]
    frame_type: String,
    protocol: String,
    nonce: String,
    ciphertext: String,
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (raw_key, raw_value) = pair.split_once('=')?;
        if raw_key == key {
            Some(percent_decode(raw_value))
        } else {
            None
        }
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'%' if idx + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2]))
                {
                    output.push(high * 16 + low);
                    idx += 3;
                    continue;
                }
            }
            b'+' => {
                output.push(b' ');
                idx += 1;
                continue;
            }
            byte => {
                output.push(byte);
                idx += 1;
                continue;
            }
        }
        output.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&output).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn nonce_for(direction: u8, counter: u64) -> [u8; 24] {
    let mut nonce = [0_u8; 24];
    nonce[0] = direction;
    nonce[8..16].copy_from_slice(&counter.to_le_bytes());
    nonce
}

fn encode_b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_b64(value: &str, label: &str) -> Result<Vec<u8>, AppError> {
    URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .map_err(|_| AppError::InvalidRequest(format!("invalid base64 for {label}")))
}

fn decode_fixed_32(value: &str, label: &str) -> Result<[u8; 32], AppError> {
    let bytes = decode_b64(value, label)?;
    bytes
        .try_into()
        .map_err(|_| AppError::InvalidRequest(format!("{label} must be 32 bytes")))
}

fn decode_fixed_24(value: &str, label: &str) -> Result<[u8; 24], AppError> {
    let bytes = decode_b64(value, label)?;
    bytes
        .try_into()
        .map_err(|_| AppError::InvalidRequest(format!("{label} must be 24 bytes")))
}

#[cfg(unix)]
async fn set_owner_only_permissions(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_owner_only_permissions(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QrRenderMode {
    HalfBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenderedQrText {
    pub(crate) text: String,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub(crate) fn render_qr_text_for_bounds(
    payload: &str,
    max_width: u16,
    max_height: u16,
) -> Result<RenderedQrText, AppError> {
    let qr = QrCode::encode_text(payload, QrCodeEcc::Low)
        .map_err(|_| AppError::InvalidRequest("pairing payload is too large for QR".to_owned()))?;
    let candidates = [
        (QrRenderMode::HalfBlock, 2),
        (QrRenderMode::HalfBlock, 1),
        (QrRenderMode::HalfBlock, 0),
    ];
    let mut best = None;
    for (mode, border) in candidates {
        let rendered = render_qr_with_mode(&qr, mode, border);
        if rendered.width <= max_width && rendered.height <= max_height {
            return Ok(rendered);
        }
        best = match best {
            Some(current) if qr_area(&current) <= qr_area(&rendered) => Some(current),
            _ => Some(rendered),
        };
    }

    Ok(best.expect("QR render candidates must not be empty"))
}

#[cfg(test)]
fn render_qr_text(payload: &str) -> Result<String, AppError> {
    let qr = QrCode::encode_text(payload, QrCodeEcc::Low)
        .map_err(|_| AppError::InvalidRequest("pairing payload is too large for QR".to_owned()))?;
    Ok(render_qr_with_mode(&qr, QrRenderMode::HalfBlock, 2).text)
}

fn render_qr_with_mode(qr: &QrCode, mode: QrRenderMode, border: i32) -> RenderedQrText {
    let text = match mode {
        QrRenderMode::HalfBlock => render_qr_half_block(qr, border),
    };
    let width = text
        .lines()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let height = text.lines().count() as u16;

    RenderedQrText {
        text,
        width,
        height,
    }
}

fn render_qr_half_block(qr: &QrCode, border: i32) -> String {
    let size = qr.size();
    let min = -border;
    let max = size + border;
    let mut lines = Vec::new();
    let mut y = min;
    while y < max {
        let mut line = String::new();
        for x in min..max {
            let top = qr.get_module(x, y);
            let bottom = y + 1 < max && qr.get_module(x, y + 1);
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
        y += 2;
    }
    lines.join("\n")
}

fn qr_area(rendered: &RenderedQrText) -> u32 {
    rendered.width as u32 * rendered.height as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use std::path::PathBuf;

    #[test]
    fn pairing_qr_embeds_selected_public_key() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_eq!(value["kind"], "todex-pairing-link");
        assert_eq!(value["authToken"], "token");
        assert_eq!(value["preferredEncryption"], "x25519");
        assert_eq!(value["protocol"]["id"], "x25519");
        assert!(value["protocol"]["publicKey"].as_str().unwrap().len() > 40);
        assert!(value.get("protocols").is_none());
        assert!(value.get("pairingUrl").is_none());

        let qr = keys
            .pairing_qr_text(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let max_width = qr.lines().map(|line| line.chars().count()).max().unwrap();
        assert!(
            max_width <= 80,
            "x25519 pairing QR should fit common terminal widths, got {max_width}"
        );
    }

    #[test]
    fn ml_kem_pairing_qr_is_split_into_segments() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let frames = keys
            .pairing_qr_payloads(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();

        assert!(frames.len() > 1, "ml-kem pairing QR should be segmented");

        let mut encoded = String::new();
        let mut checksum = String::new();

        for (index, frame) in frames.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(frame).unwrap();
            assert_eq!(value["kind"], "todex-pairing-chunk");
            assert_eq!(value["version"], PAIRING_VERSION);
            assert_eq!(value["index"], (index + 1) as u64);
            assert_eq!(value["total"], frames.len() as u64);
            if checksum.is_empty() {
                checksum = value["checksum"].as_str().unwrap().to_owned();
            } else {
                assert_eq!(value["checksum"], checksum);
            }
            encoded.push_str(value["data"].as_str().unwrap());
        }

        let decoded = decode_b64(&encoded, "pairing qr chunk payload").unwrap();
        assert_eq!(encode_b64(&Sha256::digest(&decoded)), checksum);
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["kind"], "todex-pairing-link");
        assert_eq!(value["preferredEncryption"], "ml-kem-768");
    }

    #[test]
    fn plaintext_pairing_qr_does_not_embed_a_public_key() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::None)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_eq!(value["preferredEncryption"], "none");
        assert!(value.get("protocol").is_none());
    }

    #[test]
    fn pairing_qr_renderer_uses_block_cells_instead_of_braille_dots() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let payload = keys
            .pairing_qr_payload(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let rendered = render_qr_text_for_bounds(&payload, 76, 20).unwrap();

        assert!(
            rendered.width <= 76,
            "pairing QR should fit terminal content width, got {}",
            rendered.width
        );
        assert!(
            rendered.height > 20,
            "block-cell pairing QR should report that this terminal height is too short"
        );
        assert!(
            !rendered
                .text
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "pairing QR must not use Braille dot cells"
        );
    }

    #[test]
    fn pairing_link_replaces_unspecified_bind_host_with_reachable_advertise_host() {
        let keys = PairingKeys::generate();
        let mut config = test_config();
        config.host = "0.0.0.0".to_owned();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_ne!(value["serverUrl"], "http://0.0.0.0:7345");
        assert!(value.get("pairingUrl").is_none());
    }

    #[test]
    fn encrypted_frame_round_trips() {
        let keys = PairingKeys::generate();
        let client = X25519Secret::random_from_rng(OsRng);
        let client_public = X25519PublicKey::from(&client).to_bytes();
        let query = format!("enc=x25519&client_key={}", encode_b64(&client_public));
        let session =
            TransportCryptoSession::from_headers_and_query(&keys, &HeaderMap::new(), Some(&query))
                .unwrap()
                .unwrap();

        let server_public = X25519PublicKey::from(keys.x25519_public);
        let shared = client.diffie_hellman(&server_public);
        let salt = [keys.x25519_public.as_slice(), client_public.as_slice()].concat();
        let mut key = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes())
            .expand(b"x25519", &mut key)
            .unwrap();
        let client_session = TransportCryptoSession {
            protocol: EncryptionProtocol::X25519,
            cipher: Arc::new(XChaCha20Poly1305::new(Key::from_slice(&key))),
            send_counter: Arc::new(AtomicU64::new(0)),
        };

        let wrapped = client_session
            .encrypt_client_text_for_tests(r#"{"type":"ping"}"#)
            .unwrap();
        assert_eq!(
            session.decrypt_client_text(&wrapped).unwrap(),
            r#"{"type":"ping"}"#
        );

        let wrapped = session.encrypt_server_text(r#"{"type":"pong"}"#).unwrap();
        assert_eq!(
            client_session
                .decrypt_server_text_for_tests(&wrapped)
                .unwrap(),
            r#"{"type":"pong"}"#
        );
    }

    #[tokio::test]
    async fn pairing_keys_persist_across_restarts() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-persist");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let config = test_config();
        let first = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        let first_x25519 = first
            .pairing_link_json(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let first_ml_kem = first
            .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();

        let second = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        assert_eq!(
            second
                .pairing_link_json(&config, 7345, PairingEncryption::X25519)
                .unwrap(),
            first_x25519
        );
        assert_eq!(
            second
                .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
                .unwrap(),
            first_ml_kem
        );

        let first_value: serde_json::Value = serde_json::from_str(&first_x25519).unwrap();
        let server_public = decode_fixed_32(
            first_value["protocol"]["publicKey"].as_str().unwrap(),
            "persisted x25519 public key",
        )
        .unwrap();
        let client = X25519Secret::random_from_rng(OsRng);
        let client_public = X25519PublicKey::from(&client).to_bytes();
        let query = format!("enc=x25519&client_key={}", encode_b64(&client_public));
        let session = TransportCryptoSession::from_headers_and_query(
            &second,
            &HeaderMap::new(),
            Some(&query),
        )
        .unwrap()
        .unwrap();
        let shared = client.diffie_hellman(&X25519PublicKey::from(server_public));
        let salt = [server_public.as_slice(), client_public.as_slice()].concat();
        let mut key = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes())
            .expand(b"x25519", &mut key)
            .unwrap();
        let client_session = TransportCryptoSession {
            protocol: EncryptionProtocol::X25519,
            cipher: Arc::new(XChaCha20Poly1305::new(Key::from_slice(&key))),
            send_counter: Arc::new(AtomicU64::new(0)),
        };

        let wrapped = session.encrypt_server_text(r#"{"type":"pong"}"#).unwrap();
        assert_eq!(
            client_session
                .decrypt_server_text_for_tests(&wrapped)
                .unwrap(),
            r#"{"type":"pong"}"#
        );

        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    #[test]
    fn ml_kem_768_encrypted_frame_round_trips() {
        let keys = PairingKeys::generate();
        let public_key = mlkem768::PublicKey::from_bytes(&keys.ml_kem_public).unwrap();
        let (client_shared, ciphertext) = mlkem768::encapsulate(&public_key);
        let query = format!(
            "enc=ml-kem-768&ciphertext={}",
            encode_b64(ciphertext.as_bytes())
        );
        let session =
            TransportCryptoSession::from_headers_and_query(&keys, &HeaderMap::new(), Some(&query))
                .unwrap()
                .unwrap();

        let salt = [keys.ml_kem_public.as_slice(), ciphertext.as_bytes()].concat();
        let mut key = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), client_shared.as_bytes())
            .expand(b"ml-kem-768", &mut key)
            .unwrap();
        let client_session = TransportCryptoSession {
            protocol: EncryptionProtocol::MlKem768,
            cipher: Arc::new(XChaCha20Poly1305::new(Key::from_slice(&key))),
            send_counter: Arc::new(AtomicU64::new(0)),
        };

        let wrapped = client_session
            .encrypt_client_text_for_tests(r#"{"type":"ping"}"#)
            .unwrap();
        assert_eq!(
            session.decrypt_client_text(&wrapped).unwrap(),
            r#"{"type":"ping"}"#
        );
    }

    fn test_config() -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 7345,
            pairing_encryption: PairingEncryption::default(),
            data_dir: PathBuf::from("/tmp/todex-test"),
            workspace_root: PathBuf::from("/tmp/todex-test/workspace"),
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        }
    }

    fn unique_tmp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }
}
