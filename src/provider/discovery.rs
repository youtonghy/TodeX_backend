//! Caches for provider discovery: model and command catalogs, capability
//! probes and CLI self-inspection.
//!
//! Discovery spawns the provider CLI (or calls its gateway), which takes
//! seconds; clients ask for the same catalogs on every refresh, often several
//! at once. [`DiscoveryCache`] keeps each result for a TTL and runs at most
//! one fetch per key at a time (single-flight): concurrent callers for the
//! same key wait for the running fetch, other keys proceed in parallel.
//! A failure is handed to the callers that waited for it but not cached
//! for later ones. [`DiscoveryCache::get_or_refresh`] additionally serves
//! an expired value at once and refreshes it in the background
//! (stale-while-revalidate), for lookups on a turn's critical path.
//!
//! Keys carry what invalidates a result besides time: the workspace, the
//! managed provider configuration revision
//! ([`crate::agent_providers::config_revision`]) and the installed CLI's
//! [`ExecutableStamp`], so an account switch or a CLI upgrade is picked up
//! immediately.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::AppError;

use super::process::ExecutableStamp;
use super::types::{ProviderCommandDescriptor, ProviderModelDescriptor};

/// Keys kept before expired entries are swept on insert.
const SWEEP_THRESHOLD: usize = 64;

struct Entry<V> {
    value: V,
    expires: Instant,
}

/// What a fetch for one key left behind.
struct Stored<V> {
    entry: Option<Entry<V>>,
    /// The latest failure and when it happened. Shared only with callers
    /// that were already waiting for that fetch, never with later ones.
    failure: Option<(Instant, AppError)>,
}

impl<V> Default for Stored<V> {
    fn default() -> Self {
        Self {
            entry: None,
            failure: None,
        }
    }
}

struct SlotState<V> {
    /// Read without waiting, so a running fetch never hides the last value.
    stored: Mutex<Stored<V>>,
    /// Held by the running fetch: one fetch per key at a time. Shared so a
    /// background refresh can hold it past its caller.
    flight: Arc<tokio::sync::Mutex<()>>,
}

type Slot<V> = Arc<SlotState<V>>;

impl<V: Clone> SlotState<V> {
    fn stored(&self) -> std::sync::MutexGuard<'_, Stored<V>> {
        self.stored
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fresh(&self, now: Instant) -> Option<V> {
        self.stored()
            .entry
            .as_ref()
            .filter(|entry| entry.expires > now)
            .map(|entry| entry.value.clone())
    }
}

pub(super) struct DiscoveryCache<K, V> {
    slots: Mutex<HashMap<K, Slot<V>>>,
}

impl<K, V> Default for DiscoveryCache<K, V> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Clone + Eq + Hash, V: Clone> DiscoveryCache<K, V> {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn existing(&self, key: &K) -> Option<Slot<V>> {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }

    fn slot(&self, key: &K) -> Slot<V> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = slots.get(key) {
            return slot.clone();
        }
        if slots.len() >= SWEEP_THRESHOLD {
            let now = Instant::now();
            // Idle, expired slots only: a slot someone holds is in use.
            slots.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || slot
                        .stored()
                        .entry
                        .as_ref()
                        .is_some_and(|entry| entry.expires > now)
            });
        }
        slots
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(SlotState {
                    stored: Mutex::new(Stored::default()),
                    flight: Arc::new(tokio::sync::Mutex::new(())),
                })
            })
            .clone()
    }

    /// The cached value for `key`, or the result of `fetch`, which returns
    /// the value with its time to live. Callers that waited for a fetch that
    /// failed get its error instead of running the fetch again; if the
    /// running fetch is cancelled, the next waiter fetches.
    pub(super) async fn get_or_fetch<F, Fut>(&self, key: &K, fetch: F) -> Result<V, AppError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(V, Duration), AppError>>,
    {
        let slot = self.slot(key);
        let waiting_since = Instant::now();
        if let Some(value) = slot.fresh(waiting_since) {
            return Ok(value);
        }
        let _flight = slot.flight.lock().await;
        {
            let stored = slot.stored();
            if let Some(entry) = stored
                .entry
                .as_ref()
                .filter(|entry| entry.expires > Instant::now())
            {
                return Ok(entry.value.clone());
            }
            if let Some((_, error)) = stored
                .failure
                .as_ref()
                .filter(|(failed_at, _)| *failed_at >= waiting_since)
            {
                return Err(shared_error(error));
            }
        }
        store_fetched(&slot, fetch()).await
    }

    /// Like [`Self::get_or_fetch`], but an expired value is returned at once
    /// while one background task refreshes it, and a fetch always runs on
    /// its own task: a caller that stops waiting (a timeout) does not cancel
    /// it, so the next lookup finds its result.
    pub(super) async fn get_or_refresh<F, Fut>(&self, key: &K, fetch: F) -> Result<V, AppError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(V, Duration), AppError>> + Send + 'static,
        V: Send + 'static,
    {
        let slot = self.slot(key);
        let waiting_since = Instant::now();
        let stale = slot
            .stored()
            .entry
            .as_ref()
            .map(|entry| (entry.value.clone(), entry.expires > waiting_since));
        if let Some((value, fresh)) = stale {
            if !fresh {
                // Already refreshing when the flight is taken.
                if let Ok(flight) = slot.flight.clone().try_lock_owned() {
                    let refresh = fetch();
                    let slot = slot.clone();
                    tokio::spawn(async move {
                        if let Err(error) = store_fetched(&slot, refresh).await {
                            tracing::warn!(%error, "background catalog refresh failed; keeping the previous catalog");
                        }
                        drop(flight);
                    });
                }
            }
            return Ok(value);
        }
        let flight = slot.flight.clone().lock_owned().await;
        {
            let stored = slot.stored();
            // Whatever the fetch this caller waited for stored, even if it
            // already expired: the next lookup refreshes it.
            if let Some(entry) = stored.entry.as_ref() {
                return Ok(entry.value.clone());
            }
            if let Some((_, error)) = stored
                .failure
                .as_ref()
                .filter(|(failed_at, _)| *failed_at >= waiting_since)
            {
                return Err(shared_error(error));
            }
        }
        let fetching = fetch();
        let task_slot = slot.clone();
        tokio::spawn(async move {
            let result = store_fetched(&task_slot, fetching).await;
            drop(flight);
            result
        })
        .await
        .unwrap_or_else(|error| {
            Err(AppError::ProviderUnavailable(format!(
                "catalog fetch failed: {error}"
            )))
        })
    }

    /// The last value stored for `key`, even if it expired or a fetch for it
    /// is running.
    pub(super) fn peek(&self, key: &K) -> Option<V> {
        self.existing(key)?
            .stored()
            .entry
            .as_ref()
            .map(|entry| entry.value.clone())
    }

    /// Whether `key` holds an unexpired value.
    pub(super) fn is_fresh(&self, key: &K) -> bool {
        self.existing(key)
            .is_some_and(|slot| slot.fresh(Instant::now()).is_some())
    }

    /// Stores `value` for `key` outside a fetch (a result learned on the way).
    pub(super) fn insert(&self, key: &K, value: V, ttl: Duration) {
        self.slot(key).stored().entry = Some(Entry {
            value,
            expires: Instant::now() + ttl,
        });
    }
}

/// Runs `fetch` and records its value or failure in `slot`; the caller
/// holds the slot's flight.
async fn store_fetched<V: Clone>(
    slot: &SlotState<V>,
    fetch: impl Future<Output = Result<(V, Duration), AppError>>,
) -> Result<V, AppError> {
    match fetch.await {
        Ok((value, ttl)) => {
            let mut stored = slot.stored();
            stored.entry = Some(Entry {
                value: value.clone(),
                expires: Instant::now() + ttl,
            });
            stored.failure = None;
            Ok(value)
        }
        Err(error) => {
            slot.stored().failure = Some((Instant::now(), shared_error(&error)));
            Err(error)
        }
    }
}

/// A copy of `error` for callers that shared the failed fetch. `AppError` is
/// not `Clone` (it wraps I/O errors); variants a fetch reports are copied
/// as-is, anything else keeps its message as a provider failure.
fn shared_error(error: &AppError) -> AppError {
    match error {
        AppError::ProviderUnavailable(detail) => AppError::ProviderUnavailable(detail.clone()),
        AppError::Unsupported(detail) => AppError::Unsupported(detail.clone()),
        AppError::InvalidRequest(detail) => AppError::InvalidRequest(detail.clone()),
        AppError::Conflict(detail) => AppError::Conflict(detail.clone()),
        AppError::NotFound(detail) => AppError::NotFound(detail.clone()),
        AppError::Unauthorized(detail) => AppError::Unauthorized(detail.clone()),
        AppError::ResourceExhausted(detail) => AppError::ResourceExhausted(detail.clone()),
        AppError::Unauthenticated => AppError::Unauthenticated,
        AppError::TurnCancelled => AppError::TurnCancelled,
        other => AppError::ProviderUnavailable(other.to_string()),
    }
}

/// A sweep cut short by its budget is still cached briefly: re-probing on
/// every query made each request pay the full probe under load, while a short
/// lifetime still lets a later query fill in what the sweep missed.
const PARTIAL_DISCOVERY_TTL: Duration = Duration::from_secs(60);

/// One ACP discovery probe yields both the model and the command catalog, so
/// ACP drivers cache them together per workspace.
#[derive(Clone)]
pub(super) struct DiscoverySnapshot {
    pub(super) models: Vec<ProviderModelDescriptor>,
    pub(super) commands: Vec<ProviderCommandDescriptor>,
    /// Whether every per-model option probe answered.
    pub(super) complete: bool,
}

impl DiscoverySnapshot {
    /// How long to keep this snapshot when a complete one lives `ttl`.
    pub(super) fn ttl(&self, ttl: Duration) -> Duration {
        if self.complete {
            ttl
        } else {
            ttl.min(PARTIAL_DISCOVERY_TTL)
        }
    }
}

/// What a workspace-scoped catalog depends on.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct DiscoveryKey {
    workspace: PathBuf,
    revision: u64,
    cli: Option<ExecutableStamp>,
}

impl DiscoveryKey {
    /// The key for `binary` run in `workspace` under the current managed
    /// configuration. Resolving the CLI walks PATH and stats the file, so it
    /// runs on the blocking pool.
    pub(super) async fn new(binary: &str, workspace: &Path) -> Self {
        let program = binary.to_owned();
        let cli = tokio::task::spawn_blocking(move || super::process::executable_stamp(&program))
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(error = %error, "CLI stamp lookup failed");
                None
            });
        Self {
            workspace: workspace.to_path_buf(),
            revision: crate::agent_providers::config_revision(),
            cli,
        }
    }
}

/// One provider catalog (models, commands, ...) cached per [`DiscoveryKey`]
/// for the provider profile's `discovery_cache_ttl`; without a TTL every
/// query fetches.
pub(super) struct CatalogCache<V> {
    cache: DiscoveryCache<DiscoveryKey, V>,
    ttl: Option<Duration>,
}

impl<V: Clone> CatalogCache<V> {
    pub(super) fn new(ttl: Option<Duration>) -> Self {
        Self {
            cache: DiscoveryCache::new(),
            ttl,
        }
    }

    /// The cached catalog of `binary` in `workspace`, or `fetch`'s result.
    pub(super) async fn fetch(
        &self,
        binary: &str,
        workspace: &Path,
        fetch: impl Future<Output = Result<V, AppError>>,
    ) -> Result<V, AppError> {
        let Some(ttl) = self.ttl else {
            return fetch.await;
        };
        self.cache
            .get_or_fetch(&DiscoveryKey::new(binary, workspace).await, || async move {
                Ok((fetch.await?, ttl))
            })
            .await
    }

    /// [`Self::fetch`] with stale-while-revalidate
    /// ([`DiscoveryCache::get_or_refresh`]): an expired catalog is returned
    /// at once and refreshed in the background.
    pub(super) async fn fetch_or_stale(
        &self,
        binary: &str,
        workspace: &Path,
        fetch: impl Future<Output = Result<V, AppError>> + Send + 'static,
    ) -> Result<V, AppError>
    where
        V: Send + 'static,
    {
        let Some(ttl) = self.ttl else {
            return fetch.await;
        };
        self.cache
            .get_or_refresh(&DiscoveryKey::new(binary, workspace).await, || async move {
                Ok((fetch.await?, ttl))
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn concurrent_callers_share_one_fetch_and_errors_are_not_cached() {
        let cache = Arc::new(DiscoveryCache::<&'static str, usize>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |calls: Arc<AtomicUsize>| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((
                calls.fetch_add(1, Ordering::SeqCst),
                Duration::from_secs(60),
            ))
        };
        let (a, b) = tokio::join!(
            cache.get_or_fetch(&"k", || fetch(calls.clone())),
            cache.get_or_fetch(&"k", || fetch(calls.clone())),
        );
        assert_eq!((a.unwrap(), b.unwrap()), (0, 0));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cache.is_fresh(&"k"));

        let failed = cache
            .get_or_fetch(&"other", || async {
                Err::<(usize, Duration), _>(AppError::ProviderUnavailable("down".to_owned()))
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(cache.peek(&"other"), None);
        assert_eq!(
            cache
                .get_or_fetch(&"other", || async { Ok((7, Duration::ZERO)) })
                .await
                .unwrap(),
            7
        );
        // Expired values are refetched but still peekable.
        assert!(!cache.is_fresh(&"other"));
        assert_eq!(cache.peek(&"other"), Some(7));
        assert_eq!(
            cache
                .get_or_fetch(&"other", || async { Ok((8, Duration::from_secs(1))) })
                .await
                .unwrap(),
            8
        );
    }

    #[tokio::test]
    async fn different_keys_fetch_in_parallel() {
        let cache = DiscoveryCache::<u8, u8>::new();
        let started = Instant::now();
        let slow = |value: u8| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok((value, Duration::from_secs(60)))
        };
        let (a, b) = tokio::join!(
            cache.get_or_fetch(&1, || slow(1)),
            cache.get_or_fetch(&2, || slow(2)),
        );
        assert_eq!((a.unwrap(), b.unwrap()), (1, 2));
        assert!(started.elapsed() < Duration::from_millis(190));
    }

    #[tokio::test]
    async fn keys_follow_the_managed_config_revision() {
        let workspace = Path::new("/tmp");
        let before = DiscoveryKey::new("/nonexistent/cli", workspace).await;
        crate::agent_providers::bump_config_revision();
        assert_ne!(
            before,
            DiscoveryKey::new("/nonexistent/cli", workspace).await
        );
    }

    #[tokio::test]
    async fn peek_reads_the_stored_value_while_a_refresh_runs() {
        let cache = Arc::new(DiscoveryCache::<&'static str, bool>::new());
        cache.insert(&"k", true, Duration::ZERO);
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let refresh = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(&"k", || async move {
                        let _ = held.await;
                        Ok((false, Duration::from_secs(60)))
                    })
                    .await
            }
        });
        while !cache
            .existing(&"k")
            .is_some_and(|slot| slot.flight.try_lock().is_err())
        {
            tokio::task::yield_now().await;
        }
        assert_eq!(cache.peek(&"k"), Some(true));
        assert!(!cache.is_fresh(&"k"));
        release.send(()).unwrap();
        assert!(!refresh.await.unwrap().unwrap());
        assert_eq!(cache.peek(&"k"), Some(false));
    }

    #[tokio::test]
    async fn waiters_share_a_failure_but_later_callers_fetch_again() {
        let cache = Arc::new(DiscoveryCache::<&'static str, usize>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let failing = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err::<(usize, Duration), _>(AppError::Unsupported("no catalog".to_owned()))
        };
        let (a, b, c) = tokio::join!(
            cache.get_or_fetch(&"k", || failing(calls.clone())),
            cache.get_or_fetch(&"k", || failing(calls.clone())),
            cache.get_or_fetch(&"k", || failing(calls.clone())),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for result in [a, b, c] {
            assert!(matches!(result, Err(AppError::Unsupported(detail)) if detail == "no catalog"));
        }
        assert_eq!(
            cache
                .get_or_fetch(&"k", || async { Ok((5, Duration::from_secs(60))) })
                .await
                .unwrap(),
            5
        );
    }

    #[tokio::test]
    async fn a_cancelled_fetch_lets_the_next_waiter_fetch() {
        let cache = Arc::new(DiscoveryCache::<&'static str, usize>::new());
        let leader = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(&"k", || {
                        std::future::pending::<Result<(usize, Duration), AppError>>()
                    })
                    .await
            }
        });
        while !cache
            .existing(&"k")
            .is_some_and(|slot| slot.flight.try_lock().is_err())
        {
            tokio::task::yield_now().await;
        }
        let follower = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(&"k", || async { Ok((9, Duration::from_secs(60))) })
                    .await
            }
        });
        tokio::task::yield_now().await;
        leader.abort();
        let value = tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .expect("follower hung after the leader was cancelled")
            .unwrap()
            .unwrap();
        assert_eq!(value, 9);
    }

    #[tokio::test]
    async fn expired_values_are_served_while_one_background_refresh_runs() {
        let cache = Arc::new(DiscoveryCache::<&'static str, usize>::new());
        cache.insert(&"k", 1, Duration::ZERO);
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let held = Arc::new(tokio::sync::Mutex::new(Some(held)));
        let refresh = |calls: Arc<AtomicUsize>, held: Arc<tokio::sync::Mutex<Option<_>>>| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if let Some(held) = held.lock().await.take() {
                let _: Result<(), _> = held.await;
            }
            Ok((2, Duration::from_secs(60)))
        };
        // Both callers get the stale value without waiting; one refresh runs.
        for _ in 0..2 {
            let value = tokio::time::timeout(
                Duration::from_secs(1),
                cache.get_or_refresh(&"k", || refresh(calls.clone(), held.clone())),
            )
            .await
            .expect("a stale value is returned without waiting");
            assert_eq!(value.unwrap(), 1);
        }
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cache.is_fresh(&"k") {
            assert!(Instant::now() < deadline, "refresh never stored");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            cache
                .get_or_refresh(&"k", || async { Ok((3, Duration::from_secs(60))) })
                .await
                .unwrap(),
            2
        );

        // A failed refresh keeps the stale value.
        cache.insert(&"k", 4, Duration::ZERO);
        cache
            .get_or_refresh(&"k", || async {
                Err::<(usize, Duration), _>(AppError::ProviderUnavailable("down".to_owned()))
            })
            .await
            .unwrap();
        let _ = cache.existing(&"k").unwrap().flight.lock().await;
        assert_eq!(cache.peek(&"k"), Some(4));
    }

    #[tokio::test]
    async fn a_first_fetch_outlives_a_caller_that_stops_waiting() {
        let cache = Arc::new(DiscoveryCache::<&'static str, usize>::new());
        let abandoned = tokio::time::timeout(
            Duration::from_millis(10),
            cache.get_or_refresh(&"k", || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok((5, Duration::from_secs(60)))
            }),
        )
        .await;
        assert!(abandoned.is_err());
        // The next caller waits for that fetch instead of starting its own.
        assert_eq!(
            cache
                .get_or_refresh(&"k", || async { Ok((6, Duration::from_secs(60))) })
                .await
                .unwrap(),
            5
        );
    }

    #[tokio::test]
    async fn catalogs_without_a_ttl_always_fetch() {
        let uncached = CatalogCache::<u8>::new(None);
        let workspace = Path::new("/tmp");
        for expected in [1, 2] {
            let value = uncached
                .fetch("/nonexistent/cli", workspace, async move { Ok(expected) })
                .await
                .unwrap();
            assert_eq!(value, expected);
        }
        let cached = CatalogCache::<u8>::new(Some(Duration::from_secs(60)));
        for _ in 0..2 {
            let value = cached
                .fetch("/nonexistent/cli", workspace, async { Ok(1) })
                .await
                .unwrap();
            assert_eq!(value, 1);
        }
        let value = cached
            .fetch("/nonexistent/cli", Path::new("/other"), async { Ok(2) })
            .await
            .unwrap();
        assert_eq!(value, 2);
    }
}
