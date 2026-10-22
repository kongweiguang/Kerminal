//! 原生窗口状态的当前格式、只读恢复与 CAS 契约测试。
//!
//! @author kongweiguang

use std::fs;

use kerminal_application::platform::window_state::{
    WindowRect, WindowStateLoad, WindowStateRepository, WindowStateSnapshot, WindowStateSource,
    WindowStateUnavailableReason,
};
use kerminal_infrastructure::{
    platform::window_state::WindowStateStore, storage::StorageCoordinator,
};
use tempfile::TempDir;

/// 使用跨主屏幕边界的合成几何覆盖负坐标与最大化还原状态。
fn snapshot() -> WindowStateSnapshot {
    WindowStateSnapshot {
        normal_bounds: WindowRect {
            x: -1280,
            y: 96,
            width: 1440,
            height: 900,
        },
        maximized: true,
        fullscreen: false,
    }
}

/// adapter 只接当前临时根，旧 Tauri 路径已不属于普通 reader 的输入。
///
/// @author kongweiguang
fn store(root: &TempDir) -> WindowStateStore {
    WindowStateStore::new(StorageCoordinator::new(root.path()))
}

/// 当前格式写入后按同一 revision 读取，验证文件适配器与 application CAS 接缝闭合。
///
/// @author kongweiguang
#[tokio::test]
async fn current_file_round_trips_through_atomic_store() {
    let root = tempfile::tempdir().expect("temporary window state root");
    let store = store(&root);

    let first_revision = store
        .compare_and_swap(None, snapshot())
        .await
        .expect("first state write");
    let loaded = store.load().await.expect("state load");

    assert_eq!(first_revision, 1);
    assert_eq!(
        loaded,
        WindowStateLoad::Restored {
            snapshot: snapshot(),
            revision: Some(1),
            source: WindowStateSource::Current,
        }
    );
}

/// 当前文件缺失不能再回读旧状态，即使旧文件内容完整也保持 Missing 且不改原字节。
///
/// @author kongweiguang
#[tokio::test]
async fn missing_current_state_does_not_import_legacy_data() {
    let root = tempfile::tempdir().expect("temporary window state root");
    let legacy_path = root.path().join("legacy-window-state.json");
    let legacy_bytes = br#"{"main":{"width":1440,"height":900,"x":0,"y":0,"prev_x":-1280,"prev_y":96,"maximized":true,"visible":true,"decorated":true,"fullscreen":false}}"#;
    fs::write(&legacy_path, legacy_bytes).expect("write synthetic legacy state");
    let store = store(&root);

    let loaded = store.load().await.expect("current state load");

    assert_eq!(loaded, WindowStateLoad::Missing);
    assert_eq!(
        fs::read(legacy_path).expect("read legacy state"),
        legacy_bytes
    );
    assert!(!store.state_file().exists());
}

/// 当前损坏文件仍保持原字节，旧数据不能绕过恢复失败而覆盖它。
///
/// @author kongweiguang
#[tokio::test]
async fn corrupt_current_state_is_readonly_and_does_not_fall_back() {
    let root = tempfile::tempdir().expect("temporary window state root");
    let legacy_path = root.path().join("legacy-window-state.json");
    fs::write(
        &legacy_path,
        br#"{"main":{"width":1440,"height":900,"x":0,"y":0,"maximized":false}}"#,
    )
    .expect("write synthetic legacy state");
    let current_path = root.path().join("window-state.json");
    let original = b"{broken window state";
    fs::write(&current_path, original).expect("write corrupt current state");
    let store = store(&root);

    let loaded = store
        .load()
        .await
        .expect("unavailable state is not startup failure");
    let write = store.compare_and_swap(None, snapshot()).await;

    assert_eq!(
        loaded,
        WindowStateLoad::Unavailable {
            reason: WindowStateUnavailableReason::Corrupt,
        }
    );
    assert!(write.is_err());
    assert_eq!(
        fs::read(current_path).expect("read preserved state"),
        original
    );
}

/// 未来版本先按最小 envelope 拒绝，额外字段不能被误判为当前格式损坏或覆盖。
///
/// @author kongweiguang
#[tokio::test]
async fn future_schema_with_extra_fields_is_preserved() {
    let root = tempfile::tempdir().expect("temporary window state root");
    let current_path = root.path().join("window-state.json");
    let original = br#"{"schema_version":9,"revision":4,"state":{"normal_bounds":{"x":0,"y":0,"width":1200,"height":800},"maximized":false,"fullscreen":false,"future_field":true},"next":true}"#;
    fs::write(&current_path, original).expect("write future state");
    let store = store(&root);

    let loaded = store
        .load()
        .await
        .expect("future version is a visible state");
    let write = store.compare_and_swap(Some(4), snapshot()).await;

    assert_eq!(
        loaded,
        WindowStateLoad::Unavailable {
            reason: WindowStateUnavailableReason::UnsupportedSchemaVersion(9),
        }
    );
    assert!(write.is_err());
    assert_eq!(
        fs::read(current_path).expect("read preserved state"),
        original
    );
}

/// 已有 revision 必须匹配调用方快照，另一实例先提交后拒绝陈旧窗口位置。
///
/// @author kongweiguang
#[tokio::test]
async fn compare_and_swap_rejects_stale_revision() {
    let root = tempfile::tempdir().expect("temporary window state root");
    let store = store(&root);
    store
        .compare_and_swap(None, snapshot())
        .await
        .expect("initial state write");

    let stale_write = store.compare_and_swap(None, snapshot()).await;

    assert!(stale_write.is_err());
    assert!(matches!(
        store.load().await.expect("state load"),
        WindowStateLoad::Restored {
            revision: Some(1),
            ..
        }
    ));
}
