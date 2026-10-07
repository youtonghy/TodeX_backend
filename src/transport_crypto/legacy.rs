//! Transport v1 (`todex.crypto.v1`): JSON-wrapped encrypted WebSocket text
//! frames with a key derived from the client material alone. Superseded by
//! transport v2 and accepted only while clients migrate.
use std::collections::{HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use pqcrypto_mlkem::mlkem768;
use pqcrypto_traits::kem::{Ciphertext as MlKemCiphertext, SharedSecret as MlKemSharedSecret};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519PublicKey;
use zeroize::Zeroizing;

use super::{
    decode_b64, decode_fixed_32, encode_b64, query_value, EncryptionProtocol, PairingKeys,
};
use crate::config::PairingEncryption;
use crate::error::AppError;

const WRAPPER_TYPE: &str = "todex.crypto.v1";
const AAD: &[u8] = b"todex-ws-transport-crypto-v1";
/// v1 keys depend only on the client material, so a replayed upgrade would
/// reproduce the key and nonces. The signed upgrade credential is valid for
/// ±`MAX_CLOCK_SKEW_SECS` around its timestamp (and its nonce is single-use
/// within that window), so remembering handshakes for the whole span covers
/// every replay the device signature would still accept.
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(2 * crate::device_auth::MAX_CLOCK_SKEW_SECS);
const MAX_USED_HANDSHAKES: usize = 65_536;
static USED_HANDSHAKES: OnceLock<Mutex<HandshakeRegistry>> = OnceLock::new();

/// Handshakes seen within [`HANDSHAKE_WINDOW`], oldest first.
#[derive(Default)]
struct HandshakeRegistry {
    ids: HashSet<[u8; 32]>,
    order: VecDeque<(Instant, [u8; 32])>,
}

impl HandshakeRegistry {
    fn claim(&mut self, id: [u8; 32], now: Instant) -> Result<(), AppError> {
        while let Some((seen, old)) = self.order.front().copied() {
            if now.saturating_duration_since(seen) < HANDSHAKE_WINDOW {
                break;
            }
            self.order.pop_front();
            self.ids.remove(&old);
        }
        if self.ids.contains(&id) {
            return Err(AppError::InvalidRequest(
                "transport handshake material was already used; generate fresh client key material"
                    .to_owned(),
            ));
        }
        if self.ids.len() >= MAX_USED_HANDSHAKES {
            // Temporary: entries age out of the window.
            return Err(AppError::ResourceExhausted(
                "transport handshake registry is full; retry later".to_owned(),
            ));
        }
        self.ids.insert(id);
        self.order.push_back((now, id));
        Ok(())
    }
}

#[derive(Clone)]
pub struct TransportCryptoSession {
    protocol: EncryptionProtocol,
    cipher: Arc<XChaCha20Poly1305>,
    send_counter: Arc<AtomicU64>,
    receive_counter: Arc<AtomicU64>,
}

impl TransportCryptoSession {
    pub fn from_headers_and_query(
        keys: &PairingKeys,
        required: PairingEncryption,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<Option<Self>, AppError> {
        // A present query parameter is authoritative, including invalid/empty
        // values. Never fall back to a header or plaintext on malformed input.
        let enc = match query_value(query, "enc") {
            Some(value) => Some(value),
            None => headers
                .get("x-todex-encryption")
                .map(|value| {
                    value.to_str().map(str::to_owned).map_err(|_| {
                        AppError::InvalidRequest("invalid encryption protocol header".to_owned())
                    })
                })
                .transpose()?,
        };
        let protocol = match enc.as_deref() {
            None | Some("none") => None,
            Some(value) => Some(EncryptionProtocol::parse(value).ok_or_else(|| {
                AppError::InvalidRequest(format!("unsupported encryption protocol: {value}"))
            })?),
        };
        if required != PairingEncryption::None
            && protocol.map(EncryptionProtocol::as_str) != Some(required.as_str())
        {
            return Err(AppError::Unauthorized(format!(
                "Encryption public-key transfer verification is incomplete; server requires {} transport encryption",
                required.as_str()
            )));
        }
        let Some(protocol) = protocol else {
            return Ok(None);
        };

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
                if shared.as_bytes().iter().all(|byte| *byte == 0) {
                    return Err(AppError::InvalidRequest(
                        "x25519 client public key produced an invalid shared secret".to_owned(),
                    ));
                }
                let salt = [keys.x25519_public.as_slice(), client_key.as_slice()].concat();
                (Zeroizing::new(shared.as_bytes().to_vec()), salt)
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
                (Zeroizing::new(shared.as_bytes().to_vec()), salt)
            }
        };

        let handshake_id: [u8; 32] =
            Sha256::digest([protocol.as_str().as_bytes(), salt.as_slice()].concat()).into();
        USED_HANDSHAKES
            .get_or_init(|| Mutex::new(HandshakeRegistry::default()))
            .lock()
            .map_err(|_| {
                AppError::InvalidRequest("transport handshake registry is unavailable".to_owned())
            })?
            .claim(handshake_id, Instant::now())?;

        let mut key = Zeroizing::new([0_u8; 32]);
        let hk = Hkdf::<Sha256>::new(Some(&salt), &shared);
        hk.expand(protocol.as_str().as_bytes(), key.as_mut_slice())
            .map_err(|_| AppError::InvalidRequest("failed to derive transport key".to_owned()))?;
        // The AEAD wipes its copy of the key on drop.
        let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_slice()));

        Ok(Some(Self {
            protocol,
            cipher: Arc::new(cipher),
            send_counter: Arc::new(AtomicU64::new(0)),
            receive_counter: Arc::new(AtomicU64::new(0)),
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
        let counter = self
            .send_counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |counter| {
                counter.checked_add(1)
            })
            .map_err(|_| {
                AppError::InvalidRequest(
                    "encrypted websocket frame counter is exhausted".to_owned(),
                )
            })?;
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
        if nonce[1..8].iter().any(|byte| *byte != 0) || nonce[16..].iter().any(|byte| *byte != 0) {
            return Err(AppError::InvalidRequest(
                "encrypted websocket frame nonce is malformed".to_owned(),
            ));
        }
        let counter = u64::from_le_bytes(
            nonce[8..16]
                .try_into()
                .expect("validated nonce counter slice has eight bytes"),
        );
        let expected_counter = self.receive_counter.load(Ordering::Acquire);
        let next_counter = expected_counter.checked_add(1).ok_or_else(|| {
            AppError::InvalidRequest("encrypted websocket frame counter is exhausted".to_owned())
        })?;
        if counter != expected_counter {
            return Err(AppError::InvalidRequest(
                "encrypted websocket frame was replayed or arrived out of order".to_owned(),
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
        self.receive_counter
            .compare_exchange(
                expected_counter,
                next_counter,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                AppError::InvalidRequest(
                    "encrypted websocket frame was replayed or arrived out of order".to_owned(),
                )
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

fn nonce_for(direction: u8, counter: u64) -> [u8; 24] {
    let mut nonce = [0_u8; 24];
    nonce[0] = direction;
    nonce[8..16].copy_from_slice(&counter.to_le_bytes());
    nonce
}

fn decode_fixed_24(value: &str, label: &str) -> Result<[u8; 24], AppError> {
    let bytes = decode_b64(value, label)?;
    bytes
        .try_into()
        .map_err(|_| AppError::InvalidRequest(format!("{label} must be 24 bytes")))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{test_config, unique_tmp_dir};
    use super::*;
    use pqcrypto_traits::kem::PublicKey as MlKemPublicKey;
    use rand_core::OsRng;
    use x25519_dalek::StaticSecret as X25519Secret;

    #[test]
    fn handshake_registry_forgets_entries_after_the_window_and_never_fills_permanently() {
        let mut registry = HandshakeRegistry::default();
        let start = Instant::now();
        registry.claim([1; 32], start).unwrap();
        assert!(registry
            .claim([1; 32], start + Duration::from_secs(1))
            .is_err());
        // After the credential window the same id is no longer tracked.
        registry.claim([1; 32], start + HANDSHAKE_WINDOW).unwrap();

        let mut registry = HandshakeRegistry::default();
        for index in 0..MAX_USED_HANDSHAKES {
            let mut id = [0; 32];
            id[..8].copy_from_slice(&(index as u64).to_be_bytes());
            registry.claim(id, start).unwrap();
        }
        assert!(matches!(
            registry.claim([0xff; 32], start),
            Err(AppError::ResourceExhausted(_))
        ));
        registry
            .claim([0xff; 32], start + HANDSHAKE_WINDOW)
            .unwrap();
        assert_eq!(registry.ids.len(), 1);
        assert_eq!(registry.order.len(), 1);
    }

    #[test]
    fn encrypted_frame_round_trips() {
        let keys = PairingKeys::generate();
        let client = X25519Secret::random_from_rng(OsRng);
        let client_public = X25519PublicKey::from(&client).to_bytes();
        let query = format!("enc=x25519&client_key={}", encode_b64(&client_public));
        let session = TransportCryptoSession::from_headers_and_query(
            &keys,
            PairingEncryption::None,
            &HeaderMap::new(),
            Some(&query),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            TransportCryptoSession::from_headers_and_query(
                &keys,
                PairingEncryption::None,
                &HeaderMap::new(),
                Some(&query)
            ),
            Err(AppError::InvalidRequest(message)) if message.contains("already used")
        ));

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
            receive_counter: Arc::new(AtomicU64::new(0)),
        };

        let wrapped = client_session
            .encrypt_client_text_for_tests(r#"{"type":"ping"}"#)
            .unwrap();
        assert_eq!(
            session.decrypt_client_text(&wrapped).unwrap(),
            r#"{"type":"ping"}"#
        );
        assert!(matches!(
            session.decrypt_client_text(&wrapped),
            Err(AppError::InvalidRequest(message)) if message.contains("replayed")
        ));

        let wrapped = session.encrypt_server_text(r#"{"type":"pong"}"#).unwrap();
        assert_eq!(
            client_session
                .decrypt_server_text_for_tests(&wrapped)
                .unwrap(),
            r#"{"type":"pong"}"#
        );

        session.send_counter.store(u64::MAX, Ordering::Release);
        assert!(matches!(
            session.encrypt_server_text("exhausted"),
            Err(AppError::InvalidRequest(message)) if message.contains("counter is exhausted")
        ));
        let client_wrapped = client_session
            .encrypt_client_text_for_tests("exhausted")
            .unwrap();
        session.receive_counter.store(u64::MAX, Ordering::Release);
        assert!(matches!(
            session.decrypt_client_text(&client_wrapped),
            Err(AppError::InvalidRequest(message)) if message.contains("counter is exhausted")
        ));
    }

    #[test]
    fn x25519_rejects_low_order_client_public_key() {
        let keys = PairingKeys::generate();
        let query = format!("enc=x25519&client_key={}", encode_b64(&[0_u8; 32]));
        assert!(matches!(
            TransportCryptoSession::from_headers_and_query(
                &keys,
                PairingEncryption::None,
                &HeaderMap::new(),
                Some(&query)
            ),
            Err(AppError::InvalidRequest(message)) if message.contains("invalid shared secret")
        ));
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
        let session = TransportCryptoSession::from_headers_and_query(
            &keys,
            PairingEncryption::None,
            &HeaderMap::new(),
            Some(&query),
        )
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
            receive_counter: Arc::new(AtomicU64::new(0)),
        };

        let wrapped = client_session
            .encrypt_client_text_for_tests(r#"{"type":"ping"}"#)
            .unwrap();
        assert_eq!(
            session.decrypt_client_text(&wrapped).unwrap(),
            r#"{"type":"ping"}"#
        );
    }

    #[test]
    fn transport_policy_never_downgrades_malformed_or_overridden_protocols() {
        let keys = PairingKeys::generate();
        let mut headers = HeaderMap::new();
        headers.insert("x-todex-encryption", "x25519".parse().unwrap());
        for query in ["enc=", "enc", "enc=invalid", "enc=none", "enc=ml-kem-768"] {
            assert!(
                TransportCryptoSession::from_headers_and_query(
                    &keys,
                    PairingEncryption::X25519,
                    &headers,
                    Some(query)
                )
                .is_err(),
                "{query}"
            );
        }
        for query in ["enc=", "enc", "enc=invalid"] {
            assert!(TransportCryptoSession::from_headers_and_query(
                &keys,
                PairingEncryption::None,
                &headers,
                Some(query)
            )
            .is_err());
        }
        headers.insert(
            "x-todex-encryption",
            axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(TransportCryptoSession::from_headers_and_query(
            &keys,
            PairingEncryption::None,
            &headers,
            None
        )
        .is_err());
        assert!(TransportCryptoSession::from_headers_and_query(
            &keys,
            PairingEncryption::None,
            &HeaderMap::new(),
            Some("enc=none")
        )
        .unwrap()
        .is_none());
        // Explicit query choice wins over a conflicting header.
        assert!(TransportCryptoSession::from_headers_and_query(
            &keys,
            PairingEncryption::None,
            &headers,
            Some("enc=none")
        )
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn transport_policy_real_websocket_enforces_both_protocols_and_preserves_bootstrap() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        for required in [
            PairingEncryption::None,
            PairingEncryption::X25519,
            PairingEncryption::MlKem768,
        ] {
            let root = unique_tmp_dir("todex-transport-policy");
            let mut config = test_config();
            config.data_dir = root.join("data");
            config.workspace_roots = vec![root.join("workspace")];
            config.pairing_encryption = required;
            let state = crate::app_state::AppState::new(config).await.unwrap();
            let keys = state.pairing_keys.clone();
            let app = crate::server::router(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap();
            });
            let http = reqwest::Client::new();
            let policy = http
                .get(format!("http://{address}/v2/transport-policy"))
                .send()
                .await
                .unwrap();
            assert_eq!(policy.status(), 200);
            assert_eq!(policy.headers()["cache-control"], "no-store");
            assert_eq!(
                policy.json::<serde_json::Value>().await.unwrap(),
                json!({"requiredProtocol": required.as_str(), "transportVersion": 2})
            );
            let device = crate::device_auth::test_support::TestDevice::new(11);
            device.enroll(&root.join("data"));
            let client = X25519Secret::random_from_rng(OsRng);
            let bootstrap = http
                .post(format!("http://{address}/v2/device-pairing/create"))
                .json(&json!({
                    "deviceName": "policy test",
                    "clientCommitment": encode_b64(&[1; 32]),
                    "devicePublicKey": device.public_key_b64(),
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(bootstrap.status(), 200);
            // The device signature covers the query, so `enc` handshake
            // material is bound to the device identity.
            let signed_url = |extra: &str| {
                let (path, prefix) = if extra.is_empty() {
                    ("/v2/ws".to_owned(), String::new())
                } else {
                    (format!("/v2/ws?{extra}"), format!("{extra}&"))
                };
                format!("ws://{address}/v2/ws?{prefix}{}", device.sign_query(&path))
            };
            let plaintext = tokio_tungstenite::connect_async(signed_url("")).await;
            if required == PairingEncryption::None {
                plaintext.unwrap().0.close(None).await.unwrap();
            } else {
                assert!(
                    matches!(plaintext, Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == 403)
                );
                let wrong = if required == PairingEncryption::X25519 {
                    "ml-kem-768"
                } else {
                    "x25519"
                };
                for enc in ["none", "", "invalid", wrong] {
                    assert!(
                        tokio_tungstenite::connect_async(signed_url(&format!("enc={enc}")))
                            .await
                            .is_err()
                    );
                }
            }
            // Query protocol is authoritative over a conflicting header.
            let protocol = if required == PairingEncryption::MlKem768 {
                EncryptionProtocol::MlKem768
            } else {
                EncryptionProtocol::X25519
            };
            let (query, shared, salt) = match protocol {
                EncryptionProtocol::X25519 => {
                    let public = X25519PublicKey::from(&client).to_bytes();
                    let shared = client.diffie_hellman(&X25519PublicKey::from(keys.x25519_public));
                    (
                        format!("enc=x25519&client_key={}", encode_b64(&public)),
                        shared.as_bytes().to_vec(),
                        [keys.x25519_public.as_slice(), public.as_slice()].concat(),
                    )
                }
                EncryptionProtocol::MlKem768 => {
                    let public = mlkem768::PublicKey::from_bytes(&keys.ml_kem_public).unwrap();
                    let (shared, ciphertext) = mlkem768::encapsulate(&public);
                    (
                        format!(
                            "enc=ml-kem-768&ciphertext={}",
                            encode_b64(ciphertext.as_bytes())
                        ),
                        shared.as_bytes().to_vec(),
                        [keys.ml_kem_public.as_slice(), ciphertext.as_bytes()].concat(),
                    )
                }
            };
            let mut key = [0; 32];
            Hkdf::<Sha256>::new(Some(&salt), &shared)
                .expand(protocol.as_str().as_bytes(), &mut key)
                .unwrap();
            let client_crypto = TransportCryptoSession {
                protocol,
                cipher: Arc::new(XChaCha20Poly1305::new(Key::from_slice(&key))),
                send_counter: Arc::new(AtomicU64::new(0)),
                receive_counter: Arc::new(AtomicU64::new(0)),
            };
            let mut request = signed_url(&query).into_client_request().unwrap();
            request
                .headers_mut()
                .insert("x-todex-encryption", "none".parse().unwrap());
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            socket
                .send(Message::Text(
                    client_crypto
                        .encrypt_client_text_for_tests(
                            &json!({"id":"policy-ping", "type":"server.ping", "payload":{}})
                                .to_string(),
                        )
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                        continue;
                    };
                    let plain = client_crypto.decrypt_server_text_for_tests(&text).unwrap();
                    let message: serde_json::Value = serde_json::from_str(&plain).unwrap();
                    if message["id"] == "policy-ping" {
                        assert_eq!(message["payload"]["pong"], true);
                        break;
                    }
                }
            })
            .await
            .unwrap();
            // A frame that does not decrypt closes the connection.
            socket
                .send(Message::Text(
                    client_crypto
                        .encrypt_client_text_for_tests("{}")
                        .unwrap()
                        .replace("\"nonce\":\"", "\"nonce\":\"A")
                        .into(),
                ))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match socket.next().await {
                        Some(Ok(Message::Close(Some(frame)))) => {
                            assert_eq!(u16::from(frame.code), 4400);
                            break;
                        }
                        Some(Ok(_)) => continue,
                        other => panic!("expected a close frame, got {other:?}"),
                    }
                }
            })
            .await
            .unwrap();
            server.abort();
            let _ = server.await;
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
