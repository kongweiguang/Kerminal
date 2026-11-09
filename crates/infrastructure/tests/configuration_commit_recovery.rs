//! Windows 配置提交失败后的恢复材料契约。
//!
//! @author kongweiguang

#![cfg(windows)]

use std::{fs, os::windows::fs::OpenOptionsExt};

use kerminal_application::{
    PublicErrorCode,
    preferences::{AppColorScheme, SettingsRepository, SettingsSnapshot},
};
use kerminal_infrastructure::configuration::ConfigurationStore;

/// 用真实拒绝删除共享的句柄触发 ReplaceFileW 失败；未知结果不能删掉仍存在的提交字节。
/// 目标与暂存分别回读，避免只验证错误分类而漏掉恢复材料被清理的问题。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_settings_replace_preserves_stage_and_previous_document() {
    let root = tempfile::tempdir().expect("创建隔离配置目录");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化配置仓储");
    let original = store
        .save_settings(SettingsSnapshot::default(), None)
        .await
        .expect("写入基线设置");
    let path = root.path().join("settings.toml");
    let original_bytes = fs::read(&path).expect("读取基线原文");
    let blocker = fs::OpenOptions::new()
        .read(true)
        .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE；只阻断替换所需的 DELETE 共享。
        .open(&path)
        .expect("锁住隔离目标的删除共享");
    let mut pending = original.settings.clone();
    pending.app_color_scheme = AppColorScheme::VsCode;
    let error = store
        .save_settings(pending, original.revision)
        .await
        .expect_err("替换必须受共享约束阻断");
    drop(blocker);

    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(fs::read(&path).expect("回读原目标"), original_bytes);
    let stages = fs::read_dir(root.path())
        .expect("读取隔离目录")
        .map(|entry| entry.expect("读取目录项").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("settings.toml.") && name.ends_with(".tmp"))
        })
        .collect::<Vec<_>>();
    assert_eq!(stages.len(), 1, "未知提交必须保留唯一暂存文档");
    let staged = fs::read_to_string(&stages[0]).expect("回读暂存材料");
    assert!(staged.contains("colorScheme = \"vscode\""));
    assert_eq!(
        store.load_settings().await.expect("显式回读配置").settings,
        original.settings
    );
}
