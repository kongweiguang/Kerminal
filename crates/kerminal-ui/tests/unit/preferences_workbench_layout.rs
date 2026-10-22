//! 布局末意图的有界关联与 settings 草稿门禁回归；不替代 Root 或原生布局验收。
//!
//! @author kongweiguang

use super::{WorkbenchLayoutRequests, workbench_layout_save_ready};
use crate::features::preferences::{ConfigDraft, DraftSnapshot, DraftStatus};
use kerminal_application::{
    PublicErrorCode,
    preferences::{SettingsSnapshot, WorkbenchLayoutSettings},
};

/// 用不同合法尺寸标记意图，测试只比较 typed 事实而不依赖窗口或像素测量。
///
/// @author kongweiguang
fn layout(width: u16) -> WorkbenchLayoutSettings {
    WorkbenchLayoutSettings {
        host_sidebar_width: width,
        ..WorkbenchLayoutSettings::default()
    }
}

/// 自动写入只接受已加载且干净的 Ready，任何受保护状态都不能顺带提交其它编辑。
///
/// @author kongweiguang
#[test]
fn layout_save_requires_clean_ready_draft() {
    assert!(workbench_layout_save_ready(
        Some(DraftStatus::Ready),
        false,
        false,
        false,
        false,
    ));
    for status in [
        None,
        Some(DraftStatus::Saving),
        Some(DraftStatus::Failed),
        Some(DraftStatus::ExternalConflict),
        Some(DraftStatus::OutcomeUnknown),
    ] {
        assert!(!workbench_layout_save_ready(
            status, false, false, false, false
        ));
    }
    for flags in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        assert!(!workbench_layout_save_ready(
            Some(DraftStatus::Ready),
            flags.0,
            flags.1,
            flags.2,
            flags.3,
        ));
    }
}

/// 尚未发送的值只保留最后一个；在途期间的新手势不并发写入，也不替换旧 request 关联。
///
/// @author kongweiguang
#[test]
fn pending_layout_coalesces_and_old_success_preserves_newer_intent() {
    let mut requests = WorkbenchLayoutRequests::default();
    requests.request(layout(200));
    let a = requests.request(layout(220));
    let sent_a = requests.begin().expect("one pending intent");
    assert_eq!(sent_a.request_id, a);
    assert_eq!(sent_a.layout, layout(220));

    requests.request(layout(240));
    let b = requests.request(layout(260));
    assert!(requests.begin().is_none());
    assert_eq!(requests.finish(true).expect("receipt A").request_id, a);
    assert!(requests.has_pending());

    let sent_b = requests.begin().expect("latest pending intent");
    assert_eq!(sent_b.request_id, b);
    assert_ne!(a, b);
    assert_eq!(sent_b.layout, layout(260));
    assert_eq!(requests.finish(true).expect("receipt B").request_id, b);
    assert!(!requests.has_pending());
}

/// 保存失败即使随后读到 Ready 也不能自动重放；新显式手势仍受原草稿 Unknown 门禁约束。
///
/// @author kongweiguang
#[test]
fn failure_preserves_latest_intent_and_requires_explicit_authority() {
    let mut requests = WorkbenchLayoutRequests::default();
    let a = requests.request(layout(220));
    requests.begin().expect("intent A");
    let b = requests.request(layout(260));
    assert_eq!(requests.finish(false).expect("failed A").request_id, a);
    assert_eq!(requests.pending.as_ref().expect("retained B").request_id, b);
    assert!(
        requests.begin().is_none(),
        "Ready alone cannot clear the fence"
    );

    let c = requests.request(layout(280));
    assert!(!workbench_layout_save_ready(
        Some(DraftStatus::OutcomeUnknown),
        true,
        false,
        false,
        false,
    ));
    assert!(workbench_layout_save_ready(
        Some(DraftStatus::Ready),
        false,
        false,
        false,
        false,
    ));
    let sent = requests
        .begin()
        .expect("new explicit intent after readback");
    assert_eq!(sent.request_id, c);
    assert_eq!(sent.layout, layout(280));
}

/// 无更晚手势时失败值不能丢失；显式核对差异不写入，完全相等才确认已存在的配置事实。
///
/// @author kongweiguang
#[test]
fn explicit_readback_confirms_only_matching_retained_layout() {
    let mut requests = WorkbenchLayoutRequests::default();
    let request_id = requests.request(layout(260));
    requests.begin().expect("layout save");
    requests.finish(false).expect("failed receipt");
    assert!(requests.has_pending());
    assert!(requests.confirm_readback(&layout(220)).is_none());
    assert!(
        requests.begin().is_none(),
        "different readback cannot authorize a write"
    );
    let confirmed = requests
        .confirm_readback(&layout(260))
        .expect("matching fact");
    assert_eq!(confirmed.request_id, request_id);
    assert!(!requests.has_pending());
    assert!(requests.begin().is_none());
}

/// 普通设置保存的回执不能凭空领取布局收据；没有在途布局时末意图保持不变。
///
/// @author kongweiguang
#[test]
fn unrelated_settings_receipt_does_not_consume_queued_layout() {
    let mut requests = WorkbenchLayoutRequests::default();
    let request_id = requests.request(layout(260));
    assert!(requests.finish(true).is_none());
    assert!(requests.finish(false).is_none());
    assert_eq!(
        requests.pending.as_ref().expect("queued").request_id,
        request_id
    );
    assert!(!requests.blocked_after_failure);
}

/// clean gate 不改脏稿；成功后只用确认快照推进 revision，再取出后续布局意图。
///
/// @author kongweiguang
#[test]
fn existing_draft_owns_cas_and_keeps_unrelated_edits_out_of_layout_save() {
    let baseline = SettingsSnapshot::default();
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: baseline.clone(),
        revision: 1_u64,
    });
    let mut edited = baseline.clone();
    edited.terminal.show_tab_numbers = !edited.terminal.show_tab_numbers;
    draft.replace_value(edited.clone()).expect("edit");
    assert!(!workbench_layout_save_ready(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        false,
        false,
    ));
    assert_eq!(draft.value(), &edited);
    assert_eq!(draft.revision(), &1);
    draft.discard_changes().expect("explicit discard");

    let mut requests = WorkbenchLayoutRequests::default();
    let a = requests.request(layout(260));
    let intent = requests.begin().expect("intent after discard");
    let mut candidate = draft.value().clone();
    candidate.workbench_layout = intent.layout;
    draft
        .replace_value(candidate.clone())
        .expect("layout-only candidate");
    draft.begin_save().expect("same settings CAS lifecycle");
    let b = requests.request(layout(280));
    assert!(!workbench_layout_save_ready(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        false,
        false,
    ));
    draft
        .save_succeeded(DraftSnapshot {
            value: candidate,
            revision: 2,
        })
        .expect("application-confirmed receipt");
    assert_eq!(requests.finish(true).expect("receipt A").request_id, a);
    assert_eq!(draft.revision(), &2);
    assert!(!draft.is_dirty());
    assert_eq!(draft.value().terminal, baseline.terminal);
    assert!(workbench_layout_save_ready(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        false,
        false,
    ));
    assert_eq!(requests.begin().expect("queued B").request_id, b);
}

/// 布局尚未发送时，另一 settings 写入结果未知或冲突也必须保留 fence，不能靠 watcher Ready 开写。
///
/// @author kongweiguang
#[test]
fn unresolved_ordinary_settings_write_also_fences_queued_layout() {
    for code in [PublicErrorCode::Conflict, PublicErrorCode::OutcomeUnknown] {
        let mut requests = WorkbenchLayoutRequests::default();
        let request_id = requests.request(layout(260));
        requests.block_on_settings_error(code);
        assert!(
            requests.finish(false).is_none(),
            "no layout write was attempted"
        );
        assert!(requests.has_pending());
        assert!(
            requests.begin().is_none(),
            "readback Ready is not replay authority"
        );
        assert_eq!(
            requests
                .confirm_readback(&layout(260))
                .expect("explicit equal readback")
                .request_id,
            request_id,
        );
        assert!(!requests.has_pending());
    }
}
