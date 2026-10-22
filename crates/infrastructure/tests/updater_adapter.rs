// @author kongweiguang
//! updater 配置与平台能力边界回归；不访问远端 feed 或用户安装目录。

use std::time::Duration;

use cargo_packager_updater::url::Url;
use kerminal_application::AppError;
use kerminal_application::updates::{UpdateAvailability, UpdatePort};
use kerminal_infrastructure::platform::updater::{CargoPackagerUpdater, UpdaterSettings};

fn valid_settings() -> UpdaterSettings {
    // 测试只使用保留域名和非生产公钥文本，不接触生产 updater 凭据。
    UpdaterSettings::new(
        "0.4.0-dev.0",
        vec![Url::parse("https://updates.invalid/kerminal/latest.json").expect("valid URL")],
        "test-only-public-key".to_owned(),
        Duration::from_secs(15),
        128 * 1024 * 1024,
    )
    .expect("valid updater settings")
}

#[test]
fn settings_reject_http_endpoint() {
    // updater 请求只允许 TLS，不能因为测试或本地环境退化到明文网络。
    let result = UpdaterSettings::new(
        "0.4.0",
        vec![Url::parse("http://updates.invalid/latest.json").expect("valid URL")],
        "test-only-public-key".to_owned(),
        Duration::from_secs(15),
        1024,
    );

    assert!(matches!(result, Err(AppError::Invalid { .. })));
}

/// 正式配置与测试配置统一拒绝 loopback 明文地址，CA fixture 不改变 endpoint policy。
#[test]
fn settings_reject_loopback_http_endpoint() {
    let result = UpdaterSettings::new(
        "0.4.0",
        vec![Url::parse("http://127.0.0.1:34999/latest.json").expect("valid URL")],
        "test-only-public-key".to_owned(),
        Duration::from_secs(15),
        1024,
    );

    assert!(matches!(result, Err(AppError::Invalid { .. })));
}

#[test]
fn settings_reject_empty_key_and_invalid_version() {
    // 缺失签名信任根或 semver 错误时不得构造运行态更新器。
    let endpoint = Url::parse("https://updates.invalid/latest.json").expect("valid URL");
    let empty_key = UpdaterSettings::new(
        "0.4.0",
        vec![endpoint.clone()],
        "  ".to_owned(),
        Duration::from_secs(15),
        1024,
    );
    let invalid_version = UpdaterSettings::new(
        "0.4",
        vec![endpoint],
        "test-only-public-key".to_owned(),
        Duration::from_secs(15),
        1024,
    );

    assert!(matches!(empty_key, Err(AppError::Invalid { .. })));
    assert!(matches!(invalid_version, Err(AppError::Invalid { .. })));
}

#[test]
fn settings_reject_unbounded_timeout_and_size() {
    // 请求时限和更新体积都是产品策略，不接受零值或无界配置。
    let endpoint = Url::parse("https://updates.invalid/latest.json").expect("valid URL");
    let too_long = UpdaterSettings::new(
        "0.4.0",
        vec![endpoint.clone()],
        "test-only-public-key".to_owned(),
        Duration::from_secs(301),
        1024,
    );
    let too_large = UpdaterSettings::new(
        "0.4.0",
        vec![endpoint],
        "test-only-public-key".to_owned(),
        Duration::from_secs(15),
        2 * 1024 * 1024 * 1024,
    );

    assert!(matches!(too_long, Err(AppError::Invalid { .. })));
    assert!(matches!(too_large, Err(AppError::Invalid { .. })));
}

#[test]
fn appimage_only_update_runtime_matches_current_platform() {
    // Linux DEB 必须走系统包管理器；SDK 的自更新原位替换只对 AppImage 有效。
    let supported = UpdaterSettings::supports_current_runtime();
    #[cfg(target_os = "linux")]
    assert_eq!(supported, std::env::var_os("APPIMAGE").is_some());
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    assert!(supported);
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    assert!(!supported);

    if !supported {
        let recovery_root = tempfile::tempdir().expect("create isolated recovery root");
        let adapter = CargoPackagerUpdater::new(
            Some(valid_settings()),
            recovery_root.path().join("updates/recovery"),
        )
        .expect("recovery adapter remains constructible without update support");
        assert!(matches!(
            adapter.availability(),
            UpdateAvailability::Unavailable { .. }
        ));
    }
}
