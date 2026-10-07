//! Async locks per key (conversation, or conversation + prompt kind) that
//! disappear once nobody holds or waits on them.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

#[derive(Default)]
pub(crate) struct KeyedLocks {
    locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl KeyedLocks {
    /// The lock for `key`, shared with everyone currently holding or
    /// awaiting it. Entries nobody references are pruned when a new key is
    /// added, so the map stays as small as the set of keys in use.
    pub(crate) fn get(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(key.to_owned(), Arc::downgrade(&lock));
        lock
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn locks_are_shared_while_used_and_pruned_after() {
        let locks = KeyedLocks::default();
        let a = locks.get("a");
        let held = a.lock().await;
        assert!(locks.get("a").try_lock().is_err(), "same key, same lock");
        assert!(locks.get("b").try_lock().is_ok());
        drop(held);
        drop(a);
        // "a" and "b" are unreferenced: the next new key prunes them.
        let _c = locks.get("c");
        assert_eq!(locks.len(), 1);
    }
}
