//! 串行化同一终端的命令写入，避免工作流、片段与广播交错。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use kerminal_domain::identity::SessionId;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// 会话命令互斥表仅持有弱引用，空闲会话的锁在下次访问时被回收。
#[derive(Clone, Default)]
pub(super) struct SessionCommandCoordinator {
    locks: Arc<Mutex<HashMap<SessionId, Weak<Mutex<()>>>>>,
}

impl SessionCommandCoordinator {
    /// 同一 session 的命令串行执行，但不同 session 保持并行。
    pub(super) async fn acquire(&self, session_id: &SessionId) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(session_id).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(session_id.clone(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }
}
