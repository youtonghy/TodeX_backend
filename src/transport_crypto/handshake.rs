//! Transport v2 key agreement and key schedule (`docs/transport-v2.md`).
//!
//! ```text
//! th     = SHA256(LP(label) || LP(protocol) || LP(device_id) ||
//!                 LP(server_static_public) || LP(client_material) ||
//!                 LP(client_nonce) || LP(server_nonce))
//! prk    = HKDF-Extract(salt = th, ikm = shared)
//! k_up   = HKDF-Expand(prk, label || "/up",   32)
//! k_down = HKDF-Expand(prk, label || "/down", 32)
//! ```
use hkdf::Hkdf;
use pqcrypto_mlkem::mlkem768;
use pqcrypto_traits::kem::{Ciphertext as MlKemCiphertext, SharedSecret as MlKemSharedSecret};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x25519_dalek::PublicKey as X25519PublicKey;
use zeroize::Zeroizing;

use super::{EncryptionProtocol, PairingKeys, TransportCryptoError};

pub(crate) const WS_LABEL: &str = "todex.transport.v2/ws";
pub(crate) const REST_LABEL: &str = "todex.transport.v2/rest";
/// Client and server nonces are 32 random bytes (the REST server nonce is
/// empty).
pub(crate) const NONCE_LENGTH: usize = 32;
const KEY_LENGTH: usize = 32;
const ML_KEM_768_CIPHERTEXT_LENGTH: usize = 1088;

/// Inputs to the key schedule, shared by WebSocket and REST.
pub(crate) struct KeyScheduleInput<'a> {
    pub label: &'a str,
    pub protocol: EncryptionProtocol,
    pub device_id: &'a str,
    pub server_static_public: &'a [u8],
    pub client_material: &'a [u8],
    pub client_nonce: &'a [u8],
    pub server_nonce: &'a [u8],
    pub shared: &'a [u8],
}

/// Session keys for one connection or request. Keys are wiped on drop.
pub(crate) struct TransportKeys {
    pub th: Zeroizing<[u8; 32]>,
    pub k_up: Zeroizing<[u8; KEY_LENGTH]>,
    pub k_down: Zeroizing<[u8; KEY_LENGTH]>,
}

fn update_lp(hasher: &mut Sha256, value: &[u8]) {
    // Every transcript field is far below 4 GiB (keys, nonces, ids).
    hasher.update((value.len() as u32).to_be_bytes());
    hasher.update(value);
}

pub(crate) fn transcript_hash(input: &KeyScheduleInput<'_>) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    update_lp(&mut hasher, input.label.as_bytes());
    update_lp(&mut hasher, input.protocol.as_str().as_bytes());
    update_lp(&mut hasher, input.device_id.as_bytes());
    update_lp(&mut hasher, input.server_static_public);
    update_lp(&mut hasher, input.client_material);
    update_lp(&mut hasher, input.client_nonce);
    update_lp(&mut hasher, input.server_nonce);
    Zeroizing::new(hasher.finalize().into())
}

pub(crate) fn derive_keys(
    input: &KeyScheduleInput<'_>,
) -> Result<TransportKeys, TransportCryptoError> {
    let th = transcript_hash(input);
    let hkdf = Hkdf::<Sha256>::new(Some(th.as_slice()), input.shared);
    let expand = |suffix: &str| -> Result<Zeroizing<[u8; KEY_LENGTH]>, TransportCryptoError> {
        let mut key = Zeroizing::new([0_u8; KEY_LENGTH]);
        let info = [input.label.as_bytes(), suffix.as_bytes()].concat();
        hkdf.expand(&info, key.as_mut_slice())
            .map_err(|_| TransportCryptoError::new("key derivation"))?;
        Ok(key)
    };
    Ok(TransportKeys {
        k_up: expand("/up")?,
        k_down: expand("/down")?,
        th,
    })
}

impl PairingKeys {
    /// `server_static_public`: the key clients pin at pairing.
    pub(crate) fn static_public(&self, protocol: EncryptionProtocol) -> &[u8] {
        match protocol {
            EncryptionProtocol::X25519 => &self.x25519_public,
            EncryptionProtocol::MlKem768 => &self.ml_kem_public,
        }
    }

    /// Server side of the key agreement: X25519 with the client's ephemeral
    /// public key, or ML-KEM-768 decapsulation of the client's ciphertext.
    pub(crate) fn agree(
        &self,
        protocol: EncryptionProtocol,
        client_material: &[u8],
    ) -> Result<Zeroizing<[u8; 32]>, TransportCryptoError> {
        match protocol {
            EncryptionProtocol::X25519 => {
                let client_public: [u8; 32] = client_material
                    .try_into()
                    .map_err(|_| TransportCryptoError::new("x25519 client key length"))?;
                let shared = self
                    .x25519_secret
                    .diffie_hellman(&X25519PublicKey::from(client_public));
                if bool::from(shared.as_bytes().ct_eq(&[0_u8; 32])) {
                    return Err(TransportCryptoError::new("x25519 shared secret is zero"));
                }
                Ok(Zeroizing::new(*shared.as_bytes()))
            }
            EncryptionProtocol::MlKem768 => {
                if client_material.len() != ML_KEM_768_CIPHERTEXT_LENGTH {
                    return Err(TransportCryptoError::new("ml-kem-768 ciphertext length"));
                }
                let ciphertext = mlkem768::Ciphertext::from_bytes(client_material)
                    .map_err(|_| TransportCryptoError::new("ml-kem-768 ciphertext"))?;
                let shared = mlkem768::decapsulate(&ciphertext, &self.ml_kem_secret);
                let shared: [u8; 32] = shared
                    .as_bytes()
                    .try_into()
                    .map_err(|_| TransportCryptoError::new("ml-kem-768 shared secret length"))?;
                Ok(Zeroizing::new(shared))
            }
        }
    }

    /// Agreement plus key schedule for one server-side session.
    pub(crate) fn server_session_keys(
        &self,
        label: &str,
        protocol: EncryptionProtocol,
        device_id: &str,
        client_material: &[u8],
        client_nonce: &[u8],
        server_nonce: &[u8],
    ) -> Result<TransportKeys, TransportCryptoError> {
        if client_nonce.len() != NONCE_LENGTH {
            return Err(TransportCryptoError::new("client nonce length"));
        }
        let shared = self.agree(protocol, client_material)?;
        derive_keys(&KeyScheduleInput {
            label,
            protocol,
            device_id,
            server_static_public: self.static_public(protocol),
            client_material,
            client_nonce,
            server_nonce,
            shared: shared.as_slice(),
        })
    }
}

/// Strict base64url (no padding) used by every v2 header and query value.
pub(crate) fn decode_b64url(value: &str) -> Result<Vec<u8>, TransportCryptoError> {
    base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        value.as_bytes(),
    )
    .map_err(|_| TransportCryptoError::new("invalid base64url"))
}
