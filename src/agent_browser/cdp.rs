//! A small Chrome DevTools Protocol client: requests with ids and timeouts,
//! and events of flat sessions. Unix talks to Chromium over
//! `--remote-debugging-pipe` (NUL-terminated JSON on fds 3/4), so no other
//! local process can reach the browser; Windows hands it anonymous pipes
//! by handle (`--remote-debugging-io-pipes`).
//!
//! Events take two paths so live video can never crowd out the navigation
//! guard: the few events the browser session acts on ([`HANDLED_EVENTS`])
//! go through an unbounded single-consumer queue and are never dropped (a
//! lost `Fetch.requestPaused` would hang that navigation forever), while
//! `Page.screencastFrame` lands in a latest-frame slot per session.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};

use super::BrowserError;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const SCREENCAST_FRAME: &str = "Page.screencastFrame";
/// Events the browser session handles; everything else is dropped on
/// arrival.
const HANDLED_EVENTS: &[&str] = &[
    "Fetch.requestPaused",
    "Page.javascriptDialogOpening",
    "Target.attachedToTarget",
    "Target.detachedFromTarget",
];

/// One CDP event; `session_id` is the flat session it belongs to.
#[derive(Debug)]
pub(crate) struct Event {
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>;

/// The newest unconsumed screencast frame of each session.
#[derive(Default)]
struct FrameSlot {
    /// Session id → `Page.screencastFrame` params.
    latest: Mutex<HashMap<String, Value>>,
    ready: Notify,
}

struct Inner {
    next_id: AtomicU64,
    pending: Pending,
    outgoing: mpsc::UnboundedSender<String>,
    /// Dropped when the browser goes away, which ends the event loop.
    events: Mutex<Option<mpsc::UnboundedSender<Event>>>,
    /// Taken once by the browser session's event loop.
    event_receiver: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
    frames: FrameSlot,
    closed: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct Cdp {
    inner: Arc<Inner>,
}

/// Where messages come from and go to.
pub(crate) enum Transport {
    #[cfg(unix)]
    Pipe {
        to_browser: tokio::net::unix::pipe::Sender,
        from_browser: tokio::net::unix::pipe::Receiver,
    },
    /// Anonymous pipes handed to Chromium by handle
    /// (`--remote-debugging-io-pipes`), read and written on threads.
    #[cfg(windows)]
    Pipe {
        to_browser: std::io::PipeWriter,
        from_browser: std::io::PipeReader,
    },
    /// Not started by the daemon on any platform now; kept for tests and
    /// development against a remote debugging port.
    #[allow(dead_code)]
    WebSocket(String),
}

impl Cdp {
    pub(crate) async fn connect(transport: Transport) -> Result<Self, BrowserError> {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel::<String>();
        let (events, event_receiver) = mpsc::unbounded_channel();
        let cdp = Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                outgoing,
                events: Mutex::new(Some(events)),
                event_receiver: Mutex::new(Some(event_receiver)),
                frames: FrameSlot::default(),
                closed: AtomicBool::new(false),
            }),
        };
        match transport {
            #[cfg(unix)]
            Transport::Pipe {
                to_browser,
                from_browser,
            } => cdp.run_pipe(to_browser, from_browser, outgoing_rx),
            #[cfg(windows)]
            Transport::Pipe {
                to_browser,
                from_browser,
            } => cdp.run_blocking_pipe(to_browser, from_browser, outgoing_rx),
            Transport::WebSocket(url) => cdp.run_websocket(&url, outgoing_rx).await?,
        }
        Ok(cdp)
    }

    #[cfg(unix)]
    fn run_pipe(
        &self,
        mut to_browser: tokio::net::unix::pipe::Sender,
        from_browser: tokio::net::unix::pipe::Receiver,
        mut outgoing: mpsc::UnboundedReceiver<String>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let mut bytes = message.into_bytes();
                bytes.push(0);
                if to_browser.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(from_browser);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(0, &mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buffer.last() == Some(&0) {
                            buffer.pop();
                        }
                        dispatch(&inner, &buffer);
                    }
                }
            }
            close(&inner);
        });
    }

    /// Windows: the same NUL-terminated framing over blocking anonymous
    /// pipes, one thread each way. Both end when Chromium goes away (the
    /// read sees EOF, the next write fails); `Process` ends Chromium by
    /// closing its job. Plain threads rather than `spawn_blocking`, so a
    /// read parked on a live browser cannot hold up runtime shutdown.
    #[cfg(windows)]
    fn run_blocking_pipe(
        &self,
        mut to_browser: std::io::PipeWriter,
        from_browser: std::io::PipeReader,
        mut outgoing: mpsc::UnboundedReceiver<String>,
    ) {
        use std::io::{BufRead, BufReader, Write};
        std::thread::spawn(move || {
            while let Some(message) = outgoing.blocking_recv() {
                let mut bytes = message.into_bytes();
                bytes.push(0);
                if to_browser.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(from_browser);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(0, &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buffer.last() == Some(&0) {
                            buffer.pop();
                        }
                        dispatch(&inner, &buffer);
                    }
                }
            }
            close(&inner);
        });
    }

    async fn run_websocket(
        &self,
        url: &str,
        mut outgoing: mpsc::UnboundedReceiver<String>,
    ) -> Result<(), BrowserError> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| BrowserError::failed(format!("cannot reach the browser: {error}")))?;
        let (mut sink, mut stream) = socket.split();
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(Message::Text(message.into())).await.is_err() {
                    break;
                }
            }
        });
        let inner = self.inner.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let Message::Text(text) = message {
                    dispatch(&inner, text.as_bytes());
                }
            }
            close(&inner);
        });
        Ok(())
    }

    /// Whether the browser connection is gone (crash, exit).
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// The handled events, in order; `None` after the first call.
    pub(crate) fn take_events(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.inner
            .event_receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Waits until a screencast frame may be available.
    pub(crate) async fn frame_ready(&self) {
        self.inner.frames.ready.notified().await;
    }

    /// The newest frame of each session since the last call, as
    /// (session id, `Page.screencastFrame` params).
    pub(crate) fn take_frames(&self) -> Vec<(String, Value)> {
        self.inner
            .frames
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .collect()
    }

    /// Sends a command without waiting for (or keeping) its response.
    pub(crate) fn notify(&self, method: &str, params: Value, session_id: Option<&str>) {
        send_unanswered(&self.inner, method, params, session_id);
    }

    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, BrowserError> {
        self.call_within(method, params, session_id, CALL_TIMEOUT)
            .await
    }

    /// [`Self::call`] that gives up after `timeout`.
    pub(crate) async fn call_within(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, BrowserError> {
        if self.is_closed() {
            return Err(BrowserError::failed("the browser is not running"));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            message["sessionId"] = Value::from(session_id);
        }
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, sender);
        if self.inner.outgoing.send(message.to_string()).is_err() {
            self.forget(id);
            return Err(BrowserError::failed("the browser is not running"));
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(BrowserError::failed("the browser connection closed")),
            Err(_) => {
                self.forget(id);
                Err(BrowserError::new("TIMEOUT", format!("{method} timed out")))
            }
        }
    }

    /// A connection with no browser behind it, for tests: the commands sent
    /// arrive on the returned receiver, and [`Self::inject`] /
    /// [`Self::disconnect`] play the browser's side.
    #[cfg(test)]
    pub(crate) fn scripted() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (inner, outgoing) = tests::inner();
        (
            Self {
                inner: Arc::new(inner),
            },
            outgoing,
        )
    }

    /// Delivers a message as if the browser had sent it.
    #[cfg(test)]
    pub(crate) fn inject(&self, message: &Value) {
        dispatch(&self.inner, message.to_string().as_bytes());
    }

    /// The browser goes away (crash, exit).
    #[cfg(test)]
    pub(crate) fn disconnect(&self) {
        close(&self.inner);
    }

    fn forget(&self, id: u64) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }
}

fn send_unanswered(inner: &Inner, method: &str, params: Value, session_id: Option<&str>) {
    // Its response finds no waiter and is dropped by `dispatch`.
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let mut message = json!({ "id": id, "method": method, "params": params });
    if let Some(session_id) = session_id {
        message["sessionId"] = Value::from(session_id);
    }
    let _ = inner.outgoing.send(message.to_string());
}

fn dispatch(inner: &Inner, bytes: &[u8]) {
    let Ok(mut message) = serde_json::from_slice::<Value>(bytes) else {
        tracing::debug!("unparseable CDP message");
        return;
    };
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let waiter = inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        if let Some(waiter) = waiter {
            let result = match message.get("error") {
                Some(error) => Err(BrowserError::failed(
                    error["message"].as_str().unwrap_or("CDP error").to_owned(),
                )),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(result);
        }
        return;
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    let frame = method == SCREENCAST_FRAME;
    if !frame && !HANDLED_EVENTS.contains(&method) {
        return;
    }
    let method = method.to_owned();
    let session_id = match message.get_mut("sessionId").map(Value::take) {
        Some(Value::String(session_id)) => Some(session_id),
        _ => None,
    };
    let params = message
        .get_mut("params")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if frame {
        let Some(session_id) = session_id else { return };
        let replaced = inner
            .frames
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.clone(), params);
        // Chromium sends the next frame only once this one is acked; a frame
        // replaced before anyone saw it is skipped, but still acked.
        if let Some(skipped) = replaced {
            send_unanswered(
                inner,
                "Page.screencastFrameAck",
                json!({ "sessionId": skipped["sessionId"] }),
                Some(&session_id),
            );
        }
        inner.frames.ready.notify_one();
        return;
    }
    if let Some(events) = inner
        .events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        let _ = events.send(Event {
            session_id,
            method,
            params,
        });
    }
}

/// Fails every waiting call once the browser is gone.
fn close(inner: &Inner) {
    inner.closed.store(true, Ordering::SeqCst);
    inner
        .events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    // Wakes the frame loop so it sees the browser is gone.
    inner.frames.ready.notify_one();
    let pending: Vec<_> = inner
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .collect();
    for (_, waiter) in pending {
        let _ = waiter.send(Err(BrowserError::failed("the browser exited")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn inner() -> (Inner, mpsc::UnboundedReceiver<String>) {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        let (events, event_receiver) = mpsc::unbounded_channel();
        (
            Inner {
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                outgoing,
                events: Mutex::new(Some(events)),
                event_receiver: Mutex::new(Some(event_receiver)),
                frames: FrameSlot::default(),
                closed: AtomicBool::new(false),
            },
            outgoing_rx,
        )
    }

    fn frame(session: &str, ack: u64) -> Vec<u8> {
        json!({ "method": SCREENCAST_FRAME, "sessionId": session, "params": { "sessionId": ack, "data": "AAAA" } })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn guard_events_are_never_dropped_behind_video() {
        let (inner, _outgoing) = inner();
        let mut events = inner.event_receiver.lock().unwrap().take().unwrap();
        // Far more than the old 1024-event broadcast buffer, interleaved
        // with frames and noise nobody handles.
        for index in 0..5000 {
            dispatch(&inner, &frame("s1", index));
            dispatch(
                &inner,
                json!({ "method": "Runtime.consoleAPICalled", "sessionId": "s1", "params": {} })
                    .to_string()
                    .as_bytes(),
            );
            dispatch(
                &inner,
                json!({ "method": "Fetch.requestPaused", "sessionId": "s1", "params": { "requestId": index.to_string() } })
                    .to_string()
                    .as_bytes(),
            );
        }
        for index in 0..5000 {
            let event = events.try_recv().unwrap();
            assert_eq!(event.method, "Fetch.requestPaused");
            assert_eq!(event.session_id.as_deref(), Some("s1"));
            assert_eq!(event.params["requestId"], index.to_string());
        }
        assert!(events.try_recv().is_err(), "unhandled events are dropped");
        close(&inner);
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn frames_keep_the_newest_per_session_and_ack_skipped_ones() {
        let (inner, mut outgoing) = inner();
        dispatch(&inner, &frame("s1", 1));
        dispatch(&inner, &frame("s2", 7));
        dispatch(&inner, &frame("s1", 2));
        let mut latest: Vec<(String, Value)> =
            inner.frames.latest.lock().unwrap().drain().collect();
        latest.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].1["sessionId"], 2);
        assert_eq!(latest[1].1["sessionId"], 7);
        // Frame 1 was replaced unseen: acked so Chromium keeps sending.
        let ack: Value = serde_json::from_str(&outgoing.try_recv().unwrap()).unwrap();
        assert_eq!(ack["method"], "Page.screencastFrameAck");
        assert_eq!(ack["params"]["sessionId"], 1);
        assert_eq!(ack["sessionId"], "s1");
        assert!(outgoing.try_recv().is_err());
    }

    #[test]
    fn auto_attach_events_reach_the_session() {
        // With `waitForDebuggerOnStart`, a page only runs once the session
        // sees its `attachedToTarget`; dropping it leaves every tab paused.
        let (inner, _outgoing) = inner();
        let mut events = inner.event_receiver.lock().unwrap().take().unwrap();
        dispatch(
            &inner,
            json!({ "method": "Target.attachedToTarget", "params": { "sessionId": "s2", "waitingForDebugger": true } })
                .to_string()
                .as_bytes(),
        );
        let event = events.try_recv().unwrap();
        assert_eq!(event.method, "Target.attachedToTarget");
        assert_eq!(event.params["sessionId"], "s2");
    }
}
