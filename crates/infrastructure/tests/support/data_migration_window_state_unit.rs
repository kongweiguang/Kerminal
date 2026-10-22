//! 窗口状态一次迁移规划的真实文件边界回归。
//!
//! @author kongweiguang

use std::fs;

use super::plan_window_state_migration;

const LEGACY: &[u8] = br#"{"main":{"width":1440,"height":900,"x":0,"y":0,"prev_x":-1280,"prev_y":96,"maximized":true,"visible":true,"decorated":true,"fullscreen":false}}"#;
const CURRENT: &[u8] = br#"{"schema_version":1,"revision":7,"state":{"normal_bounds":{"x":10,"y":20,"width":1200,"height":800},"maximized":false,"fullscreen":false}}"#;

/// 旧 decoder 的 prev 坐标、最大化与尺寸原样进入当前格式计划，规划本身不修改输入或创建目标。
///
/// @author kongweiguang
#[test]
fn legacy_window_geometry_is_planned_once_without_writing() {
    let fixture = tempfile::tempdir().expect("isolated migration root");
    let root = fixture.path().join("current");
    let legacy_path = fixture.path().join("old-window.json");
    fs::write(&legacy_path, LEGACY).expect("legacy fixture");
    let edits = plan_window_state_migration(&root, Some(&legacy_path)).expect("window plan");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].relative_path.to_str(), Some("window-state.json"));
    assert!(edits[0].before.is_none());
    let document: serde_json::Value =
        serde_json::from_slice(&edits[0].after).expect("current JSON");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["revision"], 1);
    assert_eq!(document["state"]["normal_bounds"]["x"], -1280);
    assert_eq!(document["state"]["normal_bounds"]["y"], 96);
    assert_eq!(document["state"]["normal_bounds"]["width"], 1440);
    assert_eq!(document["state"]["normal_bounds"]["height"], 900);
    assert_eq!(document["state"]["maximized"], true);
    assert_eq!(document["state"]["fullscreen"], false);
    assert_eq!(fs::read(&legacy_path).expect("preserved legacy"), LEGACY);
    assert!(
        !root.exists(),
        "planning must not initialize the current root"
    );
}

/// 当前格式一旦存在即不读取旧源，旧源甚至为非法目录时也不能污染已迁移状态。
///
/// @author kongweiguang
#[test]
fn current_window_state_prevents_any_legacy_lookup() {
    let root = tempfile::tempdir().expect("isolated current root");
    let current_path = root.path().join("window-state.json");
    fs::write(&current_path, CURRENT).expect("current fixture");
    assert!(
        plan_window_state_migration(root.path(), Some(root.path()))
            .expect("current wins")
            .is_empty()
    );
    assert_eq!(fs::read(current_path).expect("preserved current"), CURRENT);
    assert!(!root.path().join(".storage.lock").exists());
}

/// 损坏或未来当前文件优先阻断，不能通过有效旧源覆盖或隐藏真实失败。
///
/// @author kongweiguang
#[test]
fn invalid_current_window_state_is_not_replaced_by_legacy() {
    for bytes in [
        b"{broken".as_slice(),
        br#"{"schema_version":9,"revision":7,"state":{}}"#.as_slice(),
    ] {
        let root = tempfile::tempdir().expect("isolated invalid root");
        let current_path = root.path().join("window-state.json");
        let legacy_path = root.path().join("legacy.json");
        fs::write(&current_path, bytes).expect("invalid current");
        fs::write(&legacy_path, LEGACY).expect("valid legacy");
        assert!(plan_window_state_migration(root.path(), Some(&legacy_path)).is_err());
        assert_eq!(fs::read(current_path).expect("preserved current"), bytes);
        assert_eq!(fs::read(legacy_path).expect("preserved legacy"), LEGACY);
    }
}

/// 未显式提供旧源时不猜测宿主目录；无文件和非法旧源都不能产生悄然写入。
///
/// @author kongweiguang
#[test]
fn absent_or_invalid_legacy_state_never_initializes_current_storage() {
    let fixture = tempfile::tempdir().expect("isolated fixture");
    let root = fixture.path().join("not-created");
    assert!(
        plan_window_state_migration(&root, None)
            .expect("no source")
            .is_empty()
    );
    let legacy = fixture.path().join("legacy.json");
    for bytes in [
        b"{broken".as_slice(),
        br#"{"main":{"width":1,"height":2,"x":0,"y":0}}"#.as_slice(),
    ] {
        fs::write(&legacy, bytes).expect("invalid legacy");
        assert!(plan_window_state_migration(&root, Some(&legacy)).is_err());
        assert_eq!(fs::read(&legacy).expect("preserved invalid legacy"), bytes);
        assert!(!root.exists());
    }
}
