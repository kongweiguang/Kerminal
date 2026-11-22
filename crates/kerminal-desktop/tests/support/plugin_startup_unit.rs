//! 自动恢复必须禁用开发包和热重载，测试不读取真实配置或启动窗口。
//! @author kongweiguang

use super::*;

/// 启动侧 `HOST_API` 必须只是 application 权威常量的重导出，不再保留本地字面量：
/// 启动校验与目录装配（后者由 kerminal-migrate 共享）共用同一宿主契约版本，
/// 防止两处字面量漂移后对同一插件给出不同兼容判定。
///
/// @author kongweiguang
#[test]
fn startup_host_api_reexports_application_authority() {
    assert_eq!(
        crate::plugins::startup::HOST_API,
        kerminal_application::plugins::HOST_API_VERSION
    );
}

/// 启动目录必须消费持久内置状态，而不是默认全部启用；否则用户在扩展页停用的
/// 单元会在下次启动被悄悄放回。测试只写状态文件，不触碰真实配置根。
#[test]
fn startup_catalogs_apply_persisted_builtin_states() {
    let root = tempfile::tempdir().expect("isolated plugin startup root");
    let store = PluginStore::new(StorageCoordinator::new(root.path()))
        .expect("plugin store for isolated root");
    let snapshot = store.load().expect("read initial plugin state");
    let mut state = snapshot.state.clone();
    state.builtins.insert(
        PluginId::parse("kerminal.ssh").expect("ssh identity"),
        false,
    );
    store
        .compare_and_swap(&snapshot, state)
        .expect("persist disabled builtin state");

    let startup = StartupPlugins::load(root.path(), PluginStartupOptions::default())
        .expect("begin startup with persisted builtin state");
    assert_eq!(
        startup
            .builtin_states()
            .expect("convert persisted builtin states")
            .get(&RegistryPluginId::new("kerminal.ssh").expect("ssh identity")),
        Some(&false)
    );
    let (registry, _) = startup.catalogs("en").expect("freeze startup catalogs");
    assert!(!registry.is_enabled("kerminal.ssh"));
    assert!(registry.is_enabled("kerminal.sftp"));
}

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

/// 自动安全模式只是本次恢复状态：受管重启不能把它固化成显式 `--safe-mode`，
/// 否则首帧恢复后仍会以安全模式重启；恢复期间的开发插件也必须继续抑制。
#[test]
fn automatic_safe_mode_is_not_persisted_as_explicit_restart_option() {
    let root = tempfile::tempdir().expect("isolated plugin startup root");
    for _ in 0..2 {
        let startup = StartupPlugins::load(root.path(), PluginStartupOptions::default())
            .expect("begin startup");
        startup
            .store
            .mark_boot_phase(&startup.snapshot.boot_id, PluginBootPhase::RegistryFrozen)
            .expect("persist pre-frame crash marker");
    }
    let dev_plugin = root.path().join("missing-development-package");
    let startup = StartupPlugins::load(
        root.path(),
        PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(dev_plugin),
        },
    )
    .expect("automatic safe mode begins startup");
    assert!(startup.snapshot.safe_mode && startup.options.safe_mode);
    assert!(startup.options.dev_plugin.is_none());
    let restart = startup.restart_options();
    assert!(!restart.safe_mode);
    assert!(restart.dev_plugin.is_none());
}

/// 用户显式请求的安全模式与开发插件在普通启动中必须原样交给受管重启，
/// 只有自动恢复状态需要区分。
#[test]
fn explicit_startup_request_is_preserved_for_restart() {
    let root = tempfile::tempdir().expect("isolated plugin startup root");
    let startup = StartupPlugins::load(
        root.path(),
        PluginStartupOptions {
            safe_mode: true,
            dev_plugin: None,
        },
    )
    .expect("begin explicit safe mode startup");
    let restart = startup.restart_options();
    assert!(restart.safe_mode);
    assert!(restart.dev_plugin.is_none());
}
