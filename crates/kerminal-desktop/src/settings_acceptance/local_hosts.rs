//! 后台原生窗口中验证 Profile 模板与独立本地主机的完整连接路径。
//!
//! @author kongweiguang

use kerminal_application::connections::management::ManagedLocalHost;

use super::*;

const HOST_NAME: &str = "Acceptance Local Host";
const PROFILE_NAME: &str = "Edited Profile";
const INITIAL_CWD: &str = "C:\\Windows";
const INITIAL_ARG: &str = "/Q";
const INITIAL_ENV: &str = "K_SETTINGS_ACCEPTANCE=template";

/// 从主机表单直接进入 Profile 设置并返回草稿，所有写入仍通过生产控件后再读回。
///
/// @author kongweiguang
pub(super) async fn verify(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let original_sessions = root_state(cx, root, window)?
        .ready_local_session_ids()
        .to_vec();
    if original_sessions.is_empty() {
        return Err(
            io::Error::other("local host acceptance requires the existing terminal").into(),
        );
    }
    let profile_id = root_state(cx, root, window)?
        .profile_id()
        .ok_or_else(|| io::Error::other("saved Profile selection is missing"))?
        .to_owned();
    fill_profile_snapshot(cx, root, window, (INITIAL_CWD, INITIAL_ARG, INITIAL_ENV)).await?;
    save_profile(cx, root, window, &profile_id).await?;
    let template = profiles::read_profile(services, &profile_id).await?;
    if template.profile.name != PROFILE_NAME
        || template.profile.shell != "cmd.exe"
        || template.profile.cwd.as_deref() != Some(INITIAL_CWD)
        || template.profile.args != [INITIAL_ARG]
        || template
            .profile
            .env
            .get("K_SETTINGS_ACCEPTANCE")
            .map(String::as_str)
            != Some("template")
    {
        return Err(io::Error::other("Profile startup fields were not saved").into());
    }
    click_selector(cx, window, "preferences-close")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    wait_sidebar_default(cx, root, window, PROFILE_NAME).await?;
    if read_hosts(services)
        .await?
        .iter()
        .any(|host| host.id.as_str() == profile_id)
        || sidebar_name(cx, root, window, &profile_id)?.is_some()
    {
        return Err(io::Error::other("a Profile template appeared as a local host").into());
    }

    open_host_editor(cx, window).await?;
    select_local_protocol(cx, window).await?;
    fill_local_input(
        cx,
        root,
        window,
        "本地主机名称",
        "Local host name",
        HOST_NAME,
    )
    .await?;
    reveal_local_control(
        cx,
        root,
        window,
        LocalControl::Selector("connections-local-profile-edit"),
    )
    .await?;
    click_selector(cx, window, "connections-local-profile-edit")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.page_id() == "profiles"
    })
    .await?;
    click_selector(cx, window, "preferences-close")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    wait_connections_modal(cx, root, window, true).await?;
    reveal_local_control(
        cx,
        root,
        window,
        LocalControl::Label("本地主机名称", "Local host name"),
    )
    .await?;
    wait_input_value(cx, window, "本地主机名称", "Local host name", HOST_NAME).await?;
    // 先输入不同 Shell，再选择模板；若菜单未真正执行，后续 cmd.exe 回读必然失败。
    fill_local_input(
        cx,
        root,
        window,
        "Shell 程序路径",
        "Shell program path",
        "powershell.exe",
    )
    .await?;
    reveal_local_control(
        cx,
        root,
        window,
        LocalControl::Selector("connections-local-profile-picker"),
    )
    .await?;
    select_menu_item(cx, window, "connections-local-profile-picker", PROFILE_NAME).await?;
    reveal_local_control(
        cx,
        root,
        window,
        LocalControl::Label("Shell 程序路径", "Shell program path"),
    )
    .await?;
    wait_input_value(
        cx,
        window,
        "Shell 程序路径",
        "Shell program path",
        "cmd.exe",
    )
    .await?;
    click_selector(cx, window, "connections-local-editor-save")?;
    let host = wait_for_host(cx, window, services, HOST_NAME, true)
        .await?
        .ok_or_else(|| io::Error::other("saved local host is missing"))?;
    wait_connections_modal(cx, root, window, false).await?;
    if host.id.as_str() == profile_id
        || host.shell != template.profile.shell
        || host.args != template.profile.args
        || host.cwd != template.profile.cwd
        || host.environment_ref.is_none()
        || read_environment(services, &host).await? != template.profile.env
    {
        return Err(io::Error::other("local host did not copy the template snapshot").into());
    }
    ensure_host_row(cx, root, window, host.id.as_str(), HOST_NAME).await?;
    let created_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-local-host-created.png"),
    )?;
    screenshots.push(created_capture.clone());

    close_compact_sidebar(cx, window)?;
    press_key(cx, window, "ctrl-alt-s")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.settings_ready() && state.draft_ready()
    })
    .await?;
    ensure_page(cx, root, window, "profiles", "profiles").await?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.profile_id() == Some(profile_id.as_str()) && state.profile_ready()
    })
    .await?;
    profiles::fill_profile_field(cx, root, window, "profile-shell-field", "powershell.exe").await?;
    fill_profile_snapshot(
        cx,
        root,
        window,
        ("C:\\", "-NoLogo", "K_SETTINGS_ACCEPTANCE=changed"),
    )
    .await?;
    save_profile(cx, root, window, &profile_id).await?;
    let changed = profiles::read_profile(services, &profile_id).await?;
    if changed.profile.shell != "powershell.exe"
        || changed.profile.cwd.as_deref() != Some("C:\\")
        || changed.profile.args != ["-NoLogo"]
        || changed
            .profile
            .env
            .get("K_SETTINGS_ACCEPTANCE")
            .map(String::as_str)
            != Some("changed")
    {
        return Err(io::Error::other("edited Profile startup fields were not saved").into());
    }
    let after_edit = read_host(services, &host)
        .await?
        .ok_or_else(|| io::Error::other("local host disappeared after Profile edit"))?;
    if after_edit.shell != host.shell
        || after_edit.args != host.args
        || after_edit.cwd != host.cwd
        || after_edit.environment_ref != host.environment_ref
        || read_environment(services, &after_edit).await? != template.profile.env
    {
        return Err(
            io::Error::other("editing the template changed the existing local host").into(),
        );
    }
    click_selector(cx, window, "preferences-close")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    ensure_host_row(cx, root, window, host.id.as_str(), HOST_NAME).await?;
    right_click_selector(
        cx,
        window,
        &format!("host-navigation-profile-{}", host.id.as_str()),
    )?;
    wait_menu_label(
        cx,
        window,
        &kerminal_ui::locale::text("删除本地主机…", "Delete local host…"),
    )
    .await?;
    click_menu_label(
        cx,
        window,
        &kerminal_ui::locale::text("删除本地主机…", "Delete local host…"),
    )?;
    if selector_visible(cx, window, "workbench-close-compact-host-drawer")? {
        click_selector(cx, window, "workbench-close-compact-host-drawer")?;
    }
    wait_clickable_selector(cx, window, "connections-delete-local-host-confirm").await?;
    click_selector(cx, window, "connections-delete-local-host-confirm")?;
    wait_for_host(cx, window, services, HOST_NAME, false).await?;
    wait_sidebar_absent(cx, root, window, host.id.as_str()).await?;
    ensure_sidebar_open(cx, window).await?;
    let deleted_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-local-host-deleted.png"),
    )?;
    screenshots.push(deleted_capture.clone());
    if profiles::read_profile(services, &profile_id)
        .await?
        .profile
        .name
        != PROFILE_NAME
    {
        return Err(io::Error::other("deleting the local host also changed its template").into());
    }
    // 后续设置验收仍从生产快捷键打开原模态，保证其页面流程和焦点测试不受本轮影响。
    close_compact_sidebar(cx, window)?;
    activate_existing_terminal_tab(cx, window)?;
    wait_terminal_workspace(cx, root, window, &original_sessions).await?;
    press_key(cx, window, "ctrl-alt-s")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.settings_ready() && state.draft_ready()
    })
    .await?;
    Ok(vec![
        check("profile-template-not-a-host", "template-only", None),
        check(
            "profile-create-local-host",
            "copied-and-visible",
            Some(created_capture),
        ),
        check(
            "profile-edit-keeps-host-snapshot",
            "independent-snapshot",
            None,
        ),
        check(
            "profile-delete-local-host",
            "host-deleted-template-retained",
            Some(deleted_capture),
        ),
    ])
}

/// 报告只记录固定结果标签，避免把隔离配置、路径或测试环境值写入日志。
/// @author kongweiguang
fn check(action: &'static str, result: &str, capture: Option<CaptureRecord>) -> SettingCheck {
    SettingCheck {
        action,
        passed: true,
        expected: result.to_owned(),
        observed: result.to_owned(),
        capture,
    }
}

/// 通过已安装的输入控件设置非空字段，使参数、目录和环境复制均有实际证据。
/// @author kongweiguang
async fn fill_profile_snapshot(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    (cwd, args, env): (&str, &str, &str),
) -> Result<(), Box<dyn Error>> {
    fill_profile_input(
        cx,
        root,
        window,
        "工作目录（可选）",
        "Working directory (optional)",
        cwd,
    )
    .await?;
    reveal_settings_selector(cx, root, window, "profiles", "profile-advanced-toggle").await?;
    click_selector(cx, window, "profile-advanced-toggle")?;
    fill_profile_input(
        cx,
        root,
        window,
        "启动参数：每行一个参数",
        "Arguments: one per line",
        args,
    )
    .await?;
    fill_profile_input(
        cx,
        root,
        window,
        "环境变量：每行 KEY=VALUE",
        "Environment: one KEY=VALUE per line",
        env,
    )
    .await?;
    Ok(())
}

/// 显式等待保存门禁和可见按钮，避免将异步回声或未提交草稿当作持久化。
/// @author kongweiguang
async fn save_profile(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    id: &str,
) -> Result<(), Box<dyn Error>> {
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.profile_id() == Some(id) && state.profile_input_ready()
    })
    .await?;
    reveal_settings_selector(cx, root, window, "profiles", "profile-save").await?;
    click_selector(cx, window, "profile-save")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.profile_id() == Some(id) && state.profile_ready()
    })
    .await?;
    Ok(())
}

/// Profile 页面通过真实滚轮逐步显示目标输入；只使用其当前可访问标签。
/// @author kongweiguang
async fn fill_profile_input(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    zh: &str,
    en: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    scroll_settings_content(cx, root, window, "profiles", 5_000.)?;
    for _ in 0..20 {
        if click_labeled_input(cx, root, window, zh, en, false)? {
            return type_focused_input(cx, window, zh, en, value).await;
        }
        scroll_settings_content(cx, root, window, "profiles", -180.)?;
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Err(io::Error::other(format!("Profile input {en:?} could not be revealed")).into())
}

/// 本地编辑器使用自己的滚动区域，输入时不允许点击被裁切的字段。
/// @author kongweiguang
async fn fill_local_input(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    zh: &str,
    en: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    reveal_local_control(cx, root, window, LocalControl::Label(zh, en)).await?;
    if !click_labeled_input(cx, root, window, zh, en, true)? {
        return Err(io::Error::other(format!("local host input {en:?} is not visible")).into());
    }
    type_focused_input(cx, window, zh, en, value).await
}

#[derive(Clone, Copy)]
enum LocalControl<'a> {
    Selector(&'a str),
    Label(&'a str, &'a str),
}

/// 对真实主机表单滚动定位，比较控件与裁切区域，禁止把局部可见当成有效点击。
/// @author kongweiguang
async fn reveal_local_control(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    control: LocalControl<'_>,
) -> Result<(), Box<dyn Error>> {
    scroll_local_editor(cx, root, window, 5_000.)?;
    for _ in 0..20 {
        let found = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let clip = root.update(app, |root, cx| {
                root.settings_acceptance_local_editor_bounds(cx)
            });
            let bounds = match control {
                LocalControl::Selector(id) => selector_bounds(window, id).ok(),
                LocalControl::Label(zh, en) => {
                    labeled_input(window, zh, en).map(|item| item.bounds())
                }
            };
            Ok::<_, io::Error>(
                clip.zip(bounds)
                    .is_some_and(|(clip, bounds)| bounds_inside_viewport(bounds, clip)),
            )
        })??;
        if found {
            return Ok(());
        }
        scroll_local_editor(cx, root, window, -180.)?;
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Err(io::Error::other("local host control could not be revealed").into())
}

/// 滚轮只投递到主机表单裁切区，不操作系统输入桌面。
/// @author kongweiguang
fn scroll_local_editor(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    delta: f32,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let clip = root
            .update(app, |root, cx| {
                root.settings_acceptance_local_editor_bounds(cx)
            })
            .ok_or_else(|| io::Error::other("local host editor is not mounted"))?;
        let event = ScrollWheelEvent {
            position: clip.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            touch_phase: TouchPhase::Moved,
            ..ScrollWheelEvent::default()
        };
        window.dispatch_event(event.to_platform_input(), app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 匹配 Kit 输入实际公开的占位符标签，并要求身份唯一。
/// @author kongweiguang
fn labeled_input(
    window: &gpui_pre::Window,
    zh: &str,
    en: &str,
) -> Option<gpui_kit::base::test_support::ElementSnapshot> {
    let label = kerminal_ui::locale::text(zh.to_owned(), en.to_owned());
    let matches = gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .filter(|item| item.visible() && item.label() == Some(label.as_ref()))
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].clone())
}

/// 控件焦点由实际点击取得，不能直接改写输入实体。
/// @author kongweiguang
fn click_labeled_input(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    zh: &str,
    en: &str,
    local: bool,
) -> Result<bool, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let clip = root.update(app, |root, cx| {
            if local {
                root.settings_acceptance_local_editor_bounds(cx)
            } else {
                Some(root.settings_acceptance_content_bounds(cx))
            }
        });
        let Some(item) = labeled_input(window, zh, en) else {
            return Ok::<_, io::Error>(false);
        };
        if !clip.is_some_and(|clip| bounds_inside_viewport(item.bounds(), clip)) {
            return Ok(false);
        }
        click_bounds(window, app, item.bounds(), en)?;
        Ok(true)
    })??)
}

/// 在新绘制帧确认输入焦点后逐字符发送，避免占用系统剪贴板。
/// @author kongweiguang
async fn type_focused_input(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    zh: &str,
    en: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let focused = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(
            labeled_input(window, zh, en).is_some_and(|item| item.focused() == Some(true)),
        )
    })??;
    if !focused {
        return Err(io::Error::other(format!("input {en:?} did not receive focus")).into());
    }
    press_key(cx, window, "secondary-a")?;
    press_key(cx, window, "backspace")?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        for character in value.chars() {
            let key = character.to_string();
            window.dispatch_keystroke(
                Keystroke {
                    modifiers: gpui_pre::Modifiers::default(),
                    key: key.clone(),
                    key_char: Some(key),
                },
                app,
            );
        }
        Ok::<_, io::Error>(())
    })??;
    wait_input_value(cx, window, zh, en, value).await
}

/// 等待输入的原生可访问值，不以按键已发送代替界面接收成功。
/// @author kongweiguang
async fn wait_input_value(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    zh: &str,
    en: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let matches = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(
                labeled_input(window, zh, en).is_some_and(|item| item.value() == Some(value)),
            )
        })??;
        if matches {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "input {en:?} did not receive the expected value"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 常规窗口使用底部新增按钮；窄窗口通过抽屉背景菜单打开同一连接弹窗。
/// @author kongweiguang
async fn open_host_editor(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    ensure_sidebar_open(cx, window).await?;
    if selector_visible(cx, window, "workbench-create-host")? {
        click_selector(cx, window, "workbench-create-host")?;
    } else {
        right_click_drawer_background(cx, window)?;
        let label = kerminal_ui::locale::text("添加连接…", "Add connection…");
        wait_menu_label(cx, window, &label).await?;
        click_menu_label(cx, window, &label)?;
    }
    wait_selector(cx, window, "connections-editor-save").await
}

/// 根据生产界面实际呈现选择协议，兼容极矮窗口的下拉菜单。
/// @author kongweiguang
async fn select_local_protocol(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    if selector_visible(cx, window, "connections-protocol-None")? {
        click_selector(cx, window, "connections-protocol-None")?;
    } else {
        wait_selector(cx, window, "connections-compact-protocol").await?;
        select_menu_item(cx, window, "connections-compact-protocol", "Local").await?;
    }
    wait_selector(cx, window, "connections-local-editor-save").await
}

/// 侧栏在窄宽度下可能卸载；仅通过实际切换按钮打开相应宿主。
/// @author kongweiguang
async fn ensure_sidebar_open(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    if selector_visible(cx, window, "workbench-create-host")?
        || selector_visible(cx, window, "workbench-close-compact-host-drawer")?
    {
        return Ok(());
    }
    click_selector(cx, window, "workbench-toggle-host-sidebar")?;
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        if selector_visible(cx, window, "workbench-create-host")?
            || selector_visible(cx, window, "workbench-close-compact-host-drawer")?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("host sidebar did not open").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 设置快捷键执行前关闭紧凑抽屉，避免其焦点陷阱截获后续输入。
/// @author kongweiguang
fn close_compact_sidebar(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<(), Box<dyn Error>> {
    if selector_visible(cx, window, "workbench-close-compact-host-drawer")? {
        click_selector(cx, window, "workbench-close-compact-host-drawer")?;
    }
    Ok(())
}

/// 隔离配置没有现存主机，在抽屉底部空白区打开生产背景菜单。
/// @author kongweiguang
fn right_click_drawer_background(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let close = selector_bounds(window, "workbench-close-compact-host-drawer")?;
        let viewport = window.visual_viewport_bounds();
        let position = point(close.center().x - px(80.), viewport.bottom() - px(80.));
        dispatch_right_click(window, app, position);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 针对稳定主机 ID 发送右击，避免操作当前选中的其他对象。
/// @author kongweiguang
fn right_click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let bounds = selector_bounds(window, selector)?;
        if !bounds_inside_viewport(bounds, window.visual_viewport_bounds()) {
            return Err(io::Error::other("host row is outside the viewport"));
        }
        dispatch_right_click(window, app, bounds.center());
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 事件直接发送到隔离窗口，始终不触碰操作系统鼠标。
/// @author kongweiguang
fn dispatch_right_click(
    window: &mut gpui_pre::Window,
    app: &mut App,
    position: gpui_pre::Point<gpui_pre::Pixels>,
) {
    for event in [
        MouseMoveEvent {
            position,
            ..Default::default()
        }
        .to_platform_input(),
        MouseDownEvent {
            button: MouseButton::Right,
            position,
            click_count: 1,
            ..Default::default()
        }
        .to_platform_input(),
        MouseUpEvent {
            button: MouseButton::Right,
            position,
            click_count: 1,
            ..Default::default()
        }
        .to_platform_input(),
    ] {
        window.dispatch_event(event, app);
    }
}

/// 列表可能正在异步刷新；每次重新打开菜单读取最新条目，避免使用过期闭包。
/// @author kongweiguang
async fn select_menu_item(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    trigger: &str,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        click_selector(cx, window, trigger)?;
        let until = (Instant::now() + Duration::from_millis(700)).min(deadline);
        while Instant::now() < until {
            if menu_label_visible(cx, window, label)? {
                click_menu_label(cx, window, label)?;
                return Ok(());
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("menu item {label:?} did not appear")).into());
        }
        press_key(cx, window, "escape")?;
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 每次只查询当前帧的唯一菜单项，防止异步刷新后点击旧位置。
/// @author kongweiguang
fn menu_label_visible(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    label: &str,
) -> Result<bool, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .filter(|item| {
                    item.visible()
                        && item.role() == Some(gpui_pre::Role::MenuItem)
                        && item.label() == Some(label)
                })
                .count()
                == 1,
        )
    })??)
}

/// 等待动态构造的菜单实际出现，再触发所属动作。
/// @author kongweiguang
async fn wait_menu_label(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        if menu_label_visible(cx, window, label)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("menu item {label:?} did not appear")).into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 真实菜单点击必须命中唯一且位于视口内的可访问项。
/// @author kongweiguang
fn click_menu_label(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let items = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|item| {
                item.visible()
                    && item.role() == Some(gpui_pre::Role::MenuItem)
                    && item.label() == Some(label)
            })
            .collect::<Vec<_>>();
        if items.len() != 1 {
            return Err(io::Error::other(format!(
                "menu item {label:?} is not unique"
            )));
        }
        click_bounds(window, app, items[0].bounds(), label)
    })??;
    Ok(())
}

/// 每次重绘后再读取 selector，避免缓存已卸载页面的坐标。
/// @author kongweiguang
fn selector_visible(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<bool, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(selector_bounds(window, selector).is_ok())
    })??)
}

/// 异步编辑器或确认页面以真实按钮挂载作为可继续操作的条件。
/// @author kongweiguang
async fn wait_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        if selector_visible(cx, window, selector)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("control {selector:?} did not appear")).into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 删除确认挂载后仍可能因目录重读暂时禁用；以实际 Click 能力作为可操作证据。
/// 不通过修改面板状态跳过生产门禁，也不把缺省 disabled 标记当成启用证明。
/// @author kongweiguang
async fn wait_clickable_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    let selector_id: ElementId = selector.to_owned().into();
    loop {
        let (ready, observation) = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let target = gpui_kit::base::test_support::find(window, &[], &selector_id);
            let ready = target.as_ref().is_some_and(|target| {
                target.visible() && target.supports_click() && target.disabled() != Some(true)
            });
            Ok::<_, io::Error>((ready, format!("{target:?}")))
        })??;
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "control {selector:?} did not become clickable: {observation}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 模态关闭必须由 Root 的生产回执确认，仓储写入成功本身不证明界面完成。
/// @author kongweiguang
async fn wait_connections_modal(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    open: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let actual = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(root.read(app).workbench_acceptance_connections_modal_open())
        })??;
        if actual == open {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("connection modal did not settle").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 根据唯一可见的关闭按钮身份定位同一个真实 Tab，只点击其不与关闭按钮重叠的主体。
///
/// @author kongweiguang
fn activate_existing_terminal_tab(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let targets = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|item| item.visible())
            .filter_map(|item| match item.path().last() {
                Some(ElementId::Name(name)) => name
                    .strip_prefix("workspace-tab-close-")
                    .filter(|id| !id.is_empty())
                    .map(|id| (id.to_owned(), item.bounds())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let [(id, close_bounds)] = targets.as_slice() else {
            return Err(io::Error::other(format!(
                "expected one existing terminal tab, found {}",
                targets.len()
            )));
        };
        let body = selector_bounds(window, &format!("workspace-tab-hitbox-{id}"))?;
        if body.center().x >= close_bounds.left() {
            return Err(io::Error::other(
                "terminal tab center overlaps its close button",
            ));
        }
        click_bounds(window, app, body, "existing terminal tab")
    })??;
    Ok(())
}

/// 等待既有 Tab 真正恢复终端工作区，并确认原 Ready 会话集合没有被重建或替换。
/// @author kongweiguang
async fn wait_terminal_workspace(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    original_sessions: &[String],
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let terminal = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(root.read(app).workbench_acceptance_local_profile_selected())
        })??;
        let sessions = root_state(cx, root, window)?
            .ready_local_session_ids()
            .to_vec();
        if terminal && sessions.as_slice() == original_sessions {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "terminal workspace did not resume after host deletion: terminal={terminal}, original_sessions={}, current_sessions={}, unchanged={}",
                original_sessions.len(),
                sessions.len(),
                sessions.as_slice() == original_sessions,
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 左栏的默认模板名称证明目录完成刷新，不把初始空快照误认为模板隔离成功。
/// @author kongweiguang
async fn wait_sidebar_default(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let name = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(root.update(app, |root, cx| {
                root.settings_acceptance_default_profile_name(cx)
            }))
        })??;
        if name.as_deref() == Some(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other("sidebar did not observe the saved default Profile").into(),
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 左栏数据按主机 ID 读取，避免把同名模板误判为可见主机。
/// @author kongweiguang
fn sidebar_name(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    id: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(root.update(app, |root, cx| {
            root.settings_acceptance_local_host_name(id, cx)
        }))
    })??)
}

/// 同时要求目录身份和真实行控件，保证主机确实到达用户可操作的左栏。
/// @author kongweiguang
async fn ensure_host_row(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    id: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    ensure_sidebar_open(cx, window).await?;
    let selector = format!("host-navigation-profile-{id}");
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        if sidebar_name(cx, root, window, id)?.as_deref() == Some(name)
            && selector_visible(cx, window, &selector)?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("saved local host did not appear in sidebar").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 删除确认后等待目录监听回执，防止截图仍包含旧行。
/// @author kongweiguang
async fn wait_sidebar_absent(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    id: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        if sidebar_name(cx, root, window, id)?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("deleted local host remained in sidebar").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 列表读回在同一个隔离应用执行器上运行，不通过真实 home 或旁路文件路径。
/// @author kongweiguang
async fn read_hosts(
    services: &kerminal_ui::AppServices,
) -> Result<Vec<ManagedLocalHost>, Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host service is unavailable"))?;
    Ok(services
        .executor
        .run(move |_| async move { hosts.list_local_hosts().await })
        .await?)
}

/// 读取稳定主机 ID 的最新版本，以便区分模板改动与主机自己的快照。
/// @author kongweiguang
async fn read_host(
    services: &kerminal_ui::AppServices,
    host: &ManagedLocalHost,
) -> Result<Option<ManagedLocalHost>, Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host service is unavailable"))?;
    let id = host.id.clone();
    Ok(services
        .executor
        .run(move |_| async move { hosts.load_local_host(&id).await })
        .await?)
}

/// 环境只经正式凭据服务解密并在内存中比较，不进入报告、截图文件名或诊断日志。
/// @author kongweiguang
async fn read_environment(
    services: &kerminal_ui::AppServices,
    host: &ManagedLocalHost,
) -> Result<std::collections::BTreeMap<String, String>, Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host service is unavailable"))?;
    let host = host.clone();
    Ok(services
        .executor
        .run(move |cancel| async move { hosts.read_local_environment(&host, &cancel).await })
        .await?)
}

/// 等待唯一隔离主机的仓储结果时持续推进隐藏窗口的真实帧，确保异步回执正常消费。
/// 重复名称和超时保留实际数量及预期状态，不能将未受理点击误报为写入已完成。
/// @author kongweiguang
async fn wait_for_host(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    name: &str,
    present: bool,
) -> Result<Option<ManagedLocalHost>, Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(())
        })??;
        let mut matches = read_hosts(services)
            .await?
            .into_iter()
            .filter(|host| host.name == name)
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(io::Error::other("duplicate local hosts in isolated acceptance").into());
        }
        if matches.is_empty() != present {
            return Ok(matches.pop());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "local host persistence did not settle: expected_present={present}, actual_count={}",
                matches.len()
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}
