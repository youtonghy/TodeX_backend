//! Bearer API key authentication for `/api/v1`.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap},
    middleware::Next,
    response::Response,
};

use super::ApiKeyContext;
use crate::app_state::AppState;
use crate::error::AppError;

/// Failed authentications one address may make per window before it is
/// refused outright.
const MAX_FAILURES: u32 = 30;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Bounds the table against address churn; it is cleared when full.
const MAX_TRACKED_ADDRESSES: usize = 4096;

/// Failed API key authentications per peer address.
#[derive(Clone, Default)]
pub(crate) struct AuthFailures {
    inner: Arc<Mutex<HashMap<IpAddr, (Instant, u32)>>>,
}

impl AuthFailures {
    fn check(&self, address: Option<IpAddr>) -> Result<(), AppError> {
        let Some(address) = address else {
            return Ok(());
        };
        let failures = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match failures.get(&address) {
            Some((start, count)) if *count >= MAX_FAILURES => {
                let elapsed = start.elapsed();
                if elapsed < FAILURE_WINDOW {
                    return Err(AppError::RateLimited {
                        message: "too many failed API key attempts".to_owned(),
                        retry_after_secs: (FAILURE_WINDOW - elapsed).as_secs().max(1),
                    });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn record(&self, address: Option<IpAddr>) {
        let Some(address) = address else {
            return;
        };
        let mut failures = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failures.len() >= MAX_TRACKED_ADDRESSES && !failures.contains_key(&address) {
            failures.retain(|_, (start, _)| start.elapsed() < FAILURE_WINDOW);
            if failures.len() >= MAX_TRACKED_ADDRESSES {
                failures.clear();
            }
        }
        let entry = failures.entry(address).or_insert((Instant::now(), 0));
        if entry.0.elapsed() >= FAILURE_WINDOW {
            *entry = (Instant::now(), 0);
        }
        entry.1 += 1;
    }
}

/// `Authorization: Bearer <key>` or `X-API-Key: <key>`.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        let (scheme, token) = value.trim().split_once(' ')?;
        return scheme
            .eq_ignore_ascii_case("bearer")
            .then_some(token.trim());
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

pub(super) async fn require_api_key(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let address = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    state.api_auth_failures.check(address)?;
    let authenticated = presented_key(request.headers())
        .ok_or(AppError::Unauthenticated)
        .and_then(|token| state.api_keys.authenticate(token));
    let key = match authenticated {
        Ok(key) => key,
        Err(error) => {
            state.api_auth_failures.record(address);
            return Err(match error {
                AppError::Unauthenticated => AppError::Unauthenticated,
                other => {
                    tracing::warn!(error = %other, "API key store is unreadable");
                    AppError::Unauthenticated
                }
            });
        }
    };
    request.extensions_mut().insert(ApiKeyContext {
        record: key.record,
        seed: Arc::new(key.seed),
    });
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_limited_per_address_and_window() {
        let failures = AuthFailures::default();
        let address = Some(IpAddr::from([10, 0, 0, 1]));
        for _ in 0..MAX_FAILURES {
            assert!(failures.check(address).is_ok());
            failures.record(address);
        }
        assert!(matches!(
            failures.check(address),
            Err(AppError::RateLimited { .. })
        ));
        assert!(failures.check(Some(IpAddr::from([10, 0, 0, 2]))).is_ok());
    }

    #[test]
    fn keys_are_read_from_bearer_or_api_key_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer  tdx_abc ".parse().unwrap());
        assert_eq!(presented_key(&headers), Some("tdx_abc"));
        headers.insert(header::AUTHORIZATION, "Basic xyz".parse().unwrap());
        assert_eq!(presented_key(&headers), None);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "tdx_def".parse().unwrap());
        assert_eq!(presented_key(&headers), Some("tdx_def"));
    }
}
