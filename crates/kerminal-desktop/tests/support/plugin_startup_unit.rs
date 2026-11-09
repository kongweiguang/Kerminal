//! 自动恢复必须禁用开发包和热重载，测试不读取真实配置或启动窗口。
//! @author kongweiguang

use super::*;

/// 模拟两次冻结后崩溃；第三次即使开发路径失效，也应先进入安全模式而不校验或加载它。
#[test]
fn automatic_safe_mode_disables_development_package_and_reloader_target() {
    let root = tempfile::tempdir().expect("isolated plugin startup root");
    for _ in 0..2 {
        let startup = StartupPlugins::load(root.path(), PluginStartupOptions::default())
            .expect("begin startup");
        startup
            .store
            .mark_boot_phase(&startup.snapshot.boot_id, PluginBootPhase::RegistryFrozen)
            .expect("persist pre-frame crash marker");
    }
    let startup = StartupPlugins::load(
        root.path(),
        PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(root.path().join("missing-development-package")),
        },
    )
    .expect("automatic safe mode must not open the development directory");
    assert!(startup.snapshot.safe_mode);
    assert!(startup.options.safe_mode);
    assert!(startup.options.dev_plugin.is_none());
    assert!(startup.enabled.is_empty());
}
