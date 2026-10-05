//! A small Chrome DevTools Protocol client: requests with ids and timeouts,
//! and a broadcast of events (flat sessions). Unix talks to Chromium over
//! `--remote-debugging-pipe` (NUL-terminated JSON on fds 3/4), so no other
//! local process can reach the browser; Windows uses a loopback WebSocket.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::BrowserError;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_BUFFER: usize = 1024;

/// One CDP event; `session_id` is the flat session it belongs to.
#[derive(Debug)]
pub(crate) struct Event {
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>;

struct Inner {
    next_id: AtomicU64,
    pending: Pending,
    outgoing: mpsc::UnboundedSender<String>,
    events: broadcast::Sender<Arc<Event>>,
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
    #[cfg_attr(unix, allow(dead_code))]
    WebSocket(String),
}

impl Cdp {
    pub(crate) async fn connect(transport: Transport) -> Result<Self, BrowserError> {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel::<String>();
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let cdp = Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                outgoing,
                events,
                closed: AtomicBool::new(false),
            }),
        };
        match transport {
            #[cfg(unix)]
            Transport::Pipe {
                to_browser,
                from_browser,
            } => cdp.run_pipe(to_browser, from_browser, outgoing_rx),
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

    pub(crate) fn events(&self) -> broadcast::Receiver<Arc<Event>> {
        self.inner.events.subscribe()
    }

    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
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
        match tokio::time::timeout(CALL_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(BrowserError::failed("the browser connection closed")),
            Err(_) => {
                self.forget(id);
                Err(BrowserError::failed(format!("{method} timed out")))
            }
        }
    }

    fn forget(&self, id: u64) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }
}

fn dispatch(inner: &Inner, bytes: &[u8]) {
    let Ok(message) = serde_json::from_slice::<Value>(bytes) else {
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
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        let _ = inner.events.send(Arc::new(Event {
            session_id: message
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            method: method.to_owned(),
            params: message.get("params").cloned().unwrap_or(Value::Null),
        }));
    }
}

/// Fails every waiting call once the browser is gone.
fn close(inner: &Inner) {
    inner.closed.store(true, Ordering::SeqCst);
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
