//! AGY ends an ACP prompt before its background commands and model follow-ups
//! finish. Its embedded language server provides the completion boundary:
//! WaitForConversationFullyIdle. Discover the loopback port from this process's
//! private startup log, not from another Antigravity instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

use crate::error::AppError;

const STARTUP_LOG_BYTES: u64 = 64 * 1024;
const INACTIVITY_SECONDS: u64 = 1;
const STABILIZATION_SECONDS: u64 = 1;

#[derive(Debug)]
pub(in crate::provider) struct NativeIdle {
    directory: PathBuf,
    client: reqwest::Client,
    port: OnceCell<u16>,
}

impl NativeIdle {
    pub(super) fn new() -> Result<Arc<Self>, AppError> {
        // The server may hold response headers while the root model runs.
        // Do not impose a total turn timeout here: ACP cancellation, process
        // exit and the supervisor's inactivity watchdog remain in force.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| unavailable(format!("could not create status client: {error}")))?;
        let directory = std::env::temp_dir().join(format!(
            "todex-agy-runtime-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.recursive(false).create(&directory)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Err(error) = options.open(directory.join("acp.log")) {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(AppError::Io(error));
        }
        Ok(Arc::new(Self {
            directory,
            client,
            port: OnceCell::new(),
        }))
    }

    pub(super) fn log_path(&self) -> PathBuf {
        self.directory.join("acp.log")
    }

    /// `true` means AGY's native waiter timed out while work remained active.
    pub(in crate::provider) async fn wait(&self, session_id: &str) -> Result<bool, AppError> {
        let log_path = self.log_path();
        let port = self
            .port
            .get_or_try_init(|| async { read_port(&log_path).await })
            .await?;
        let url = format!(
            "http://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/WaitForConversationFullyIdle"
        );
        let response = self
            .client
            .post(url)
            .header("connect-protocol-version", "1")
            .json(&json!({
                "conversationId": session_id,
                "inactivityTimeoutSeconds": INACTIVITY_SECONDS,
                "stabilizationDurationSeconds": STABILIZATION_SECONDS,
                "returnOnExecutorError": false,
            }))
            .send()
            .await
            .map_err(|error| unavailable(format!("request failed: {error}")))?;
        if !response.status().is_success() {
            return Err(unavailable(format!(
                "request returned HTTP {}",
                response.status()
            )));
        }
        let body = tokio::time::timeout(Duration::from_secs(5), async {
            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| unavailable(format!("could not read response: {error}")))?
            {
                if body.len() + chunk.len() > 4096 {
                    return Err(unavailable("response exceeded 4096 bytes"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
        .await
        .map_err(|_| unavailable("response body timed out"))??;
        let response: IdleResponse = serde_json::from_slice(&body)
            .map_err(|error| unavailable(format!("invalid response: {error}")))?;
        Ok(response.timed_out)
    }
}

/// Protobuf JSON omits false fields, so `{}` is a successful idle result.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdleResponse {
    #[serde(default)]
    timed_out: bool,
}

async fn read_port(log: &Path) -> Result<u16, AppError> {
    let file = tokio::fs::File::open(log).await?;
    let mut bytes = Vec::new();
    file.take(STARTUP_LOG_BYTES).read_to_end(&mut bytes).await?;
    http_port(&String::from_utf8_lossy(&bytes))
        .ok_or_else(|| unavailable("the process did not announce its native HTTP port"))
}

fn http_port(log: &str) -> Option<u16> {
    log.lines().find_map(|line| {
        let (_, rest) = line.split_once("Language server listening on random port at ")?;
        let port = rest.strip_suffix(" for HTTP")?.trim().parse::<u16>().ok()?;
        (port != 0).then_some(port)
    })
}

fn unavailable(detail: impl std::fmt::Display) -> AppError {
    AppError::ProviderUnavailable(format!(
        "Antigravity could not confirm completion: {detail}"
    ))
}

impl Drop for NativeIdle {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            tracing::warn!(%error, "could not remove Antigravity runtime log directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_announced_http_port_is_used() {
        assert_eq!(
            http_port(
                "Language server listening on random port at 64474 for HTTPS (gRPC)\nI1117 server.go:1315] Language server listening on random port at 64475 for HTTP\n"
            ),
            Some(64475)
        );
        for log in [
            "64475",
            "Language server listening on random port at 0 for HTTP",
            "Language server listening on random port at 65536 for HTTP",
            "Language server listening on random port at example.com for HTTP",
        ] {
            assert_eq!(http_port(log), None);
        }
    }

    #[test]
    fn an_empty_response_means_idle_and_timeout_means_still_active() {
        assert!(
            !serde_json::from_str::<IdleResponse>("{}")
                .unwrap()
                .timed_out
        );
        assert!(
            serde_json::from_str::<IdleResponse>(r#"{"timedOut":true}"#)
                .unwrap()
                .timed_out
        );
        assert!(
            !serde_json::from_str::<IdleResponse>(r#"{"timedOut":false}"#)
                .unwrap()
                .timed_out
        );
    }
}
