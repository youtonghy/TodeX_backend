//! End-to-end transport v2 tests: the WebSocket handshake and frames over a
//! real socket, and the `/v2/sealed` tunnel through the real router.
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{any, get};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use pqcrypto_mlkem::mlkem768;
use pqcrypto_traits::kem::{Ciphertext as _, PublicKey as _, SharedSecret as _};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use super::sealed::ArrivedViaTransportV2;
use crate::app_state::AppState;
use crate::config::{Config, PairingEncryption};
use crate::device_auth::test_support::TestDevice;
use crate::transport_crypto::channel::{
    RecordCipher, SecureChannel, DIRECTION_DOWN, DIRECTION_UP, WS_CLOSE_CODE, WS_CLOSE_REASON,
};
use crate::transport_crypto::envelope::{
    seal_record_stream, RecordStreamDecoder, SEALED_CONTENT_TYPE, SEALED_RESPONSE_CONTENT_TYPE,
};
use crate::transport_crypto::handshake::{
    derive_keys, derive_rest_keys, KeyScheduleInput, RestKeys, TransportKeys,
    RESPONSE_NONCE_LENGTH, REST_LABEL, WS_LABEL,
};
use crate::transport_crypto::{encode_b64, EncryptionProtocol, PairingKeys};

pub(crate) struct TestServer {
    pub address: SocketAddr,
    pub state: AppState,
    pub root: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub(crate) async fn start(pairing_encryption: PairingEncryption) -> Self {
        let root =
            std::env::temp_dir().join(format!("todex-transport-v2-{}", uuid::Uuid::new_v4()));
        let config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspace")],
            pairing_encryption,
            ..Config::default()
        };
        let state = AppState::new_for_tests(config).await.unwrap();
        let app = super::router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            address,
            state,
            root,
            server,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Client side of the key agreement against the server's static key.
pub(crate) fn client_handshake(
    keys: &PairingKeys,
    protocol: EncryptionProtocol,
) -> (Vec<u8>, Vec<u8>) {
    match protocol {
        EncryptionProtocol::X25519 => {
            let secret = X25519Secret::random_from_rng(OsRng);
            let server: [u8; 32] = keys.static_public(protocol).try_into().unwrap();
            let shared = secret.diffie_hellman(&X25519PublicKey::from(server));
            (
                X25519PublicKey::from(&secret).as_bytes().to_vec(),
                shared.as_bytes().to_vec(),
            )
        }
        EncryptionProtocol::MlKem768 => {
            let public = mlkem768::PublicKey::from_bytes(keys.static_public(protocol)).unwrap();
            let (shared, ciphertext) = mlkem768::encapsulate(&public);
            (ciphertext.as_bytes().to_vec(), shared.as_bytes().to_vec())
        }
    }
}

fn random_nonce() -> [u8; 32] {
    let mut nonce = [0; 32];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

#[allow(clippy::too_many_arguments)]
fn client_keys(
    keys: &PairingKeys,
    protocol: EncryptionProtocol,
    label: &str,
    device_id: &str,
    material: &[u8],
    shared: &[u8],
    client_nonce: &[u8],
    server_nonce: &[u8],
) -> TransportKeys {
    derive_keys(&KeyScheduleInput {
        label,
        protocol,
        device_id,
        server_static_public: keys.static_public(protocol),
        client_material: material,
        client_nonce,
        server_nonce,
        shared,
    })
    .unwrap()
}

/// `tv=2` WebSocket query (without the device credential) plus what the
/// client needs once the hello arrives.
pub(crate) struct WsOffer {
    pub query: String,
    material: Vec<u8>,
    shared: Vec<u8>,
    client_nonce: [u8; 32],
    protocol: EncryptionProtocol,
}

pub(crate) fn ws_offer(keys: &PairingKeys, protocol: EncryptionProtocol) -> WsOffer {
    let (material, shared) = client_handshake(keys, protocol);
    let client_nonce = random_nonce();
    let material_key = match protocol {
        EncryptionProtocol::X25519 => "client_key",
        EncryptionProtocol::MlKem768 => "ciphertext",
    };
    WsOffer {
        query: format!(
            "tv=2&enc={}&client_nonce={}&{material_key}={}",
            protocol.as_str(),
            encode_b64(&client_nonce),
            encode_b64(&material)
        ),
        material,
        shared,
        client_nonce,
        protocol,
    }
}

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl WsOffer {
    /// Reads the hello and derives the client channel.
    pub(crate) async fn accept_hello(
        &self,
        keys: &PairingKeys,
        device_id: &str,
        socket: &mut ClientSocket,
    ) -> SecureChannel {
        let Some(Ok(Message::Text(hello))) = socket.next().await else {
            panic!("expected the transport hello first");
        };
        let hello: Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(hello["type"], "todex.transport.hello");
        assert_eq!(hello["version"], 2);
        let server_nonce = crate::transport_crypto::decode_b64(
            hello["serverNonce"].as_str().unwrap(),
            "server nonce",
        )
        .unwrap();
        assert_eq!(server_nonce.len(), 32);
        SecureChannel::client(&client_keys(
            keys,
            self.protocol,
            WS_LABEL,
            device_id,
            &self.material,
            &self.shared,
            &self.client_nonce,
            &server_nonce,
        ))
    }
}

pub(crate) fn signed_ws_url(address: SocketAddr, device: &TestDevice, extra: &str) -> String {
    let path = format!("/v2/ws?{extra}");
    format!("ws://{address}{path}&{}", device.sign_query(&path))
}

async fn expect_crypto_close(socket: &mut ClientSocket) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    assert_eq!(u16::from(frame.code), WS_CLOSE_CODE);
                    assert_eq!(frame.reason.as_str(), WS_CLOSE_REASON);
                    return;
                }
                Some(Ok(_)) => continue,
                other => panic!("expected a 4400 close, got {other:?}"),
            }
        }
    })
    .await
    .unwrap();
}

async fn ping(socket: &mut ClientSocket, channel: &mut SecureChannel, id: &str) {
    let ping = json!({"id": id, "type": "server.ping", "payload": {}}).to_string();
    socket
        .send(Message::Binary(
            channel.sealer.seal_text(&ping).unwrap().into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Binary(frame) => {
                    let message: Value =
                        serde_json::from_str(&channel.opener.open_frame(&frame).unwrap()).unwrap();
                    if message["id"] == id {
                        assert_eq!(message["payload"]["pong"], true);
                        return;
                    }
                }
                Message::Text(text) => panic!("plaintext frame after hello: {text}"),
                _ => continue,
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ws_v2_round_trips_over_a_real_socket_and_closes_4400_on_tampering() {
    for required in [PairingEncryption::X25519, PairingEncryption::MlKem768] {
        let server = TestServer::start(required).await;
        let keys = server.state.pairing_keys.current().unwrap();
        let protocol = EncryptionProtocol::parse(required.as_str()).unwrap();
        let device = TestDevice::new(31);
        device.enroll(&server.root.join("data"));

        let offer = ws_offer(&keys, protocol);
        let (mut socket, _) =
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &offer.query))
                .await
                .unwrap();
        let mut channel = offer
            .accept_hello(&keys, &device.device_id, &mut socket)
            .await;
        ping(&mut socket, &mut channel, "v2-1").await;
        ping(&mut socket, &mut channel, "v2-2").await;
        // A flipped bit in the next frame is fatal.
        let mut frame = channel
            .sealer
            .seal_text(r#"{"id":"x","type":"server.ping"}"#)
            .unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 1;
        socket.send(Message::Binary(frame.into())).await.unwrap();
        expect_crypto_close(&mut socket).await;

        // A device id other than the signed one derives other keys.
        let offer = ws_offer(&keys, protocol);
        let (mut socket, _) =
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &offer.query))
                .await
                .unwrap();
        let mut wrong = offer.accept_hello(&keys, "dev_other", &mut socket).await;
        let ping = wrong
            .sealer
            .seal_text(r#"{"id":"y","type":"server.ping"}"#)
            .unwrap();
        socket.send(Message::Binary(ping.into())).await.unwrap();
        expect_crypto_close(&mut socket).await;

        // Text after the hello is fatal too.
        let offer = ws_offer(&keys, protocol);
        let (mut socket, _) =
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &offer.query))
                .await
                .unwrap();
        offer
            .accept_hello(&keys, &device.device_id, &mut socket)
            .await;
        socket
            .send(Message::Text(r#"{"id":"z","type":"server.ping"}"#.into()))
            .await
            .unwrap();
        expect_crypto_close(&mut socket).await;

        // Malformed handshake material: upgrade, then 4400 without a hello.
        let bad = offer.query.replace(
            &format!("client_nonce={}", encode_b64(&offer.client_nonce)),
            "client_nonce=AAAA",
        );
        let (mut socket, _) =
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &bad))
                .await
                .unwrap();
        expect_crypto_close(&mut socket).await;

        // Unknown transport version: refused before the upgrade.
        let future = offer.query.replace("tv=2", "tv=3");
        assert!(matches!(
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &future)).await,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == 426
        ));
        // A protocol other than the required one is refused.
        let other = if protocol == EncryptionProtocol::X25519 {
            EncryptionProtocol::MlKem768
        } else {
            EncryptionProtocol::X25519
        };
        let offer = ws_offer(&keys, other);
        assert!(matches!(
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &offer.query)).await,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == 403
        ));
    }
}

/// Client side of one sealed request.
pub(crate) struct SealedRequest {
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    down: RestKeys,
}

pub(crate) fn encode_inner(head: &Value, body: &[u8]) -> Vec<u8> {
    let head = serde_json::to_vec(head).unwrap();
    let mut out = (head.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&head);
    out.extend_from_slice(body);
    out
}

/// The client's REST keys for one request.
fn client_rest_keys(
    keys: &PairingKeys,
    protocol: EncryptionProtocol,
    material: &[u8],
    shared: &[u8],
    client_nonce: &[u8],
) -> RestKeys {
    derive_rest_keys(&KeyScheduleInput {
        label: REST_LABEL,
        protocol,
        device_id: "",
        server_static_public: keys.static_public(protocol),
        client_material: material,
        client_nonce,
        server_nonce: &[],
        shared,
    })
    .unwrap()
}

pub(crate) fn seal_request(
    keys: &PairingKeys,
    protocol: EncryptionProtocol,
    plaintext: &[u8],
) -> SealedRequest {
    let (material, shared) = client_handshake(keys, protocol);
    let nonce = random_nonce();
    let session = client_rest_keys(keys, protocol, &material, &shared, &nonce);
    let mut up = RecordCipher::new(&session.k_up, &session.th, DIRECTION_UP);
    let material_header = match protocol {
        EncryptionProtocol::X25519 => "x-todex-client-key",
        EncryptionProtocol::MlKem768 => "x-todex-kem-ciphertext",
    };
    SealedRequest {
        headers: vec![
            ("content-type", SEALED_CONTENT_TYPE.to_owned()),
            ("x-todex-transport", "2".to_owned()),
            ("x-todex-encryption", protocol.as_str().to_owned()),
            (material_header, encode_b64(&material)),
            ("x-todex-request-nonce", encode_b64(&nonce)),
            ("x-todex-sealed-revision", "2".to_owned()),
        ],
        body: seal_record_stream(&mut up, plaintext).unwrap(),
        down: session,
    }
}

/// Signed inner request plaintext, as a client would build it.
pub(crate) fn signed_inner(device: &TestDevice, method: &str, uri: &str, body: &[u8]) -> Vec<u8> {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let mut headers = serde_json::Map::new();
    headers.insert("content-type".into(), json!("application/json"));
    headers.insert("accept".into(), json!("application/json"));
    for (name, value) in device.sign(method, uri, body) {
        headers.insert(name.as_str().to_owned(), json!(value));
    }
    let mut head = json!({"method": method, "path": path, "headers": headers});
    if !query.is_empty() {
        head["query"] = json!(query);
    }
    encode_inner(&head, body)
}

pub(crate) struct OpenedResponse {
    pub status: u16,
    pub headers: Value,
    pub body: Vec<u8>,
    pub response_nonce: Vec<u8>,
}

/// The response cipher once the response nonce in front of the records is
/// known; `prk` is gone afterwards.
fn response_cipher(keys: RestKeys, nonce: &[u8]) -> RecordCipher {
    let nonce: [u8; RESPONSE_NONCE_LENGTH] = nonce.try_into().unwrap();
    let (th, k_down) = keys.into_k_down(&nonce).unwrap();
    RecordCipher::new(&k_down, &th, DIRECTION_DOWN)
}

pub(crate) fn open_response(down: RestKeys, stream: &[u8]) -> OpenedResponse {
    let (nonce, records) = stream.split_at(RESPONSE_NONCE_LENGTH);
    let mut decoder = RecordStreamDecoder::new(response_cipher(down, nonce));
    let mut plaintext = Vec::new();
    decoder.push(records, &mut plaintext).unwrap();
    decoder.finish().unwrap();
    let length = u32::from_be_bytes(plaintext[..4].try_into().unwrap()) as usize;
    let head: Value = serde_json::from_slice(&plaintext[4..4 + length]).unwrap();
    OpenedResponse {
        status: head["status"].as_u64().unwrap() as u16,
        headers: head["headers"].clone(),
        body: plaintext[4 + length..].to_vec(),
        response_nonce: nonce.to_vec(),
    }
}

impl SealedRequest {
    pub(crate) async fn send(self, address: SocketAddr) -> (reqwest::Response, RestKeys) {
        let mut request = reqwest::Client::new()
            .post(format!("http://{address}/v2/sealed"))
            .body(self.body);
        for (name, value) in &self.headers {
            request = request.header(*name, value);
        }
        (request.send().await.unwrap(), self.down)
    }

    pub(crate) async fn send_and_open(self, address: SocketAddr) -> OpenedResponse {
        let (response, down) = self.send(address).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            SEALED_RESPONSE_CONTENT_TYPE
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        open_response(down, &response.bytes().await.unwrap())
    }
}

async fn expect_outer_failure(response: reqwest::Response) {
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"code": "TRANSPORT_CRYPTO_FAILED", "message": "transport crypto failure"})
    );
}

#[tokio::test]
async fn sealed_tunnel_runs_signed_inner_requests_through_the_router() {
    for required in [PairingEncryption::X25519, PairingEncryption::MlKem768] {
        let server = TestServer::start(required).await;
        let keys = server.state.pairing_keys.current().unwrap();
        let protocol = EncryptionProtocol::parse(required.as_str()).unwrap();
        let device = TestDevice::new(32);
        device.enroll(&server.root.join("data"));

        // Signed GET with a query.
        let inner = signed_inner(&device, "GET", "/v2/workspaces?source=sealed", &[]);
        let opened = seal_request(&keys, protocol, &inner)
            .send_and_open(server.address)
            .await;
        assert_eq!(opened.status, 200);
        assert!(opened.headers["content-type"]
            .as_str()
            .unwrap()
            .starts_with("application/json"));
        let value: Value = serde_json::from_slice(&opened.body).unwrap();
        assert!(value["workspaces"].is_array());

        // Signed PUT with a body; the signature covers the inner body.
        let body = br#"{"workspaces":[]}"#;
        let inner = signed_inner(&device, "PUT", "/v2/workspaces", body);
        let opened = seal_request(&keys, protocol, &inner)
            .send_and_open(server.address)
            .await;
        assert_eq!(
            opened.status,
            200,
            "{}",
            String::from_utf8_lossy(&opened.body)
        );
        let mut tampered = signed_inner(&device, "PUT", "/v2/workspaces", body);
        let last = tampered.len() - 2;
        tampered[last] = b' ';
        let opened = seal_request(&keys, protocol, &tampered)
            .send_and_open(server.address)
            .await;
        assert_eq!(opened.status, 401);

        // Unsigned inner request: the inner device auth answers 401.
        let inner = encode_inner(
            &json!({"method": "GET", "path": "/v2/workspaces", "headers": {}}),
            &[],
        );
        let opened = seal_request(&keys, protocol, &inner)
            .send_and_open(server.address)
            .await;
        assert_eq!(opened.status, 401);
        let error: Value = serde_json::from_slice(&opened.body).unwrap();
        assert_eq!(error["code"], "UNAUTHENTICATED");

        // Outer failures: plain 400 without detail.
        let mut sealed = seal_request(
            &keys,
            protocol,
            &signed_inner(&device, "GET", "/v2/workspaces", &[]),
        );
        let last = sealed.body.len() - 1;
        sealed.body[last] ^= 1;
        expect_outer_failure(sealed.send(server.address).await.0).await;
        let mut sealed = seal_request(
            &keys,
            protocol,
            &signed_inner(&device, "GET", "/v2/workspaces", &[]),
        );
        sealed.body.truncate(sealed.body.len() - 3);
        expect_outer_failure(sealed.send(server.address).await.0).await;
        let mut sealed = seal_request(
            &keys,
            protocol,
            &signed_inner(&device, "GET", "/v2/workspaces", &[]),
        );
        sealed.headers[0].1 = "application/json".to_owned();
        expect_outer_failure(sealed.send(server.address).await.0).await;
        let mut sealed = seal_request(
            &keys,
            protocol,
            &signed_inner(&device, "GET", "/v2/workspaces", &[]),
        );
        sealed.headers[4].1 = "AAAA".to_owned();
        expect_outer_failure(sealed.send(server.address).await.0).await;
        // A replayed request body under fresh outer material fails too.
        let first = seal_request(
            &keys,
            protocol,
            &signed_inner(&device, "GET", "/v2/workspaces", &[]),
        );
        let mut second = seal_request(&keys, protocol, b"anything");
        second.body = first.body;
        expect_outer_failure(second.send(server.address).await.0).await;

        // Nesting is refused.
        let inner = encode_inner(
            &json!({"method": "POST", "path": "/v2/sealed", "headers": {}}),
            &[],
        );
        expect_outer_failure(
            seal_request(&keys, protocol, &inner)
                .send(server.address)
                .await
                .0,
        )
        .await;
    }
}

/// The tunnel against a probe router: header allow-list, the server-only
/// extension, the peer address and streaming bodies.
async fn probe(request: Request) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.to_string());
    let via_v2 = request
        .extensions()
        .get::<ArrivedViaTransportV2>()
        .is_some();
    let headers: serde_json::Map<String, Value> = request
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), json!(value.to_str().unwrap())))
        .collect();
    let uri = request.uri().to_string();
    let body = to_bytes(request.into_body(), 1024).await.unwrap();
    Response::builder()
        .header("content-type", "application/json")
        .header("x-probe", "1")
        .body(Body::from(
            json!({"peer": peer, "viaV2": via_v2, "headers": headers, "uri": uri,
                   "body": String::from_utf8_lossy(&body)})
            .to_string(),
        ))
        .unwrap()
}

async fn chunks(fail: bool) -> Response {
    let pieces: Vec<Result<Bytes, std::io::Error>> = (0..3_u8)
        .map(|index| Ok(Bytes::from(vec![index; 50_000])))
        .chain(fail.then(|| Err(std::io::Error::other("remote failed"))))
        .collect();
    Response::builder()
        .header("content-type", "application/octet-stream")
        .header("content-length", if fail { "200000" } else { "150000" })
        .body(Body::from_stream(futures_util::stream::iter(pieces)))
        .unwrap()
}

#[tokio::test]
async fn sealed_tunnel_filters_headers_marks_requests_and_streams_bodies() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let keys = server.state.pairing_keys.current().unwrap();
    let api = Router::new()
        .route("/probe", any(probe))
        .route("/download", get(|| chunks(false)))
        .route("/broken", get(|| chunks(true)));
    let app = super::sealed::routes(api, server.state.clone());
    // The probe routes have no device auth, but the tunnel still wants a
    // credential from a registered device before it reads an inner body.
    let device = TestDevice::new(35);
    device.enroll(&server.root.join("data"));
    let credential = |method: &str, uri: &str| -> serde_json::Map<String, Value> {
        device
            .sign(method, uri, &[])
            .into_iter()
            .map(|(name, value)| (name.as_str().to_owned(), json!(value)))
            .collect()
    };
    let peer: SocketAddr = "192.0.2.7:4242".parse().unwrap();
    let call = |sealed: SealedRequest| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v2/sealed")
            .header(header::HOST, "example.test:7345")
            .header("x-outer-only", "1")
            .extension(ConnectInfo(peer));
        for (name, value) in &sealed.headers {
            request = request.header(*name, value);
        }
        let request = request.body(Body::from(sealed.body)).unwrap();
        let app = app.clone();
        async move { (app.oneshot(request).await.unwrap(), sealed.down) }
    };

    let mut probe_headers = credential("POST", "/probe?a=1&b=2");
    for (name, value) in [
        ("content-type", "text/plain"),
        ("Accept", "application/json"),
        ("x-todex-transport", "2"),
        ("x-todex-verified-device", "dev_forged"),
        ("cookie", "secret=1"),
        ("connection", "close"),
        ("host", "inner.test"),
    ] {
        probe_headers.insert(name.to_owned(), json!(value));
    }
    let inner = encode_inner(
        &json!({"method": "POST", "path": "/probe", "query": "a=1&b=2",
                "headers": probe_headers}),
        b"hello",
    );
    let (response, down) = call(seal_request(&keys, EncryptionProtocol::X25519, &inner)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let opened = open_response(down, &bytes);
    assert_eq!(opened.status, 200);
    assert_eq!(opened.headers["x-probe"], "1");
    let seen: Value = serde_json::from_slice(&opened.body).unwrap();
    assert_eq!(seen["peer"], peer.to_string());
    assert_eq!(seen["viaV2"], true);
    assert_eq!(seen["uri"], "/probe?a=1&b=2");
    assert_eq!(seen["body"], "hello");
    let mut names: Vec<&str> = seen["headers"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "accept",
            "content-type",
            "host",
            "x-todex-auth-nonce",
            "x-todex-auth-sig",
            "x-todex-auth-ts",
            "x-todex-device-id"
        ]
    );
    assert_eq!(
        seen["headers"]["host"], "example.test:7345",
        "Host comes from the outer request"
    );

    // Streaming download: several records, the inner content length kept.
    let inner = encode_inner(
        &json!({"method": "GET", "path": "/download",
                "headers": credential("GET", "/download")}),
        &[],
    );
    let (response, down) = call(seal_request(&keys, EncryptionProtocol::X25519, &inner)).await;
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let opened = open_response(down, &bytes);
    assert_eq!(opened.headers["content-length"], "150000");
    assert_eq!(opened.body.len(), 150_000);
    assert!(opened.body[..50_000].iter().all(|byte| *byte == 0));
    assert!(opened.body[100_000..].iter().all(|byte| *byte == 2));
    // Records follow the producer's chunks: more than one record.
    assert!(bytes.len() > RESPONSE_NONCE_LENGTH + 150_000 + 2 * 20);

    // A failing inner body never gets a final record.
    let inner = encode_inner(
        &json!({"method": "GET", "path": "/broken",
                "headers": credential("GET", "/broken")}),
        &[],
    );
    let (response, down) = call(seal_request(&keys, EncryptionProtocol::X25519, &inner)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    // The response nonce arrives first, on its own.
    let nonce = body.next().await.unwrap().unwrap();
    let mut decoder = RecordStreamDecoder::new(response_cipher(down, &nonce));
    let mut plaintext = Vec::new();
    let mut failed = false;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) => decoder.push(&chunk, &mut plaintext).unwrap(),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert!(decoder.finish().is_err());

    // Outer requests for an unrouted inner path answer the inner 404.
    let inner = encode_inner(
        &json!({"method": "GET", "path": "/missing",
                "headers": credential("GET", "/missing")}),
        &[],
    );
    let (response, down) = call(seal_request(&keys, EncryptionProtocol::X25519, &inner)).await;
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(open_response(down, &bytes).status, 404);
}

async fn oneshot_status(app: &Router, request: Request) -> (StatusCode, Bytes) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 1 << 20).await.unwrap(),
    )
}

#[tokio::test]
async fn non_loopback_peers_only_reach_bootstrap_routes_and_transport_v2() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let keys = server.state.pairing_keys.current().unwrap();
    let device = TestDevice::new(33);
    device.enroll(&server.root.join("data"));
    let app = super::router(server.state.clone());
    let remote: SocketAddr = "192.0.2.9:5555".parse().unwrap();
    let loopback: SocketAddr = "127.0.0.1:5555".parse().unwrap();
    let mapped: SocketAddr = "[::ffff:127.0.0.1]:5555".parse().unwrap();
    let signed_get = |peer: Option<SocketAddr>, uri: &str| {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in device.sign("GET", uri, &[]) {
            builder = builder.header(name, value);
        }
        if let Some(peer) = peer {
            builder = builder.extension(ConnectInfo(peer));
        }
        builder.body(Body::empty()).unwrap()
    };

    // Direct REST: loopback (also IPv4-mapped) passes, remote or unknown
    // peers get 426.
    for peer in [loopback, mapped] {
        assert_eq!(
            oneshot_status(&app, signed_get(Some(peer), "/v2/workspaces"))
                .await
                .0,
            StatusCode::OK
        );
    }
    for peer in [Some(remote), None] {
        let (status, body) = oneshot_status(&app, signed_get(peer, "/v2/workspaces")).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "PROTOCOL_UPGRADE_REQUIRED");
    }
    // A plain WebSocket upgrade, or a v1 one, from a remote peer.
    for uri in ["/v2/ws", "/v2/ws?enc=x25519"] {
        assert_eq!(
            oneshot_status(&app, signed_get(Some(remote), uri)).await.0,
            StatusCode::UPGRADE_REQUIRED
        );
    }
    // Bootstrap routes stay direct.
    for uri in ["/health", "/v2/transport-policy", "/v2/version"] {
        let request = Request::builder()
            .uri(uri)
            .extension(ConnectInfo(remote))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            oneshot_status(&app, request).await.0,
            StatusCode::OK,
            "{uri}"
        );
    }
    let create = Request::builder()
        .method("POST")
        .uri("/v2/device-pairing/create")
        .header("content-type", "application/json")
        .extension(ConnectInfo(remote))
        .body(Body::from(
            json!({
                "clientCommitment": encode_b64(&[1; 32]),
                "transportBinding": 1,
                "deviceNameBinding": 1,
                "deviceName": "remote",
                "devicePublicKey": device.public_key_b64(),
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(oneshot_status(&app, create).await.0, StatusCode::OK);
    // A tv=2 upgrade passes the filter (this one-shot request cannot
    // actually upgrade, so the WebSocket extractor rejects it instead).
    let tv2 = signed_get(Some(remote), "/v2/ws?tv=2&enc=x25519");
    assert_ne!(
        oneshot_status(&app, tv2).await.0,
        StatusCode::UPGRADE_REQUIRED
    );

    // The tunnel works for the remote peer; its inner request is marked as
    // arriving through v2 and reaches the authenticated route.
    let inner = signed_inner(&device, "GET", "/v2/workspaces", &[]);
    let sealed = seal_request(&keys, EncryptionProtocol::X25519, &inner);
    let mut request = Request::builder()
        .method("POST")
        .uri("/v2/sealed")
        .extension(ConnectInfo(remote));
    for (name, value) in &sealed.headers {
        request = request.header(*name, value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(sealed.body)).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(open_response(sealed.down, &bytes).status, 200);
}

#[tokio::test]
async fn websocket_without_tv_is_plaintext_only_and_transport_v1_is_retired() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let device = TestDevice::new(34);
    device.enroll(&server.root.join("data"));
    let keys = server.state.pairing_keys.current().unwrap();
    let offer = ws_offer(&keys, EncryptionProtocol::X25519);
    let v1 = offer.query.replace("tv=2&", "");
    let status = |result: Result<_, tokio_tungstenite::tungstenite::Error>| match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(other) => panic!("{other}"),
    };
    assert_eq!(
        status(tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &v1)).await),
        426
    );
    // Loopback plaintext keeps today's rule: refused while the server
    // requires encryption.
    assert_eq!(
        status(
            tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, "enc=none"))
                .await
        ),
        403
    );

    let plain = TestServer::start(PairingEncryption::None).await;
    device.enroll(&plain.root.join("data"));
    let (mut socket, _) =
        tokio_tungstenite::connect_async(signed_ws_url(plain.address, &device, "enc=none"))
            .await
            .unwrap();
    socket
        .send(Message::Text(
            json!({"id": "plain", "type": "server.ping", "payload": {}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let message: Value = serde_json::from_str(&text).unwrap();
                if message["id"] == "plain" {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
}

/// The tunnel checks the inner credential as soon as the inner head is
/// decrypted, so an unsigned peer cannot make it buffer a large inner body.
#[tokio::test]
async fn sealed_tunnel_checks_the_credential_before_reading_the_inner_body() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let keys = server.state.pairing_keys.current().unwrap();
    let device = TestDevice::new(36);
    device.enroll(&server.root.join("data"));
    let app = super::router(server.state.clone());
    let peer: SocketAddr = "192.0.2.10:6000".parse().unwrap();
    // Only the first record (the inner head plus the start of the body)
    // arrives; reading any further fails, which the tunnel reports as an
    // outer 400. A sealed inner answer proves the rest was never polled.
    let first_record_then_failure = |sealed: &SealedRequest| {
        let length = u32::from_be_bytes(sealed.body[..4].try_into().unwrap()) as usize;
        assert!(4 + length < sealed.body.len(), "needs more than one record");
        let first = Bytes::copy_from_slice(&sealed.body[..4 + length]);
        Body::from_stream(futures_util::stream::iter([
            Ok(first),
            Err(std::io::Error::other(
                "inner body must not be read before the credential check",
            )),
        ]))
    };
    let send = |sealed: &SealedRequest, body: Body| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v2/sealed")
            .extension(ConnectInfo(peer));
        for (name, value) in &sealed.headers {
            request = request.header(*name, value);
        }
        app.clone().oneshot(request.body(body).unwrap())
    };
    let large = vec![b'x'; 200_000];

    // Unsigned, and signed by an unknown device: inner 401, body unread.
    let stranger = TestDevice::new(37);
    let mut stranger_headers = serde_json::Map::new();
    for (name, value) in stranger.sign("PUT", "/v2/workspaces", &large) {
        stranger_headers.insert(name.as_str().to_owned(), json!(value));
    }
    for headers in [json!({}), Value::Object(stranger_headers)] {
        let inner = encode_inner(
            &json!({"method": "PUT", "path": "/v2/workspaces", "headers": headers}),
            &large,
        );
        let sealed = seal_request(&keys, EncryptionProtocol::X25519, &inner);
        let body = first_record_then_failure(&sealed);
        let response = send(&sealed, body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let opened = open_response(sealed.down, &bytes);
        assert_eq!(opened.status, 401);
        let error: Value = serde_json::from_slice(&opened.body).unwrap();
        assert_eq!(error["code"], "UNAUTHENTICATED");
    }

    // A registered device passes the head check, so the body is read (and
    // here fails to read).
    let inner = signed_inner(&device, "PUT", "/v2/workspaces", &large);
    let sealed = seal_request(&keys, EncryptionProtocol::X25519, &inner);
    let body = first_record_then_failure(&sealed);
    let response = send(&sealed, body).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Public routes need no credential but take at most 64 KiB of body.
    let inner = encode_inner(
        &json!({"method": "POST", "path": "/v2/device-pairing/create", "headers": {}}),
        &large,
    );
    let sealed = seal_request(&keys, EncryptionProtocol::X25519, &inner);
    let response = send(&sealed, Body::from(sealed.body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let inner = encode_inner(
        &json!({"method": "GET", "path": "/v2/version", "headers": {}}),
        &[],
    );
    let sealed = seal_request(&keys, EncryptionProtocol::X25519, &inner);
    let response = send(&sealed, Body::from(sealed.body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(open_response(sealed.down, &bytes).status, 200);
}

/// A TUI reset rewrites `pairing_keys.json` while the daemon runs: the next
/// pairing delivers the new key and the handshake (WS and REST) switches to it
/// in the same step, without a restart.
#[tokio::test]
async fn key_reset_switches_pairing_and_handshake_together() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let data_dir = server.root.join("data");
    let device = TestDevice::new(36);
    device.enroll(&data_dir);
    let protocol = EncryptionProtocol::X25519;
    let created_key = |peer: u8| {
        let address = server.address;
        let device_public_key = TestDevice::new(peer).public_key_b64();
        async move {
            let response: Value = reqwest::Client::new()
                .post(format!("http://{address}/v2/device-pairing/create"))
                .json(&json!({
                    "clientCommitment": encode_b64(&[peer; 32]),
                    "transportBinding": 1,
                "deviceNameBinding": 1,
                    "deviceName": "reset test",
                    "devicePublicKey": device_public_key,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["transportProtocol"], "x25519");
            response["transportPublicKey"].as_str().unwrap().to_owned()
        }
    };

    let old = server.state.pairing_keys.current().unwrap();
    assert_eq!(
        created_key(41).await,
        encode_b64(old.static_public(protocol))
    );

    let reset = PairingKeys::reset(&data_dir).await.unwrap();
    assert_ne!(reset.static_public(protocol), old.static_public(protocol));
    assert_eq!(
        created_key(42).await,
        encode_b64(reset.static_public(protocol))
    );

    // The handshake now answers only for the new key.
    let offer = ws_offer(&reset, protocol);
    let (mut socket, _) =
        tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &offer.query))
            .await
            .unwrap();
    let mut channel = offer
        .accept_hello(&reset, &device.device_id, &mut socket)
        .await;
    ping(&mut socket, &mut channel, "after-reset").await;

    let offer = ws_offer(&old, protocol);
    let (mut socket, _) =
        tokio_tungstenite::connect_async(signed_ws_url(server.address, &device, &offer.query))
            .await
            .unwrap();
    let mut stale = offer
        .accept_hello(&old, &device.device_id, &mut socket)
        .await;
    let frame = stale
        .sealer
        .seal_text(r#"{"id":"stale","type":"server.ping"}"#)
        .unwrap();
    socket.send(Message::Binary(frame.into())).await.unwrap();
    expect_crypto_close(&mut socket).await;

    let inner = signed_inner(&device, "GET", "/v2/workspaces", &[]);
    assert_eq!(
        seal_request(&reset, protocol, &inner)
            .send_and_open(server.address)
            .await
            .status,
        200
    );
}

/// Sealed revision 2: the outer request must say so, every response starts
/// with a fresh response nonce, and a replayed request gets a new key.
#[tokio::test]
async fn sealed_revision_2_requires_the_header_and_freshens_every_response_key() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let keys = server.state.pairing_keys.current().unwrap();
    let protocol = EncryptionProtocol::X25519;
    let app = super::router(server.state.clone());
    let peer: SocketAddr = "192.0.2.20:7000".parse().unwrap();
    let build = |headers: &[(&'static str, String)], body: Body| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v2/sealed")
            .extension(ConnectInfo(peer));
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        request.body(body).unwrap()
    };
    // Without the revision header (or with another revision) the server
    // answers a plain 426 before it reads the body: this body would fail.
    let version = encode_inner(
        &json!({"method": "GET", "path": "/v2/version", "headers": {}}),
        &[],
    );
    for revision in [None, Some("1"), Some("3")] {
        let sealed = seal_request(&keys, protocol, &version);
        let mut headers = sealed.headers.clone();
        headers.retain(|(name, _)| *name != "x-todex-sealed-revision");
        if let Some(revision) = revision {
            headers.push(("x-todex-sealed-revision", revision.to_owned()));
        }
        let unread = Body::from_stream(futures_util::stream::iter([Err::<Bytes, _>(
            std::io::Error::other("the body must not be read"),
        )]));
        let (status, body) = oneshot_status(&app, build(&headers, unread)).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "{revision:?}");
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "PROTOCOL_UPGRADE_REQUIRED");
    }

    // The same captured request twice: both answer, under different nonces.
    let sealed = seal_request(&keys, protocol, &version);
    let replay_headers = sealed.headers.clone();
    let replay_body = sealed.body.clone();
    let mut nonces = Vec::new();
    let mut down = Some(sealed.down);
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(build(&replay_headers, Body::from(replay_body.clone())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            SEALED_RESPONSE_CONTENT_TYPE
        );
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        nonces.push(bytes[..RESPONSE_NONCE_LENGTH].to_vec());
        if let Some(keys) = down.take() {
            let opened = open_response(keys, &bytes);
            assert_eq!(opened.status, 200);
            assert_eq!(opened.response_nonce, nonces[0]);
        }
    }
    assert_ne!(nonces[0], nonces[1]);

    // A response sealed under the old `k_down` (label || "/down", no nonce)
    // does not open with the revision 2 key, and vice versa.
    let (material, shared) = client_handshake(&keys, protocol);
    let client_nonce = random_nonce();
    let legacy = client_keys(
        &keys,
        protocol,
        REST_LABEL,
        "",
        &material,
        &shared,
        &client_nonce,
        &[],
    );
    let rest = client_rest_keys(&keys, protocol, &material, &shared, &client_nonce);
    assert_eq!(legacy.k_up.as_slice(), rest.k_up.as_slice());
    let mut legacy_down = RecordCipher::new(&legacy.k_down, &legacy.th, DIRECTION_DOWN);
    let stream = seal_record_stream(&mut legacy_down, b"\0\0\0\x02{}").unwrap();
    let nonce = [7_u8; RESPONSE_NONCE_LENGTH];
    let mut decoder = RecordStreamDecoder::new(response_cipher(rest, &nonce));
    assert!(decoder.push(&stream, &mut Vec::new()).is_err());

    // The policy advertises the revision.
    let request = Request::builder()
        .uri("/v2/transport-policy")
        .extension(ConnectInfo(peer))
        .body(Body::empty())
        .unwrap();
    let (status, body) = oneshot_status(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    let policy: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(policy["sealedRevision"], 2);
    assert_eq!(policy["transportVersion"], 2);
}

/// Body that delivers `first` and then never ends.
fn stalled_body(first: &[u8]) -> Body {
    Body::from_stream(
        futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::copy_from_slice(first))])
            .chain(futures_util::stream::pending()),
    )
}

/// The first wire record of a sealed request (it carries the inner head).
fn first_record(sealed: &SealedRequest) -> Vec<u8> {
    let length = u32::from_be_bytes(sealed.body[..4].try_into().unwrap()) as usize;
    assert!(4 + length < sealed.body.len(), "needs more than one record");
    sealed.body[..4 + length].to_vec()
}

#[tokio::test]
async fn tunnel_admission_bounds_time_and_keeps_public_and_signed_pools_apart() {
    let server = TestServer::start(PairingEncryption::X25519).await;
    let keys = server.state.pairing_keys.current().unwrap();
    let protocol = EncryptionProtocol::X25519;
    let device = TestDevice::new(38);
    device.enroll(&server.root.join("data"));
    let api = Router::new()
        .route("/probe", any(probe))
        .route("/v2/version", get(|| async { "version" }));
    let limits = super::sealed::TunnelLimits {
        head_deadline: Duration::from_millis(400),
        // Longer than the permit wait, so a stalled holder keeps its slot
        // until the waiter gives up.
        idle_timeout: Duration::from_millis(800),
        permit_wait: Duration::from_millis(200),
    };
    let app = super::sealed::routes_with_limits(api, server.state.clone(), limits, 1);
    let peer: SocketAddr = "192.0.2.21:7001".parse().unwrap();
    let send = |sealed: &SealedRequest, body: Body| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v2/sealed")
            .extension(ConnectInfo(peer));
        for (name, value) in &sealed.headers {
            request = request.header(*name, value);
        }
        app.clone().oneshot(request.body(body).unwrap())
    };
    let open = |response: Response, down: RestKeys| async move {
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        open_response(down, &bytes)
    };
    let large = vec![b'x'; 100_000];

    // Nothing arrives: the inner head is never decrypted, a plain 400.
    let sealed = seal_request(
        &keys,
        protocol,
        &signed_inner(&device, "PUT", "/probe", &large),
    );
    let started = std::time::Instant::now();
    let response = send(&sealed, stalled_body(&[])).await.unwrap();
    expect_outer_failure_response(response).await;
    assert!(started.elapsed() < Duration::from_secs(5));

    // A slow drip that keeps the connection busy but never completes the
    // head still hits the head deadline.
    let sealed = seal_request(
        &keys,
        protocol,
        &signed_inner(&device, "PUT", "/probe", &large),
    );
    let drip = futures_util::stream::iter(sealed.body.clone().into_iter().take(64)).then(
        |byte| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<_, std::io::Error>(Bytes::from(vec![byte]))
        },
    );
    let response = send(&sealed, Body::from_stream(drip)).await.unwrap();
    expect_outer_failure_response(response).await;

    // The head arrives, then the body stalls: a sealed 408.
    let sealed = seal_request(
        &keys,
        protocol,
        &signed_inner(&device, "PUT", "/probe", &large),
    );
    let response = send(&sealed, stalled_body(&first_record(&sealed)))
        .await
        .unwrap();
    let opened = open(response, sealed.down).await;
    assert_eq!(opened.status, 408);
    let error: Value = serde_json::from_slice(&opened.body).unwrap();
    assert_eq!(error["code"], "REQUEST_TIMEOUT");

    // One signed slot: while it is held, a second signed request waits and
    // then gets a sealed 503 TRANSPORT_BUSY without running.
    let holder = seal_request(
        &keys,
        protocol,
        &signed_inner(&device, "PUT", "/probe", &large),
    );
    let held = tokio::spawn(send(&holder, stalled_body(&first_record(&holder))));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let inner = signed_inner(&device, "POST", "/probe", b"once");
    let busy = seal_request(&keys, protocol, &inner);
    let opened = open(
        send(&busy, Body::from(busy.body.clone())).await.unwrap(),
        busy.down,
    )
    .await;
    assert_eq!(opened.status, 503);
    assert_eq!(opened.headers["retry-after"], "1");
    let error: Value = serde_json::from_slice(&opened.body).unwrap();
    assert_eq!(error["code"], "TRANSPORT_BUSY");

    // Public routes have their own pool: they still answer meanwhile.
    let version = encode_inner(
        &json!({"method": "GET", "path": "/v2/version", "headers": {}}),
        &[],
    );
    let public = seal_request(&keys, protocol, &version);
    let opened = open(
        send(&public, Body::from(public.body.clone()))
            .await
            .unwrap(),
        public.down,
    )
    .await;
    assert_eq!(opened.status, 200);
    assert_eq!(opened.body, b"version");

    // The holder times out (408) and frees the slot; the refused request
    // was never run, so the same signed inner request still works.
    let opened = open(held.await.unwrap().unwrap(), holder.down).await;
    assert_eq!(opened.status, 408);
    let retry = seal_request(&keys, protocol, &inner);
    let opened = open(
        send(&retry, Body::from(retry.body.clone())).await.unwrap(),
        retry.down,
    )
    .await;
    assert_eq!(opened.status, 200);
    let seen: Value = serde_json::from_slice(&opened.body).unwrap();
    assert_eq!(seen["body"], "once");

    // Four stalled public requests fill the public pool; a fifth is busy
    // while a signed request is unaffected.
    let public_large = encode_inner(
        &json!({"method": "POST", "path": "/v2/version", "headers": {}}),
        &vec![b'p'; 65_500],
    );
    let mut stalled = Vec::new();
    for _ in 0..4 {
        let sealed = seal_request(&keys, protocol, &public_large);
        let body = stalled_body(&first_record(&sealed));
        stalled.push(tokio::spawn(send(&sealed, body)));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let fifth = seal_request(&keys, protocol, &version);
    let opened = open(
        send(&fifth, Body::from(fifth.body.clone())).await.unwrap(),
        fifth.down,
    )
    .await;
    assert_eq!(opened.status, 503);
    let signed = seal_request(
        &keys,
        protocol,
        &signed_inner(&device, "GET", "/probe", &[]),
    );
    let opened = open(
        send(&signed, Body::from(signed.body.clone()))
            .await
            .unwrap(),
        signed.down,
    )
    .await;
    assert_eq!(opened.status, 200);
    for task in stalled {
        assert_eq!(task.await.unwrap().unwrap().status(), StatusCode::OK);
    }
}

async fn expect_outer_failure_response(response: Response) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"code": "TRANSPORT_CRYPTO_FAILED", "message": "transport crypto failure"})
    );
}
