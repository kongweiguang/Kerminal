//! 应用层共享终端输出脱敏规则测试。
//!
//! @author kongweiguang

use kerminal_application::sessions::logging::redact_terminal_text;

/// 固定旧版凭据赋值、Bearer 和 API key 规则，避免日志与 MCP 快照分叉。
#[test]
fn redacts_legacy_terminal_secret_shapes() {
    let input = "password=fixture-secret bearer fixture-token-1234567890 sk-abcdefghij123456";

    let (redacted, changed) = redact_terminal_text(input);

    assert!(changed);
    assert!(!redacted.contains("fixture-secret"));
    assert!(!redacted.contains("fixture-token-1234567890"));
    assert!(!redacted.contains("sk-abcdefghij123456"));
    assert!(redacted.contains("password=[已脱敏]"));
    assert!(redacted.contains("bearer [已脱敏]"));
    assert!(redacted.contains("[已脱敏:api-key]"));
}

/// 无匹配文本必须保持逐字节内容且报告未发生改写，方便 MCP 保留 redacted 标志语义。
#[test]
fn leaves_unmatched_terminal_text_unchanged() {
    let input = "ordinary terminal output and tokenized command";

    let (redacted, changed) = redact_terminal_text(input);

    assert!(!changed);
    assert_eq!(redacted.as_str(), input);
}
