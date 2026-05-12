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
    Ciphertext as MlKemCiphertext, PublicKey as MlKemPublicKey, SharedSecret as MlKemSharedSecret,
};
use qrcodegen::{QrCode, QrCodeEcc};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use crate::config::{Config, PairingEncryption};
use crate::error::AppError;

const PAIRING_VERSION: u8 = 1;
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

    pub fn pairing_payload(&self, config: &Config, port: u16) -> PairingPayload {
        self.pairing_payload_with_preference(config, port, config.pairing_encryption)
    }

    pub fn pairing_payload_with_preference(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> PairingPayload {
        let host = config.host.clone();
        let server_url = format!("http://{host}:{port}");
        PairingPayload {
            kind: "todex-pairing".to_owned(),
            version: PAIRING_VERSION,
            server_url,
            ws_url: format!("ws://{host}:{port}/v1/ws"),
            host,
            port,
            auth_token: config.security.auth_token.clone(),
            preferred_encryption: Some(preferred_encryption),
            protocols: vec![
                PairingProtocol {
                    id: EncryptionProtocol::X25519.as_str().to_owned(),
                    public_key: encode_b64(&self.x25519_public),
                },
                PairingProtocol {
                    id: EncryptionProtocol::MlKem768.as_str().to_owned(),
                    public_key: encode_b64(&self.ml_kem_public),
                },
            ],
        }
    }

    pub fn pairing_qr_text(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        render_qr_text(&self.pairing_link_json(config, port, preferred_encryption)?)
    }

    fn pairing_link_json(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        Ok(serde_json::to_string(&PairingLinkPayload {
            kind: "todex-pairing-link".to_owned(),
            version: PAIRING_VERSION,
            server_url: format!("http://{}:{port}", config.host),
            pairing_url: format!("http://{}:{port}/v1/pairing", config.host),
            auth_token: config.security.auth_token.clone(),
            preferred_encryption: Some(preferred_encryption),
        })?)
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingPayload {
    kind: String,
    version: u8,
    server_url: String,
    ws_url: String,
    host: String,
    port: u16,
    auth_token: Option<String>,
    preferred_encryption: Option<PairingEncryption>,
    protocols: Vec<PairingProtocol>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingProtocol {
    id: String,
    public_key: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingLinkPayload {
    kind: String,
    version: u8,
    server_url: String,
    pairing_url: String,
    auth_token: Option<String>,
    preferred_encryption: Option<PairingEncryption>,
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

fn render_qr_text(payload: &str) -> Result<String, AppError> {
    let qr = QrCode::encode_text(payload, QrCodeEcc::Low)
        .map_err(|_| AppError::InvalidRequest("pairing payload is too large for QR".to_owned()))?;
    let border = 2;
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
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use std::path::PathBuf;

    #[test]
    fn pairing_payload_contains_both_protocols() {
        let keys = PairingKeys::generate();
        let payload = keys.pairing_payload(&test_config(), 7345);
        let protocols = payload
            .protocols
            .iter()
            .map(|protocol| protocol.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(protocols, vec!["x25519", "ml-kem-768"]);
        assert_eq!(payload.server_url, "http://127.0.0.1:7345");
        assert_eq!(payload.auth_token.as_deref(), Some("token"));
    }

    #[test]
    fn pairing_payload_serializes_preferred_encryption_and_token() {
        let keys = PairingKeys::generate();
        let payload =
            keys.pairing_payload_with_preference(&test_config(), 7345, PairingEncryption::X25519);
        let json = serde_json::to_value(payload).unwrap();

        assert_eq!(json["preferredEncryption"], "x25519");
        assert_eq!(json["authToken"], "token");
        assert_eq!(json["serverUrl"], "http://127.0.0.1:7345");
    }

    #[test]
    fn pairing_qr_uses_compact_authenticated_link() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_eq!(value["kind"], "todex-pairing-link");
        assert_eq!(value["pairingUrl"], "http://127.0.0.1:7345/v1/pairing");
        assert_eq!(value["authToken"], "token");
        assert_eq!(value["preferredEncryption"], "ml-kem-768");
        assert!(value.get("protocols").is_none());

        let qr = keys
            .pairing_qr_text(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();
        let max_width = qr.lines().map(|line| line.chars().count()).max().unwrap();
        assert!(
            max_width <= 80,
            "compact pairing QR should fit common terminal widths, got {max_width}"
        );
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
}
