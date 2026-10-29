// @author kongweiguang

use kerminal_domain::identity::AgentSessionId;

use super::{AgentPendingSendPreview, pending_send_preview_expired, pending_source_short_label};

/// 过期或格式损坏的 preview 在 UI 层关闭确认入口，Root service 仍需复核 TTL。
///
/// @author kongweiguang
#[test]
fn invalid_or_expired_preview_fails_closed() {
    let preview = |expires_at: &str| AgentPendingSendPreview {
        agent_session_id: AgentSessionId::new(),
        byte_length: 4,
        expires_at: expires_at.to_owned(),
        preview_id: "preview-1".to_owned(),
        redacted: false,
        request_id: "request-1".to_owned(),
        text: "test".to_owned(),
        truncated: false,
    };

    assert!(pending_send_preview_expired(&preview("not-a-timestamp")));
    assert!(pending_send_preview_expired(&preview(
        "2000-01-01T00:00:00Z"
    )));
    assert!(!pending_send_preview_expired(&preview(
        "2999-01-01T00:00:00Z"
    )));
}

/// 窄右栏的来源 metadata 设定固定最大字符数，正文不会进入列表标签。
///
/// @author kongweiguang
#[test]
fn pending_source_label_is_bounded() {
    let label = "source".repeat(20);
    let bounded = pending_source_short_label(&label);
    assert!(bounded.chars().count() <= 43);
    assert!(bounded.ends_with('…'));
}
