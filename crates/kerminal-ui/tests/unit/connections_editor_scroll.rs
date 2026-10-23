//! 窄窗口主机编辑器焦点定位与滚动生命周期的 GPUI 回归。
//!
//! @author kongweiguang

use super::super::super::local_hosts::LocalHostEditorSection;
use super::*;
use gpui_kit::{FocusHandle, Focusable as _, ScrollHandle, px, size};

const VIEWPORT_SELECTOR: &str = "connections-editor-viewport";
const USERNAME_ROW_SELECTOR: &str = "connections-editor-username-row";
const PRESERVE_MTIME_ROW_SELECTOR: &str = "connections-editor-preserve-mtime-row";

/// GPUI 仅向活动窗口派发 focus-in；测试必须模拟用户正在操作窗口，避免只移动焦点而不派发事件。
/// @author kongweiguang
fn activate_editor_window(visual: &mut VisualTestContext) {
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    assert!(visual.update(|window, _| window.is_window_active()));
}

/// 继续使用真实编辑器的 ScrollHandle 和输入焦点，避免为了测试另造滚动状态。
/// @author kongweiguang
fn editor_focus_targets(
    panel: &Entity<ConnectionsPanel>,
    visual: &mut VisualTestContext,
) -> (ScrollHandle, FocusHandle, FocusHandle) {
    visual.update(|_, app| {
        let editor = panel
            .read(app)
            .editor
            .as_ref()
            .expect("远程主机编辑器已打开");
        (
            editor.scroll_handle.clone(),
            editor.inputs.name.read(app).focus_handle(app),
            editor.inputs.username.read(app).focus_handle(app),
        )
    })
}

/// 验证生产表单字段的实际裁剪 bounds，而不是只根据滚动偏移推断可见性。
/// @author kongweiguang
fn assert_row_fully_visible(visual: &mut VisualTestContext, row_selector: &'static str) {
    let viewport = visual
        .debug_bounds(VIEWPORT_SELECTOR)
        .expect("编辑器视口应提供当前帧 bounds");
    let row = visual
        .debug_bounds(row_selector)
        .expect("字段行应提供当前帧 bounds");
    assert!(
        row.origin.y >= viewport.origin.y && row.bottom() <= viewport.bottom(),
        "字段行未完整进入编辑器视口：row={row:?}, viewport={viewport:?}"
    );
}

/// 窄窗中 Tab 到屏外用户名后应自动露出字段，Shift+Tab 回到名称也应把视口带回。
/// @author kongweiguang
#[gpui_kit::test]
fn tab_and_shift_tab_reveal_fields_in_a_narrow_editor(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    visual.simulate_resize(size(px(640.), px(420.)));
    draw(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
    });
    let (scroll, name_focus, username_focus) = editor_focus_targets(&panel, visual);
    let viewport = visual
        .debug_bounds(VIEWPORT_SELECTOR)
        .expect("编辑器视口应提供当前帧 bounds");
    let username = visual
        .debug_bounds(USERNAME_ROW_SELECTOR)
        .expect("用户名字段行应提供当前帧 bounds");
    assert!(
        username.origin.y < viewport.origin.y || username.bottom() > viewport.bottom(),
        "窄窗口 fixture 必须让用户名字段初始未完整显示：row={username:?}, viewport={viewport:?}"
    );

    visual.update(|window, app| window.focus(&name_focus, app));
    draw(visual);
    let mut reached_username = false;
    for _ in 0..16 {
        if visual.update(|window, _| username_focus.is_focused(window)) {
            reached_username = true;
            break;
        }
        visual.simulate_keystrokes("tab");
        draw(visual);
    }
    assert!(reached_username, "Tab 顺序应能到达用户名输入框");
    assert!(
        scroll.offset().y < px(0.),
        "到达原先屏外字段时滚动偏移应向下移动"
    );
    assert_row_fully_visible(visual, USERNAME_ROW_SELECTOR);

    let offset_at_username = scroll.offset().y;
    let mut returned_to_name = false;
    for _ in 0..16 {
        visual.simulate_keystrokes("shift-tab");
        draw(visual);
        if visual.update(|window, _| name_focus.is_focused(window)) {
            returned_to_name = true;
            break;
        }
    }
    assert!(returned_to_name, "Shift+Tab 顺序应能回到名称输入框");
    assert!(
        scroll.offset().y > offset_at_username,
        "Shift+Tab 回到顶部字段时视口应向上跟随"
    );
}

/// 视口已经完整显示用户名时重新取得焦点不应重置用户当前滚动位置。
/// @author kongweiguang
#[gpui_kit::test]
fn focusing_an_already_visible_field_preserves_scroll_position(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    visual.simulate_resize(size(px(640.), px(420.)));
    draw(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
    });
    let (scroll, name_focus, username_focus) = editor_focus_targets(&panel, visual);
    visual.update(|window, app| window.focus(&name_focus, app));
    draw(visual);

    let mut reached_username = false;
    for _ in 0..16 {
        if visual.update(|window, _| username_focus.is_focused(window)) {
            reached_username = true;
            break;
        }
        visual.simulate_keystrokes("tab");
        draw(visual);
    }
    assert!(reached_username, "Tab 顺序应能到达用户名输入框");
    assert_row_fully_visible(visual, USERNAME_ROW_SELECTOR);
    let visible_offset = scroll.offset();
    assert!(visible_offset.y < px(0.));

    visual.update(|window, app| {
        window.blur(app);
        window.focus(&username_focus, app);
        window.draw(app).clear(app);
    });
    draw(visual);

    assert!(visual.update(|window, _| username_focus.is_focused(window)));
    assert_eq!(
        scroll.offset(),
        visible_offset,
        "再次聚焦完整可见字段不得把滚动位置跳回顶部"
    );
    assert_row_fully_visible(visual, USERNAME_ROW_SELECTOR);
}

/// 从真实并发输入 Tab 到紧邻的保留时间按钮，确保非文本控件离开视口时整行也会被带入。
/// 按钮用生产 ElementId 的 keyed FocusHandle 验证 Tab 目标，避免仅凭偏移变化误判焦点路径。
/// @author kongweiguang
#[gpui_kit::test]
fn tab_to_preserve_mtime_toggle_reveals_the_whole_non_text_row(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    visual.simulate_resize(size(px(640.), px(420.)));
    draw(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.select_editor_section(HostEditorSection::Transfer, window, cx);
    });
    draw(visual);

    let (scroll, concurrency_focus) = visual.update(|_, app| {
        let editor = panel
            .read(app)
            .editor
            .as_ref()
            .expect("SSH 主机编辑器已打开");
        let concurrency_focus = editor.inputs.sftp_concurrency.read(app).focus_handle(app);
        (editor.scroll_handle.clone(), concurrency_focus)
    });

    let viewport = visual
        .debug_bounds(VIEWPORT_SELECTOR)
        .expect("编辑器视口应提供当前帧 bounds");
    let preserve_mtime = visual
        .debug_bounds(PRESERVE_MTIME_ROW_SELECTOR)
        .expect("保留修改时间行应提供当前帧 bounds");
    assert!(
        preserve_mtime.origin.y < viewport.origin.y || preserve_mtime.bottom() > viewport.bottom(),
        "窄窗口 fixture 必须让开关行初始处于视口外：row={preserve_mtime:?}, viewport={viewport:?}"
    );

    visual.update(|window, app| window.focus(&concurrency_focus, app));
    draw(visual);
    assert!(
        visual.update(|window, _| concurrency_focus.is_focused(window)),
        "并发数输入必须先取得真实焦点"
    );
    let viewport = visual
        .debug_bounds(VIEWPORT_SELECTOR)
        .expect("编辑器视口应提供当前帧 bounds");
    let preserve_mtime = visual
        .debug_bounds(PRESERVE_MTIME_ROW_SELECTOR)
        .expect("保留修改时间行应提供当前帧 bounds");
    assert!(
        preserve_mtime.origin.y < viewport.origin.y || preserve_mtime.bottom() > viewport.bottom(),
        "Tab 前紧邻开关行仍须处于视口外：row={preserve_mtime:?}, viewport={viewport:?}"
    );
    let offset_before_toggle = scroll.offset();

    visual.simulate_keystrokes("tab");
    draw(visual);
    assert!(
        !visual.update(|window, _| concurrency_focus.is_focused(window)),
        "Tab 应离开并发数输入并把焦点交给紧邻的保留修改时间按钮"
    );
    assert!(
        scroll.offset().y < offset_before_toggle.y,
        "焦点进入屏外开关时滚动偏移应向下移动"
    );
    assert_row_fully_visible(visual, PRESERVE_MTIME_ROW_SELECTOR);
}

/// Local 多行字段通过实际编辑器 ScrollHandle 显示；Tab 到环境变量后验证句柄与绘制视口仍连通。
/// @author kongweiguang
#[gpui_kit::test]
fn local_terminal_tab_reveals_environment_with_the_owned_scroll_handle(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    visual.simulate_resize(size(px(640.), px(420.)));
    draw(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_local_host_section(LocalHostEditorSection::Terminal, window, cx);
    });

    let (scroll, args_focus, env_focus) = visual.update(|_, app| {
        let editor = panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .expect("本地主机编辑器已打开");
        (
            editor.scroll_handle.clone(),
            editor.args.read(app).focus_handle(app),
            editor.env.read(app).focus_handle(app),
        )
    });
    assert!(
        scroll.bounds().size.height > px(0.) && scroll.max_offset().y > px(0.),
        "Local 正文句柄必须绑定可滚动的实际正文视口：bounds={:?}, max_offset={:?}",
        scroll.bounds(),
        scroll.max_offset()
    );

    visual.update(|window, app| window.focus(&args_focus, app));
    draw(visual);
    assert!(visual.update(|window, _| args_focus.is_focused(window)));
    let env_bounds = visual.update(|_, app| {
        panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .expect("本地主机编辑器仍打开")
            .env
            .read(app)
            .input_bounds()
    });
    let initial_viewport = scroll.bounds();
    assert!(
        env_bounds.origin.y < initial_viewport.origin.y
            || env_bounds.bottom() > initial_viewport.bottom(),
        "窄窗口 fixture 必须让环境变量输入初始处于视口外：field={env_bounds:?}, viewport={initial_viewport:?}"
    );
    let offset_before_env = scroll.offset();

    visual.simulate_keystrokes("tab");
    draw(visual);
    assert!(visual.update(|window, _| env_focus.is_focused(window)));
    assert!(
        scroll.offset().y < offset_before_env.y,
        "Tab 到屏外环境变量后应通过 Local 正文句柄向下滚动"
    );
    let env_bounds = visual.update(|_, app| {
        panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .expect("本地主机编辑器仍打开")
            .env
            .read(app)
            .input_bounds()
    });
    let viewport = scroll.bounds();
    assert!(
        env_bounds.origin.y >= viewport.origin.y && env_bounds.bottom() <= viewport.bottom(),
        "环境变量实际输入框应进入同一个滚动视口：field={env_bounds:?}, viewport={viewport:?}"
    );
}

/// 协议或分类改变后从正文起点开始，包括复用 Local/远程草稿；重复当前目标不打断浏览。
/// @author kongweiguang
#[gpui_kit::test]
fn section_and_protocol_changes_reset_only_when_the_target_changes(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    visual.simulate_resize(size(px(640.), px(420.)));
    draw(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
    });
    let scroll = visual.update(|_, app| {
        panel
            .read(app)
            .editor
            .as_ref()
            .expect("远程主机编辑器已打开")
            .scroll_handle
            .clone()
    });

    scroll.scroll_to_bottom();
    draw(visual);
    assert!(scroll.offset().y < px(0.), "fixture 应有可滚动正文");
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_editor_section(HostEditorSection::Network, window, cx);
    });
    assert_eq!(scroll.offset().y, px(0.));

    scroll.scroll_to_bottom();
    draw(visual);
    let network_offset = scroll.offset();
    assert!(network_offset.y < px(0.));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_editor_section(HostEditorSection::Network, window, cx);
    });
    assert_eq!(
        scroll.offset(),
        network_offset,
        "重复选择当前分类不应清零滚动位置"
    );

    scroll.scroll_to_bottom();
    draw(visual);
    assert!(scroll.offset().y < px(0.));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Sftp, window, cx);
    });
    assert_eq!(scroll.offset().y, px(0.));

    scroll.scroll_to_bottom();
    draw(visual);
    assert!(scroll.offset().y < px(0.));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_local_host_section(LocalHostEditorSection::Terminal, window, cx);
    });
    let local_scroll = visual.update(|_, app| {
        panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .expect("切协议后应保留独立的 Local 草稿")
            .scroll_handle
            .clone()
    });
    local_scroll.scroll_to_bottom();
    draw(visual);
    let local_offset = local_scroll.offset();
    assert!(local_offset.y < px(0.));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    assert_eq!(local_scroll.offset(), local_offset);

    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Sftp, window, cx);
    });
    assert_eq!(scroll.offset().y, px(0.), "返回远程草稿应从顶部开始");
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    assert_eq!(
        local_scroll.offset().y,
        px(0.),
        "返回 Local 草稿应从顶部开始"
    );
}

/// 切换替换或隐藏焦点字段后应交给当前可见输入，否则原生 Esc 无法沿表单树冒泡。
/// @author kongweiguang
#[gpui_kit::test]
fn protocol_and_section_changes_handoff_focus_to_visible_fields(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.focus_editor_primary_input(window, cx);
        panel.activate_remote_protocol(RemoteProtocol::Rdp, window, cx);
    });
    visual.update(|window, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        assert!(
            editor
                .inputs
                .host
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_editor_section(HostEditorSection::Display, window, cx);
    });
    visual.update(|window, app| {
        let panel = panel.read(app);
        assert!(panel.editor.as_ref().unwrap().rdp_fullscreen);
        assert!(panel.editor_fields_focus.is_focused(window));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    visual.update(|window, app| {
        let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
        assert!(editor.name.read(app).focus_handle(app).is_focused(window));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_local_host_section(LocalHostEditorSection::Terminal, window, cx);
    });
    visual.update(|window, app| {
        let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
        assert!(editor.args.read(app).focus_handle(app).is_focused(window));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Rdp, window, cx);
    });
    visual.update(|window, app| {
        assert!(panel.read(app).editor_fields_focus.is_focused(window));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    visual.update(|window, app| {
        let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
        assert!(editor.args.read(app).focus_handle(app).is_focused(window));
    });
    close_clean_editor(&panel, visual);
}

/// 认证切换与删除正聚焦的跳板同样会卸载输入，必须保留可用焦点且允许还原后直接关闭。
/// @author kongweiguang
#[gpui_kit::test]
fn authentication_and_jump_removal_handoff_unmounted_focus(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    activate_editor_window(visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        let input = &panel.editor.as_ref().unwrap().inputs.secret;
        window.focus(&input.read(cx).focus_handle(cx), cx);
        panel.set_authentication_mode(HostAuthenticationMode::Agent, window, cx);
    });
    visual.update(|window, app| {
        assert!(panel.read(app).editor_fields_focus.is_focused(window));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.set_authentication_mode(HostAuthenticationMode::PrivateKey, window, cx);
    });
    visual.update(|window, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        assert!(
            editor
                .inputs
                .key_path
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.set_authentication_mode(HostAuthenticationMode::Password, window, cx);
        panel.select_editor_section(HostEditorSection::JumpHosts, window, cx);
        panel.add_jump(window, cx);
        let jump = &panel.editor.as_ref().unwrap().jumps[0];
        window.focus(&jump.host.read(cx).focus_handle(cx), cx);
        panel.remove_jump(0, window, cx);
    });
    visual.update(|window, app| {
        assert!(panel.read(app).editor_fields_focus.is_focused(window));
    });
    close_clean_editor(&panel, visual);
}
