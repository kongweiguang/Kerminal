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

/// 密码提示同样在 root prompt-only 卡片内显示进度和失败；取消后专用消息必须清理，
/// 避免下一请求继承上一条错误。
/// @author kongweiguang
#[gpui_kit::test]
fn external_password_prompt_shows_progress_and_failure_inside_card(app: &mut TestAppContext) {
    let (runtime, root, _, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    let mut target = routed_target(temporary, "external:password-prompt-status");
    target.authentication_required = true;
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| {
                panel.request_external_target(target, cx);
                assert!(
                    panel.root_confirmation_active(),
                    "request must open the password prompt synchronously; state={:?}",
                    panel.debug_external_prompt_state()
                );
                panel.focus_root_confirmation(window, cx);
            });
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| {
                assert!(
                    panel.root_confirmation_active(),
                    "password prompt must be mounted in the root overlay; state={:?}",
                    panel.debug_external_prompt_state()
                );
                panel.seed_external_password_for_test("secret-pass", window, cx);
                panel.submit_external_password_for_test(window, cx);
                assert!(
                    panel.external_authenticating_for_test(),
                    "password input is disabled while authenticating"
                );
                let (message, is_error) = panel
                    .external_prompt_message()
                    .expect("password progress must be visible inside the prompt card");
                assert!(!is_error);
                assert!(!message.is_empty(), "message={message}");
            });
        });
    });
    settle(&runtime, visual);
    visual.update(|_, cx| {
        let panel = root.read(cx).connections_panel.as_ref().unwrap().read(cx);
        assert!(
            panel.root_confirmation_active(),
            "failed password prompt stays open"
        );
        assert!(!panel.external_authenticating_for_test());
        let (message, is_error) = panel
            .external_prompt_message()
            .expect("password failure must be visible inside the prompt card");
        assert!(is_error);
        assert!(!message.is_empty());
    });
    click_root_button(visual, "connections-external-password-cancel");
    settle(&runtime, visual);
    visual.update(|_, cx| {
        let panel = root.read(cx).connections_panel.as_ref().unwrap().read(cx);
        assert!(!panel.root_confirmation_active());
        assert!(panel.external_prompt_message().is_none());
    });
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
        // 直接调用生产提交入口并在同一帧读取专用进度字段，避免测试泵把瞬时“提交中”状态错过。
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                let panel = root.connections_panel.as_ref().unwrap().clone();
                panel.update(cx, |panel, cx| {
                    panel.submit_external_username_for_test(window, cx);
                    let (message, is_error) = panel
                        .external_prompt_message()
                        .expect("submitting status must be visible inside the prompt card");
                    assert!(
                        !is_error,
                        "in-flight message must not look like an error: {message}"
                    );
                    assert!(!message.is_empty(), "message={message}");
                });
            });
        });
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
        visual.update(|_, cx| {
            let panel = root.read(cx).connections_panel.as_ref().unwrap().read(cx);
            assert!(panel.root_confirmation_active(), "failed prompt stays open");
            let (message, is_error) = panel
                .external_prompt_message()
                .expect("failure reason must be visible inside the prompt card");
            assert!(is_error);
            assert!(!message.is_empty());
        });
        assert!(sessions.creates.lock().unwrap().is_empty());
    }
    click_root_button(visual, "connections-external-username-cancel");
    settle(&runtime, visual);
    visual.update(|_, cx| {
        let panel = root.read(cx).connections_panel.as_ref().unwrap().read(cx);
        assert!(!panel.root_confirmation_active());
        assert!(
            panel.external_prompt_message().is_none(),
            "cancel must clear the prompt-only message"
        );
    });
}
