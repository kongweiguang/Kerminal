//! 资产 URL 语法契约的判别性回归，样本同时作为 CLI 侧的黄金输入。
//!
//! 本文件的 ACCEPTED/REJECTED 列表与 `sdk/cli/tests/asset_url.rs` 保持逐字一致；
//! 任一侧调整样本时必须同步另一侧，保证解析器、安装器网络适配与 CLI 三处
//! 对同一输入给出同一结论。这里只覆盖纯语法：地址分类、DNS、重定向策略由
//! `crates/infrastructure/tests/unit/plugin_install_network.rs` 负责。
//!
//! @author kongweiguang

use kerminal_plugin_protocol::{AssetUrlRejection, validate_asset_url, validate_redirect_target};
use url::Url;

/// 必须被接受的输入：合法 HTTP(S)、公网/局域网/回环、IPv4/IPv6、非默认端口、
/// Unicode IDN 原文与 A-label（含宿主已证实的 `xn--2ba`）、合法 path/query。
const ACCEPTED: &[&str] = &[
    "https://example.com/asset",
    "http://example.com/asset?version=1&mirror=cn",
    "https://example.com/@user/asset",
    "https://example.com/a?x=@y&mirror=cn",
    "https://example.com/路径@名/asset",
    "https://example.com/a b/asset",
    "http://127.0.0.1:8080/asset",
    "http://10.0.0.5/asset",
    "https://[::1]:8443/asset?mirror=cn",
    "https://[2001:db8::1]:8443/asset",
    "https://example.com:8443/asset",
    "HTTPS://EXAMPLE.COM/asset",
    "https://例子.测试/asset",
    "https://xn--2ba.tv/asset",
    "https://xn--fsq.tv/path",
    "http://lan-host.local/asset",
];

/// 必须被拒绝的输入与期望原因：歧义 authority、空/非空 userinfo、fragment、
/// 控制字符、反斜杠、首尾空格、非 HTTP(S)、缺失主机与 0 端口。
const REJECTED: &[(&str, AssetUrlRejection)] = &[
    (
        "ftp://example.com/asset",
        AssetUrlRejection::UnsupportedScheme,
    ),
    (
        "data:text/plain,asset",
        AssetUrlRejection::UnsupportedScheme,
    ),
    (
        "https://user:pass@example.com/asset",
        AssetUrlRejection::UserInfo,
    ),
    (
        "https://user@example.com/asset",
        AssetUrlRejection::UserInfo,
    ),
    ("http://@emptyuser.com/asset", AssetUrlRejection::UserInfo),
    ("http://:@emptypass.com/asset", AssetUrlRejection::UserInfo),
    (
        "http://user:@host.example/asset",
        AssetUrlRejection::UserInfo,
    ),
    (
        "http://:pass@host.example/asset",
        AssetUrlRejection::UserInfo,
    ),
    ("https://@/asset", AssetUrlRejection::UserInfo),
    (
        "https://example.com/asset#fragment",
        AssetUrlRejection::Fragment,
    ),
    ("", AssetUrlRejection::Malformed),
    ("https:///missing", AssetUrlRejection::Malformed),
    ("https:////example.com/asset", AssetUrlRejection::Malformed),
    ("https:/example.com/asset", AssetUrlRejection::Malformed),
    ("http:example.com/asset", AssetUrlRejection::Malformed),
    ("http:", AssetUrlRejection::Malformed),
    ("https://example.com/a\tb", AssetUrlRejection::Malformed),
    (" https://example.com/asset", AssetUrlRejection::Malformed),
    ("https://example.com/asset ", AssetUrlRejection::Malformed),
    (
        "https://exa\u{7f}mple.com/asset",
        AssetUrlRejection::Malformed,
    ),
    ("https://example.com/a\\b", AssetUrlRejection::Malformed),
    ("https://example.com\\asset", AssetUrlRejection::Malformed),
    ("http://example.com:0/asset", AssetUrlRejection::ZeroPort),
    ("https://example.com:0/asset", AssetUrlRejection::ZeroPort),
];

#[test]
/// 接受列表全部通过，且 IDN 原文按 url crate 的 IDNA 规范化为 A-label。
fn accepted_samples_pass_and_idn_normalizes() {
    for value in ACCEPTED {
        let url = validate_asset_url(value).unwrap_or_else(|reason| {
            panic!("expected acceptance: {value} got {}", reason.describe())
        });
        assert_eq!(
            url.scheme(),
            match value
                .split(':')
                .next()
                .unwrap()
                .to_ascii_lowercase()
                .as_str()
            {
                "http" => "http",
                _ => "https",
            }
        );
    }
    let idn = validate_asset_url("https://例子.测试/asset").expect("idn");
    assert!(idn.host_str().is_some_and(|host| host.starts_with("xn--")));
    // 宿主已证实的 A-label：url 2.5.8 的 idna 1.1.0 接受 xn--2ba（解码为 ¿）。
    let a_label = validate_asset_url("https://xn--2ba.tv/asset").expect("a-label");
    assert_eq!(a_label.host_str(), Some("xn--2ba.tv"));
}

#[test]
/// 拒绝列表逐条给出精确原因；错误文案只含固定规则，不回显输入任何部分。
///
/// 空输入的 `contains("")` 恒真，不能用来判断泄露：空样本改为断言固定文案
/// 非空且就是分类常量；非空样本仍严格检查原文不出现。
fn rejected_samples_fail_with_exact_reason_and_no_echo() {
    for (value, expected) in REJECTED {
        let reason = validate_asset_url(value).expect_err(&format!("expected rejection: {value}"));
        assert_eq!(&reason, expected, "{value}");
        let rendered = reason.to_string();
        assert!(!rendered.is_empty(), "rejection text must not be empty");
        assert_eq!(rendered, expected.describe(), "text must stay constant");
        if !value.is_empty() {
            assert!(
                !rendered.contains(value),
                "error must not echo input: {value}"
            );
        }
    }
}

#[test]
/// 重定向 Location 与绝对地址共用同一套语法判定，相对路径与网络路径引用仍可用。
fn redirect_targets_share_the_same_contract() {
    let current = Url::parse("https://public.example/asset").expect("base");
    // 普通相对路径、带 @ 的 path/query、同文档 query 与安全网络路径引用。
    assert_eq!(
        validate_redirect_target(&current, "/next")
            .expect("relative")
            .as_str(),
        "https://public.example/next"
    );
    assert_eq!(
        validate_redirect_target(&current, "/@user/asset")
            .expect("at path")
            .as_str(),
        "https://public.example/@user/asset"
    );
    assert_eq!(
        validate_redirect_target(&current, "?token=@value")
            .expect("query")
            .as_str(),
        "https://public.example/asset?token=@value"
    );
    assert_eq!(
        validate_redirect_target(&current, "//cdn.example/asset")
            .expect("network path")
            .as_str(),
        "https://cdn.example/asset"
    );

    // 歧义与凭据原文在 join 前即拒绝：空 userinfo 会被 join 静默丢弃。
    for location in [
        "//@host/path",
        "//:@host/path",
        "///host/path",
        "//host\\path",
        "#fragment",
        "http:///missing",
        "http://user:pass@public.example/asset",
    ] {
        assert!(
            validate_redirect_target(&current, location).is_err(),
            "{location}"
        );
    }
}

#[test]
/// 相对 Location 沿用基准的协议与主机；基准本身必须先通过资产校验。
fn redirect_base_must_be_valid_and_relative_inherits_scheme() {
    let base = validate_asset_url("https://mirror.example:8443/a").expect("base");
    let next = validate_redirect_target(&base, "b").expect("relative join");
    assert_eq!(next.as_str(), "https://mirror.example:8443/b");
}
