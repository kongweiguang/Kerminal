//! 外部用户名续接失败后的生产输入与重试回归。
//!
//! @author kongweiguang

use super::*;
use kerminal_application::platform::external_launch::{
    ExternalLaunchRequest, ExternalLaunchSource, ExternalTargetDraft,
};

/// 从实际模态的可访问性快照读取普通账号，避免测试通过直接写 owner 状态掩盖输入丢失。
/// @author kongweiguang
fn username_snapshot(
    visual: &mut VisualTestContext,
) -> gpui_kit::base::test_support::ElementSnapshot {
    visual.run_until_parked();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let id: gpui_kit::ElementId = "connections-external-username".into();
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.path().contains(&id) && node.value().is_some())
            .min_by_key(|node| node.path().len())
            .expect("username input is mounted")
    })
}

/// 临时目标注册明确失败时保留账号以便直接重试；取消仍清除该请求并释放模态。
/// 提交中的输入通过真实键入验证冻结，避免依赖不向文本子节点继承的 disabled 快照属性。
/// @author kongweiguang
#[gpui_kit::test]
fn external_username_failure_preserves_input_for_retry(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| {
                panel.request_external_username(
                    ExternalLaunchRequest {
                        source: ExternalLaunchSource::Kerminal,
                        target: ExternalTargetDraft {
                            protocol: RemoteProtocol::Ssh,
                            host: "example.invalid".into(),
                            port: 22,
                            username: None,
                            jump_hosts: Vec::new(),
                        },
                        intent: ExternalLaunchIntent::SshTerminal {
                            remote_command: Vec::new(),
                        },
                        display_name: None,
                        identity_file: None,
                        open_sftp_after_connect: false,
                    },
                    cx,
                );
                panel.focus_root_confirmation(window, cx);
            });
        });
    });
    settle(&runtime, visual);
    let input = username_snapshot(visual);
    visual.simulate_click(input.bounds().center(), Default::default());
    visual.simulate_input("operator");
    assert_eq!(username_snapshot(visual).value(), Some("operator"));
    for _ in 0..2 {
        click_root_button(visual, "connections-external-username-submit");
        let pending = username_snapshot(visual);
        assert_eq!(pending.value(), Some("operator"));
        visual.simulate_input("ignored-while-submitting");
        assert_eq!(
            username_snapshot(visual).value(),
            Some("operator"),
            "submitted account is frozen"
        );
        settle(&runtime, visual);
        let input = username_snapshot(visual);
        assert_eq!(input.value(), Some("operator"));
        assert_ne!(input.disabled(), Some(true), "failure permits editing");
        assert!(sessions.creates.lock().unwrap().is_empty());
    }
    click_root_button(visual, "connections-external-username-cancel");
    settle(&runtime, visual);
    visual.update(|_, cx| {
        assert!(
            !root
                .read(cx)
                .connections_panel
                .as_ref()
                .unwrap()
                .read(cx)
                .root_confirmation_active()
        );
    });
}
