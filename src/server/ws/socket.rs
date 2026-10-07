//! Socket read/write loops for `/v2/ws`, shared by every transport.
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::warn;

use super::codec::{FrameOpener, FrameSealer, Inbound};
use crate::transport_crypto::channel::{WS_CLOSE_CODE, WS_CLOSE_REASON};

/// Keep idle connections alive; mirrors the legacy `/v1/ws` socket so clients
/// without an application-level heartbeat are not reaped. A Ping draws an
/// automatic Pong, which counts as receive activity below.
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) const WS_CLIENT_TIMEOUT: Duration = Duration::from_secs(90);
/// A peer whose socket accepts no frame for this long is treated as gone, so a
/// stalled client cannot hold the send task (and its queue) forever.
pub(crate) const WS_SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendFailure {
    /// The receiving side is gone.
    Closed,
    /// The receiving side did not accept the item before the deadline.
    Stalled,
}

/// Bounds one send on the websocket or its outgoing queue. Either failure
/// means the connection can no longer deliver and must be torn down.
pub(crate) async fn send_with_deadline<E>(
    deadline: Duration,
    send: impl std::future::Future<Output = Result<(), E>>,
) -> Result<(), SendFailure> {
    match tokio::time::timeout(deadline, send).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(SendFailure::Closed),
        Err(_) => Err(SendFailure::Stalled),
    }
}

fn log_socket_send_failure(failure: SendFailure) {
    match failure {
        SendFailure::Closed => {}
        SendFailure::Stalled => warn!(
            timeout_secs = WS_SOCKET_SEND_TIMEOUT.as_secs(),
            "v2 websocket peer stopped accepting frames, closing connection"
        ),
    }
}

fn crypto_close_frame() -> Message {
    Message::Close(Some(CloseFrame {
        code: WS_CLOSE_CODE,
        reason: WS_CLOSE_REASON.into(),
    }))
}

/// `4400 transport crypto failure`, with no further detail.
pub(crate) async fn close_for_crypto_failure<S>(sink: &mut S)
where
    S: futures_util::Sink<Message> + Unpin,
{
    let _ = send_with_deadline(WS_SOCKET_SEND_TIMEOUT, sink.send(crypto_close_frame())).await;
}

/// Lets the read loop ask the send task to close the socket with 4400.
#[derive(Clone)]
pub(crate) struct CloseHandle(mpsc::Sender<()>);

impl CloseHandle {
    pub(crate) fn crypto_failure(&self) {
        // Capacity 1: a pending request is already enough.
        let _ = self.0.try_send(());
    }
}

/// Seals queued JSON values and writes them, plus periodic pings. Ends when
/// the queue closes, the peer stops accepting frames, sealing fails or a
/// close is requested (the last two close with 4400).
pub(crate) fn spawn_sender(
    mut sink: SplitSink<WebSocket, Message>,
    mut sealer: Box<dyn FrameSealer>,
    mut outgoing: mpsc::Receiver<Value>,
) -> (tokio::task::JoinHandle<()>, CloseHandle) {
    let (close_tx, mut close_rx) = mpsc::channel::<()>(1);
    let task = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(WS_PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut close_open = true;
        loop {
            tokio::select! {
                biased;
                request = close_rx.recv(), if close_open => {
                    if request.is_none() {
                        close_open = false;
                        continue;
                    }
                    close_for_crypto_failure(&mut sink).await;
                    break;
                }
                value = outgoing.recv() => {
                    let Some(value) = value else { break };
                    let text = match serde_json::to_string(&value) {
                        Ok(text) => text,
                        Err(error) => {
                            warn!(error = %error, "failed to serialize v2 websocket event");
                            continue;
                        }
                    };
                    let message = match sealer.seal(&text) {
                        Ok(message) => message,
                        Err(error) => {
                            warn!(reason = error.reason(), "failed to seal v2 websocket frame");
                            close_for_crypto_failure(&mut sink).await;
                            break;
                        }
                    };
                    if let Err(failure) = send_with_deadline(WS_SOCKET_SEND_TIMEOUT, sink.send(message)).await {
                        log_socket_send_failure(failure);
                        break;
                    }
                }
                _ = ping_interval.tick() => {
                    if let Err(failure) = send_with_deadline(WS_SOCKET_SEND_TIMEOUT, sink.send(Message::Ping(Default::default()))).await {
                        log_socket_send_failure(failure);
                        break;
                    }
                }
            }
        }
    });
    (task, CloseHandle(close_tx))
}

/// Reads frames until one carries JSON text for the dispatcher. `None`
/// means the connection is over (closed, timed out, failed, or a crypto
/// failure, after which the send task was asked to close with 4400).
pub(crate) async fn next_text(
    receiver: &mut SplitStream<WebSocket>,
    opener: &mut dyn FrameOpener,
    close: &CloseHandle,
) -> Option<String> {
    loop {
        let message = match tokio::time::timeout(WS_CLIENT_TIMEOUT, receiver.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                warn!(error = %error, "v2 websocket receive failed");
                return None;
            }
            Ok(None) => return None,
            Err(_) => {
                warn!(
                    timeout_secs = WS_CLIENT_TIMEOUT.as_secs(),
                    "v2 websocket client inactive, closing connection"
                );
                return None;
            }
        };
        match opener.open(message) {
            Ok(Inbound::Text(text)) => return Some(text),
            Ok(Inbound::Ignore) => continue,
            Ok(Inbound::Close) => return None,
            Err(error) => {
                warn!(
                    reason = error.reason(),
                    "v2 websocket transport crypto failure"
                );
                close.crypto_failure();
                return None;
            }
        }
    }
}
