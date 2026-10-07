//! Transport v2 REST record streams and the inner request/response.
//!
//! On the wire each record is `u32_be(len(ciphertext)) || ciphertext` with at
//! most 64 KiB of plaintext; exactly the last record has `final = 0x01` and
//! nothing may follow it. The concatenated plaintext is
//! `u32_be(len(head)) || head || body` with a JSON head of at most 64 KiB.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::channel::{RecordCipher, TransportCryptoError, TAG_LENGTH};

pub(crate) const RECORD_PLAINTEXT_MAX: usize = 65_536;
const RECORD_CIPHERTEXT_MAX: usize = RECORD_PLAINTEXT_MAX + TAG_LENGTH;
/// Wire overhead of one record: the length prefix plus the tag.
pub(crate) const RECORD_OVERHEAD: usize = 4 + TAG_LENGTH;
pub(crate) const MAX_HEAD_BYTES: usize = 65_536;
/// Media type of a sealed request; sealed responses add the `r` parameter.
pub(crate) const SEALED_CONTENT_TYPE: &str = "application/vnd.todex.sealed";
/// The sealed revision this server speaks: responses start with a 32-byte
/// response nonce that the response key mixes in (`docs/transport-v2.md`).
pub(crate) const SEALED_REVISION: u8 = 2;
/// Outer request header that names the sealed revision the client speaks.
pub(crate) const SEALED_REVISION_HEADER: &str = "x-todex-sealed-revision";
/// `Content-Type` of a sealed response. Clients decrypt only a `200` with
/// exactly this revision.
pub(crate) const SEALED_RESPONSE_CONTENT_TYPE: &str = "application/vnd.todex.sealed; r=2";
pub(crate) const SEALED_PATH: &str = "/v2/sealed";

/// Wire size of a record stream carrying `plaintext` bytes.
pub(crate) const fn sealed_stream_length(plaintext: usize) -> usize {
    let records = if plaintext == 0 {
        1
    } else {
        plaintext.div_ceil(RECORD_PLAINTEXT_MAX)
    };
    plaintext + records * RECORD_OVERHEAD
}

fn push_record(
    out: &mut Vec<u8>,
    cipher: &mut RecordCipher,
    plaintext: &[u8],
    final_record: bool,
) -> Result<(), TransportCryptoError> {
    let (_, ciphertext) = cipher.seal(plaintext, final_record)?;
    out.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
    out.extend_from_slice(&ciphertext);
    Ok(())
}

/// Seals `data` as records of at most 64 KiB; the last one is final when
/// `final_last` is set. Empty data yields one empty record.
fn push_records(
    out: &mut Vec<u8>,
    cipher: &mut RecordCipher,
    data: &[u8],
    final_last: bool,
) -> Result<(), TransportCryptoError> {
    if data.is_empty() {
        return push_record(out, cipher, data, final_last);
    }
    let mut chunks = data.chunks(RECORD_PLAINTEXT_MAX).peekable();
    while let Some(chunk) = chunks.next() {
        push_record(out, cipher, chunk, final_last && chunks.peek().is_none())?;
    }
    Ok(())
}

/// One-shot sealing of a complete plaintext (vectors, tests).
#[cfg(test)]
pub(crate) fn seal_record_stream(
    cipher: &mut RecordCipher,
    plaintext: &[u8],
) -> Result<Vec<u8>, TransportCryptoError> {
    let mut out = Vec::with_capacity(sealed_stream_length(plaintext.len()));
    push_records(&mut out, cipher, plaintext, true)?;
    Ok(out)
}

/// Streaming sealer. It holds back the latest chunk until it knows whether
/// more data follows, so records follow the producer's chunks (split at
/// 64 KiB) and a body produced in one chunk seals exactly like a one-shot
/// stream of the whole plaintext.
pub(crate) struct RecordStreamSealer {
    cipher: RecordCipher,
    held: Vec<u8>,
    merge_next: bool,
}

impl RecordStreamSealer {
    /// `prefix` (the inner head) is merged with the first body chunk.
    pub(crate) fn new(cipher: RecordCipher, prefix: Vec<u8>) -> Self {
        Self {
            cipher,
            held: prefix,
            merge_next: true,
        }
    }

    /// Wire bytes that can be sent now.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, TransportCryptoError> {
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        if self.merge_next {
            self.merge_next = false;
            self.held.extend_from_slice(chunk);
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(sealed_stream_length(self.held.len()));
        if !self.held.is_empty() {
            push_records(&mut out, &mut self.cipher, &self.held, false)?;
        }
        self.held.clear();
        self.held.extend_from_slice(chunk);
        Ok(out)
    }

    /// The remaining records, the last one final.
    pub(crate) fn finish(mut self) -> Result<Vec<u8>, TransportCryptoError> {
        let mut out = Vec::with_capacity(sealed_stream_length(self.held.len()));
        push_records(&mut out, &mut self.cipher, &self.held, true)?;
        Ok(out)
    }
}

/// Incremental decoder for a sealed record stream.
pub(crate) struct RecordStreamDecoder {
    cipher: RecordCipher,
    buffer: Vec<u8>,
    saw_final: bool,
}

impl RecordStreamDecoder {
    pub(crate) fn new(cipher: RecordCipher) -> Self {
        Self {
            cipher,
            buffer: Vec::new(),
            saw_final: false,
        }
    }

    /// Opens every record completed by `chunk` and appends the plaintext to
    /// `out`. Bytes after the final record are fatal.
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), TransportCryptoError> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.saw_final {
            return Err(TransportCryptoError::new("bytes after final record"));
        }
        self.buffer.extend_from_slice(chunk);
        let mut offset = 0;
        while self.buffer.len() - offset >= 4 {
            let length = u32::from_be_bytes(
                self.buffer[offset..offset + 4]
                    .try_into()
                    .expect("four-byte length prefix"),
            ) as usize;
            if !(TAG_LENGTH..=RECORD_CIPHERTEXT_MAX).contains(&length) {
                return Err(TransportCryptoError::new("record length"));
            }
            if self.buffer.len() - offset - 4 < length {
                break;
            }
            let ciphertext = &self.buffer[offset + 4..offset + 4 + length];
            offset += 4 + length;
            let (plaintext, final_record) = self.cipher.open_next_either(ciphertext)?;
            out.extend_from_slice(&plaintext);
            if final_record {
                self.saw_final = true;
                if offset != self.buffer.len() {
                    return Err(TransportCryptoError::new("bytes after final record"));
                }
            }
        }
        self.buffer.drain(..offset);
        Ok(())
    }

    /// The stream ended: the final record must have arrived.
    pub(crate) fn finish(&self) -> Result<(), TransportCryptoError> {
        if !self.saw_final || !self.buffer.is_empty() {
            return Err(TransportCryptoError::new("truncated record stream"));
        }
        Ok(())
    }
}

/// `{"method", "path", "query"?, "headers"}` of an inner request.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct InnerRequestHead {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// `{"status", "headers"}` of an inner response.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct InnerResponseHead {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

/// `path` starts with `/`, carries no query or fragment and never targets
/// the tunnel itself.
pub(crate) fn validate_inner_path(path: &str) -> Result<(), TransportCryptoError> {
    if !path.starts_with('/') || path.contains(['?', '#']) {
        return Err(TransportCryptoError::new("invalid inner request path"));
    }
    if path == SEALED_PATH || path.starts_with(&format!("{SEALED_PATH}/")) {
        return Err(TransportCryptoError::new("nested sealed request"));
    }
    Ok(())
}

/// Parses the request head at the start of `u32_be(len(head)) || head ||
/// body` once enough plaintext has arrived. Returns the head and the offset
/// where the body starts, or `None` while the head is still incomplete.
pub(crate) fn parse_inner_head(
    plaintext: &[u8],
) -> Result<Option<(InnerRequestHead, usize)>, TransportCryptoError> {
    let Some(prefix) = plaintext.get(..4) else {
        return Ok(None);
    };
    let length = u32::from_be_bytes(prefix.try_into().expect("four-byte head length")) as usize;
    if length > MAX_HEAD_BYTES {
        return Err(TransportCryptoError::new("inner head too large"));
    }
    let Some(json) = plaintext.get(4..4 + length) else {
        return Ok(None);
    };
    let head: InnerRequestHead = serde_json::from_slice(json)
        .map_err(|_| TransportCryptoError::new("malformed inner request head"))?;
    validate_inner_path(&head.path)?;
    Ok(Some((head, 4 + length)))
}

/// Splits `u32_be(len(head)) || head || body` and parses the request head.
#[cfg(test)]
pub(crate) fn split_inner_request(
    mut plaintext: Vec<u8>,
) -> Result<(InnerRequestHead, Vec<u8>), TransportCryptoError> {
    if plaintext.len() < 4 {
        return Err(TransportCryptoError::new("inner message too short"));
    }
    let Some((head, body_start)) = parse_inner_head(&plaintext)? else {
        return Err(TransportCryptoError::new("inner head truncated"));
    };
    // Reuse the allocation for the body.
    plaintext.drain(..body_start);
    Ok((head, plaintext))
}

/// `u32_be(len(head)) || head` for an inner response.
pub(crate) fn encode_response_head(
    head: &InnerResponseHead,
) -> Result<Vec<u8>, TransportCryptoError> {
    let json =
        serde_json::to_vec(head).map_err(|_| TransportCryptoError::new("inner response head"))?;
    if json.len() > MAX_HEAD_BYTES {
        return Err(TransportCryptoError::new("inner response head too large"));
    }
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport_crypto::channel::DIRECTION_DOWN;

    fn cipher() -> RecordCipher {
        RecordCipher::new(&[3; 32], &[4; 32], DIRECTION_DOWN)
    }

    fn open_all(stream: &[u8], chunk: usize) -> Result<Vec<u8>, TransportCryptoError> {
        let mut decoder = RecordStreamDecoder::new(cipher());
        let mut out = Vec::new();
        for piece in stream.chunks(chunk.max(1)) {
            decoder.push(piece, &mut out)?;
        }
        decoder.finish()?;
        Ok(out)
    }

    #[test]
    fn streaming_sealer_matches_one_shot_and_decodes_in_any_chunking() {
        let body: Vec<u8> = (0..150_000_u32).map(|i| (i % 251) as u8).collect();
        let one_shot = seal_record_stream(&mut cipher(), &body).unwrap();
        let mut sealer = RecordStreamSealer::new(cipher(), Vec::new());
        let mut streamed = sealer.push(&body).unwrap();
        streamed.extend(sealer.finish().unwrap());
        assert_eq!(streamed, one_shot);
        for chunk in [1, 7, 4096, 70_000, usize::MAX] {
            assert_eq!(open_all(&one_shot, chunk).unwrap(), body);
        }

        // Several producer chunks: still one final record at the end.
        let mut sealer = RecordStreamSealer::new(cipher(), b"head".to_vec());
        let mut wire = Vec::new();
        for piece in body.chunks(10_000) {
            wire.extend(sealer.push(piece).unwrap());
        }
        wire.extend(sealer.finish().unwrap());
        let mut expected = b"head".to_vec();
        expected.extend_from_slice(&body);
        assert_eq!(open_all(&wire, 333).unwrap(), expected);
    }

    #[test]
    fn decoder_rejects_truncation_trailing_bytes_and_bad_lengths() {
        let stream = seal_record_stream(&mut cipher(), b"hello").unwrap();
        assert!(open_all(&stream[..stream.len() - 1], 4).is_err());
        let mut trailing = stream.clone();
        trailing.push(0);
        assert!(open_all(&trailing, usize::MAX).is_err());
        // Trailing bytes in a later chunk are fatal too.
        let mut decoder = RecordStreamDecoder::new(cipher());
        let mut out = Vec::new();
        decoder.push(&stream, &mut out).unwrap();
        assert!(decoder.push(&[0], &mut out).is_err());
        assert!(open_all(&[], 1).is_err());
        assert!(open_all(&[0, 0, 0, 1, 0], 8).is_err());
        assert!(open_all(&[0, 1, 0, 17], 8).is_err());
        // An empty plaintext is one empty final record.
        let empty = seal_record_stream(&mut cipher(), b"").unwrap();
        assert_eq!(empty.len(), RECORD_OVERHEAD);
        assert_eq!(open_all(&empty, 3).unwrap(), b"");
    }

    #[test]
    fn inner_request_head_is_bounded_and_rejects_nesting() {
        let encode = |head: &str, body: &[u8]| {
            let mut out = (head.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(body);
            out
        };
        let (head, body) = split_inner_request(encode(
            r#"{"method":"GET","path":"/v2/workspaces","query":"a=b","headers":{"accept":"x"},"extra":1}"#,
            b"body",
        ))
        .unwrap();
        assert_eq!(
            (head.method.as_str(), head.path.as_str()),
            ("GET", "/v2/workspaces")
        );
        assert_eq!(head.query.as_deref(), Some("a=b"));
        assert_eq!(body, b"body");
        for path in ["/v2/sealed", "/v2/sealed/x", "v2/x", "/v2/x?y", "/v2/x#y"] {
            let head = format!(r#"{{"method":"GET","path":"{path}","headers":{{}}}}"#);
            assert!(split_inner_request(encode(&head, b"")).is_err(), "{path}");
        }
        assert!(split_inner_request(vec![0, 0]).is_err());
        assert!(
            split_inner_request(vec![0, 1, 0, 1]).is_err(),
            "head > 64 KiB"
        );
        assert!(split_inner_request(vec![0, 0, 0, 9, b'{']).is_err());
        assert!(split_inner_request(encode("[]", b"")).is_err());
    }
}
