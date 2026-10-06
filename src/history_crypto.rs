//! History crypto v1: the primitives for end-to-end encrypted conversation
//! history, identical in the TS (`TodeX_protocol/src/historyCrypto.ts`) and
//! Swift (`TodexCore/HistoryCrypto.swift`) clients and pinned by
//! `tests/fixtures/history-crypto-v1.json`.
//!
//! Devices own X-Wing (ML-KEM-768 + X25519) key pairs. The daemon only sees
//! their public keys: it seals history content under a random per-segment key
//! (DEK) that lives in memory, and wraps that DEK for every recipient device.
//! It can never unwrap a DEK again; only the test helper does.
//!
//! Storage and protocol wiring arrive in later steps, so nothing outside the
//! tests calls this module yet.
#![allow(dead_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x_wing::{Encapsulate, KeyExport};
use zeroize::{Zeroize, Zeroizing};

use crate::error::AppError;

type Result<T> = std::result::Result<T, AppError>;

pub(crate) const LABEL: &[u8] = b"todex-history-v1";
pub(crate) const PUBLIC_KEY_LEN: usize = x_wing::ENCAPSULATION_KEY_SIZE;
pub(crate) const KEM_CIPHERTEXT_LEN: usize = x_wing::CIPHERTEXT_SIZE;
pub(crate) const RECIPIENT_ID_LEN: usize = 16;
pub(crate) const KID_LEN: usize = 16;
pub(crate) const DEK_LEN: usize = 32;
/// The wrapped DEK plus its Poly1305 tag.
pub(crate) const WRAPPED_DEK_LEN: usize = DEK_LEN + 16;

/// Which record a content ciphertext belongs to. The stream and its counter
/// (event sequence or frame index) form the nonce, so each pair must be
/// sealed at most once per segment key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentStream {
    EventSummary = 1,
    EventFull = 2,
    FrameSummary = 3,
    FrameFull = 4,
}

impl ContentStream {
    pub(crate) fn as_u32(self) -> u32 {
        self as u32
    }
}

impl TryFrom<u32> for ContentStream {
    type Error = AppError;

    fn try_from(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::EventSummary),
            2 => Ok(Self::EventFull),
            3 => Ok(Self::FrameSummary),
            4 => Ok(Self::FrameFull),
            _ => Err(invalid("unknown history content stream")),
        }
    }
}

/// A device's X-Wing public key, validated on parse.
#[derive(Clone)]
pub(crate) struct RecipientPublicKey {
    key: x_wing::EncapsulationKey,
    rid: [u8; RECIPIENT_ID_LEN],
}

impl RecipientPublicKey {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != PUBLIC_KEY_LEN {
            return Err(invalid("invalid history recipient public key length"));
        }
        let key = x_wing::EncapsulationKey::try_from(bytes)
            .map_err(|_| invalid("invalid history recipient public key"))?;
        Ok(Self {
            key,
            rid: recipient_id(bytes),
        })
    }

    pub(crate) fn from_base64url(value: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| invalid("invalid history recipient public key encoding"))?;
        Self::from_bytes(&bytes)
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        self.key.to_bytes().to_vec()
    }

    /// `SHA-256(pk)[0..16]`, how wrapped keys name their recipient.
    pub(crate) fn rid(&self) -> [u8; RECIPIENT_ID_LEN] {
        self.rid
    }
}

impl std::fmt::Debug for RecipientPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecipientPublicKey")
            .field("rid", &URL_SAFE_NO_PAD.encode(self.rid))
            .finish()
    }
}

/// A per-segment content key. Zeroized on drop; `Debug` omits the DEK.
pub(crate) struct SegmentKey {
    kid: [u8; KID_LEN],
    dek: [u8; DEK_LEN],
}

impl SegmentKey {
    /// A fresh random key id and DEK from the OS RNG.
    pub(crate) fn generate() -> Self {
        let mut key = Self {
            kid: [0; KID_LEN],
            dek: [0; DEK_LEN],
        };
        OsRng.fill_bytes(&mut key.kid);
        OsRng.fill_bytes(&mut key.dek);
        key
    }

    pub(crate) fn from_parts(kid: [u8; KID_LEN], dek: [u8; DEK_LEN]) -> Self {
        Self { kid, dek }
    }

    pub(crate) fn kid(&self) -> [u8; KID_LEN] {
        self.kid
    }
}

impl Drop for SegmentKey {
    fn drop(&mut self) {
        self.dek.zeroize();
        self.kid.zeroize();
    }
}

impl std::fmt::Debug for SegmentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentKey")
            .field("kid", &URL_SAFE_NO_PAD.encode(self.kid))
            .finish_non_exhaustive()
    }
}

/// A DEK wrapped for one recipient. JSON form: `{"rid","kemCt","wrapped"}`,
/// each base64url without padding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WrappedKeyJson", into = "WrappedKeyJson")]
pub(crate) struct WrappedKey {
    pub(crate) rid: [u8; RECIPIENT_ID_LEN],
    pub(crate) kem_ct: x_wing::Ciphertext,
    pub(crate) wrapped: [u8; WRAPPED_DEK_LEN],
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WrappedKeyJson {
    rid: String,
    kem_ct: String,
    wrapped: String,
}

impl From<WrappedKey> for WrappedKeyJson {
    fn from(value: WrappedKey) -> Self {
        Self {
            rid: URL_SAFE_NO_PAD.encode(value.rid),
            kem_ct: URL_SAFE_NO_PAD.encode(value.kem_ct),
            wrapped: URL_SAFE_NO_PAD.encode(value.wrapped),
        }
    }
}

impl TryFrom<WrappedKeyJson> for WrappedKey {
    type Error = AppError;

    fn try_from(value: WrappedKeyJson) -> Result<Self> {
        fn decode<const N: usize>(value: &str) -> Result<[u8; N]> {
            URL_SAFE_NO_PAD
                .decode(value)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| invalid("invalid wrapped history key"))
        }
        Ok(Self {
            rid: decode(&value.rid)?,
            kem_ct: decode::<KEM_CIPHERTEXT_LEN>(&value.kem_ct)?.into(),
            wrapped: decode(&value.wrapped)?,
        })
    }
}

/// Wraps `key`'s DEK for `recipient` under a fresh X-Wing encapsulation.
pub(crate) fn wrap(key: &SegmentKey, recipient: &RecipientPublicKey) -> Result<WrappedKey> {
    let (kem_ct, shared) = recipient.key.encapsulate();
    let shared = Zeroizing::new(<[u8; 32]>::from(shared));
    wrap_encapsulated(key, recipient.rid, kem_ct, &shared)
}

fn wrap_encapsulated(
    key: &SegmentKey,
    rid: [u8; RECIPIENT_ID_LEN],
    kem_ct: x_wing::Ciphertext,
    shared: &[u8; 32],
) -> Result<WrappedKey> {
    let info = wrap_info(&key.kid, &rid);
    let kek = derive_kek(&kem_ct, shared, &info);
    // A zero nonce is safe: every encapsulation yields a fresh KEK.
    let wrapped = ChaCha20Poly1305::new(Key::from_slice(kek.as_slice()))
        .encrypt(
            &Nonce::default(),
            Payload {
                msg: &key.dek,
                aad: &info,
            },
        )
        .map_err(|_| invalid("unable to wrap history key"))?
        .try_into()
        .map_err(|_| invalid("unable to wrap history key"))?;
    Ok(WrappedKey {
        rid,
        kem_ct,
        wrapped,
    })
}

/// Seals one history record. `counter` is the event sequence for event
/// streams and the frame index for frame streams.
pub(crate) fn seal(
    key: &SegmentKey,
    conversation_id: &str,
    stream: ContentStream,
    counter: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(&key.dek))
        .encrypt(
            &content_nonce(stream, counter).into(),
            Payload {
                msg: plaintext,
                aad: &content_aad(conversation_id, &key.kid, stream, counter),
            },
        )
        .map_err(|_| invalid("unable to seal history content"))
}

pub(crate) fn open(
    key: &SegmentKey,
    conversation_id: &str,
    stream: ContentStream,
    counter: u64,
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(&key.dek))
        .decrypt(
            &content_nonce(stream, counter).into(),
            Payload {
                msg: ciphertext,
                aad: &content_aad(conversation_id, &key.kid, stream, counter),
            },
        )
        .map_err(|_| invalid("history content authentication failed"))
}

fn recipient_id(public_key: &[u8]) -> [u8; RECIPIENT_ID_LEN] {
    let digest = Sha256::digest(public_key);
    let mut rid = [0; RECIPIENT_ID_LEN];
    rid.copy_from_slice(&digest[..RECIPIENT_ID_LEN]);
    rid
}

/// `LABEL || "/wrap" || kid || rid`: the HKDF info and the AEAD AAD.
fn wrap_info(kid: &[u8; KID_LEN], rid: &[u8; RECIPIENT_ID_LEN]) -> Vec<u8> {
    [LABEL, b"/wrap", kid, rid].concat()
}

fn derive_kek(kem_ct: &[u8], shared: &[u8; 32], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut kek = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(Some(kem_ct), shared)
        .expand(info, kek.as_mut_slice())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    kek
}

fn content_nonce(stream: ContentStream, counter: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[..4].copy_from_slice(&stream.as_u32().to_be_bytes());
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// `LABEL || "/content" || 0 || conversation_id || 0 || kid || stream || counter`.
/// Everything after the second separator has a fixed length, so the
/// conversation id needs no escaping.
fn content_aad(
    conversation_id: &str,
    kid: &[u8; KID_LEN],
    stream: ContentStream,
    counter: u64,
) -> Vec<u8> {
    [
        LABEL,
        b"/content\0",
        conversation_id.as_bytes(),
        b"\0",
        kid,
        &stream.as_u32().to_be_bytes(),
        &counter.to_be_bytes(),
    ]
    .concat()
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

/// Device-side unwrap. The daemon never holds device private keys, so this
/// exists only to verify the wrap format in tests.
#[cfg(test)]
pub(crate) fn unwrap_for_tests(
    seed: [u8; 32],
    wrapped: &WrappedKey,
    kid: [u8; KID_LEN],
) -> Result<SegmentKey> {
    use x_wing::{Decapsulate, Decapsulator};

    let device = x_wing::DecapsulationKey::from(seed);
    let rid = recipient_id(&device.encapsulation_key().to_bytes());
    if rid != wrapped.rid {
        return Err(invalid("wrapped history key is for another recipient"));
    }
    let shared = Zeroizing::new(<[u8; 32]>::from(device.decapsulate(&wrapped.kem_ct)));
    let info = wrap_info(&kid, &rid);
    let kek = derive_kek(&wrapped.kem_ct, &shared, &info);
    let dek = Zeroizing::new(
        ChaCha20Poly1305::new(Key::from_slice(kek.as_slice()))
            .decrypt(
                &Nonce::default(),
                Payload {
                    msg: &wrapped.wrapped,
                    aad: &info,
                },
            )
            .map_err(|_| invalid("wrapped history key authentication failed"))?,
    );
    let dek = dek
        .as_slice()
        .try_into()
        .map_err(|_| invalid("invalid wrapped history key"))?;
    Ok(SegmentKey::from_parts(kid, dek))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use x_wing::Decapsulator;

    const CONVERSATION_ID: &str = "d61c17f5-dd61-4b85-90a8-6327e287bb57";

    fn hex(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn sequence<const N: usize>(start: u8) -> [u8; N] {
        std::array::from_fn(|index| start.wrapping_add(index as u8))
    }

    fn public_key_for(seed: [u8; 32]) -> RecipientPublicKey {
        let device = x_wing::DecapsulationKey::from(seed);
        RecipientPublicKey::from_bytes(&device.encapsulation_key().to_bytes()).unwrap()
    }

    fn random_seed() -> [u8; 32] {
        let mut seed = [0; 32];
        OsRng.fill_bytes(&mut seed);
        seed
    }

    /// (stream, counter, plaintext) cases, including empty and multi-KB
    /// UTF-8 plaintexts and counters with high bytes set.
    fn content_cases() -> Vec<(ContentStream, u64, Vec<u8>)> {
        let long = (1..=120)
            .map(|index| format!("第{index}段：对话历史端到端加密，后端只保存密文。🔐\n"))
            .collect::<String>();
        vec![
            (ContentStream::EventSummary, 0, Vec::new()),
            (
                ContentStream::EventFull,
                1,
                br#"{"type":"turn.completed","seq":1}"#.to_vec(),
            ),
            (
                ContentStream::FrameSummary,
                4_294_967_296,
                b"frame summary".to_vec(),
            ),
            (
                ContentStream::FrameFull,
                0x0001_0203_0405_0607,
                long.into_bytes(),
            ),
        ]
    }

    fn vector() -> Value {
        let seed = sequence::<32>(1);
        let kid = sequence::<KID_LEN>(0x40);
        let dek = sequence::<DEK_LEN>(0x80);
        let randomness = sequence::<{ x_wing::ENCAPSULATION_RANDOMNESS_SIZE }>(0xc0);
        let recipient = public_key_for(seed);
        let key = SegmentKey::from_parts(kid, dek);
        let (kem_ct, shared) = recipient.key.encapsulate_deterministic(&randomness.into());
        let shared = <[u8; 32]>::from(shared);
        let wrapped = wrap_encapsulated(&key, recipient.rid(), kem_ct, &shared).unwrap();
        let kek = derive_kek(&wrapped.kem_ct, &shared, &wrap_info(&kid, &recipient.rid()));
        let cases = content_cases()
            .into_iter()
            .map(|(stream, counter, plaintext)| {
                json!({
                    "stream": stream.as_u32(),
                    "counter": counter,
                    "plaintext": hex(&plaintext),
                    "nonce": hex(content_nonce(stream, counter)),
                    "aad": hex(content_aad(CONVERSATION_ID, &kid, stream, counter)),
                    "ciphertext": hex(seal(&key, CONVERSATION_ID, stream, counter, &plaintext).unwrap()),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "label": std::str::from_utf8(LABEL).unwrap(),
            "seed": hex(seed),
            "pk": hex(recipient.to_bytes()),
            "pkSha256": hex(Sha256::digest(recipient.to_bytes())),
            "rid": hex(recipient.rid()),
            "ridText": URL_SAFE_NO_PAD.encode(recipient.rid()),
            "kid": hex(kid),
            "dek": hex(dek),
            "conversationId": CONVERSATION_ID,
            "wrap": {
                "encapsulationRandomness": hex(randomness),
                "sharedSecret": hex(shared),
                "info": hex(wrap_info(&kid, &recipient.rid())),
                "kek": hex(kek.as_slice()),
                "kemCt": hex(wrapped.kem_ct),
                "wrapped": hex(wrapped.wrapped),
                "json": serde_json::to_value(&wrapped).unwrap(),
            },
            "content": cases,
        })
    }

    #[test]
    fn history_crypto_cross_language_vector_matches() {
        let actual = vector();
        if std::env::var_os("TODEX_WRITE_FIXTURES").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/history-crypto-v1.json");
            std::fs::write(&path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
        }
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/history-crypto-v1.json")).unwrap();
        assert_eq!(actual, expected);

        // The committed vector also unwraps and opens through the public API.
        let wrapped: WrappedKey = serde_json::from_value(expected["wrap"]["json"].clone()).unwrap();
        let key = unwrap_for_tests(sequence(1), &wrapped, sequence(0x40)).unwrap();
        assert_eq!(hex(key.dek), expected["dek"]);
        for case in content_cases() {
            let (stream, counter, plaintext) = case;
            let sealed = seal(&key, CONVERSATION_ID, stream, counter, &plaintext).unwrap();
            assert_eq!(
                open(&key, CONVERSATION_ID, stream, counter, &sealed).unwrap(),
                plaintext
            );
        }
    }

    #[test]
    fn history_crypto_random_round_trip() {
        let seed = random_seed();
        let recipient = public_key_for(seed);
        let key = SegmentKey::generate();
        let first = wrap(&key, &recipient).unwrap();
        let second = wrap(&key, &recipient).unwrap();
        assert_ne!(first.kem_ct, second.kem_ct);
        let json = serde_json::to_string(&first).unwrap();
        let parsed: WrappedKey = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, first);
        let unwrapped = unwrap_for_tests(seed, &parsed, key.kid()).unwrap();
        assert_eq!(unwrapped.dek, key.dek);

        let sealed = seal(&key, "conversation", ContentStream::EventFull, 7, b"hello").unwrap();
        assert_eq!(sealed.len(), 5 + 16);
        assert_eq!(
            open(
                &unwrapped,
                "conversation",
                ContentStream::EventFull,
                7,
                &sealed
            )
            .unwrap(),
            b"hello"
        );
    }

    #[test]
    fn history_crypto_rejects_tampering_and_wrong_context() {
        let seed = random_seed();
        let recipient = public_key_for(seed);
        let key = SegmentKey::generate();
        let sealed = seal(&key, "c1", ContentStream::FrameFull, 3, b"secret").unwrap();
        let mut flipped = sealed.clone();
        flipped[0] ^= 1;
        assert!(open(&key, "c1", ContentStream::FrameFull, 3, &flipped).is_err());
        assert!(open(&key, "c2", ContentStream::FrameFull, 3, &sealed).is_err());
        assert!(open(&key, "c1", ContentStream::FrameSummary, 3, &sealed).is_err());
        assert!(open(&key, "c1", ContentStream::FrameFull, 4, &sealed).is_err());
        let other = SegmentKey::from_parts(key.kid, SegmentKey::generate().dek);
        assert!(open(&other, "c1", ContentStream::FrameFull, 3, &sealed).is_err());

        let wrapped = wrap(&key, &recipient).unwrap();
        assert!(unwrap_for_tests(random_seed(), &wrapped, key.kid()).is_err());
        assert!(unwrap_for_tests(seed, &wrapped, [0; KID_LEN]).is_err());
        let mut tampered = wrapped.clone();
        tampered.wrapped[0] ^= 1;
        assert!(unwrap_for_tests(seed, &tampered, key.kid()).is_err());
        let mut tampered = wrapped.clone();
        tampered.kem_ct[0] ^= 1;
        assert!(unwrap_for_tests(seed, &tampered, key.kid()).is_err());
    }

    #[test]
    fn history_crypto_validates_inputs() {
        for stream in 1..=4 {
            assert_eq!(ContentStream::try_from(stream).unwrap().as_u32(), stream);
        }
        assert!(ContentStream::try_from(0).is_err());
        assert!(ContentStream::try_from(5).is_err());
        assert!(RecipientPublicKey::from_bytes(&[0; PUBLIC_KEY_LEN - 1]).is_err());
        let recipient = public_key_for(sequence(1));
        let encoded = URL_SAFE_NO_PAD.encode(recipient.to_bytes());
        assert_eq!(
            RecipientPublicKey::from_base64url(&encoded).unwrap().rid(),
            recipient.rid()
        );
        assert!(serde_json::from_value::<WrappedKey>(
            json!({"rid": "AA", "kemCt": "AA", "wrapped": "AA"})
        )
        .is_err());
        let key = SegmentKey::generate();
        assert!(!format!("{key:?}").contains(&format!("{:?}", key.dek)));
    }
}
