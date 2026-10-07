//! Shared transport v2 vectors (`tests/fixtures/transport-v2.json`, a
//! verbatim copy of the file `TodeX_protocol` generates). The ML-KEM side
//! only decapsulates: encapsulation is randomized in the Rust library, so
//! the vector carries the ciphertext with its expected shared secret.
use std::sync::Arc;

use pqcrypto_mlkem::mlkem768;
use pqcrypto_traits::kem::SecretKey as MlKemSecretKey;
use serde_json::Value;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use super::channel::{RecordCipher, SecureChannel, DIRECTION_DOWN, DIRECTION_UP};
use super::envelope::{
    seal_record_stream, split_inner_request, RecordStreamDecoder, RecordStreamSealer,
};
use super::handshake::{derive_keys, KeyScheduleInput, TransportKeys, REST_LABEL, WS_LABEL};
use super::{encode_b64, EncryptionProtocol, PairingKeys};

pub(crate) fn fixture() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/transport-v2.json")).unwrap()
}

pub(crate) fn hex(value: &Value) -> Vec<u8> {
    let text = value.as_str().expect("hex string");
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Server keys from the vector; the other protocol's half is random.
fn server_keys(protocol: EncryptionProtocol, server: &Value) -> PairingKeys {
    let mut keys = PairingKeys::generate();
    match protocol {
        EncryptionProtocol::X25519 => {
            let secret: [u8; 32] = hex(&server["secretKey"]).try_into().unwrap();
            let secret = X25519Secret::from(secret);
            keys.x25519_public = X25519PublicKey::from(&secret).to_bytes();
            keys.x25519_secret = Arc::new(secret);
            assert_eq!(keys.x25519_public.to_vec(), hex(&server["publicKey"]));
        }
        EncryptionProtocol::MlKem768 => {
            keys.ml_kem_secret =
                Arc::new(mlkem768::SecretKey::from_bytes(&hex(&server["secretKey"])).unwrap());
            keys.ml_kem_public = hex(&server["publicKey"]);
        }
    }
    keys
}

fn assert_keys(keys: &TransportKeys, vector: &Value) {
    assert_eq!(to_hex(keys.th.as_slice()), vector["th"]);
    assert_eq!(to_hex(keys.k_up.as_slice()), vector["kUp"]);
    assert_eq!(to_hex(keys.k_down.as_slice()), vector["kDown"]);
}

fn client_keys(
    protocol: EncryptionProtocol,
    server: &PairingKeys,
    vector: &Value,
) -> TransportKeys {
    let shared = hex(&vector["shared"]);
    derive_keys(&KeyScheduleInput {
        label: vector["label"].as_str().unwrap(),
        protocol,
        device_id: vector["deviceId"].as_str().unwrap(),
        server_static_public: server.static_public(protocol),
        client_material: &hex(&vector["clientMaterial"]),
        client_nonce: &hex(&vector["clientNonce"]),
        server_nonce: &hex(&vector["serverNonce"]),
        shared: &shared,
    })
    .unwrap()
}

fn open_stream(
    cipher: RecordCipher,
    stream: &[u8],
) -> Result<Vec<u8>, super::TransportCryptoError> {
    let mut decoder = RecordStreamDecoder::new(cipher);
    let mut out = Vec::new();
    decoder.push(stream, &mut out)?;
    decoder.finish()?;
    Ok(out)
}

#[test]
fn transport_v2_vectors_match() {
    let fixture = fixture();
    assert_eq!(fixture["constants"]["wsLabel"], WS_LABEL);
    assert_eq!(fixture["constants"]["restLabel"], REST_LABEL);
    assert_eq!(fixture["constants"]["directionUp"], DIRECTION_UP);
    assert_eq!(fixture["constants"]["directionDown"], DIRECTION_DOWN);
    assert_eq!(
        fixture["constants"]["recordPlaintextMax"],
        super::envelope::RECORD_PLAINTEXT_MAX
    );
    assert_eq!(
        fixture["constants"]["wsCloseCode"],
        super::channel::WS_CLOSE_CODE
    );
    assert_eq!(
        fixture["constants"]["wsCloseReason"],
        super::channel::WS_CLOSE_REASON
    );
    assert_eq!(
        fixture["constants"]["failureCode"],
        crate::error::AppError::TransportCryptoFailed.code()
    );
    let protocols = fixture["protocols"].as_array().unwrap();
    assert_eq!(protocols.len(), 2);
    for vector in protocols {
        let protocol = EncryptionProtocol::parse(vector["protocol"].as_str().unwrap()).unwrap();
        let server = server_keys(protocol, &vector["server"]);
        check_ws(protocol, &server, &vector["ws"]);
        check_rest(protocol, &server, &vector["rest"]);
        check_failures(protocol, &server, vector);
    }
}

fn check_ws(protocol: EncryptionProtocol, server: &PairingKeys, ws: &Value) {
    let material = hex(&ws["clientMaterial"]);
    let material_key = match protocol {
        EncryptionProtocol::X25519 => "client_key",
        EncryptionProtocol::MlKem768 => "ciphertext",
    };
    assert_eq!(ws["query"][material_key], encode_b64(&material));
    assert_eq!(
        ws["query"]["client_nonce"],
        encode_b64(&hex(&ws["clientNonce"]))
    );
    assert_eq!(ws["query"]["enc"], protocol.as_str());
    assert_eq!(ws["query"]["tv"], "2");
    assert_eq!(
        to_hex(server.agree(protocol, &material).unwrap().as_slice()),
        ws["shared"]
    );
    let hello: Value = serde_json::from_str(ws["hello"].as_str().unwrap()).unwrap();
    assert_eq!(hello["serverNonce"], encode_b64(&hex(&ws["serverNonce"])));

    let keys = server
        .server_session_keys(
            WS_LABEL,
            protocol,
            ws["deviceId"].as_str().unwrap(),
            &material,
            &hex(&ws["clientNonce"]),
            &hex(&ws["serverNonce"]),
        )
        .unwrap();
    assert_keys(&keys, ws);
    let mut channel = SecureChannel::server(&keys);
    for frame in ws["frames"].as_array().unwrap() {
        let plaintext = frame["plaintext"].as_str().unwrap();
        let bytes = hex(&frame["frame"]);
        if frame["direction"] == "up" {
            assert_eq!(channel.opener.open_frame(&bytes).unwrap(), plaintext);
        } else {
            assert_eq!(channel.sealer.seal_text(plaintext).unwrap(), bytes);
        }
    }
}

fn check_rest(protocol: EncryptionProtocol, server: &PairingKeys, rest: &Value) {
    let material = hex(&rest["clientMaterial"]);
    let headers = &rest["outerHeaders"];
    assert_eq!(
        headers["content-type"],
        super::envelope::SEALED_CONTENT_TYPE
    );
    assert_eq!(headers["x-todex-transport"], "2");
    assert_eq!(headers["x-todex-encryption"], protocol.as_str());
    let material_header = match protocol {
        EncryptionProtocol::X25519 => "x-todex-client-key",
        EncryptionProtocol::MlKem768 => "x-todex-kem-ciphertext",
    };
    assert_eq!(headers[material_header], encode_b64(&material));
    assert_eq!(
        headers["x-todex-request-nonce"],
        encode_b64(&hex(&rest["clientNonce"]))
    );
    assert_eq!(rest["deviceId"], "");
    assert_eq!(rest["serverNonce"], "");
    let keys = server
        .server_session_keys(
            REST_LABEL,
            protocol,
            "",
            &material,
            &hex(&rest["clientNonce"]),
            &[],
        )
        .unwrap();
    assert_keys(&keys, rest);
    let up = || RecordCipher::new(&keys.k_up, &keys.th, DIRECTION_UP);
    let down = || RecordCipher::new(&keys.k_down, &keys.th, DIRECTION_DOWN);

    // Request: open the sealed stream and parse the inner request.
    let request = &rest["request"];
    let plaintext = open_stream(up(), &hex(&request["stream"])).unwrap();
    assert_eq!(to_hex(&plaintext), request["plaintext"]);
    let (head, body) = split_inner_request(plaintext).unwrap();
    let expected_head: Value = serde_json::from_str(request["headJson"].as_str().unwrap()).unwrap();
    assert_eq!(serde_json::to_value(&head).unwrap(), expected_head);
    assert_eq!(to_hex(&body), request["body"]);

    // Response: seal the given plaintext, one-shot and streamed.
    let response = &rest["response"];
    let plaintext = hex(&response["plaintext"]);
    let stream = hex(&response["stream"]);
    assert_eq!(seal_record_stream(&mut down(), &plaintext).unwrap(), stream);
    let head_len = u32::from_be_bytes(plaintext[..4].try_into().unwrap()) as usize;
    let mut sealer = RecordStreamSealer::new(down(), plaintext[..4 + head_len].to_vec());
    let mut streamed = sealer.push(&plaintext[4 + head_len..]).unwrap();
    streamed.extend(sealer.finish().unwrap());
    assert_eq!(streamed, stream);
    let head: Value = serde_json::from_slice(&plaintext[4..4 + head_len]).unwrap();
    assert_eq!(head["status"], response["status"]);

    // Multi-record response, compared by digest.
    let multi = &rest["multiRecordResponse"];
    let head_json = multi["headJson"].as_str().unwrap().as_bytes();
    let body_length = multi["bodyLength"].as_u64().unwrap() as usize;
    let mut plaintext = (head_json.len() as u32).to_be_bytes().to_vec();
    plaintext.extend_from_slice(head_json);
    plaintext.extend((0..body_length).map(|index| (index % 251) as u8));
    assert_eq!(
        plaintext.len() as u64,
        multi["plaintextLength"].as_u64().unwrap()
    );
    assert_eq!(
        to_hex(&Sha256::digest(&plaintext)),
        multi["plaintextSha256"]
    );
    let stream = seal_record_stream(&mut down(), &plaintext).unwrap();
    assert_eq!(stream.len() as u64, multi["streamLength"].as_u64().unwrap());
    assert_eq!(to_hex(&Sha256::digest(&stream)), multi["streamSha256"]);
    let mut offset = 0;
    for record in multi["records"].as_array().unwrap() {
        let length = u32::from_be_bytes(stream[offset..offset + 4].try_into().unwrap()) as usize;
        assert_eq!(length as u64, record["ciphertextLength"].as_u64().unwrap());
        let ciphertext = &stream[offset + 4..offset + 4 + length];
        assert_eq!(
            to_hex(&Sha256::digest(ciphertext)),
            record["ciphertextSha256"]
        );
        offset += 4 + length;
    }
    assert_eq!(offset, stream.len());
    assert_eq!(open_stream(down(), &stream).unwrap(), plaintext);
}

/// Every failure case is offered to the client's receiving side.
fn check_failures(protocol: EncryptionProtocol, server: &PairingKeys, vector: &Value) {
    let failures = vector["failures"].as_array().unwrap();
    assert!(!failures.is_empty());
    for failure in failures {
        assert_eq!(failure["receive"], "down");
        let input = hex(&failure["input"]);
        let name = failure["name"].as_str().unwrap();
        match failure["kind"].as_str().unwrap() {
            "ws" => {
                let keys = client_keys(protocol, server, &vector["ws"]);
                let mut channel = SecureChannel::client(&keys);
                assert!(channel.opener.open_frame(&input).is_err(), "{name}");
            }
            "rest" => {
                let keys = client_keys(protocol, server, &vector["rest"]);
                let down = RecordCipher::new(&keys.k_down, &keys.th, DIRECTION_DOWN);
                assert!(open_stream(down, &input).is_err(), "{name}");
            }
            other => panic!("unknown failure kind {other}"),
        }
    }
}
