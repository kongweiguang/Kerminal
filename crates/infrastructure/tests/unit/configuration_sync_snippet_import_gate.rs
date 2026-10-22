//! Git 本地快照遇到片段导入事务时的 fail-closed 路径测试。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::AppError;
use uuid::Uuid;

use super::ensure_no_pending_snippet_import;

struct TemporaryRoot(PathBuf);

impl TemporaryRoot {
    /// 为保护路径检查创建隔离根，不启动 Git，也不读取用户配置。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("snippet-sync-gate-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建隔离同步目录");
        Self(root)
    }
}

impl Drop for TemporaryRoot {
    /// 只清理本测试新建的目录树，不对其它仓库路径执行删除。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 无 marker 时允许本地仓库快照；任何活动 marker 都阻断并保持原字节。
#[test]
fn active_marker_blocks_snapshot_and_is_never_rewritten() {
    let temporary = TemporaryRoot::new();
    ensure_no_pending_snippet_import(&temporary.0).expect("无事务时允许本地快照");

    let transaction = temporary.0.join(".storage-transactions/snippet-import");
    fs::create_dir_all(&transaction).expect("创建受保护事务目录");
    let marker = transaction.join("active.json");
    let original = b"future or partial journal";
    fs::write(&marker, original).expect("写入合成活动标记");

    assert!(matches!(
        ensure_no_pending_snippet_import(&temporary.0),
        Err(AppError::OutcomeUnknown { .. })
    ));
    assert_eq!(fs::read(marker).expect("回读活动标记"), original);
}

/// 非普通事务父节点不能被跟随或当作空目录，从而避免 Git 越过受保护状态。
#[test]
fn unsafe_transaction_parent_blocks_snapshot_and_is_preserved() {
    let temporary = TemporaryRoot::new();
    let parent = temporary.0.join(".storage-transactions");
    let original = b"existing regular file";
    fs::write(&parent, original).expect("创建合成非目录父节点");

    assert!(matches!(
        ensure_no_pending_snippet_import(&temporary.0),
        Err(AppError::OutcomeUnknown { .. })
    ));
    assert_eq!(fs::read(parent).expect("确认父节点未被替换"), original);
}
