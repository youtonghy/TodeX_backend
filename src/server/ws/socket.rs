//! Socket read/write loops for `/v2/ws`, shared by every transport.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, Notify};
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

/// When frames are pending, one is sent at least after this many queued
/// messages, so a busy event stream cannot starve a live view.
const MAX_MESSAGES_BEFORE_FRAME: usize = 16;

/// One queued outgoing message.
#[derive(Debug)]
pub(crate) enum Outbound {
    /// Built for this connection; serialized by the send task.
    Json(Value),
    /// Already-serialized text, typically shared by every connection that
    /// forwards the same published event.
    Text(Arc<str>),
}

impl From<Value> for Outbound {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

impl Outbound {
    /// The message as JSON, for tests that inspect queued frames.
    #[cfg(test)]
    pub(crate) fn into_value(self) -> Value {
        match self {
            Self::Json(value) => value,
            Self::Text(text) => serde_json::from_str(&text).expect("queued text is JSON"),
        }
    }
}

/// Latest-wins slots for live views (`agentBrowser.frame`), one per key,
/// drained by the send task beside the shared queue. A newer frame replaces
/// an unsent one instead of queueing behind it, and nothing put here is
/// dropped for lack of room, so the final state (e.g. `closed`) always
/// arrives while the connection lives.
#[derive(Clone, Default)]
pub(crate) struct FrameSlots {
    inner: Arc<FrameSlotsInner>,
}

#[derive(Default)]
struct FrameSlotsInner {
    /// Pending frames in first-put order; at most one per key.
    pending: Mutex<VecDeque<(String, Outbound)>>,
    ready: Notify,
}

impl FrameSlots {
    /// Replaces `key`'s unsent frame, or queues it.
    pub(crate) fn put(&self, key: &str, frame: Outbound) {
        {
            let mut pending = self.lock();
            match pending
                .iter_mut()
                .find(|(pending_key, _)| pending_key == key)
            {
                Some(slot) => slot.1 = frame,
                None => pending.push_back((key.to_owned(), frame)),
            }
        }
        self.inner.ready.notify_one();
    }

    /// Drops `key`'s unsent frame (the view was closed).
    pub(crate) fn remove(&self, key: &str) {
        self.lock().retain(|(pending_key, _)| pending_key != key);
    }

    fn take(&self) -> Option<Outbound> {
        let mut pending = self.lock();
        let frame = pending.pop_front().map(|(_, frame)| frame);
        if !pending.is_empty() {
            // Keep the send task coming back for the rest.
            self.inner.ready.notify_one();
        }
        frame
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<(String, Outbound)>> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

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

/// Seals queued messages and live-view frames and writes them, plus periodic
/// pings. Queued messages go first; a pending frame is sent when the queue
/// is idle or after [`MAX_MESSAGES_BEFORE_FRAME`] messages. Ends when the
/// queue closes, the peer stops accepting frames, sealing fails or a close
/// is requested (the last two close with 4400).
pub(crate) fn spawn_sender<S>(
    mut sink: S,
    mut sealer: Box<dyn FrameSealer>,
    mut outgoing: mpsc::Receiver<Outbound>,
    frames: FrameSlots,
) -> (tokio::task::JoinHandle<()>, CloseHandle)
where
    S: futures_util::Sink<Message> + Unpin + Send + 'static,
{
    let (close_tx, mut close_rx) = mpsc::channel::<()>(1);
    let task = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(WS_PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut close_open = true;
        let mut messages_since_frame = 0usize;
        loop {
            let starved_frame = if messages_since_frame >= MAX_MESSAGES_BEFORE_FRAME {
                frames.take()
            } else {
                None
            };
            let next = match starved_frame {
                Some(frame) => {
                    messages_since_frame = 0;
                    Some(frame)
                }
                None => tokio::select! {
                    biased;
                    request = close_rx.recv(), if close_open => {
                        if request.is_none() {
                            close_open = false;
                            continue;
                        }
                        close_for_crypto_failure(&mut sink).await;
                        break;
                    }
                    message = outgoing.recv() => {
                        let Some(message) = message else { break };
                        messages_since_frame += 1;
                        Some(message)
                    }
                    _ = frames.inner.ready.notified() => {
                        messages_since_frame = 0;
                        frames.take()
                    }
                    _ = ping_interval.tick() => {
                        if let Err(failure) = send_with_deadline(WS_SOCKET_SEND_TIMEOUT, sink.send(Message::Ping(Default::default()))).await {
                            log_socket_send_failure(failure);
                            break;
                        }
                        None
                    }
                },
            };
            // A notification can outlive the frame it announced.
            let Some(next) = next else { continue };
            let sealed = match &next {
                Outbound::Json(value) => match serde_json::to_string(value) {
                    Ok(text) => sealer.seal(&text),
                    Err(error) => {
                        warn!(error = %error, "failed to serialize v2 websocket event");
                        continue;
                    }
                },
                Outbound::Text(text) => sealer.seal(text),
            };
            let message = match sealed {
                Ok(message) => message,
                Err(error) => {
                    warn!(reason = error.reason(), "failed to seal v2 websocket frame");
                    close_for_crypto_failure(&mut sink).await;
                    break;
                }
            };
            if let Err(failure) =
                send_with_deadline(WS_SOCKET_SEND_TIMEOUT, sink.send(message)).await
            {
                log_socket_send_failure(failure);
                break;
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::transport_crypto::TransportCryptoError;

    struct TextSealer;
    impl FrameSealer for TextSealer {
        fn seal(&mut self, text: &str) -> Result<Message, TransportCryptoError> {
            Ok(Message::Text(text.into()))
        }
    }

    fn text(value: Value) -> Outbound {
        Outbound::Text(value.to_string().into())
    }

    /// Starts a send task whose socket is a channel of the sent texts.
    fn sender(
        frames: FrameSlots,
    ) -> (
        mpsc::Sender<Outbound>,
        mpsc::UnboundedReceiver<Value>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sent_tx, sent_rx) = mpsc::unbounded_channel();
        let sink = Box::pin(futures_util::sink::unfold(
            sent_tx,
            |sent_tx, message: Message| async move {
                if let Message::Text(text) = message {
                    let _ = sent_tx.send(serde_json::from_str(text.as_str()).unwrap());
                }
                Ok::<_, std::convert::Infallible>(sent_tx)
            },
        ));
        let (outgoing_tx, outgoing_rx) = mpsc::channel(256);
        let (task, _close) = spawn_sender(sink, Box::new(TextSealer), outgoing_rx, frames);
        (outgoing_tx, sent_rx, task)
    }

    async fn next(sent: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(2), sent.recv())
            .await
            .expect("a message is sent")
            .expect("send task alive")
    }

    #[test]
    fn frame_slots_keep_the_latest_frame_per_key() {
        let slots = FrameSlots::default();
        slots.put("a", text(json!({ "seq": 1 })));
        slots.put("b", text(json!({ "seq": 10 })));
        slots.put("a", text(json!({ "seq": 2 })));
        slots.put("c", text(json!({ "seq": 20 })));
        slots.remove("c");
        assert_eq!(slots.take().unwrap().into_value(), json!({ "seq": 2 }));
        assert_eq!(slots.take().unwrap().into_value(), json!({ "seq": 10 }));
        assert!(slots.take().is_none());
    }

    #[tokio::test]
    async fn queued_messages_go_first_and_the_final_frame_is_delivered() {
        let frames = FrameSlots::default();
        let (outgoing, mut sent, task) = sender(frames.clone());
        // Everything is pending before the task looks at either source.
        for index in 0..3 {
            outgoing
                .send(json!({ "message": index }).into())
                .await
                .unwrap();
        }
        frames.put("view", text(json!({ "seq": 1 })));
        frames.put("view", text(json!({ "seq": 2 })));
        frames.put("view", text(json!({ "closed": true })));
        for index in 0..3 {
            assert_eq!(next(&mut sent).await, json!({ "message": index }));
        }
        assert_eq!(next(&mut sent).await, json!({ "closed": true }));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sent.recv())
                .await
                .is_err(),
            "replaced frames are never sent"
        );
        drop(outgoing);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_busy_queue_does_not_starve_frames() {
        let frames = FrameSlots::default();
        let (outgoing, mut sent, task) = sender(frames.clone());
        for index in 0..40 {
            outgoing
                .send(json!({ "message": index }).into())
                .await
                .unwrap();
        }
        frames.put("view", text(json!({ "frame": true })));
        let mut order = Vec::new();
        for _ in 0..41 {
            order.push(next(&mut sent).await);
        }
        let position = order
            .iter()
            .position(|message| message == &json!({ "frame": true }))
            .expect("frame sent");
        assert!(position <= MAX_MESSAGES_BEFORE_FRAME, "frame at {position}");
        drop(outgoing);
        task.await.unwrap();
    }
}
