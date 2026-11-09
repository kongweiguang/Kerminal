//! Quick Open 模态的真实 Tab 边界与取消后焦点恢复回归。
//!
//! @author kongweiguang

use gpui_kit::{TestAppContext, base::active_focus_trap};

use std::sync::Arc;

use super::{
    EmptyConfiguration, EmptyHosts, NoRemote, fixture, fixture_with_configuration_and_services,
    saved_sftp_host, settle,
};
use crate::features::quick_open::QuickOpenMode;
use kerminal_application::connections::management::HostManagementService;
use kerminal_domain::identity::HostId;

/// 使用真实 Root 控件和 Command 查询框，确保模态内 Tab 不泄漏到被遮挡的工作台。
/// Escape 走生产事件订阅，不能用手动关闭来掩盖失去取消入口的问题。
///
/// @author kongweiguang
#[gpui_kit::test]
fn quick_open_tab_stays_modal_and_escape_restores_trigger(app: &mut TestAppContext) {
    let (runtime, root, _, _, visual) = fixture(app);
    settle(&runtime, visual);
    let origin = visual.update(|window, app| {
        window.draw(app).clear(app);
        window.focus_next(app);
        let origin = window.focused(app).expect("工作台存在真实可聚焦控件");
        assert_ne!(origin, root.read(app).focus_handle);
        origin
    });
    for mode in [QuickOpenMode::QuickOpen, QuickOpenMode::CommandPalette] {
        visual.update(|window, app| {
            root.update(app, |root, cx| root.open_quick_open(mode, window, cx));
        });
        settle(&runtime, visual);
        visual.update(|window, app| window.draw(app).clear(app));
        for key in ["tab", "shift-tab", "tab", "shift-tab"] {
            visual.simulate_keystrokes(key);
            settle(&runtime, visual);
            visual.update(|window, app| {
                window.draw(app).clear(app);
                assert!(
                    active_focus_trap(window, app).is_some(),
                    "{key} 必须留在 Quick Open"
                );
                assert!(
                    !origin.is_focused(window),
                    "底层控件不能在模态打开时取得焦点"
                );
            });
        }
        visual.simulate_keystrokes("escape");
        settle(&runtime, visual);
        visual.update(|window, app| {
            window.draw(app).clear(app);
            assert!(!root.read(app).quick_open_modal_open);
            assert!(origin.is_focused(window), "取消后返回实际发起控件");
        });
    }
}

/// 搜索真实内存主机并按 Enter，覆盖只更新导航选择而不打开新页面的关闭分支。
/// 焦点不能留在已卸载的查询框，且不得为了恢复焦点自动建立 SSH 会话。
///
/// @author kongweiguang
#[gpui_kit::test]
fn quick_open_host_selection_restores_trigger_without_connecting(app: &mut TestAppContext) {
    let host_id = HostId::new();
    let saved = saved_sftp_host(host_id.clone());
    let (runtime, root, sessions, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |services| {
            services.with_connections(
                HostManagementService::new(Arc::new(EmptyHosts { saved: vec![saved] })),
                Arc::new(NoRemote),
                None,
            )
        },
    );
    settle(&runtime, visual);
    let origin = visual.update(|window, app| {
        window.draw(app).clear(app);
        window.focus_next(app);
        let origin = window.focused(app).expect("工作台存在真实发起控件");
        root.update(app, |root, cx| {
            root.open_quick_open(QuickOpenMode::QuickOpen, window, cx)
        });
        origin
    });
    settle(&runtime, visual);
    visual.simulate_input(host_id.as_str());
    settle(&runtime, visual);
    visual.simulate_keystrokes("enter");
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.draw(app).clear(app);
        assert!(!root.read(app).quick_open_modal_open);
        assert_eq!(root.read(app).selected_host_id.as_ref(), Some(&host_id));
        assert!(origin.is_focused(window), "主机选择完成后恢复发起控件");
    });
    assert!(
        sessions.creates.lock().unwrap().is_empty(),
        "选择主机不能自动连接"
    );
}
