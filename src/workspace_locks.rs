use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Debug, Default)]
pub struct WorkspaceLocks {
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl WorkspaceLocks {
    pub async fn lock(&self, workspace_id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(workspace_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time;

    use super::*;

    #[tokio::test]
    async fn same_workspace_lock_waits() {
        let locks = WorkspaceLocks::default();
        let first = locks.lock("demo").await;

        let second = time::timeout(Duration::from_millis(20), locks.lock("demo")).await;
        assert!(second.is_err());

        drop(first);
        let second = time::timeout(Duration::from_millis(20), locks.lock("demo")).await;
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn different_workspace_locks_do_not_block_each_other() {
        let locks = WorkspaceLocks::default();
        let _first = locks.lock("a").await;

        let second = time::timeout(Duration::from_millis(20), locks.lock("b")).await;
        assert!(second.is_ok());
    }
}
