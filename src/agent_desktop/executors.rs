//! Desktop clients that registered as executors on their `/v2/ws`
//! connection, and the daemon → executor request/response channel.
//!
//! An executor is one socket, not one device: a desktop holds a dedicated
//! connection per backend, and a device may (briefly, while reconnecting)
//! have two. Results are only accepted from the connection an invoke was
//! sent to.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Waiting for room in a connection's outgoing queue; a queue that stays
/// full means the executor stopped reading.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
/// Capability names an executor may announce.
pub(crate) const CAPABILITY_BROWSER: &str = "browser";
const KNOWN_CAPABILITIES: &[&str] = &[CAPABILITY_BROWSER];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutorInfo {
    pub executor_id: u64,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub capabilities: Vec<String>,
}

struct Executor {
    info: ExecutorInfo,
    outgoing: mpsc::Sender<Value>,
}

/// What the executor is asked to do for one tool call.
#[derive(Clone, Debug)]
pub(crate) struct InvokeRequest {
    pub conversation_id: String,
    /// `{ id?, path }`: selects the browser partition on the desktop.
    pub workspace: Value,
    pub tool: String,
    pub args: Value,
}

struct Pending {
    executor_id: u64,
    sender: oneshot::Sender<Result<Value, ExecutorError>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ExecutorError {
    #[error("the desktop executor disconnected")]
    Gone,
    #[error("the desktop executor did not answer within {0} seconds")]
    Timeout(u64),
    #[error("the call was cancelled")]
    Cancelled,
    #[error("the desktop executor is not reading its connection")]
    Stalled,
    /// Reported by the executor itself.
    #[error("{message}")]
    Failed { code: String, message: String },
}

impl ExecutorError {
    pub(crate) fn code(&self) -> &str {
        match self {
            Self::Gone => "EXECUTOR_GONE",
            Self::Timeout(_) => "EXECUTOR_TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::Stalled => "EXECUTOR_STALLED",
            Self::Failed { code, .. } => code,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Executors {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: AtomicU64,
    executors: Mutex<HashMap<u64, Executor>>,
    pending: Mutex<HashMap<String, Pending>>,
}

/// One connection's registration; unregisters when dropped, which fails
/// every call still waiting on that connection.
pub(crate) struct Registration {
    executors: Executors,
    executor_id: u64,
}

impl Registration {
    pub(crate) fn executor_id(&self) -> u64 {
        self.executor_id
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.executors.unregister(self.executor_id);
    }
}

pub(crate) fn validate_capabilities(capabilities: &[String]) -> Result<Vec<String>, String> {
    let mut accepted = Vec::new();
    for capability in capabilities {
        if !KNOWN_CAPABILITIES.contains(&capability.as_str()) {
            return Err(format!("unknown executor capability: {capability}"));
        }
        if !accepted.contains(capability) {
            accepted.push(capability.clone());
        }
    }
    Ok(accepted)
}

impl Executors {
    pub(crate) fn register(
        &self,
        device_id: String,
        device_name: String,
        platform: String,
        capabilities: Vec<String>,
        outgoing: mpsc::Sender<Value>,
    ) -> Registration {
        let executor_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let executor = Executor {
            info: ExecutorInfo {
                executor_id,
                device_id,
                device_name,
                platform,
                capabilities,
            },
            outgoing,
        };
        self.inner
            .executors
            .lock()
            .expect("executor lock")
            .insert(executor_id, executor);
        Registration {
            executors: self.clone(),
            executor_id,
        }
    }

    fn unregister(&self, executor_id: u64) {
        self.inner
            .executors
            .lock()
            .expect("executor lock")
            .remove(&executor_id);
        let orphaned: Vec<Pending> = {
            let mut pending = self.inner.pending.lock().expect("executor pending lock");
            let ids: Vec<String> = pending
                .iter()
                .filter(|(_, call)| call.executor_id == executor_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| pending.remove(id)).collect()
        };
        for call in orphaned {
            let _ = call.sender.send(Err(ExecutorError::Gone));
        }
    }

    /// Online executors offering `capability`, newest first.
    pub(crate) fn online(&self, capability: &str) -> Vec<ExecutorInfo> {
        let mut online: Vec<ExecutorInfo> = self
            .inner
            .executors
            .lock()
            .expect("executor lock")
            .values()
            .filter(|executor| executor.info.capabilities.iter().any(|c| c == capability))
            .map(|executor| executor.info.clone())
            .collect();
        online.sort_by_key(|executor| std::cmp::Reverse(executor.executor_id));
        online
    }

    /// The newest executor of `device_id` offering `capability`.
    pub(crate) fn for_device(&self, device_id: &str, capability: &str) -> Option<ExecutorInfo> {
        self.online(capability)
            .into_iter()
            .find(|executor| executor.device_id == device_id)
    }

    /// Sends `executor.invoke` and waits for the matching `executor.result`.
    /// `cancel` resolving (the agent abandoned the tool call) or the
    /// deadline passing sends `executor.cancel` and fails the call.
    pub(crate) async fn invoke(
        &self,
        executor_id: u64,
        request: InvokeRequest,
        timeout: Duration,
        cancel: impl Future<Output = ()>,
    ) -> Result<Value, ExecutorError> {
        let outgoing = self
            .inner
            .executors
            .lock()
            .expect("executor lock")
            .get(&executor_id)
            .map(|executor| executor.outgoing.clone())
            .ok_or(ExecutorError::Gone)?;
        let invoke_id = format!("inv_{}", Uuid::new_v4().simple());
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .expect("executor pending lock")
            .insert(
                invoke_id.clone(),
                Pending {
                    executor_id,
                    sender,
                },
            );
        // Removes the pending entry however this call ends.
        let _pending = PendingGuard {
            executors: self,
            invoke_id: &invoke_id,
        };
        let frame = json!({
            "type": "executor.invoke",
            "payload": {
                "invokeId": invoke_id,
                "conversationId": request.conversation_id,
                "workspace": request.workspace,
                "tool": request.tool,
                "args": request.args,
                "timeoutMs": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            },
        });
        match tokio::time::timeout(QUEUE_TIMEOUT, outgoing.send(frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(ExecutorError::Gone),
            Err(_) => return Err(ExecutorError::Stalled),
        }
        let outcome = tokio::select! {
            result = receiver => result.unwrap_or(Err(ExecutorError::Gone)),
            _ = tokio::time::sleep(timeout) => Err(ExecutorError::Timeout(timeout.as_secs())),
            _ = cancel => Err(ExecutorError::Cancelled),
        };
        if matches!(
            outcome,
            Err(ExecutorError::Timeout(_) | ExecutorError::Cancelled)
        ) {
            // Best effort: the executor stops work nobody waits for.
            let _ = outgoing.try_send(json!({
                "type": "executor.cancel",
                "payload": { "invokeId": invoke_id },
            }));
        }
        outcome
    }

    /// Tells an executor that a conversation no longer has access, so it
    /// can close what it opened for it.
    pub(crate) fn release(&self, executor_id: u64, conversation_id: &str) {
        let outgoing = self
            .inner
            .executors
            .lock()
            .expect("executor lock")
            .get(&executor_id)
            .map(|executor| executor.outgoing.clone());
        if let Some(outgoing) = outgoing {
            let frame = json!({
                "type": "executor.release",
                "payload": { "conversationId": conversation_id },
            });
            if outgoing.try_send(frame).is_err() {
                tracing::warn!(
                    executor_id,
                    "could not notify executor of a released conversation"
                );
            }
        }
    }

    /// Routes an `executor.result` frame. Results for unknown calls, or from
    /// a connection the call was not sent to, are dropped.
    pub(crate) fn complete(&self, executor_id: u64, payload: &Value) -> Result<(), String> {
        let invoke_id = payload["invokeId"]
            .as_str()
            .ok_or("executor.result needs invokeId")?;
        let result = match payload["ok"].as_bool() {
            Some(true) => Ok(payload.get("result").cloned().unwrap_or(Value::Null)),
            Some(false) => Err(ExecutorError::Failed {
                code: payload["error"]["code"]
                    .as_str()
                    .unwrap_or("EXECUTOR_FAILED")
                    .chars()
                    .take(64)
                    .collect(),
                message: payload["error"]["message"]
                    .as_str()
                    .unwrap_or("the desktop executor reported an error")
                    .chars()
                    .take(4096)
                    .collect(),
            }),
            None => return Err("executor.result needs ok".to_owned()),
        };
        let mut pending = self.inner.pending.lock().expect("executor pending lock");
        match pending.get(invoke_id) {
            Some(call) if call.executor_id == executor_id => {}
            // Late (timed out, cancelled) or foreign results are not errors
            // the executor can act on.
            _ => return Ok(()),
        }
        let call = pending.remove(invoke_id).expect("checked above");
        let _ = call.sender.send(result);
        Ok(())
    }
}

struct PendingGuard<'a> {
    executors: &'a Executors,
    invoke_id: &'a str,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.executors
            .inner
            .pending
            .lock()
            .expect("executor pending lock")
            .remove(self.invoke_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(conversation: &str, tool: &str, args: Value) -> InvokeRequest {
        InvokeRequest {
            conversation_id: conversation.to_owned(),
            workspace: json!({ "path": "/w" }),
            tool: tool.to_owned(),
            args,
        }
    }

    fn register(executors: &Executors, device: &str) -> (Registration, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(8);
        let registration = executors.register(
            device.to_owned(),
            format!("{device} Mac"),
            "darwin".to_owned(),
            vec![CAPABILITY_BROWSER.to_owned()],
            tx,
        );
        (registration, rx)
    }

    #[tokio::test]
    async fn invoke_round_trips_and_rejects_foreign_results() {
        let executors = Executors::default();
        let (a, mut a_rx) = register(&executors, "dev_a");
        let (b, _b_rx) = register(&executors, "dev_b");
        assert_eq!(executors.online(CAPABILITY_BROWSER)[0].device_id, "dev_b");
        assert_eq!(
            executors
                .for_device("dev_a", CAPABILITY_BROWSER)
                .unwrap()
                .executor_id,
            a.executor_id()
        );
        assert!(executors.for_device("dev_a", "screen").is_none());

        let call = {
            let executors = executors.clone();
            let id = a.executor_id();
            tokio::spawn(async move {
                executors
                    .invoke(
                        id,
                        request(
                            "conv",
                            "browser_open",
                            json!({ "url": "http://localhost:1" }),
                        ),
                        Duration::from_secs(5),
                        std::future::pending(),
                    )
                    .await
            })
        };
        let frame = a_rx.recv().await.unwrap();
        assert_eq!(frame["type"], "executor.invoke");
        assert_eq!(frame["payload"]["tool"], "browser_open");
        assert_eq!(frame["payload"]["conversationId"], "conv");
        let invoke_id = frame["payload"]["invokeId"].as_str().unwrap().to_owned();

        // Another connection cannot answer a call it was not sent.
        executors
            .complete(
                b.executor_id(),
                &json!({ "invokeId": invoke_id, "ok": true, "result": "forged" }),
            )
            .unwrap();
        executors
            .complete(
                a.executor_id(),
                &json!({ "invokeId": invoke_id, "ok": true, "result": { "title": "x" } }),
            )
            .unwrap();
        assert_eq!(call.await.unwrap(), Ok(json!({ "title": "x" })));
        assert!(executors.inner.pending.lock().unwrap().is_empty());
        assert!(executors
            .complete(a.executor_id(), &json!({ "ok": true }))
            .is_err());
    }

    #[tokio::test]
    async fn executor_errors_timeouts_cancels_and_disconnects() {
        let executors = Executors::default();
        let (a, mut a_rx) = register(&executors, "dev_a");
        let id = a.executor_id();

        let failing = {
            let executors = executors.clone();
            tokio::spawn(async move {
                executors
                    .invoke(
                        id,
                        request("c", "t", json!({})),
                        Duration::from_secs(5),
                        std::future::pending(),
                    )
                    .await
            })
        };
        let invoke_id = a_rx.recv().await.unwrap()["payload"]["invokeId"].clone();
        executors
            .complete(
                id,
                &json!({ "invokeId": invoke_id, "ok": false, "error": { "code": "NAV_BLOCKED", "message": "no" } }),
            )
            .unwrap();
        let error = failing.await.unwrap().unwrap_err();
        assert_eq!(error.code(), "NAV_BLOCKED");

        let timeout = executors
            .invoke(
                id,
                request("c", "t", json!({})),
                Duration::from_millis(20),
                std::future::pending(),
            )
            .await;
        assert_eq!(timeout, Err(ExecutorError::Timeout(0)));
        assert_eq!(a_rx.recv().await.unwrap()["type"], "executor.invoke");
        assert_eq!(a_rx.recv().await.unwrap()["type"], "executor.cancel");

        let cancelled = executors
            .invoke(
                id,
                request("c", "t", json!({})),
                Duration::from_secs(5),
                std::future::ready(()),
            )
            .await;
        assert_eq!(cancelled, Err(ExecutorError::Cancelled));

        let waiting = {
            let executors = executors.clone();
            tokio::spawn(async move {
                executors
                    .invoke(
                        id,
                        request("c", "t", json!({})),
                        Duration::from_secs(5),
                        std::future::pending(),
                    )
                    .await
            })
        };
        while a_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(a);
        assert_eq!(waiting.await.unwrap(), Err(ExecutorError::Gone));
        assert!(executors.online(CAPABILITY_BROWSER).is_empty());
        assert_eq!(
            executors
                .invoke(
                    id,
                    request("c", "t", json!({})),
                    Duration::from_secs(1),
                    std::future::pending()
                )
                .await,
            Err(ExecutorError::Gone)
        );
    }

    #[test]
    fn capabilities_are_validated() {
        assert_eq!(
            validate_capabilities(&["browser".into(), "browser".into()]).unwrap(),
            vec!["browser".to_owned()]
        );
        assert!(validate_capabilities(&["screen".into()]).is_err());
    }
}
