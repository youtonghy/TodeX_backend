use crate::{
    app_state::AppState,
    device_pairing::{
        CreateDevicePairingRequest, CreateDevicePairingResponse, DevicePairingPollResponse,
        DevicePairingProofRequest,
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
        let state = AppState::new(config.clone()).await.unwrap();
        let app = super::super::router(state);
        let client = StaticSecret::from([7; 32]);
        let client_public = PublicKey::from(&client);
        let device = crate::device_auth::test_support::TestDevice::new(21);
        let device_public = device.key.verifying_key().to_bytes();
        let (status, created) = post(
            &app,
            "/v2/device-pairing/create",
            json!({
                "clientPublicKey": URL_SAFE_NO_PAD.encode(client_public.as_bytes()),
                "deviceName": "HTTP test",
                "devicePublicKey": URL_SAFE_NO_PAD.encode(device_public),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(created.get("authToken").is_none());
        assert!(created.get("verificationCode").is_none());
        let id = created["requestId"].as_str().unwrap();
        let server_public: [u8; 32] = URL_SAFE_NO_PAD
            .decode(created["serverPublicKey"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let transcript = [
            b"todex.device-pairing.v2/transcript\0".as_slice(),
            id.as_bytes(),
            &[0],
            client_public.as_bytes(),
            &server_public,
            &[0],
            &device_public,
        ]
        .concat();
        let shared = client.diffie_hellman(&PublicKey::from(server_public));
        let hkdf = Hkdf::<Sha256>::new(Some(&Sha256::digest(&transcript)), shared.as_bytes());
        let mut proof = [0; 32];
        hkdf.expand(b"todex.device-pairing.v2/poll-proof", &mut proof)
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
        assert!(approved["ciphertext"].is_string());
        assert!(approved.get("authToken").is_none());
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
