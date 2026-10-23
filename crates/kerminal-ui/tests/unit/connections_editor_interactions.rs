//! 协议返回、嵌套确认焦点及字段门禁的生产表单回归。
//!
//! @author kongweiguang

use gpui_kit::Focusable as _;

use super::super::super::local_hosts::LocalHostEditorSection;
use super::*;

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

/// 显式应用默认值跨协议中断后仍是显式意图；重试不能退化为仅填初始空字段。
/// @author kongweiguang
#[gpui_kit::test]
fn interrupted_explicit_defaults_retain_request_snapshot(app: &mut TestAppContext) {
    let (profiles, release) = delayed_profiles();
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx)
    });
    release.add_permits(1);
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
        panel.use_default_local_shell(window, cx);
    });
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Ssh, window, cx)
    });
    release.add_permits(1);
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
    release.add_permits(1);
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
