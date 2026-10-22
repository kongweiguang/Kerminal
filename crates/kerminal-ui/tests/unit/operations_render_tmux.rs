//! tmux 窄栏只在接入条件受限时显示简短、面向用户的提示。
//!
//! @author kongweiguang

use super::tmux_attach_guidance;
use super::{
    SectionPhase,
    tmux::{TmuxHeaderTone, tmux_header_status, tmux_header_tone},
};
use kerminal_application::operations::TmuxCapability;

/// 就绪状态不占正文，未绑定与目标不匹配时保留各自的可执行原因。
///
/// @author kongweiguang
#[test]
fn tmux_attach_guidance_is_conditional_and_user_facing() {
    assert_eq!(tmux_attach_guidance(true, false), None);
    assert_eq!(tmux_attach_guidance(true, true), None);
    assert_eq!(
        tmux_attach_guidance(false, false),
        Some((
            "请聚焦当前目标对应的终端，再连接会话。",
            "Focus a terminal for this target to attach.",
        ))
    );
    assert_eq!(
        tmux_attach_guidance(false, true),
        Some((
            "当前终端与列表目标不一致，请切换到对应终端。",
            "The focused terminal does not match this list; switch to the matching terminal.",
        ))
    );
}

/// 标题状态只在真实 tmux 探测成功时呈现可用色，并保留失败与取消的重试语义。
///
/// @author kongweiguang
#[test]
fn tmux_header_reflects_query_phase_and_capability() {
    let available = TmuxCapability {
        target_ref: "local".to_owned(),
        available: true,
        version: Some("tmux 3.2a".to_owned()),
        reason: None,
    };
    let unavailable = TmuxCapability {
        target_ref: "local".to_owned(),
        available: false,
        version: None,
        reason: Some("tmux command was not found".to_owned()),
    };

    assert!(!tmux_header_status(SectionPhase::Ready, Some(&available)).is_empty());
    assert_eq!(
        tmux_header_tone(SectionPhase::Ready, Some(&available)),
        TmuxHeaderTone::Ready
    );
    assert_eq!(
        tmux_header_status(SectionPhase::Ready, Some(&unavailable)),
        "tmux command was not found"
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Ready, Some(&unavailable)),
        TmuxHeaderTone::Error
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Loading, Some(&available)),
        TmuxHeaderTone::Loading
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Cancelled, None),
        TmuxHeaderTone::Neutral
    );
    assert_eq!(
        tmux_header_status(SectionPhase::Ready, None),
        "Status unknown"
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Ready, None),
        TmuxHeaderTone::Neutral
    );
    assert_eq!(
        tmux_header_status(SectionPhase::Failed, Some(&available)),
        "Read failed; retry available"
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Failed, Some(&available)),
        TmuxHeaderTone::Error
    );
}
