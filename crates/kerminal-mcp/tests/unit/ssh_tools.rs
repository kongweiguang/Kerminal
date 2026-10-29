//! ssh_tools 私有单元测试。
//
// @author kongweiguang

use super::{
    HostSelector, StreamCapture, normalize_script, proxy_environment, redact_summary_sample,
};

/// 确保输入保留原始 shell 脚本语义，但只在远端明确识别环境字段前执行规范化。
#[test]
fn normalizes_script_line_endings_and_rejects_empty_or_nul() {
    assert_eq!(
        normalize_script("  printf 'a'\r\n  ").unwrap(),
        "printf 'a'\n"
    );
    assert!(normalize_script(" \r\n ").is_err());
    assert!(normalize_script("echo\0x").is_err());
}

/// 确保代理 URL 仅落入旧兼容环境变量，并且 SOCKS 协议不会改写 SSH route。
#[test]
fn proxy_environment_matches_legacy_protocol_mapping() {
    let http = proxy_environment(Some("http://user:pass@proxy.example:8080"), None).unwrap();
    assert_eq!(
        http.get("HTTP_PROXY").unwrap(),
        "http://user:pass@proxy.example:8080"
    );
    assert_eq!(
        http.get("https_proxy").unwrap(),
        "http://user:pass@proxy.example:8080"
    );
    assert_eq!(http.get("NO_PROXY").unwrap(), "localhost,127.0.0.1");
    let socks = proxy_environment(Some("socks5h://proxy.example:1080"), None).unwrap();
    assert_eq!(
        socks.get("ALL_PROXY").unwrap(),
        "socks5h://proxy.example:1080"
    );
    assert!(proxy_environment(Some("file:///tmp/proxy"), None).is_err());
    assert!(proxy_environment(Some("http://proxy\nexport X=1"), None).is_err());
}

/// 验证每个输出流独立限制缓存并继续计数，确保事件消费不会在截断处停止。
#[test]
fn capture_is_bounded_and_reports_truncation() {
    let mut capture = StreamCapture::new(3);
    capture.push(b"ab");
    capture.push(b"cdef");
    assert_eq!(capture.text(), "abc");
    assert_eq!(capture.captured_bytes(), 3);
    assert!(capture.truncated());
}

/// 确认没有 selector 时不会误选，后续 resolver 可明确返回 missingTarget。
#[test]
fn empty_selector_is_rejected_and_defaults_are_empty() {
    let selector = HostSelector::default();
    assert!(!selector.has_any());
}

/// 合并后的命令入口必须同时接受旧 hostId 请求和名称定位，且不扩展为接收连接密码。
#[test]
fn unified_command_accepts_both_selectors_and_rejects_inline_credentials() {
    let direct: super::ResolvedCommandArgs = serde_json::from_value(serde_json::json!({
        "hostId":"saved:fixture", "command":"pwd"
    }))
    .unwrap();
    assert_eq!(
        HostSelector::from_args(&direct).host_id.as_deref(),
        Some("saved:fixture")
    );
    let named: super::ResolvedCommandArgs = serde_json::from_value(serde_json::json!({
        "groupName":"测试", "name":"fixture", "command":"pwd"
    }))
    .unwrap();
    let selector = HostSelector::from_args(&named);
    assert!(selector.has_any());
    assert_eq!(selector.group_name.as_deref(), Some("测试"));
    assert_eq!(selector.name.as_deref(), Some("fixture"));
    assert!(
        serde_json::from_value::<super::ResolvedCommandArgs>(serde_json::json!({
            "hostId":"saved:fixture", "command":"pwd", "password":"forbidden"
        }))
        .is_err()
    );
}

/// 摘要副本隐藏最常见明文凭据，同时不改写实际捕获的 stdout 字段。
#[test]
fn summary_sample_masks_common_secret_assignments() {
    let (sample, redacted) = redact_summary_sample("user=kong token=abc123 done");
    assert!(redacted);
    assert!(sample.contains("token=[redacted]"));
    assert!(!sample.contains("abc123"));
}
