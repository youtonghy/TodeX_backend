//! Frame codecs for `/v2/ws`. Both transports implement the same pair of
//! traits, so the read/write loops and the business dispatcher never branch
//! on encryption:
//!
//! - plaintext: JSON text frames (loopback peers);
//! - transport v2 (`docs/transport-v2.md`): a text hello from the server,
//!   then sealed binary frames in both directions.
use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use rand_core::{OsRng, RngCore};
use serde_json::json;
use tracing::warn;

use crate::app_state::AppState;
use crate::config::PairingEncryption;
use crate::error::AppError;
use crate::transport_crypto::channel::{SecureChannel, WsFrameOpener, WsFrameSealer};
use crate::transport_crypto::handshake::{decode_b64url, NONCE_LENGTH, WS_LABEL};
use crate::transport_crypto::{query_value, EncryptionProtocol, TransportCryptoError};

/// What one received WebSocket message means to the read loop.
pub(crate) enum Inbound {
    /// A JSON text message for the dispatcher.
    Text(String),
    /// Control or irrelevant frame (ping, pong, plaintext binary).
    Ignore,
    /// The peer closed the socket.
    Close,
}

/// Turns outgoing JSON text into a WebSocket message.
pub(crate) trait FrameSealer: Send + 'static {
    fn seal(&mut self, text: &str) -> Result<Message, TransportCryptoError>;
}

/// Turns a received WebSocket message into JSON text. An error is fatal:
/// the connection closes with `4400 transport crypto failure`.
pub(crate) trait FrameOpener: Send + 'static {
    fn open(&mut self, message: Message) -> Result<Inbound, TransportCryptoError>;
}

/// Both halves of a connection's codec plus what connection events report.
pub(crate) struct WsCodec {
    pub sealer: Box<dyn FrameSealer>,
    pub opener: Box<dyn FrameOpener>,
    pub protocol: Option<EncryptionProtocol>,
    /// `0` plaintext, `2` transport v2.
    pub transport_version: u8,
}

fn control(message: Message) -> Inbound {
    match message {
        Message::Close(_) => Inbound::Close,
        _ => Inbound::Ignore,
    }
}

struct PlainSealer;
impl FrameSealer for PlainSealer {
    fn seal(&mut self, text: &str) -> Result<Message, TransportCryptoError> {
        Ok(Message::Text(text.into()))
    }
}

struct PlainOpener;
impl FrameOpener for PlainOpener {
    fn open(&mut self, message: Message) -> Result<Inbound, TransportCryptoError> {
        Ok(match message {
            Message::Text(text) => Inbound::Text(text.to_string()),
            other => control(other),
        })
    }
}

struct V2Sealer(WsFrameSealer);
impl FrameSealer for V2Sealer {
    fn seal(&mut self, text: &str) -> Result<Message, TransportCryptoError> {
        self.0
            .seal_text(text)
            .map(|frame| Message::Binary(frame.into()))
    }
}

struct V2Opener(WsFrameOpener);
impl FrameOpener for V2Opener {
    fn open(&mut self, message: Message) -> Result<Inbound, TransportCryptoError> {
        match message {
            Message::Binary(frame) => self.0.open_frame(&frame).map(Inbound::Text),
            Message::Text(_) => Err(TransportCryptoError::new("text frame after hello")),
            other => Ok(control(other)),
        }
    }
}

impl WsCodec {
    fn plaintext() -> Self {
        Self {
            sealer: Box::new(PlainSealer),
            opener: Box::new(PlainOpener),
            protocol: None,
            transport_version: 0,
        }
    }

    fn v2(channel: SecureChannel, protocol: EncryptionProtocol) -> Self {
        Self {
            sealer: Box::new(V2Sealer(channel.sealer)),
            opener: Box::new(V2Opener(channel.opener)),
            protocol: Some(protocol),
            transport_version: 2,
        }
    }
}

/// The transport an upgrade request asked for, checked before the upgrade.
pub(crate) enum WsTransport {
    Plaintext,
    V2(V2Offer),
}

/// Raw `tv=2` handshake parameters. They are decoded after the upgrade so a
/// malformed value closes the socket with 4400 like any other crypto failure.
pub(crate) struct V2Offer {
    protocol: EncryptionProtocol,
    client_material: String,
    client_nonce: String,
}

impl WsTransport {
    /// Reads `tv`, `enc` and the handshake material from the (signed)
    /// upgrade query. Protocol-level problems answer before the upgrade: an
    /// unknown `tv` or a v1 `enc=` without `tv` is `426
    /// PROTOCOL_UPGRADE_REQUIRED`; plaintext while the server requires
    /// encryption, or a protocol other than the required one, is `403`.
    pub(crate) fn negotiate(
        state: &AppState,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<Self, AppError> {
        let required = state.config.pairing_encryption;
        let Some(version) = query_value(query, "tv") else {
            // Transport v1 (`enc=` without `tv`) is retired.
            if query_value(query, "enc").is_some_and(|enc| enc != "none")
                || headers
                    .get("x-todex-encryption")
                    .is_some_and(|value| value.as_bytes() != b"none")
            {
                return Err(AppError::ProtocolUpgradeRequired(
                    "transport v1 is retired; connect with tv=2".to_owned(),
                ));
            }
            if required != PairingEncryption::None {
                return Err(AppError::Unauthorized(format!(
                    "Encryption public-key transfer verification is incomplete; server requires {} transport encryption",
                    required.as_str()
                )));
            }
            return Ok(Self::Plaintext);
        };
        if version != "2" {
            return Err(AppError::ProtocolUpgradeRequired(format!(
                "transport version {version} is not supported; use tv=2"
            )));
        }
        let enc = query_value(query, "enc").unwrap_or_default();
        let protocol = EncryptionProtocol::parse(&enc).ok_or_else(|| {
            AppError::InvalidRequest(format!("unsupported encryption protocol: {enc}"))
        })?;
        if required != PairingEncryption::None && required.as_str() != protocol.as_str() {
            return Err(AppError::Unauthorized(format!(
                "server requires {} transport encryption",
                required.as_str()
            )));
        }
        let material_key = match protocol {
            EncryptionProtocol::X25519 => "client_key",
            EncryptionProtocol::MlKem768 => "ciphertext",
        };
        Ok(Self::V2(V2Offer {
            protocol,
            client_material: query_value(query, material_key).unwrap_or_default(),
            client_nonce: query_value(query, "client_nonce").unwrap_or_default(),
        }))
    }

    /// Completes the handshake on the upgraded socket. For v2 this derives
    /// the session keys with a fresh server nonce and sends the hello; on
    /// failure the socket is closed with 4400 and `None` is returned.
    pub(crate) async fn establish(
        self,
        state: &AppState,
        socket: &mut WebSocket,
        device_id: &str,
    ) -> Option<WsCodec> {
        let offer = match self {
            Self::Plaintext => return Some(WsCodec::plaintext()),
            Self::V2(offer) => offer,
        };
        let mut server_nonce = [0_u8; NONCE_LENGTH];
        OsRng.fill_bytes(&mut server_nonce);
        let pairing_keys = match state.pairing_keys.current() {
            Ok(keys) => keys,
            Err(error) => {
                warn!(%error, "pairing keys unavailable; refusing v2 websocket handshake");
                super::socket::close_for_crypto_failure(socket).await;
                return None;
            }
        };
        let keys = decode_b64url(&offer.client_material).and_then(|material| {
            let client_nonce = decode_b64url(&offer.client_nonce)?;
            pairing_keys.server_session_keys(
                WS_LABEL,
                offer.protocol,
                device_id,
                &material,
                &client_nonce,
                &server_nonce,
            )
        });
        let keys = match keys {
            Ok(keys) => keys,
            Err(error) => {
                warn!(reason = error.reason(), "v2 websocket handshake failed");
                super::socket::close_for_crypto_failure(socket).await;
                return None;
            }
        };
        let hello = json!({
            "type": "todex.transport.hello",
            "version": 2,
            "serverNonce": crate::transport_crypto::encode_b64(&server_nonce),
        })
        .to_string();
        if super::socket::send_with_deadline(
            super::socket::WS_SOCKET_SEND_TIMEOUT,
            futures_util::SinkExt::send(socket, Message::Text(hello.into())),
        )
        .await
        .is_err()
        {
            return None;
        }
        Some(WsCodec::v2(SecureChannel::server(&keys), offer.protocol))
    }
}
