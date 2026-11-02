//! 隐藏设置草稿请求退出时的可见确认与取消回归。
//!
//! @author kongweiguang

use gpui_kit::TestAppContext;

use super::{fixture, settle};
use crate::{
    features::preferences::PreferencesEvent,
    root::{QuitIntent, RootConfirmationOwner, RootHostLifecycle},
};

/// 从真实订阅发出隐藏面板的确认请求，验证取消后不再占用退出意图或派发宿主收尾。
///
/// @author kongweiguang
#[gpui_kit::test]
fn hidden_preferences_quit_confirmation_opens_and_cancel_keeps_app_running(
    app: &mut TestAppContext,
) {
    let (runtime, root, _, _, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            assert!(!root.preferences_modal_open);
            root.pending_quit_intent = Some(QuitIntent::Exit);
            root.preferences_panel.update(cx, |_, cx| {
                cx.emit(PreferencesEvent::QuitConfirmationRequested);
            });
        });
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            assert!(root.preferences_modal_open);
            assert_eq!(root.pending_quit_intent, Some(QuitIntent::Exit));
            assert_eq!(root.host_lifecycle, RootHostLifecycle::Running);
            assert!(!root.host_shutdown_requested);
            root.preferences_panel
                .update(cx, |panel, cx| panel.cancel_quit(cx));
        });
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.preferences_modal_open);
            assert!(root.pending_quit_intent.is_none());
            assert!(!root.quit_ready_emitted);
            assert!(!root.host_shutdown_requested);
        });
    });
}

/// 连接模态不能被后台确认抢占；取消原退出后允许用户继续处理当前主机表单。
///
/// @author kongweiguang
#[gpui_kit::test]
fn blocked_preferences_quit_confirmation_releases_pending_intent(app: &mut TestAppContext) {
    let (runtime, root, _, _, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            root.connections_modal_open = true;
            root.pending_quit_intent = Some(QuitIntent::Exit);
            root.preferences_panel.update(cx, |_, cx| {
                cx.emit(PreferencesEvent::QuitConfirmationRequested);
            });
        });
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.connections_modal_open);
            assert!(!root.preferences_modal_open);
            assert!(root.pending_quit_intent.is_none());
            assert_eq!(root.host_lifecycle, RootHostLifecycle::Running);
            assert!(!root.quit_ready_emitted);
            assert!(!root.host_shutdown_requested);
            assert!(root.status.is_some());
        });
    });
}

/// 运维、文件、认证、片段和 Quick Open 的呈现与键盘优先级必须一致，不能留下隐藏退出确认。
///
/// @author kongweiguang
#[gpui_kit::test]
fn existing_modal_owners_keep_preferences_quit_confirmation_hidden_and_cancel_exit(
    app: &mut TestAppContext,
) {
    #[derive(Clone, Copy, Debug)]
    enum ExistingModal {
        ConnectionPrompt,
        RootConfirmation(RootConfirmationOwner),
        Snippet,
        QuickOpen,
    }
    let (runtime, root, _, _, visual) = fixture(app);
    settle(&runtime, visual);
    for modal in [
        ExistingModal::ConnectionPrompt,
        ExistingModal::RootConfirmation(RootConfirmationOwner::Operations),
        ExistingModal::RootConfirmation(RootConfirmationOwner::FileDelete),
        ExistingModal::RootConfirmation(RootConfirmationOwner::Editor),
        ExistingModal::Snippet,
        ExistingModal::QuickOpen,
    ] {
        visual.update(|_, app| {
            root.update(app, |root, cx| {
                root.connection_prompt_modal_open = false;
                root.root_confirmation_owner = None;
                root.snippet_editor_modal_open = false;
                root.quick_open_modal_open = false;
                match modal {
                    ExistingModal::ConnectionPrompt => root.connection_prompt_modal_open = true,
                    ExistingModal::RootConfirmation(owner) => {
                        root.root_confirmation_owner = Some(owner);
                    }
                    ExistingModal::Snippet => root.snippet_editor_modal_open = true,
                    ExistingModal::QuickOpen => root.quick_open_modal_open = true,
                }
                root.pending_quit_intent = Some(QuitIntent::Exit);
                root.preferences_panel.update(cx, |_, cx| {
                    cx.emit(PreferencesEvent::QuitConfirmationRequested);
                });
            });
        });
        settle(&runtime, visual);
        visual.update(|_, app| {
            root.update(app, |root, _| {
                assert!(!root.preferences_modal_open, "{modal:?}");
                assert!(root.pending_quit_intent.is_none(), "{modal:?}");
                assert!(!root.host_shutdown_requested);
                assert!(!root.quit_ready_emitted);
                match modal {
                    ExistingModal::ConnectionPrompt => assert!(root.connection_prompt_modal_open),
                    ExistingModal::RootConfirmation(owner) => {
                        assert_eq!(root.root_confirmation_owner, Some(owner));
                    }
                    ExistingModal::Snippet => assert!(root.snippet_editor_modal_open),
                    ExistingModal::QuickOpen => assert!(root.quick_open_modal_open),
                }
            });
        });
    }
}

/// 取消 effect 到达前仍由旧退出请求占位，队列中的新更新请求不能先开始再被旧取消撤销。
///
/// @author kongweiguang
#[gpui_kit::test]
fn deferred_preferences_cancellation_keeps_original_quit_intent_until_its_receipt(
    app: &mut TestAppContext,
) {
    let (runtime, root, _, _, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.connections_modal_open = true;
            root.pending_quit_intent = Some(QuitIntent::Exit);
            root.show_preferences_quit_confirmation(window, cx);
            assert_eq!(root.pending_quit_intent, Some(QuitIntent::Exit));
            assert!(!root.request_quit(
                QuitIntent::RestartForUpdate {
                    version: "queued-new-request".to_owned(),
                },
                cx,
            ));
        });
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.pending_quit_intent.is_none());
            assert!(!root.quit_ready_emitted);
            assert!(!root.host_shutdown_requested);
        });
    });
}
