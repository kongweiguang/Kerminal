//! 协议返回、嵌套确认焦点及字段门禁的生产表单回归。
//!
//! @author kongweiguang

use gpui_kit::Focusable as _;

use super::super::super::local_hosts::LocalHostEditorSection;
use super::*;

/// 长指纹和错误说明不能把信任操作挤出窄矮窗口，取消仍须撤销原待处理请求。
/// @author kongweiguang
#[gpui_kit::test]
fn host_key_confirmation_keeps_actions_visible_in_a_short_window(app: &mut TestAppContext) {
    use super::super::{PendingAction, PendingTrust};
    use kerminal_application::connections::{HostKeyIdentity, HostKeyStatus};
    use kerminal_domain::files::FileEndpoint;

    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(480.), gpui_kit::px(300.)));
    let host_id = HostId::new();
    let fingerprint = format!("SHA256:{}", "a".repeat(43));
    let challenge = HostKeyChallenge {
        host_id: host_id.clone(),
        peer: HostKeyPeer::Target,
        endpoint: HostEndpoint {
            host: "long-host-name.example.test".to_owned(),
            port: 22,
            username: "fixture".to_owned(),
        },
        algorithm: "ssh-ed25519".to_owned(),
        fingerprint_sha256: fingerprint.clone(),
        identity: HostKeyIdentity {
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint_sha256: fingerprint,
            public_key_openssh: "ssh-ed25519 AAAA".to_owned(),
        },
        status: HostKeyStatus::Changed {
            previous_fingerprint_sha256: format!("SHA256:{}", "b".repeat(43)),
        },
    };
    update_panel(&panel, visual, |panel, _, cx| {
        panel.pending_trust = Some(PendingTrust {
            challenge,
            action: PendingAction::SftpSelection {
                endpoint: FileEndpoint::Local {
                    path: std::path::PathBuf::from("fixture"),
                },
                request_id: "fixture".to_owned(),
            },
            display_host: None,
            external_classification: None,
            second_confirmation: false,
            operation_busy: false,
            operation_cancellation: None,
        });
        cx.notify();
    });

    let body = visual
        .debug_bounds("connections-hostkey-body")
        .expect("信任正文应可测量");
    let actions = visual
        .debug_bounds("connections-hostkey-actions")
        .expect("信任操作栏应可测量");
    let viewport = visual.update(|window, _| window.viewport_size());
    assert!(body.size.height > gpui_kit::px(0.));
    assert!(
        body.bottom() <= actions.origin.y,
        "正文视口与操作栏重叠：body={body:?}, actions={actions:?}, viewport={viewport:?}"
    );
    assert!(actions.bottom() <= viewport.height);
    let identity = visual
        .debug_bounds("connections-hostkey-identity")
        .expect("信任正文首行应可测量");
    let position = body.center();
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_event(gpui_kit::ScrollWheelEvent {
        position,
        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(-12.))),
        ..Default::default()
    });
    draw(visual);
    let scrolled = visual
        .debug_bounds("connections-hostkey-identity")
        .expect("信任正文滚动后仍应可测量");
    assert!(scrolled.origin.y < identity.origin.y, "正文应独立滚动");
    assert_eq!(
        visual.debug_bounds("connections-hostkey-body"),
        Some(body),
        "滚动正文不能移动可视窗口"
    );
    assert_eq!(
        visual.debug_bounds("connections-hostkey-actions"),
        Some(actions),
        "滚动正文不能移动确认操作栏"
    );

    let cancel = visual
        .debug_bounds("connections-hostkey-cancel")
        .expect("取消按钮应可点击");
    visual.simulate_click(cancel.center(), Default::default());
    draw(visual);
    visual.update(|_, app| assert!(panel.read(app).pending_trust.is_none()));
}

/// 本地和远程表单出现或清除反馈时，字段起点与保存按钮位置都不能随之跳动。
/// @author kongweiguang
#[gpui_kit::test]
fn host_editor_feedback_preserves_field_and_action_positions(app: &mut TestAppContext) {
    for local in [false, true] {
        let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
        visual.simulate_resize(gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(700.)));
        update_panel(&panel, visual, |panel, window, cx| {
            if local {
                panel.begin_create_local_host(None, window, cx);
            } else {
                panel.begin_create_host(window, cx);
            }
        });
        pump_runtime(&runtime, visual);
        update_panel(&panel, visual, |panel, _, cx| {
            panel.status = None;
            panel.local_hosts.status = None;
            cx.notify();
        });
        let (field, save) = if local {
            (
                "connections-local-editor-viewport",
                "connections-local-editor-save",
            )
        } else {
            ("connections-editor-username-row", "connections-editor-save")
        };
        let field_before = visual.debug_bounds(field).expect("表单字段应可测量");
        let save_before = visual.debug_bounds(save).expect("保存按钮应可测量");
        update_panel(&panel, visual, |panel, _, cx| {
            if local {
                panel.local_hosts.status = Some("读取失败，请重试。".repeat(12));
            } else {
                panel.status = Some("连接测试失败，请重试。".repeat(12));
            }
            cx.notify();
        });
        assert!(visual.debug_bounds("connections-editor-feedback").is_some());
        assert_eq!(
            visual.debug_bounds(field).expect("字段仍应存在").origin.y,
            field_before.origin.y
        );
        assert_eq!(
            visual
                .debug_bounds(save)
                .expect("保存按钮仍应存在")
                .origin
                .y,
            save_before.origin.y
        );
        update_panel(&panel, visual, |panel, _, cx| {
            if local {
                panel.local_hosts.status = None;
            } else {
                panel.status = None;
            }
            cx.notify();
        });
        assert!(visual.debug_bounds("connections-editor-feedback").is_none());
    }
}

/// 迟到的旧回执不能填隐藏表单；返回后必须重新取得默认值，同时保护用户后来输入。
/// @author kongweiguang
#[gpui_kit::test]
fn interrupted_local_defaults_resume_without_overwriting_edits(app: &mut TestAppContext) {
    let (profiles, release) = delayed_profiles();
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    for edit_arguments in [false, true] {
        update_panel(&panel, visual, |panel, window, cx| {
            panel.begin_create_local_host(None, window, cx);
        });
        pump_runtime(&runtime, visual);
        update_panel(&panel, visual, |panel, window, cx| {
            panel.activate_remote_protocol(RemoteProtocol::Ssh, window, cx);
        });
        release.add_permits(1);
        pump_runtime(&runtime, visual);
        visual.update(|_, app| {
            let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
            assert!(editor.shell.read(app).value().is_empty());
        });
        update_panel(&panel, visual, |panel, window, cx| {
            panel.activate_local_host_editor(window, cx);
            if edit_arguments {
                panel
                    .local_hosts
                    .editor
                    .as_ref()
                    .unwrap()
                    .args
                    .update(cx, |input, cx| input.replace_all("--manual", window, cx));
            }
        });
        release.add_permits(1);
        pump_runtime(&runtime, visual);
        visual.update(|_, app| {
            let panel = panel.read(app);
            let editor = panel.local_hosts.editor.as_ref().unwrap();
            assert_eq!(
                editor.shell.read(app).value().as_str(),
                default_profile().shell
            );
            assert_eq!(editor.cwd.read(app).value().as_str(), "C:\\fixture");
            assert_eq!(
                editor.args.read(app).value().as_str(),
                if edit_arguments {
                    "--manual"
                } else {
                    "-NoLogo"
                }
            );
            assert_eq!(panel.local_host_is_dirty(app), edit_arguments);
        });
        if edit_arguments {
            update_panel(&panel, visual, |panel, window, cx| {
                panel.cancel_editor(window, cx)
            });
            visual.update(|_, app| assert!(panel.read(app).pending_editor_discard));
            update_panel(&panel, visual, |panel, window, cx| {
                panel.confirm_discard_editor(window, cx)
            });
        } else {
            close_clean_editor(&panel, visual);
        }
    }
}

/// 显式选择已保存 Profile 跨协议中断后仍是显式意图；重试不能退化为仅填初始空字段。
/// @author kongweiguang
#[gpui_kit::test]
fn interrupted_profile_selection_retains_request_snapshot(app: &mut TestAppContext) {
    let release_default = Arc::new(Semaphore::new(0));
    let release_selected = Arc::new(Semaphore::new(0));
    let profile = default_profile();
    let profile_id = profile.id.clone();
    let row = Arc::new(Mutex::new(VersionedProfile {
        profile,
        revision: ConfigRevision::from_sha256("stored-revision".into()),
    }));
    let profiles = ProfileService::new(
        Arc::new(DelayedProfileDirectory {
            release_default: release_default.clone(),
            selected: Some((row, release_selected.clone())),
        }),
        Arc::new(NoPlatformShellFallback),
    );
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx)
    });
    release_default.add_permits(1);
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .args
            .update(cx, |input, cx| {
                input.replace_all("--before-request", window, cx)
            });
        panel.use_local_profile(profile_id, cx);
    });
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Ssh, window, cx)
    });
    release_selected.add_permits(1);
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .update(cx, |input, cx| {
                input.replace_all("KEEP=after-request", window, cx)
            });
    });
    release_selected.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.local_hosts.editor.as_ref().unwrap();
        assert_eq!(editor.args.read(app).value().as_str(), "-NoLogo");
        assert_eq!(editor.env.read(app).value().as_str(), "KEEP=after-request");
        assert!(panel.local_host_is_dirty(app));
    });
}

/// 继续编辑应返回用户刚离开的可见字段，不将焦点交给当前分区没有渲染的名称输入。
/// @author kongweiguang
#[gpui_kit::test]
fn keep_editing_restores_visible_remote_and_local_input_focus(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.select_editor_section(HostEditorSection::Network, window, cx);
        panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .update(cx, |input, cx| input.replace_all("45", window, cx));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        let focus = panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        panel.cancel_editor(window, cx);
    });
    visual.update(|window, app| {
        let panel = panel.read(app);
        assert!(panel.pending_editor_discard);
        assert!(panel.discard_focus.contains_focused(window, app));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|window, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        assert_eq!(editor.section, HostEditorSection::Network);
        assert!(
            editor
                .inputs
                .keepalive
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx);
        panel.confirm_discard_editor(window, cx);
        panel.begin_create_local_host(None, window, cx);
        panel.select_local_host_section(LocalHostEditorSection::Terminal, window, cx);
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .update(cx, |input, cx| input.replace_all("KEEP=focus", window, cx));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        let focus = panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        panel.cancel_editor(window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|window, app| {
        let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
        assert_eq!(editor.section, LocalHostEditorSection::Terminal);
        assert!(editor.env.read(app).focus_handle(app).is_focused(window));
    });
}

/// 先前焦点属于已卸载分区时，继续编辑应落在当前可见字段而不复活旧分区焦点。
/// @author kongweiguang
#[gpui_kit::test]
fn keep_editing_falls_back_to_current_section_after_focus_unmount(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.select_editor_section(HostEditorSection::Network, window, cx);
        panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .update(cx, |input, cx| input.replace_all("45", window, cx));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        let focus = panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        panel.cancel_editor(window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_editor_section(HostEditorSection::Terminal, window, cx)
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|window, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        assert_eq!(editor.section, HostEditorSection::Terminal);
        assert!(
            editor
                .inputs
                .term
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
}

/// 收藏与表单字段共享所有锁；测试取消释放锁后，既有草稿必须能够继续修改。
/// @author kongweiguang
#[gpui_kit::test]
fn favorite_respects_field_locks_and_becomes_editable_after_probe_cancel(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.recovery.mode = ConfigurationMode::ReadOnlyRecovery;
        panel.toggle_favorite(cx);
        assert!(!panel.editor.as_ref().unwrap().favorite);
        panel.recovery.mode = ConfigurationMode::ReadWrite;
        panel.saving = true;
        panel.toggle_favorite(cx);
        assert!(!panel.editor.as_ref().unwrap().favorite);
        panel.saving = false;
        panel.save_outcome_unknown = true;
        panel.toggle_favorite(cx);
        assert!(!panel.editor.as_ref().unwrap().favorite);
        panel.save_outcome_unknown = false;
        panel.conflict_reload_in_progress =
            Some(super::super::ConflictReloadTarget::Remote(HostId::new()));
        panel.toggle_favorite(cx);
        assert!(!panel.editor.as_ref().unwrap().favorite);
        panel.conflict_reload_in_progress = None;
        panel.probe_running = true;
        panel.toggle_favorite(cx);
        assert!(!panel.editor.as_ref().unwrap().favorite);
        assert!(panel.probe_running, "被禁止的收藏不能顺带取消测试");
        panel.cancel_connection_probe(cx);
        panel.toggle_favorite(cx);
        assert!(panel.editor.as_ref().unwrap().favorite);
        assert!(panel.editor.as_ref().unwrap().is_dirty(cx));
    });
}
