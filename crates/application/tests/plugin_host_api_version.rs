//! 宿主插件 API 版本单一来源的兼容回归；不启动插件进程或桌面。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use kerminal_application::{
    external_contributions::{ExternalContributionError, ExternalDeclarations, PreparedPlugin},
    plugins::HOST_API_VERSION,
};
use kerminal_domain::plugins::{PluginId, PluginVersion};
use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};
use serde_json::json;

/// 从真实协议解析清单，聚焦宿主 API 兼容判定本身。
///
/// @author kongweiguang
fn prepared_plugin(api: &str) -> PreparedPlugin {
    let document = json!({
        "schema":1,"id":"acme.qrcode","name":{"en":"QR Code"},
        "description":{"en":"QR tools"},"version":"1.0.0","api":api,
        "publisher":"acme","runtime":{"kind":"none"},
        "views":{},"contributes":{}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&document).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    PreparedPlugin {
        id: PluginId::parse("acme.qrcode").unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        directory: PathBuf::from("unused"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    }
}

/// 新能力插件声明 `>=1.1.0 <2.0.0` 时，旧 1.0 宿主必须拒绝装配，
/// 不能把缺能力当成兼容；错误保留插件身份便于安装界面定位。
#[test]
fn api_1_1_only_plugin_is_rejected_by_legacy_1_0_host() {
    let plugin = prepared_plugin(">=1.1.0 <2.0.0");
    let error = ExternalDeclarations::prepare(&plugin, "1.0.0", "en")
        .expect_err("1.0 宿主不得接受 1.1 插件");
    // 对引用匹配并用引用绑定身份：按值绑定会移走 `Incompatible(String)` 的载荷，
    // 后续诊断格式化 `{error:?}` 就变成借用已部分移动的值而无法编译。
    assert!(
        matches!(&error, ExternalContributionError::Incompatible(id) if id == "acme.qrcode"),
        "实际错误: {error:?}"
    );
}

/// 同一插件在当前权威宿主契约（1.1.0）下必须兼容，声明收敛到单一常量后
/// 目录装配与启动校验看到的是同一个版本。
#[test]
fn api_1_1_only_plugin_is_accepted_by_authoritative_host_version() {
    let plugin = prepared_plugin(">=1.1.0 <2.0.0");
    ExternalDeclarations::prepare(&plugin, HOST_API_VERSION, "en")
        .expect("当前宿主契约必须接受 1.1 插件");
}

/// 旧插件仍声明 `^1.0` 时，1.1 宿主按 semver 向后兼容继续接受，
/// 避免 single-source 收敛升级把存量插件一次性打成不兼容。
#[test]
fn legacy_caret_1_0_plugin_stays_compatible_with_1_1_host() {
    let plugin = prepared_plugin("^1.0");
    ExternalDeclarations::prepare(&plugin, HOST_API_VERSION, "en")
        .expect("旧 ^1.0 插件在 1.1 宿主仍须兼容");
}

/// 权威常量本身必须仍是 1.1.0：防止后续误改回 1.0 使新能力插件重新可装，
/// 也防止误升主版本破坏 ^1.0 存量兼容；SDK 与 JSON-RPC 版本不在此维度内。
#[test]
fn authoritative_host_api_version_is_pinned_to_1_1() {
    assert_eq!(HOST_API_VERSION, "1.1.0");
}
