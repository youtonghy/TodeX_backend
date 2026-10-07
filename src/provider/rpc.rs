//! JSON-RPC request/response plumbing shared by the provider drivers.
//!
//! [`RpcClient`] runs out-of-turn exchanges (handshakes, capability probes,
//! discovery, forks): it sends a request, waits for the matching response
//! under one deadline, declines agent→client requests nobody can serve and
//! optionally collects `session/update` notifications. In-turn loops, which
//! also stream events and answer permission prompts, share the response
//! matching and error mapping through [`RpcPeer::outcome`].

use serde_json::{json, Value};
use tokio::time::{Duration, Instant};

use crate::error::AppError;

use super::process::{
    control_timeout, provider_exit_error, redact_sensitive_text, JsonLineProcess,
};

/// Longest provider error text carried into an error or the journal.
const MAX_ERROR_CHARS: usize = 500;

/// Provider-supplied text, safe to journal and display: trimmed, secrets
/// redacted, at most [`MAX_ERROR_CHARS`] characters.
pub(crate) fn safe_text(text: &str) -> String {
    redact_sensitive_text(text.trim())
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

/// [`safe_text`] of a JSON-RPC error: its `message`, or the error itself
/// when a provider sends a bare string.
pub(crate) fn safe_error_text(error: &Value) -> String {
    safe_text(
        error
            .as_str()
            .or_else(|| error.get("message").and_then(Value::as_str))
            .unwrap_or("provider returned an error"),
    )
}

/// Whether `message` carries the response id `expected` (string or number).
pub(super) fn id_matches(message: &Value, expected: &str) -> bool {
    message.get("id").is_some_and(|id| match id {
        Value::String(value) => value == expected,
        Value::Number(value) => value.to_string() == expected,
        _ => false,
    })
}

/// A request id no earlier request on the same process used. When a wait is
/// abandoned (timeout, cancellation) the late response stays queued on
/// stdout; a reused id would let the next wait take it as its own answer.
pub(super) fn fresh_id(method: &str) -> String {
    format!("{method}:{}", uuid::Uuid::new_v4().simple())
}

/// How errors name a failed request.
#[derive(Clone, Copy, Debug)]
pub(super) enum FailureWording {
    /// `"<peer> <method> failed: …"`.
    Method,
    /// `"<peer> request <id> failed: …"`.
    RequestId,
    /// `"<peer> request failed: …"`.
    Request,
}

/// How one provider speaks JSON-RPC.
#[derive(Clone, Copy, Debug)]
pub(super) struct RpcPeer {
    /// The provider as named in errors.
    pub name: &'static str,
    pub failure: FailureWording,
    /// Reported, with the stderr tail, when stdout closes mid-request.
    pub closed: &'static str,
    /// Agent→client requests arriving while waiting are declined with this
    /// message; `None` leaves them unanswered.
    pub decline: Option<&'static str>,
    /// Whether frames carry `"jsonrpc": "2.0"` (Codex app-server omits it).
    pub jsonrpc_field: bool,
    /// Whether a response without `result` means `null`.
    pub result_optional: bool,
    /// Maps provider-specific errors before the generic failure.
    pub classify_error: Option<fn(&Value) -> Option<AppError>>,
}

impl RpcPeer {
    /// The outcome of `message` if it answers request `id`.
    pub(super) fn outcome(
        &self,
        message: &Value,
        id: &str,
        method: &str,
    ) -> Option<Result<Value, AppError>> {
        if !id_matches(message, id) {
            return None;
        }
        if let Some(error) = message.get("error") {
            if let Some(mapped) = self.classify_error.and_then(|classify| classify(error)) {
                return Some(Err(mapped));
            }
            let detail = safe_error_text(error);
            let name = self.name;
            return Some(Err(AppError::ProviderUnavailable(match self.failure {
                FailureWording::Method => format!("{name} {method} failed: {detail}"),
                FailureWording::RequestId => format!("{name} request {id} failed: {detail}"),
                FailureWording::Request => format!("{name} request failed: {detail}"),
            })));
        }
        Some(match message.get("result") {
            Some(result) => Ok(result.clone()),
            None if self.result_optional => Ok(Value::Null),
            None => Err(AppError::InvalidRequest(format!(
                "{} {method} response has no result",
                self.name
            ))),
        })
    }

    fn request_frame(&self, id: &str, method: &str, params: Value) -> Value {
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if self.jsonrpc_field {
            frame["jsonrpc"] = json!("2.0");
        }
        frame
    }

    fn decline_frame(&self, id: &Value, message: &str) -> Value {
        let mut frame = json!({ "id": id, "error": { "code": -32601, "message": message } });
        if self.jsonrpc_field {
            frame["jsonrpc"] = json!("2.0");
        }
        frame
    }
}

/// Out-of-turn requests to one provider process.
pub(super) struct RpcClient<'a> {
    process: &'a mut JsonLineProcess,
    peer: RpcPeer,
}

impl<'a> RpcClient<'a> {
    pub(super) fn new(process: &'a mut JsonLineProcess, peer: RpcPeer) -> Self {
        Self { process, peer }
    }

    /// Sends `method` as request `id` and waits for its result. `timeout`
    /// defaults to the configurable provider control timeout.
    pub(super) async fn request(
        &mut self,
        id: &str,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AppError> {
        self.send(id, method, params).await?;
        self.response(id, method, timeout, None).await
    }

    /// [`Self::request`] that also collects the `params` of every
    /// `session/update` notification received while waiting.
    pub(super) async fn request_collecting(
        &mut self,
        id: &str,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        updates: &mut Vec<Value>,
    ) -> Result<Value, AppError> {
        self.send(id, method, params).await?;
        self.response(id, method, timeout, Some(updates)).await
    }

    pub(super) async fn send(
        &mut self,
        id: &str,
        method: &str,
        params: Value,
    ) -> Result<(), AppError> {
        self.process
            .send(&self.peer.request_frame(id, method, params))
            .await
    }

    /// Waits for the response to an already sent request `id`.
    pub(super) async fn response(
        &mut self,
        id: &str,
        method: &str,
        timeout: Option<Duration>,
        mut updates: Option<&mut Vec<Value>>,
    ) -> Result<Value, AppError> {
        let timeout = match timeout {
            Some(timeout) => timeout,
            None => control_timeout()?,
        };
        let deadline = Instant::now() + timeout;
        loop {
            let message = tokio::time::timeout_at(deadline, self.process.read())
                .await
                .map_err(|_| {
                    AppError::ProviderUnavailable(format!("{} {method} timed out", self.peer.name))
                })??;
            let Some(message) = message else {
                return Err(provider_exit_error(self.process, self.peer.closed).await);
            };
            if message.is_null() {
                continue;
            }
            if let Some(outcome) = self.peer.outcome(&message, id, method) {
                return outcome;
            }
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                continue;
            };
            if let Some(request_id) = message.get("id") {
                if let Some(decline) = self.peer.decline {
                    self.process
                        .send(&self.peer.decline_frame(request_id, decline))
                        .await?;
                }
            } else if method == "session/update" {
                if let Some(updates) = updates.as_deref_mut() {
                    updates.push(message.get("params").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: RpcPeer = RpcPeer {
        name: "Fixture",
        failure: FailureWording::Method,
        closed: "Fixture closed stdout",
        decline: Some("not supported here"),
        jsonrpc_field: true,
        result_optional: false,
        classify_error: None,
    };

    #[test]
    fn error_text_is_redacted_bounded_and_accepts_bare_strings() {
        let long = format!("Bearer secret-token {}", "x".repeat(700));
        let text = safe_error_text(&json!({ "message": long }));
        assert!(!text.contains("secret-token"));
        assert_eq!(text.chars().count(), MAX_ERROR_CHARS);
        assert_eq!(safe_error_text(&json!("plain")), "plain");
        assert_eq!(safe_error_text(&json!({})), "provider returned an error");
    }

    #[test]
    fn outcomes_match_ids_and_name_failures_per_peer() {
        assert!(PEER
            .outcome(&json!({"id":"b","result":1}), "a", "m")
            .is_none());
        assert_eq!(
            PEER.outcome(&json!({"id":7,"result":1}), "7", "m")
                .unwrap()
                .unwrap(),
            json!(1)
        );
        let failed = PEER
            .outcome(
                &json!({"id":"a","error":{"message":"boom"}}),
                "a",
                "session/new",
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(
            failed.to_string(),
            "provider unavailable: Fixture session/new failed: boom"
        );
        let by_id = RpcPeer {
            failure: FailureWording::RequestId,
            result_optional: true,
            ..PEER
        };
        assert!(by_id
            .outcome(&json!({"id":"a","error":{"message":"boom"}}), "a", "m")
            .unwrap()
            .unwrap_err()
            .to_string()
            .ends_with("Fixture request a failed: boom"));
        assert_eq!(
            by_id
                .outcome(&json!({"id":"a"}), "a", "m")
                .unwrap()
                .unwrap(),
            Value::Null
        );
        assert!(PEER.outcome(&json!({"id":"a"}), "a", "m").unwrap().is_err());
    }

    #[test]
    fn codex_frames_omit_the_jsonrpc_field() {
        let codex = RpcPeer {
            jsonrpc_field: false,
            ..PEER
        };
        assert!(codex
            .request_frame("i", "initialize", json!({}))
            .get("jsonrpc")
            .is_none());
        assert_eq!(
            PEER.request_frame("i", "initialize", json!({}))["jsonrpc"],
            "2.0"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_declines_agent_requests_and_collects_updates() {
        use super::super::process::CommandSpec;
        // Echo an agent request and an update before answering request "q".
        let mut spec = CommandSpec::new("/bin/sh", std::env::temp_dir());
        spec.args = vec![
            "-c".to_owned(),
            r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":"agent-1","method":"fs/read_text_file","params":{}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"n":1}}'
read decline
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":\"q\",\"result\":{\"declined\":$decline}}"
"#
            .to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await.unwrap();
        let mut updates = Vec::new();
        let result = RpcClient::new(&mut process, PEER)
            .request_collecting("q", "session/new", json!({}), None, &mut updates)
            .await
            .unwrap();
        process.terminate().await;
        assert_eq!(updates, vec![json!({"n":1})]);
        assert_eq!(result["declined"]["id"], "agent-1");
        assert_eq!(result["declined"]["error"]["message"], "not supported here");
    }
}
