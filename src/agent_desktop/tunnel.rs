//! TCP streams from a desktop executor to this machine's loopback ports, so
//! a desktop browser can load `localhost` pages of a remote daemon. Streams
//! ride the executor's encrypted `/v2/ws` connection as base64 chunks
//! (`tunnel.*` frames) with per-direction credit windows.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, Semaphore},
    task::JoinHandle,
};

/// Raw bytes per `tunnel.data` frame.
pub(crate) const CHUNK_BYTES: usize = 32 * 1024;
/// Unacknowledged raw bytes per stream and direction.
pub(crate) const WINDOW_BYTES: usize = 256 * 1024;
pub(crate) const MAX_STREAMS: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_STREAM_ID: usize = 64;

struct Stream {
    /// Data from the desktop, to write to the TCP socket.
    inbound: mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes received from the desktop and not yet written (enforces its window).
    inbound_unacked: Arc<std::sync::atomic::AtomicUsize>,
    /// Credit for sending to the desktop; its acks add permits.
    outbound_credit: Arc<Semaphore>,
    task: JoinHandle<()>,
}

/// One executor connection's streams; dropping it closes them all.
pub(crate) struct TunnelSet {
    outgoing: mpsc::Sender<Value>,
    streams: HashMap<String, Stream>,
}

impl Drop for TunnelSet {
    fn drop(&mut self) {
        for (_, stream) in self.streams.drain() {
            stream.task.abort();
        }
    }
}

fn frame(kind: &str, payload: Value) -> Value {
    json!({ "type": kind, "payload": payload })
}

async fn send(outgoing: &mpsc::Sender<Value>, value: Value) -> bool {
    matches!(
        tokio::time::timeout(QUEUE_TIMEOUT, outgoing.send(value)).await,
        Ok(Ok(()))
    )
}

/// Dev servers listen on 127.0.0.1, ::1, or both; try both.
async fn connect_loopback(port: u16) -> std::io::Result<TcpStream> {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(v4)).await {
        Ok(Ok(stream)) => Ok(stream),
        first => {
            let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(v6)).await {
                Ok(result) => result,
                Err(_) => match first {
                    Ok(Err(error)) => Err(error),
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "connect timed out",
                    )),
                },
            }
        }
    }
}

impl TunnelSet {
    pub(crate) fn new(outgoing: mpsc::Sender<Value>) -> Self {
        Self {
            outgoing,
            streams: HashMap::new(),
        }
    }

    /// Opens a stream to `port` on loopback. The caller has checked that the
    /// conversation may reach the port.
    pub(crate) fn open(&mut self, stream_id: String, port: u16) -> Result<(), String> {
        self.streams.retain(|_, stream| !stream.task.is_finished());
        if stream_id.is_empty() || stream_id.len() > MAX_STREAM_ID {
            return Err("invalid streamId".to_owned());
        }
        if self.streams.contains_key(&stream_id) {
            return Err(format!("stream {stream_id} already exists"));
        }
        if self.streams.len() >= MAX_STREAMS {
            return Err(format!(
                "at most {MAX_STREAMS} tunnel streams per connection"
            ));
        }
        let (inbound, mut inbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let inbound_unacked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outbound_credit = Arc::new(Semaphore::new(WINDOW_BYTES));
        let outgoing = self.outgoing.clone();
        let task = {
            let stream_id = stream_id.clone();
            let inbound_unacked = inbound_unacked.clone();
            let outbound_credit = outbound_credit.clone();
            tokio::spawn(async move {
                let socket = match connect_loopback(port).await {
                    Ok(socket) => socket,
                    Err(error) => {
                        send(&outgoing, frame("tunnel.close", json!({ "streamId": stream_id, "error": format!("cannot connect to localhost:{port}: {error}") }))).await;
                        return;
                    }
                };
                if !send(
                    &outgoing,
                    frame("tunnel.opened", json!({ "streamId": stream_id })),
                )
                .await
                {
                    return;
                }
                let (mut reader, mut writer) = socket.into_split();
                let upstream = {
                    let outgoing = outgoing.clone();
                    let stream_id = stream_id.clone();
                    async move {
                        let mut buffer = vec![0_u8; CHUNK_BYTES];
                        loop {
                            let read = match reader.read(&mut buffer).await {
                                Ok(0) => return None,
                                Ok(read) => read,
                                Err(error) => return Some(error.to_string()),
                            };
                            let Ok(permit) = outbound_credit.acquire_many(read as u32).await else {
                                return None;
                            };
                            permit.forget();
                            let data = BASE64.encode(&buffer[..read]);
                            if !send(
                                &outgoing,
                                frame(
                                    "tunnel.data",
                                    json!({ "streamId": stream_id, "data": data }),
                                ),
                            )
                            .await
                            {
                                return None;
                            }
                        }
                    }
                };
                let downstream = {
                    let outgoing = outgoing.clone();
                    let stream_id = stream_id.clone();
                    async move {
                        while let Some(bytes) = inbound_rx.recv().await {
                            if let Err(error) = writer.write_all(&bytes).await {
                                return Some(error.to_string());
                            }
                            inbound_unacked
                                .fetch_sub(bytes.len(), std::sync::atomic::Ordering::Relaxed);
                            if !send(
                                &outgoing,
                                frame(
                                    "tunnel.ack",
                                    json!({ "streamId": stream_id, "bytes": bytes.len() }),
                                ),
                            )
                            .await
                            {
                                return None;
                            }
                        }
                        // The desktop closed its side.
                        let _ = writer.shutdown().await;
                        None
                    }
                };
                let error = tokio::select! {
                    error = upstream => error,
                    error = downstream => error,
                };
                let mut payload = json!({ "streamId": stream_id });
                if let Some(error) = error {
                    payload["error"] = Value::String(error);
                }
                send(&outgoing, frame("tunnel.close", payload)).await;
            })
        };
        self.streams.insert(
            stream_id,
            Stream {
                inbound,
                inbound_unacked,
                outbound_credit,
                task,
            },
        );
        Ok(())
    }

    /// Bytes from the desktop. Exceeding the window closes the stream.
    pub(crate) fn data(&mut self, stream_id: &str, data: &str) -> Result<(), String> {
        let bytes = BASE64
            .decode(data)
            .map_err(|_| "tunnel.data is not base64".to_owned())?;
        let Some(stream) = self.streams.get(stream_id) else {
            return Ok(()); // already closed
        };
        let unacked = stream
            .inbound_unacked
            .fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed)
            + bytes.len();
        if bytes.len() > CHUNK_BYTES || unacked > WINDOW_BYTES {
            self.close(stream_id);
            return Err(format!(
                "stream {stream_id} exceeded its flow-control window"
            ));
        }
        if stream.inbound.send(bytes).is_err() {
            self.streams.remove(stream_id);
        }
        Ok(())
    }

    pub(crate) fn ack(&mut self, stream_id: &str, bytes: usize) {
        if let Some(stream) = self.streams.get(stream_id) {
            // Never more credit than one window.
            let available = stream.outbound_credit.available_permits();
            let grant = bytes.min(WINDOW_BYTES.saturating_sub(available));
            stream.outbound_credit.add_permits(grant);
        }
    }

    pub(crate) fn close(&mut self, stream_id: &str) {
        if let Some(stream) = self.streams.remove(stream_id) {
            stream.task.abort();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.streams
            .values()
            .filter(|stream| !stream.task.is_finished())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn next(rx: &mut mpsc::Receiver<Value>, kind: &str) -> Value {
        loop {
            let value = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("frame")
                .expect("open channel");
            if value["type"] == kind {
                return value["payload"].clone();
            }
        }
    }

    #[tokio::test]
    async fn streams_relay_both_ways_with_flow_control() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // An echo server that also sends a large greeting.
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(&vec![b'x'; WINDOW_BYTES + CHUNK_BYTES])
                .await
                .unwrap();
            let mut buffer = [0_u8; 64];
            let read = socket.read(&mut buffer).await.unwrap();
            socket.write_all(&buffer[..read]).await.unwrap();
        });
        let (tx, mut rx) = mpsc::channel(1024);
        let mut tunnels = TunnelSet::new(tx);
        tunnels.open("s1".into(), port).unwrap();
        assert!(tunnels.open("s1".into(), port).is_err());
        next(&mut rx, "tunnel.opened").await;

        // Without acks the daemon stops after one window.
        let mut received = 0;
        while received < WINDOW_BYTES {
            let data = next(&mut rx, "tunnel.data").await;
            received += BASE64.decode(data["data"].as_str().unwrap()).unwrap().len();
        }
        assert_eq!(received, WINDOW_BYTES);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), next(&mut rx, "tunnel.data"))
                .await
                .is_err()
        );
        tunnels.ack("s1", received);
        let mut rest = 0;
        while rest < CHUNK_BYTES {
            let data = next(&mut rx, "tunnel.data").await;
            rest += BASE64.decode(data["data"].as_str().unwrap()).unwrap().len();
        }
        tunnels.ack("s1", rest);

        tunnels.data("s1", &BASE64.encode(b"ping")).unwrap();
        assert_eq!(next(&mut rx, "tunnel.ack").await["bytes"], 4);
        let echo = next(&mut rx, "tunnel.data").await;
        assert_eq!(
            BASE64.decode(echo["data"].as_str().unwrap()).unwrap(),
            b"ping"
        );
        // The server closed after echoing.
        let closed = next(&mut rx, "tunnel.close").await;
        assert_eq!(closed["streamId"], "s1");
    }

    #[tokio::test]
    async fn refused_ports_and_window_violations_close_streams() {
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = unused.local_addr().unwrap().port();
        drop(unused);
        let (tx, mut rx) = mpsc::channel(64);
        let mut tunnels = TunnelSet::new(tx);
        tunnels.open("dead".into(), port).unwrap();
        let closed = next(&mut rx, "tunnel.close").await;
        assert!(closed["error"].as_str().unwrap().contains("cannot connect"));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        tunnels.open("s".into(), port).unwrap();
        next(&mut rx, "tunnel.opened").await;
        assert!(tunnels.data("s", "%%%").is_err());
        assert!(tunnels
            .data("s", &BASE64.encode(vec![0; CHUNK_BYTES + 1]))
            .is_err());
        assert_eq!(tunnels.len(), 0);
        assert!(tunnels.open(String::new(), port).is_err());
    }
}
