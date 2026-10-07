//! Transport v2 key agreement and key schedule (`docs/transport-v2.md`).
//!
//! ```text
//! th     = SHA256(LP(label) || LP(protocol) || LP(device_id) ||
//!                 LP(server_static_public) || LP(client_material) ||
//!                 LP(client_nonce) || LP(server_nonce))
//! prk    = HKDF-Extract(salt = th, ikm = shared)
//! k_up   = HKDF-Expand(prk, label || "/up",   32)
//! k_down = HKDF-Expand(prk, label || "/down", 32)                    (WebSocket)
//! k_down = HKDF-Expand(prk, label || "/down" || response_nonce, 32)  (REST, sealed revision 2)
//! ```
//!
//! REST has no round trip for a server nonce, so the server mixes a fresh
//! 32-byte `response_nonce` into every response key instead and sends it in
//! front of the response records.
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
/// REST response nonces (sealed revision 2) are 32 random bytes too.
pub(crate) const RESPONSE_NONCE_LENGTH: usize = 32;
const KEY_LENGTH: usize = 32;
const ML_KEM_768_CIPHERTEXT_LENGTH: usize = 1088;

/// pqcrypto-mlkem 0.1's `SharedSecret` is a plain `Copy` byte array: it does
/// not zeroize on drop and offers no mutable access. Overwrite the copy we
/// own in place with a volatile write of an all-zero secret. Copies the
/// crate makes internally (the FFI output inside `decapsulate`, on its own
/// stack frame) are out of reach; the stack is reused by later calls.
fn wipe_ml_kem_shared_secret(shared: &mut mlkem768::SharedSecret) {
    let Ok(zero) = mlkem768::SharedSecret::from_bytes(&[0_u8; 32]) else {
        // Unreachable for ML-KEM-768 (32-byte secrets); nothing to wipe with.
        return;
    };
    // SAFETY: `shared` is a valid, aligned, exclusive reference and `zero`
    // is a valid value of the same type, which has no `Drop` to skip.
    unsafe { std::ptr::write_volatile(shared, zero) };
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

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

/// `th` and `prk = HKDF-Extract(salt = th, ikm = shared)`.
fn extract(input: &KeyScheduleInput<'_>) -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>) {
    let th = transcript_hash(input);
    let (prk, _) = Hkdf::<Sha256>::extract(Some(th.as_slice()), input.shared);
    let mut out = Zeroizing::new([0_u8; 32]);
    out.copy_from_slice(prk.as_slice());
    // `GenericArray` has no zeroize support; wipe the returned copy.
    let mut prk = prk;
    zeroize::Zeroize::zeroize(prk.as_mut_slice());
    (th, out)
}

/// `HKDF-Expand(prk, concat(info), 32)`. hkdf 0.12 keeps the HMAC state of
/// `prk` in a value it does not zeroize; it lives only for this call.
fn expand(
    prk: &[u8; 32],
    info: &[&[u8]],
) -> Result<Zeroizing<[u8; KEY_LENGTH]>, TransportCryptoError> {
    let hkdf =
        Hkdf::<Sha256>::from_prk(prk).map_err(|_| TransportCryptoError::new("key derivation"))?;
    let mut key = Zeroizing::new([0_u8; KEY_LENGTH]);
    hkdf.expand_multi_info(info, key.as_mut_slice())
        .map_err(|_| TransportCryptoError::new("key derivation"))?;
    Ok(key)
}

/// WebSocket key schedule: both directions from the handshake alone.
pub(crate) fn derive_keys(
    input: &KeyScheduleInput<'_>,
) -> Result<TransportKeys, TransportCryptoError> {
    let (th, prk) = extract(input);
    let label = input.label.as_bytes();
    Ok(TransportKeys {
        k_up: expand(&prk, &[label, b"/up"])?,
        k_down: expand(&prk, &[label, b"/down"])?,
        th,
    })
}

/// `(th, k_down)` of one REST response.
pub(crate) type ResponseKey = (Zeroizing<[u8; 32]>, Zeroizing<[u8; KEY_LENGTH]>);

/// REST key schedule before the response nonce is known: `th`, `k_up`, and
/// `prk` for [`RestKeys::k_down`]. Everything is wiped on drop.
pub(crate) struct RestKeys {
    pub th: Zeroizing<[u8; 32]>,
    pub k_up: Zeroizing<[u8; KEY_LENGTH]>,
    prk: Zeroizing<[u8; 32]>,
}

impl RestKeys {
    /// The pseudorandom key, for the shared vectors.
    #[cfg(test)]
    pub(crate) fn prk(&self) -> &[u8; 32] {
        &self.prk
    }

    /// `k_down = HKDF-Expand(prk, label || "/down" || response_nonce, 32)`.
    /// Consumes the keys, so `prk` is wiped once the response key exists.
    pub(crate) fn into_k_down(
        self,
        response_nonce: &[u8; RESPONSE_NONCE_LENGTH],
    ) -> Result<ResponseKey, TransportCryptoError> {
        let k_down = expand(
            &self.prk,
            &[REST_LABEL.as_bytes(), b"/down", response_nonce],
        )?;
        Ok((self.th, k_down))
    }
}

/// REST key schedule (`label` is [`REST_LABEL`]).
pub(crate) fn derive_rest_keys(
    input: &KeyScheduleInput<'_>,
) -> Result<RestKeys, TransportCryptoError> {
    if input.label != REST_LABEL {
        return Err(TransportCryptoError::new("rest key schedule label"));
    }
    let (th, prk) = extract(input);
    Ok(RestKeys {
        k_up: expand(&prk, &[REST_LABEL.as_bytes(), b"/up"])?,
        th,
        prk,
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
                // x25519-dalek's `SharedSecret` zeroizes itself on drop; copy
                // straight into the zeroizing buffer (no stack temporary).
                let mut out = Zeroizing::new([0_u8; 32]);
                out.copy_from_slice(shared.as_bytes());
                Ok(out)
            }
            EncryptionProtocol::MlKem768 => {
                if client_material.len() != ML_KEM_768_CIPHERTEXT_LENGTH {
                    return Err(TransportCryptoError::new("ml-kem-768 ciphertext length"));
                }
                let ciphertext = mlkem768::Ciphertext::from_bytes(client_material)
                    .map_err(|_| TransportCryptoError::new("ml-kem-768 ciphertext"))?;
                let mut shared = mlkem768::decapsulate(&ciphertext, &self.ml_kem_secret);
                let mut out = Zeroizing::new([0_u8; 32]);
                let copied = if shared.as_bytes().len() == out.len() {
                    out.copy_from_slice(shared.as_bytes());
                    Ok(out)
                } else {
                    Err(TransportCryptoError::new("ml-kem-768 shared secret length"))
                };
                wipe_ml_kem_shared_secret(&mut shared);
                copied
            }
        }
    }

    /// Agreement plus the WebSocket key schedule for one server-side
    /// session.
    pub(crate) fn server_session_keys(
        &self,
        label: &str,
        protocol: EncryptionProtocol,
        device_id: &str,
        client_material: &[u8],
        client_nonce: &[u8],
        server_nonce: &[u8],
    ) -> Result<TransportKeys, TransportCryptoError> {
        self.server_schedule(
            label,
            protocol,
            device_id,
            client_material,
            client_nonce,
            server_nonce,
            derive_keys,
        )
    }

    /// Agreement plus the REST key schedule for one tunnel request
    /// (empty device id and server nonce).
    pub(crate) fn server_rest_keys(
        &self,
        protocol: EncryptionProtocol,
        client_material: &[u8],
        client_nonce: &[u8],
    ) -> Result<RestKeys, TransportCryptoError> {
        self.server_schedule(
            REST_LABEL,
            protocol,
            "",
            client_material,
            client_nonce,
            &[],
            derive_rest_keys,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn server_schedule<T>(
        &self,
        label: &str,
        protocol: EncryptionProtocol,
        device_id: &str,
        client_material: &[u8],
        client_nonce: &[u8],
        server_nonce: &[u8],
        schedule: fn(&KeyScheduleInput<'_>) -> Result<T, TransportCryptoError>,
    ) -> Result<T, TransportCryptoError> {
        if client_nonce.len() != NONCE_LENGTH {
            return Err(TransportCryptoError::new("client nonce length"));
        }
        let shared = self.agree(protocol, client_material)?;
        schedule(&KeyScheduleInput {
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
