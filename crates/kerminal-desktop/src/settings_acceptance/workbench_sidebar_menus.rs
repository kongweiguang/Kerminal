//! 隔离原生窗口中验证左栏菜单归属、菜单动作及关闭；不使用 OS 输入，不访问真实主机。
//!
//! @author kongweiguang

use super::*;

/// 每个主题使用生产菜单和真正的编辑器入口，截图与动作结果分别保留，避免用静态像素替代接线。
pub(super) async fn capture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    directory: &Path,
    theme: &str,
) -> Result<(), Box<dyn Error>> {
    let targets = [
        (
            "group",
            format!("host-navigation-group-{WORKBENCH_GROUP_ID}"),
            "添加连接到此分组…",
            "Add connection to this group…",
        ),
        (
            "host",
            format!("host-navigation-host-{WORKBENCH_HOST_ID}"),
            "添加同组连接…",
            "Add connection to this group…",
        ),
        (
            "profile",
            format!("host-navigation-profile-{WORKBENCH_PROFILE_ID}"),
            "添加同组连接…",
            "Add connection to this group…",
        ),
        (
            "ungrouped-host",
            format!("host-navigation-host-{WORKBENCH_UNGROUPED_HOST_ID}"),
            "添加同组连接…",
            "Add connection to this group…",
        ),
        (
            "sftp-copy-ssh",
            format!("host-navigation-host-{WORKBENCH_SFTP_HOST_ID}"),
            "复制为 SSH 主机…",
            "Duplicate as SSH host…",
        ),
        ("background", String::new(), "添加连接…", "Add connection…"),
    ];
    let mut records = Vec::new();
    for (target, selector, zh, en) in targets {
        let before_focus = right_click(cx, window, &selector)?;
        wait_for_menu(cx, window, true).await?;
        let labels = menu_labels(cx, window)?;
        let wanted = kerminal_ui::locale::text(zh, en).to_string();
        if !labels.contains(&wanted) {
            return Err(
                io::Error::other(format!("{target} menu lacks {wanted:?}: {labels:?}")).into(),
            );
        }
        let refresh = kerminal_ui::locale::text("刷新主机列表", "Refresh hosts").to_string();
        if target != "background" && labels.contains(&refresh) {
            return Err(io::Error::other(format!(
                "{target} unexpectedly opened the background menu"
            ))
            .into());
        }
        let screenshot = save_frame(
            cx,
            window,
            directory.join(format!("sidebar-menu-{theme}-{target}.png")),
        )?;
        press_key(cx, window, "escape")?;
        wait_for_menu(cx, window, false).await?;
        let after_focus = window.update(cx, |_, window, app| window.focused(app))?;
        if before_focus != after_focus {
            return Err(io::Error::other(format!(
                "{target} Escape did not restore focus: {before_focus:?} -> {after_focus:?}"
            ))
            .into());
        }

        // 第二次打开并点击实际菜单项；无草稿修改，所以 Escape 应直接关闭统一编辑器。
        right_click(cx, window, &selector)?;
        wait_for_menu(cx, window, true).await?;
        click_menu_label(cx, window, &wanted)?;
        wait_for_editor(cx, window, true).await?;
        if target == "sftp-copy-ssh" {
            // Button.selected 只控制样式，不发布可访问性 selected；终端分类只会由 SSH 草稿提供。
            let ssh_selected = window.update(cx, |_, window, _| {
                gpui_kit::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.visible()
                            && node.label()
                                == Some(kerminal_ui::locale::text("终端", "Terminal").as_ref())
                            && node.role() == Some(gpui_pre::Role::Button)
                    })
            })?;
            if !ssh_selected {
                return Err(io::Error::other("SFTP duplicate did not open an SSH draft").into());
            }
        }
        let editor_capture = save_frame(
            cx,
            window,
            directory.join(format!("sidebar-action-{theme}-{target}.png")),
        )?;
        press_key(cx, window, "escape")?;
        wait_for_editor(cx, window, false).await?;
        records.push(serde_json::json!({
            "target": target,
            "labels": labels,
            "menuCount": 1,
            "escapeRestoredFocus": true,
            "actionOpenedEditor": true,
            "editorCancelled": true,
            "menuCapture": screenshot,
            "actionCapture": editor_capture,
        }));
    }
    fs::write(
        directory.join(format!("sidebar-menus-{theme}.json")),
        serde_json::to_vec_pretty(&records)?,
    )?;
    if theme == "system" {
        capture_connection_failure(cx, root, window, directory).await?;
    }
    Ok(())
}

/// 使用拒绝连接的本机隔离地址覆盖后台预检失败，验证本地化恢复指引可见且不借用本地终端。
/// @author kongweiguang
async fn capture_connection_failure(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let (before_local, before_status) = root.read_with(cx, |root, _| {
        (
            root.workbench_acceptance_local_state(),
            root.workbench_acceptance_terminal_creation_state().2,
        )
    });
    right_click(
        cx,
        window,
        &format!("host-navigation-host-{WORKBENCH_HOST_ID}"),
    )?;
    wait_for_menu(cx, window, true).await?;
    click_menu_label(cx, window, &kerminal_ui::locale::text("容器", "Containers"))?;
    let deadline = Instant::now() + ACTION_TIMEOUT;
    let message = loop {
        window.update(cx, |_, window, app| {
            window.refresh();
            draw_acceptance_frame(window, app);
        })?;
        let (busy, owner, status) = root.read_with(cx, |root, _| {
            root.workbench_acceptance_terminal_creation_state()
        });
        if !busy
            && !owner
            && status != before_status
            && let Some(message) = status
        {
            break message;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(
                "sidebar container preflight failure was not visible in the workbench",
            )
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    };
    let expected = kerminal_ui::locale::text(
        "无法连接 SSH 主机，请检查网络、地址和端口后重试。",
        "Cannot connect to the SSH host. Check the network, address and port, then retry.",
    );
    if message != expected.as_ref() {
        return Err(io::Error::other(format!(
            "sidebar failure did not show the localized recovery guidance: {message:?}"
        ))
        .into());
    }
    let after_local = root.read_with(cx, |root, _| root.workbench_acceptance_local_state());
    if before_local != after_local {
        return Err(io::Error::other(
            "failed SSH container request changed the local terminal sessions",
        )
        .into());
    }
    let capture = save_frame(
        cx,
        window,
        directory.join("sidebar-container-preflight-failure.png"),
    )?;
    fs::write(
        directory.join("sidebar-container-preflight-failure.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "workbenchFailureVisible": true,
            "terminalCreationIdle": true,
            "localSessionsUnchanged": true,
            "message": message,
            "capture": capture,
        }))?,
    )?;
    Ok(())
}

/// 根据当前帧取得行边界；空白点取侧栏底部，不能按上一次菜单的位置重新解释目标。
fn right_click(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    selector: &str,
) -> Result<Option<gpui_kit::FocusHandle>, Box<dyn Error>> {
    Ok(handle.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        let position = if selector.is_empty() {
            let group = selector_bounds(
                window,
                &format!("host-navigation-group-{WORKBENCH_GROUP_ID}"),
            )?;
            point(
                group.center().x,
                window.visual_viewport_bounds().bottom() - px(80.),
            )
        } else {
            selector_bounds(window, selector)?.center()
        };
        let mut trigger_focus = None;
        for (index, event) in [
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
        ]
        .into_iter()
        .enumerate()
        {
            window.dispatch_event(event, app);
            // Button 会在按下时聚焦右击目标；Kit 在同一次分发后、deferred 菜单打开前保存这个焦点。
            if index == 1 {
                trigger_focus = window.focused(app);
            }
        }
        Ok::<_, io::Error>(trigger_focus)
    })??)
}

/// 计数实际绘制的 Menu 节点，重复菜单即失败；键盘关闭与 deferred 打开都等待真实下一帧。
async fn wait_for_menu(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    open: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let count = handle.update(cx, |_, window, app| {
            window.refresh();
            draw_acceptance_frame(window, app);
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .filter(|node| node.visible() && node.role() == Some(gpui_pre::Role::Menu))
                .count()
        })?;
        if count > 1 {
            return Err(io::Error::other(format!("one right click opened {count} menus")).into());
        }
        if (count == 1) == open {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("sidebar menu did not settle").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 标签来自当前生产菜单项的可访问性信息，不使用序号猜测经过能力裁剪后的条目位置。
fn menu_labels(cx: &mut AsyncApp, handle: AnyWindowHandle) -> Result<Vec<String>, Box<dyn Error>> {
    Ok(handle.update(cx, |_, window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.visible() && node.role() == Some(gpui_pre::Role::MenuItem))
            .filter_map(|node| node.label().map(str::to_owned))
            .collect()
    })?)
}

/// 仅在唯一当前菜单项中定位动作，真实分发鼠标按下/抬起以覆盖 Kit 菜单执行与关闭路径。
fn click_menu_label(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    handle.update(cx, |_, window, app| {
        let items = gpui_kit::base::test_support::snapshots(window);
        let item = items
            .iter()
            .find(|node| {
                node.visible()
                    && node.role() == Some(gpui_pre::Role::MenuItem)
                    && node.label() == Some(label)
            })
            .ok_or_else(|| io::Error::other(format!("missing menu action {label}")))?;
        let position = item.bounds().center();
        for event in [
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 编辑器成功挂载与取消均读实际保存按钮；菜单消失本身不能证明新建动作已接线。
async fn wait_for_editor(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    open: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let visible = workbench_selector_visible(cx, handle, "connections-editor-save")?;
        if visible == open {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other(format!("connection editor open={open} did not settle")).into(),
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}
