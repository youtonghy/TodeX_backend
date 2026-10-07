use crate::{
    app_state::AppState,
    device_pairing::{
        CreateDevicePairingRequest, CreateDevicePairingResponse, DevicePairingPollResponse,
        DevicePairingProofRequest, RevealDevicePairingRequest, RevealDevicePairingResponse,
    },
    error::AppError,
};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, FromRequest, State},
    http::header,
    routing::post,
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::net::SocketAddr;

/// Wraps `Json` so deserialization failures return the API error envelope
/// (400 INVALID_REQUEST) instead of axum's bare 422 text — old clients hitting
/// the v2 pairing schema then surface a readable "missing field" message.
struct ApiJson<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(
        request: axum::extract::Request,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(AppError::InvalidRequest(format!(
                "invalid request body: {rejection}"
            ))),
        }
    }
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/device-pairing/create", post(create))
        .route("/v2/device-pairing/reveal", post(reveal))
        .route("/v2/device-pairing/poll", post(poll))
        .route("/v2/device-pairing/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(2048))
}

async fn create(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(request): ApiJson<CreateDevicePairingRequest>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<CreateDevicePairingResponse>,
    ),
    AppError,
> {
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(state.device_pairing.create(peer.ip(), request)?),
    ))
}
async fn reveal(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(request): ApiJson<RevealDevicePairingRequest>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<RevealDevicePairingResponse>,
    ),
    AppError,
> {
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(state.device_pairing.reveal(peer.ip(), request)?),
    ))
}
async fn poll(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(request): ApiJson<DevicePairingProofRequest>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<DevicePairingPollResponse>,
    ),
    AppError,
> {
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(state.device_pairing.poll(peer.ip(), request)?),
    ))
}
async fn cancel(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(request): ApiJson<DevicePairingProofRequest>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    state.device_pairing.cancel(peer.ip(), request)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"status":"cancelled"})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        Key, XChaCha20Poly1305, XNonce,
    };
    use hkdf::Hkdf;
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;
    use x25519_dalek::{PublicKey, StaticSecret};

    async fn post(app: &Router, route: &str, value: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(route)
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
            ))
            .body(Body::from(value.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        if status.is_success() {
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn device_pairing_http_requires_local_approval_and_enrolls_device() {
        let root = std::env::temp_dir().join(format!(
            "todex-device-pairing-http-{}",
            uuid::Uuid::new_v4()
        ));
        let config = crate::config::Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..crate::config::Config::default()
        };
        let state = AppState::new_for_tests(config.clone()).await.unwrap();
        let handshake_keys = state.pairing_keys.clone();
        let app = super::super::loopback_test_router(state);
        let client = StaticSecret::from([7; 32]);
        let client_public = PublicKey::from(&client);
        let device = crate::device_auth::test_support::TestDevice::new(21);
        let device_public = device.key.verifying_key().to_bytes();
        // Pairing v2 (key in `create`) is retired.
        let (status, legacy) = post(
            &app,
            "/v2/device-pairing/create",
            json!({
                "clientPublicKey": URL_SAFE_NO_PAD.encode(client_public.as_bytes()),
                "deviceName": "HTTP test",
                "devicePublicKey": URL_SAFE_NO_PAD.encode(device_public),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(legacy["code"], "PROTOCOL_UPGRADE_REQUIRED");
        let client_nonce = [3_u8; 32];
        let commit_label = b"todex.device-pairing.v3/commit";
        let commitment = Sha256::digest(
            [
                (commit_label.len() as u32).to_be_bytes().as_slice(),
                commit_label,
                client_public.as_bytes(),
                &client_nonce,
            ]
            .concat(),
        );
        let mut create = json!({
            "clientCommitment": URL_SAFE_NO_PAD.encode(commitment),
            "deviceName": "HTTP test",
            "devicePublicKey": URL_SAFE_NO_PAD.encode(device_public),
        });
        // Clients that do not take the transport key from pairing.
        let (status, unbound) = post(&app, "/v2/device-pairing/create", create.clone()).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(unbound["code"], "PROTOCOL_UPGRADE_REQUIRED");
        create["transportBinding"] = json!(1);
        let (status, created) = post(&app, "/v2/device-pairing/create", create).await;
        assert_eq!(status, StatusCode::OK);
        assert!(created.get("authToken").is_none());
        // The delivered key is the handshake's current static key.
        assert_eq!(created["transportProtocol"], "ml-kem-768");
        let transport_public = URL_SAFE_NO_PAD
            .decode(created["transportPublicKey"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            transport_public,
            handshake_keys
                .current()
                .unwrap()
                .static_public(crate::transport_crypto::EncryptionProtocol::MlKem768)
        );
        assert!(created.get("verificationCode").is_none());
        let id = created["requestId"].as_str().unwrap();
        let server_public: [u8; 32] = URL_SAFE_NO_PAD
            .decode(created["serverPublicKey"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert!(
            crate::device_pairing::list_device_pairing_requests(&config.data_dir)
                .unwrap()
                .is_empty()
        );
        let reveal = json!({
            "requestId": id,
            "clientPublicKey": URL_SAFE_NO_PAD.encode(client_public.as_bytes()),
            "clientNonce": URL_SAFE_NO_PAD.encode(client_nonce),
        });
        let (status, revealed) = post(&app, "/v2/device-pairing/reveal", reveal.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(revealed, json!({"status": "pending"}));
        assert_eq!(
            post(&app, "/v2/device-pairing/reveal", reveal).await.0,
            StatusCode::CONFLICT
        );
        let transcript = [
            b"todex.device-pairing.v3/transcript\0".as_slice(),
            id.as_bytes(),
            &[0],
            client_public.as_bytes(),
            &server_public,
            &[0],
            &device_public,
            &client_nonce,
            &10_u32.to_be_bytes(),
            b"ml-kem-768",
            &(transport_public.len() as u32).to_be_bytes(),
            &transport_public,
        ]
        .concat();
        let listed = crate::device_pairing::list_device_pairing_requests(&config.data_dir).unwrap();
        let digest = Sha256::digest(&transcript);
        let short: String = digest[..5]
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        assert_eq!(
            listed[0].verification_code,
            format!("{}-{}", &short[..5], &short[5..])
        );
        let shared = client.diffie_hellman(&PublicKey::from(server_public));
        let hkdf = Hkdf::<Sha256>::new(Some(&Sha256::digest(&transcript)), shared.as_bytes());
        let mut proof = [0; 32];
        hkdf.expand(b"todex.device-pairing.v3/poll-proof", &mut proof)
            .unwrap();
        let request = json!({"requestId": id, "proof": URL_SAFE_NO_PAD.encode(proof)});
        assert_eq!(
            post(&app, "/v2/device-pairing/approve", request.clone())
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(
                &app,
                "/v2/device-pairing/poll",
                json!({"requestId": id, "proof": URL_SAFE_NO_PAD.encode([0;32])})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post(&app, "/v2/device-pairing/poll", request.clone())
                .await
                .1["status"],
            "pending"
        );
        crate::device_pairing::decide_device_pairing(&config.data_dir, id, true).unwrap();
        let approved = post(&app, "/v2/device-pairing/poll", request.clone())
            .await
            .1;
        assert_eq!(approved["status"], "approved");
        assert!(approved.get("authToken").is_none());
        let mut wrap_key = [0; 32];
        hkdf.expand(b"todex.device-pairing.v3/wrap-key", &mut wrap_key)
            .unwrap();
        let decode = |field: &str| {
            URL_SAFE_NO_PAD
                .decode(approved[field].as_str().unwrap())
                .unwrap()
        };
        let credential = XChaCha20Poly1305::new(Key::from_slice(&wrap_key))
            .decrypt(
                XNonce::from_slice(&decode("nonce")),
                Payload {
                    msg: &decode("ciphertext"),
                    aad: &transcript,
                },
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&credential).unwrap(),
            json!({
                "deviceId": device.device_id,
                "transportProtocol": "ml-kem-768",
                "transportPublicKey": created["transportPublicKey"],
            })
        );
        assert_eq!(
            post(&app, "/v2/device-pairing/poll", request).await.1["status"],
            "expired"
        );

        // Unsigned requests fail; the enrolled device authenticates by
        // signature.
        assert_eq!(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/v2/workspaces")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let signed_headers = device.sign("GET", "/v2/workspaces", &[]);
        let signed_request = || {
            let mut builder = Request::builder().uri("/v2/workspaces");
            for (name, value) in &signed_headers {
                builder = builder.header(name, value);
            }
            builder.body(Body::empty()).unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(signed_request())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        // Replaying the exact same credential must fail on the nonce.
        assert_eq!(
            app.clone()
                .oneshot(signed_request())
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
