//! In-memory registry of open remote connections.
//!
//! Each session serializes its operations behind an async mutex. Sessions idle
//! for [`IDLE_TIMEOUT`] are closed by a reaper task that only runs while at
//! least one session exists. Nothing here is persisted or holds credentials.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use super::{now_millis, RemoteFs};
use crate::error::AppError;

pub(crate) const MAX_SESSIONS: usize = 16;
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// How long a request waits for a session that is busy with another one
/// (for example a large download) before giving up.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RemoteKind {
    Sftp,
    Ftp,
}

/// What a session was opened against; becomes part of its public view.
#[derive(Clone, Debug)]
pub(crate) enum OpenedBy {
    Sftp { host: String },
    Ftp { site_id: String, name: String },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteConnection {
    pub id: String,
    pub kind: RemoteKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    pub label: String,
    pub home_directory: String,
    pub opened_at: u64,
    pub last_used_at: u64,
}

struct Session {
    info: RemoteConnection,
    last_used: AtomicU64,
    fs: Arc<AsyncMutex<Box<dyn RemoteFs>>>,
}

impl Session {
    fn view(&self) -> RemoteConnection {
        RemoteConnection {
            last_used_at: self.last_used.load(Ordering::Relaxed),
            ..self.info.clone()
        }
    }

    fn touch(&self) {
        self.last_used.store(now_millis(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Arc<Session>>,
    reaper_running: bool,
}

struct Inner {
    state: Mutex<State>,
    idle: Duration,
    sweep: Duration,
}

#[derive(Clone)]
pub(crate) struct RemoteSessions {
    inner: Arc<Inner>,
}

impl Default for RemoteSessions {
    fn default() -> Self {
        Self::with_timing(IDLE_TIMEOUT, SWEEP_INTERVAL)
    }
}

/// Exclusive access to one session's file system for one operation.
pub(crate) struct SessionGuard {
    session: Arc<Session>,
    fs: OwnedMutexGuard<Box<dyn RemoteFs>>,
}

impl SessionGuard {
    pub(crate) fn fs(&mut self) -> &mut dyn RemoteFs {
        self.fs.as_mut()
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.session.touch();
    }
}

impl RemoteSessions {
    fn with_timing(idle: Duration, sweep: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                idle,
                sweep,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding this lock cannot leave the map inconsistent.
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn list(&self) -> Vec<RemoteConnection> {
        let mut connections: Vec<RemoteConnection> =
            self.state().sessions.values().map(|s| s.view()).collect();
        connections.sort_by(|a, b| a.opened_at.cmp(&b.opened_at).then(a.id.cmp(&b.id)));
        connections
    }

    /// Checked before connecting so a full registry does not cost a login.
    pub(crate) fn ensure_capacity(&self) -> Result<(), AppError> {
        if self.state().sessions.len() >= MAX_SESSIONS {
            return Err(too_many());
        }
        Ok(())
    }

    /// Registers a connected file system.
    pub(crate) async fn insert(
        &self,
        opened_by: OpenedBy,
        mut fs: Box<dyn RemoteFs>,
    ) -> Result<RemoteConnection, AppError> {
        let home_directory = match fs.home_dir().await {
            Ok(home) => home,
            Err(error) => {
                fs.close().await;
                return Err(error);
            }
        };
        let now = now_millis();
        let (kind, host, site_id, label) = match opened_by {
            OpenedBy::Sftp { host } => (RemoteKind::Sftp, Some(host.clone()), None, host),
            OpenedBy::Ftp { site_id, name } => (RemoteKind::Ftp, None, Some(site_id), name),
        };
        let info = RemoteConnection {
            id: Uuid::new_v4().to_string(),
            kind,
            host,
            site_id,
            label,
            home_directory,
            opened_at: now,
            last_used_at: now,
        };
        let session = Arc::new(Session {
            info: info.clone(),
            last_used: AtomicU64::new(now),
            fs: Arc::new(AsyncMutex::new(fs)),
        });
        let start_reaper = {
            let mut state = self.state();
            if state.sessions.len() >= MAX_SESSIONS {
                None
            } else {
                state.sessions.insert(info.id.clone(), session.clone());
                Some(!std::mem::replace(&mut state.reaper_running, true))
            }
        };
        match start_reaper {
            None => {
                close_session(session).await;
                Err(too_many())
            }
            Some(start) => {
                if start {
                    tokio::spawn(reap(Arc::downgrade(&self.inner)));
                }
                Ok(info)
            }
        }
    }

    /// Waits for exclusive use of a session.
    pub(crate) async fn lock(&self, id: &str) -> Result<SessionGuard, AppError> {
        let session = self
            .state()
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("remote connection {id}")))?;
        session.touch();
        let fs = tokio::time::timeout(LOCK_TIMEOUT, session.fs.clone().lock_owned())
            .await
            .map_err(|_| {
                AppError::Conflict("remote connection is busy with another operation".to_owned())
            })?;
        Ok(SessionGuard { session, fs })
    }

    /// Removes a session; the connection is closed once its current
    /// operation, if any, has finished. Returns whether it existed.
    pub(crate) fn close(&self, id: &str) -> bool {
        let Some(session) = self.state().sessions.remove(id) else {
            return false;
        };
        tokio::spawn(close_session(session));
        true
    }
}

fn too_many() -> AppError {
    AppError::ResourceExhausted(format!(
        "at most {MAX_SESSIONS} remote connections may be open; close one first"
    ))
}

async fn close_session(session: Arc<Session>) {
    let mut fs = session.fs.clone().lock_owned().await;
    fs.close().await;
}

/// Closes idle sessions until none are left; holds only a weak reference so
/// it also ends when the registry itself is dropped.
async fn reap(inner: Weak<Inner>) {
    loop {
        let Some(sweep) = inner.upgrade().map(|inner| inner.sweep) else {
            return;
        };
        tokio::time::sleep(sweep).await;
        let Some(strong) = inner.upgrade() else {
            return;
        };
        let (expired, done) = {
            let mut state = strong
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cutoff = now_millis().saturating_sub(strong.idle.as_millis() as u64);
            let idle_ids: Vec<String> = state
                .sessions
                .iter()
                // A held lock means an operation (e.g. a download) is running.
                .filter(|(_, s)| {
                    s.last_used.load(Ordering::Relaxed) <= cutoff && s.fs.try_lock().is_ok()
                })
                .map(|(id, _)| id.clone())
                .collect();
            let expired: Vec<Arc<Session>> = idle_ids
                .iter()
                .filter_map(|id| state.sessions.remove(id))
                .collect();
            let done = state.sessions.is_empty();
            if done {
                state.reaper_running = false;
            }
            (expired, done)
        };
        drop(strong);
        for session in expired {
            close_session(session).await;
        }
        if done {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_fs::{Listing, RemoteStat};
    use async_trait::async_trait;
    use axum::body::Bytes;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::mpsc;

    struct FakeFs {
        closed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RemoteFs for FakeFs {
        async fn home_dir(&mut self) -> Result<String, AppError> {
            Ok("/home/me".into())
        }
        async fn list(&mut self, _: &str) -> Result<Listing, AppError> {
            Ok(Listing {
                entries: Vec::new(),
                truncated: false,
            })
        }
        async fn stat(&mut self, _: &str) -> Result<Option<RemoteStat>, AppError> {
            Ok(None)
        }
        async fn read(&mut self, _: &str, _: u64) -> Result<Vec<u8>, AppError> {
            Ok(Vec::new())
        }
        async fn replace(&mut self, _: &str, _: &[u8]) -> Result<(), AppError> {
            Ok(())
        }
        async fn write_at(&mut self, _: &str, _: u64, _: &[u8]) -> Result<(), AppError> {
            Ok(())
        }
        async fn download(
            &mut self,
            _: &str,
            _: u64,
            _: &mpsc::Sender<std::io::Result<Bytes>>,
        ) -> Result<(), AppError> {
            Ok(())
        }
        async fn rename(&mut self, _: &str, _: &str) -> Result<(), AppError> {
            Ok(())
        }
        async fn remove(&mut self, _: &str) -> Result<(), AppError> {
            Ok(())
        }
        async fn mkdir(&mut self, _: &str) -> Result<(), AppError> {
            Ok(())
        }
        async fn close(&mut self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn fake(closed: &Arc<AtomicUsize>) -> Box<dyn RemoteFs> {
        Box::new(FakeFs {
            closed: closed.clone(),
        })
    }

    fn sftp(host: &str) -> OpenedBy {
        OpenedBy::Sftp { host: host.into() }
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn registers_lists_and_closes_sessions() {
        let closed = Arc::new(AtomicUsize::new(0));
        let sessions = RemoteSessions::default();
        let connection = sessions.insert(sftp("db"), fake(&closed)).await.unwrap();
        assert_eq!(connection.home_directory, "/home/me");
        assert_eq!(connection.host.as_deref(), Some("db"));
        assert_eq!(sessions.list(), vec![connection.clone()]);
        let json = serde_json::to_value(&connection).unwrap();
        assert_eq!(json["kind"], "sftp");
        assert!(json.get("siteId").is_none());

        sessions
            .lock(&connection.id)
            .await
            .unwrap()
            .fs()
            .mkdir("/x")
            .await
            .unwrap();
        assert!(sessions.close(&connection.id));
        assert!(!sessions.close(&connection.id));
        settle().await;
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert!(matches!(
            sessions.lock(&connection.id).await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn enforces_the_session_limit() {
        let closed = Arc::new(AtomicUsize::new(0));
        let sessions = RemoteSessions::default();
        for _ in 0..MAX_SESSIONS {
            sessions.insert(sftp("db"), fake(&closed)).await.unwrap();
        }
        assert!(matches!(
            sessions.ensure_capacity(),
            Err(AppError::ResourceExhausted(_))
        ));
        let error = sessions
            .insert(sftp("db"), fake(&closed))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::ResourceExhausted(_)));
        // The rejected connection was closed rather than leaked.
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn evicts_idle_sessions_but_not_busy_ones() {
        let closed = Arc::new(AtomicUsize::new(0));
        let sessions =
            RemoteSessions::with_timing(Duration::from_millis(0), Duration::from_millis(40));
        let idle = sessions.insert(sftp("a"), fake(&closed)).await.unwrap();
        let busy = sessions.insert(sftp("b"), fake(&closed)).await.unwrap();
        let guard = sessions.lock(&busy.id).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        settle().await;
        let open: Vec<String> = sessions.list().into_iter().map(|c| c.id).collect();
        assert_eq!(open, std::slice::from_ref(&busy.id));
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert!(sessions.lock(&idle.id).await.is_err());

        drop(guard);
        tokio::time::sleep(Duration::from_millis(200)).await;
        settle().await;
        assert!(sessions.list().is_empty());
        assert_eq!(closed.load(Ordering::SeqCst), 2);
        assert!(!sessions.state().reaper_running);
    }
}
