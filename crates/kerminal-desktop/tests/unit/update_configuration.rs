//! 编译期更新源配置的单独回归，不访问网络或真实用户目录。
//!
//! @author kongweiguang

use super::settings_for_build;

const PUBLIC_FIXTURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDc5QjdDMkI5MTlDRDBBOUEKUldTYUNzMFp1Y0szZWJYTEN0Q0hyREpHMlc5T3FuVkJyUElrb3NrRVFvYThEMUI0QldMZ0E0Vm8K";

/// 未签名构建必须显式保持不可用，而不是把空配置伪装成已接入。
#[test]
fn missing_key_keeps_updater_unavailable() {
    assert!(
        settings_for_build("0.4.0-dev.0", None)
            .expect("missing key")
            .is_none()
    );
    assert!(
        settings_for_build("0.4.0-dev.0", Some(""))
            .expect("empty key")
            .is_none()
    );
}

/// 已注入但无效的信任根必须拒绝，避免正式候选在下载后才暴露配置错误。
#[test]
fn malformed_key_is_rejected() {
    assert!(settings_for_build("0.4.0", Some("placeholder-key")).is_err());
}

/// 有效公钥应接入真实更新适配器，避免配置存在却仍保持不可用状态。
#[test]
fn valid_key_enables_configured_feed() {
    assert!(
        settings_for_build("0.4.0-dev.0", Some(PUBLIC_FIXTURE))
            .expect("valid test public key")
            .is_some()
    );
}
