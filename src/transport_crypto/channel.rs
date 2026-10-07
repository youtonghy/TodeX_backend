//! Transport v2 sealed records and WebSocket frames.
//!
//! ```text
//! nonce_i = 16 zero bytes || u64_be(i)
//! aad_i   = th || direction || final
//! ```
//!
//! Each direction has its own key and a strictly increasing counter that
//! starts at 0. A receiver accepts only the next expected counter; any
//! mismatch, authentication failure or exhausted counter is fatal.
use std::fmt;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use super::handshake::TransportKeys;

pub(crate) const DIRECTION_UP: u8 = 0x02;
pub(crate) const DIRECTION_DOWN: u8 = 0x01;
pub(crate) const TAG_LENGTH: usize = 16;
/// A WebSocket binary frame is `u64_be(i) || ciphertext`.
pub(crate) const WS_FRAME_OVERHEAD: usize = 8 + TAG_LENGTH;
pub(crate) const WS_CLOSE_CODE: u16 = 4400;
pub(crate) const WS_CLOSE_REASON: &str = "transport crypto failure";

/// Any failure to open, decode or authenticate v2 data. The reason is for
/// local logs only; peers only ever see `transport crypto failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransportCryptoError(&'static str);

impl TransportCryptoError {
    pub(crate) const fn new(reason: &'static str) -> Self {
        Self(reason)
    }

    pub(crate) fn reason(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for TransportCryptoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "transport crypto failure: {}", self.0)
    }
}

impl std::error::Error for TransportCryptoError {}

impl From<TransportCryptoError> for crate::error::AppError {
    fn from(_: TransportCryptoError) -> Self {
        Self::TransportCryptoFailed
    }
}

/// One direction of a keyed record stream. The counter advances only after
/// a record is sealed or opened successfully. The key (inside the AEAD) and
/// the transcript hash are wiped on drop.
pub(crate) struct RecordCipher {
    cipher: XChaCha20Poly1305,
    th: Zeroizing<[u8; 32]>,
    direction: u8,
    counter: u64,
}

impl RecordCipher {
    pub(crate) fn new(key: &[u8; 32], th: &[u8; 32], direction: u8) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(Key::from_slice(key)),
            th: Zeroizing::new(*th),
            direction,
            counter: 0,
        }
    }

    /// The counter of the next record.
    #[cfg(test)]
    pub(crate) fn next_counter(&self) -> u64 {
        self.counter
    }

    fn check_counter(counter: u64) -> Result<(), TransportCryptoError> {
        // `u64::MAX` is never used, so "next expected" always fits.
        if counter == u64::MAX {
            return Err(TransportCryptoError::new("record counter exhausted"));
        }
        Ok(())
    }

    fn nonce(counter: u64) -> [u8; 24] {
        let mut nonce = [0_u8; 24];
        nonce[16..].copy_from_slice(&counter.to_be_bytes());
        nonce
    }

    fn aad(&self, final_record: bool) -> [u8; 34] {
        let mut aad = [0_u8; 34];
        aad[..32].copy_from_slice(self.th.as_slice());
        aad[32] = self.direction;
        aad[33] = u8::from(final_record);
        aad
    }

    pub(crate) fn seal(
        &mut self,
        plaintext: &[u8],
        final_record: bool,
    ) -> Result<(u64, Vec<u8>), TransportCryptoError> {
        let counter = self.counter;
        Self::check_counter(counter)?;
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&Self::nonce(counter)),
                Payload {
                    msg: plaintext,
                    aad: &self.aad(final_record),
                },
            )
            .map_err(|_| TransportCryptoError::new("record seal"))?;
        self.counter = counter + 1;
        Ok((counter, ciphertext))
    }

    /// Opens record `counter`; anything but the next expected counter fails.
    pub(crate) fn open(
        &mut self,
        counter: u64,
        ciphertext: &[u8],
        final_record: bool,
    ) -> Result<Vec<u8>, TransportCryptoError> {
        Self::check_counter(counter)?;
        if counter != self.counter {
            return Err(TransportCryptoError::new("unexpected record counter"));
        }
        if ciphertext.len() < TAG_LENGTH {
            return Err(TransportCryptoError::new("record too short"));
        }
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(&Self::nonce(counter)),
                Payload {
                    msg: ciphertext,
                    aad: &self.aad(final_record),
                },
            )
            .map_err(|_| TransportCryptoError::new("record authentication"))?;
        self.counter = counter + 1;
        Ok(plaintext)
    }

    /// Opens the next record whose `final` flag is unknown: a streaming
    /// reader cannot tell whether more bytes follow, so it learns the flag
    /// from the tag. A forged record fails both attempts and the counter
    /// does not move.
    pub(crate) fn open_next_either(
        &mut self,
        ciphertext: &[u8],
    ) -> Result<(Vec<u8>, bool), TransportCryptoError> {
        let counter = self.counter;
        if let Ok(plaintext) = self.open(counter, ciphertext, false) {
            return Ok((plaintext, false));
        }
        Ok((self.open(counter, ciphertext, true)?, true))
    }
}

/// Seals JSON text messages as WebSocket binary frames.
pub(crate) struct WsFrameSealer(RecordCipher);

impl WsFrameSealer {
    pub(crate) fn seal_text(&mut self, text: &str) -> Result<Vec<u8>, TransportCryptoError> {
        let (counter, ciphertext) = self.0.seal(text.as_bytes(), false)?;
        let mut frame = Vec::with_capacity(8 + ciphertext.len());
        frame.extend_from_slice(&counter.to_be_bytes());
        frame.extend_from_slice(&ciphertext);
        Ok(frame)
    }
}

/// Opens WebSocket binary frames into JSON text messages.
pub(crate) struct WsFrameOpener(RecordCipher);

impl WsFrameOpener {
    pub(crate) fn open_frame(&mut self, frame: &[u8]) -> Result<String, TransportCryptoError> {
        if frame.len() < WS_FRAME_OVERHEAD {
            return Err(TransportCryptoError::new("frame too short"));
        }
        let (counter, ciphertext) = frame.split_at(8);
        let counter = u64::from_be_bytes(counter.try_into().expect("eight-byte counter prefix"));
        let plaintext = self.0.open(counter, ciphertext, false)?;
        String::from_utf8(plaintext).map_err(|_| TransportCryptoError::new("frame is not UTF-8"))
    }
}

/// Both directions of a v2 WebSocket session.
pub(crate) struct SecureChannel {
    pub sealer: WsFrameSealer,
    pub opener: WsFrameOpener,
}

impl SecureChannel {
    /// Server role: seal with `k_down`, open with `k_up`.
    pub(crate) fn server(keys: &TransportKeys) -> Self {
        Self {
            sealer: WsFrameSealer(RecordCipher::new(&keys.k_down, &keys.th, DIRECTION_DOWN)),
            opener: WsFrameOpener(RecordCipher::new(&keys.k_up, &keys.th, DIRECTION_UP)),
        }
    }

    /// Client role, for tests that talk to the server.
    #[cfg(test)]
    pub(crate) fn client(keys: &TransportKeys) -> Self {
        Self {
            sealer: WsFrameSealer(RecordCipher::new(&keys.k_up, &keys.th, DIRECTION_UP)),
            opener: WsFrameOpener(RecordCipher::new(&keys.k_down, &keys.th, DIRECTION_DOWN)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (RecordCipher, RecordCipher) {
        (
            RecordCipher::new(&[7; 32], &[9; 32], DIRECTION_UP),
            RecordCipher::new(&[7; 32], &[9; 32], DIRECTION_UP),
        )
    }

    #[test]
    fn counters_are_strict_and_only_advance_on_success() {
        let (mut sealer, mut opener) = pair();
        let (zero, first) = sealer.seal(b"a", false).unwrap();
        let (one, second) = sealer.seal(b"b", false).unwrap();
        assert_eq!((zero, one), (0, 1));
        assert!(opener.open(1, &second, false).is_err());
        assert_eq!(opener.next_counter(), 0);
        let mut tampered = first.clone();
        tampered[0] ^= 1;
        assert!(opener.open(0, &tampered, false).is_err());
        assert!(
            opener.open(0, &first, true).is_err(),
            "final flag is authenticated"
        );
        assert_eq!(opener.open(0, &first, false).unwrap(), b"a");
        assert!(opener.open(0, &first, false).is_err(), "replay");
        assert_eq!(opener.open(1, &second, false).unwrap(), b"b");
    }

    #[test]
    fn direction_is_authenticated_and_counters_exhaust() {
        let mut up = RecordCipher::new(&[7; 32], &[9; 32], DIRECTION_UP);
        let mut down = RecordCipher::new(&[7; 32], &[9; 32], DIRECTION_DOWN);
        let (_, sealed) = up.seal(b"x", false).unwrap();
        assert!(down.open(0, &sealed, false).is_err());

        up.counter = u64::MAX;
        assert_eq!(
            up.seal(b"x", false).unwrap_err().reason(),
            "record counter exhausted"
        );
        down.counter = u64::MAX;
        assert!(down.open(u64::MAX, &sealed, false).is_err());
    }

    #[test]
    fn either_open_learns_the_final_flag() {
        let (mut sealer, mut opener) = pair();
        let (_, first) = sealer.seal(b"a", false).unwrap();
        let (_, last) = sealer.seal(b"b", true).unwrap();
        assert_eq!(
            opener.open_next_either(&first).unwrap(),
            (b"a".to_vec(), false)
        );
        assert_eq!(
            opener.open_next_either(&last).unwrap(),
            (b"b".to_vec(), true)
        );
        let mut forged = last;
        forged[1] ^= 1;
        assert!(opener.open_next_either(&forged).is_err());
        assert_eq!(opener.next_counter(), 2);
    }
}
