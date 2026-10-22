//! 连接面板配置变更订阅的路径筛选回归测试。
//!
//! @author kongweiguang

use super::host_configuration_changed;

/// 主机文档变化与全量重扫必须刷新连接目录。
#[test]
fn host_documents_and_full_rescan_refresh_the_catalog() {
    assert!(host_configuration_changed(
        &["hosts/groups.toml".to_owned()],
        false
    ));
    assert!(host_configuration_changed(
        &["hosts/workstation.toml".to_owned()],
        false
    ));
    assert!(host_configuration_changed(&[], true));
}

/// 无关配置不触发 host 文件读取，同时 Windows 分隔符仍识别为 host 文档。
#[test]
fn unrelated_documents_are_ignored_and_windows_paths_are_normalized() {
    assert!(!host_configuration_changed(
        &[
            "settings.toml".to_owned(),
            "profiles/default.toml".to_owned(),
        ],
        false
    ));
    assert!(host_configuration_changed(
        &[r"hosts\workstation.toml".to_owned()],
        false
    ));
}
