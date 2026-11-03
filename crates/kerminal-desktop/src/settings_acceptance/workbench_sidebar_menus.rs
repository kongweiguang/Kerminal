//! 隔离原生窗口中验证左栏菜单归属、菜单动作及关闭；不使用 OS 输入，不访问真实主机。
//!
//! @author kongweiguang

use super::*;

/// 每个主题使用生产菜单和真正的编辑器入口，截图与动作结果分别保留，避免用静态像素替代接线。
pub(super) async fn capture(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    directory: &Path,
    theme: &str,
) -> Result<(), Box<dyn Error>> {
    ensure_ungrouped_hosts_render_at_tree_root(cx, window)?;
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
        if target == "host" {
            for (removed_zh, removed_en) in [
                ("查看连接详情", "Connection details"),
                ("容器", "Containers"),
            ] {
                let removed = kerminal_ui::locale::text(removed_zh, removed_en).to_string();
                if labels.contains(&removed) {
                    return Err(io::Error::other(format!(
                        "{target} menu retained removed action {removed:?}: {labels:?}"
                    ))
                    .into());
                }
            }
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
    Ok(())
}

/// 未分组主机必须直接挂在目录根层：旧“未分组”虚拟目录行选择器一旦重现即失败，
/// 且根层行必须比同一帧内分组成员更靠左，避免缩进或分组容器把根层悄悄吞回组内。
/// @author kongweiguang
fn ensure_ungrouped_hosts_render_at_tree_root(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let (ungrouped_header, grouped_x, ungrouped_x) = handle.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        let header = selector_bounds(window, "host-navigation-group-__ungrouped").is_ok();
        let grouped =
            selector_bounds(window, &format!("host-navigation-host-{WORKBENCH_HOST_ID}"))?
                .origin
                .x;
        let ungrouped = selector_bounds(
            window,
            &format!("host-navigation-host-{WORKBENCH_UNGROUPED_HOST_ID}"),
        )?
        .origin
        .x;
        Ok::<_, io::Error>((header, grouped, ungrouped))
    })??;
    if ungrouped_header {
        return Err(io::Error::other("ungrouped hosts recreated the virtual group header").into());
    }
    if ungrouped_x >= grouped_x {
        return Err(io::Error::other(format!(
            "ungrouped host row is not at the tree root: ungrouped x={ungrouped_x:?} grouped x={grouped_x:?}"
        ))
        .into());
    }
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

#[derive(Clone)]
struct DragReadback {
    group_id: Option<String>,
    revision: ConfigRevision,
}

/// 从真实服务读取远端或本地归属和 revision，截图不参与持久化判定。
///
/// @author kongweiguang
async fn read_drag_host(
    _cx: &mut AsyncApp,
    services: &kerminal_ui::AppServices,
    host_id: &str,
    local: bool,
) -> Result<DragReadback, Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host management service is unavailable"))?;
    let host_id = HostId::parse(host_id)?;
    services
        .executor
        .clone()
        .run(move |_| async move {
            if local {
                hosts
                    .load_local_host(&host_id)
                    .await?
                    .map(|host| DragReadback {
                        group_id: host.group_id,
                        revision: host.revision,
                    })
            } else {
                hosts.load_host(&host_id).await?.map(|host| DragReadback {
                    group_id: host.group_id,
                    revision: host.revision,
                })
            }
            .ok_or_else(|| kerminal_application::AppError::not_found("验收主机不存在"))
        })
        .await
        .map_err(Into::into)
}

/// 有界等待目标归属和新 revision，确保生产 drop 已完成保存与读回。
///
/// @author kongweiguang
async fn wait_drag_host(
    cx: &mut AsyncApp,
    services: &kerminal_ui::AppServices,
    host_id: &str,
    local: bool,
    destination: Option<&str>,
    previous_revision: &ConfigRevision,
) -> Result<DragReadback, Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let current = read_drag_host(cx, services, host_id, local).await?;
        if current.group_id.as_deref() == destination && current.revision != *previous_revision {
            return Ok(current);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "host {host_id} did not reach {destination:?}: {:?} at {:?}",
                current.group_id, current.revision
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 恢复夹具时使用最新 revision 调用同一仅移动分组用例，不构造完整草稿。
///
/// @author kongweiguang
async fn restore_drag_host(
    cx: &mut AsyncApp,
    services: &kerminal_ui::AppServices,
    host_id: &str,
    local: bool,
    destination: Option<&str>,
) -> Result<DragReadback, Box<dyn Error>> {
    let current = read_drag_host(cx, services, host_id, local).await?;
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host management service is unavailable"))?;
    let id = HostId::parse(host_id)?;
    let destination_owned = destination.map(str::to_owned);
    let expected_revision = current.revision.clone();
    services
        .executor
        .clone()
        .run(move |_| async move {
            hosts
                .move_host_to_group(id, destination_owned, expected_revision)
                .await
        })
        .await?;
    wait_drag_host(cx, services, host_id, local, destination, &current.revision).await
}

/// 读取共享会话注册表数量，证明主机移动不创建连接或终端。
///
/// @author kongweiguang
async fn drag_terminal_count(
    _cx: &mut AsyncApp,
    services: &kerminal_ui::AppServices,
) -> Result<usize, Box<dyn Error>> {
    let sessions = services.sessions.clone();
    Ok(services
        .executor
        .clone()
        .run(move |_| async move {
            sessions
                .list(kerminal_application::sessions::SessionFilter::default())
                .await
        })
        .await?
        .len())
}

/// 等待目录读回后源行重新启用拖动；加载中的缓存行不能用于启动新手势。
///
/// @author kongweiguang
async fn wait_drag_source_ready(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    source: &str,
) -> Result<(), Box<dyn Error>> {
    const STABLE_FOR: Duration = Duration::from_millis(200);
    let deadline = Instant::now() + ACTION_TIMEOUT;
    let mut ready_since = None;
    loop {
        let ready = handle.update(cx, |_, window, app| {
            window.refresh();
            draw_acceptance_frame(window, app);
            let id = source.to_owned().into();
            gpui_kit::base::test_support::find(window, &[], &id)
                .and_then(|node| node.label().map(str::to_owned))
                .is_some_and(|label| {
                    label.contains("可拖动到分组") || label.contains("drag to a group")
                })
        })?;
        let now = Instant::now();
        if ready {
            let stable_since = *ready_since.get_or_insert(now);
            if now.duration_since(stable_since) >= STABLE_FOR {
                return Ok(());
            }
        } else {
            ready_since = None;
        }
        if now >= deadline {
            return Err(
                io::Error::other(format!("host row did not become draggable: {source}")).into(),
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 在当前 selector bounds 上开始真实拖动，先越过 GPUI 阈值再移到目标点。
///
/// @author kongweiguang
fn start_drag_point(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    source: &str,
    destination: gpui_kit::Point<gpui_kit::Pixels>,
) -> Result<(), Box<dyn Error>> {
    handle.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        let start = selector_bounds(window, source)?.center();
        for event in [
            MouseMoveEvent {
                position: start,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position: start,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseMoveEvent {
                position: start + point(px(8.), px(0.)),
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            window.dispatch_event(event, app);
        }
        draw_acceptance_frame(window, app);
        if !app.has_active_drag() {
            return Err(io::Error::other(format!(
                "host drag did not start for {source}"
            )));
        }
        window.dispatch_event(
            MouseMoveEvent {
                position: destination,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 解析生产目标 selector 后开始拖动，不调用内部 request 或直接修改状态。
///
/// @author kongweiguang
async fn start_drag_selector(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    source: &str,
    target: &str,
) -> Result<gpui_kit::Point<gpui_kit::Pixels>, Box<dyn Error>> {
    wait_drag_source_ready(cx, handle, source).await?;
    let destination = handle.update(cx, |_, window, app| {
        window.refresh();
        draw_acceptance_frame(window, app);
        selector_bounds(window, target).map(|bounds| bounds.center())
    })??;
    start_drag_point(cx, handle, source, destination)?;
    Ok(destination)
}

/// 在指定坐标发送真实 MouseUp；取消后的释放也复用此入口以证明不会迟到提交。
///
/// @author kongweiguang
fn release_drag_point(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    destination: gpui_kit::Point<gpui_kit::Pixels>,
) -> Result<(), Box<dyn Error>> {
    handle.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position: destination,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 完成一次生产拖放，可在 MouseUp 前保存目标高亮证据。
///
/// @author kongweiguang
async fn drag_selector(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    source: &str,
    target: &str,
    before_release: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let destination = start_drag_selector(cx, handle, source, target).await?;
    if let Some(path) = before_release {
        let _ = save_frame(cx, handle, path)?;
    }
    release_drag_point(cx, handle, destination)
}

/// 读取组头折叠状态，只在状态需要变化时点击，避免每轮无条件 toggle。
///
/// @author kongweiguang
fn set_drop_group_collapsed(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    collapsed: bool,
) -> Result<(), Box<dyn Error>> {
    let selector = format!("host-navigation-group-{WORKBENCH_DROP_GROUP_ID}");
    let should_click = handle.update(cx, |_, window, app| {
        window.refresh();
        draw_acceptance_frame(window, app);
        let id = selector.clone().into();
        let node = gpui_kit::base::test_support::find(window, &[], &id)
            .ok_or_else(|| io::Error::other("drop group header is unavailable"))?;
        let label = node.label().unwrap_or_default();
        let is_collapsed = label.contains("已折叠") || label.contains("collapsed");
        Ok::<_, io::Error>(is_collapsed != collapsed)
    })??;
    if should_click {
        click_selector(cx, handle, &selector)?;
    }
    Ok(())
}

/// 三种主题都通过生产鼠标/键盘事件验证真实保存、空根层返回和 Escape 零提交。
///
/// @author kongweiguang
pub(super) async fn capture_drag(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    directory: &Path,
    theme: &str,
) -> Result<(), Box<dyn Error>> {
    let terminals_before = drag_terminal_count(cx, services).await?;
    let scale_factor = window.update(cx, |_, window, _| window.scale_factor())?;
    set_drop_group_collapsed(cx, window, true)?;
    let grouped_selector = format!("host-navigation-host-{WORKBENCH_HOST_ID}");
    let ungrouped_selector = format!("host-navigation-host-{WORKBENCH_UNGROUPED_HOST_ID}");
    let local_selector = format!("host-navigation-profile-{WORKBENCH_PROFILE_ID}");
    let drop_section = format!("host-navigation-section-{WORKBENCH_DROP_GROUP_ID}");
    let mut steps = Vec::new();

    let before = read_drag_host(cx, services, WORKBENCH_HOST_ID, false).await?;
    drag_selector(
        cx,
        window,
        &grouped_selector,
        "host-navigation-section-ungrouped",
        None,
    )
    .await?;
    let after = wait_drag_host(
        cx,
        services,
        WORKBENCH_HOST_ID,
        false,
        None,
        &before.revision,
    )
    .await?;
    steps.push(serde_json::json!({
        "source": WORKBENCH_HOST_ID,
        "destination": null,
        "beforeRevision": before.revision,
        "afterRevision": after.revision,
    }));
    restore_drag_host(
        cx,
        services,
        WORKBENCH_HOST_ID,
        false,
        Some(WORKBENCH_GROUP_ID),
    )
    .await?;

    let before = read_drag_host(cx, services, WORKBENCH_UNGROUPED_HOST_ID, false).await?;
    drag_selector(
        cx,
        window,
        &ungrouped_selector,
        &drop_section,
        Some(directory.join(format!("sidebar-drag-{theme}-target.png"))),
    )
    .await?;
    let after = wait_drag_host(
        cx,
        services,
        WORKBENCH_UNGROUPED_HOST_ID,
        false,
        Some(WORKBENCH_DROP_GROUP_ID),
        &before.revision,
    )
    .await?;
    steps.push(serde_json::json!({
        "source": WORKBENCH_UNGROUPED_HOST_ID,
        "destination": WORKBENCH_DROP_GROUP_ID,
        "beforeRevision": before.revision,
        "afterRevision": after.revision,
        "collapsedTarget": true,
    }));

    let before = read_drag_host(cx, services, WORKBENCH_PROFILE_ID, true).await?;
    drag_selector(cx, window, &local_selector, &drop_section, None).await?;
    let after = wait_drag_host(
        cx,
        services,
        WORKBENCH_PROFILE_ID,
        true,
        Some(WORKBENCH_DROP_GROUP_ID),
        &before.revision,
    )
    .await?;
    steps.push(serde_json::json!({
        "source": WORKBENCH_PROFILE_ID,
        "destination": WORKBENCH_DROP_GROUP_ID,
        "beforeRevision": before.revision,
        "afterRevision": after.revision,
        "local": true,
    }));
    let _ = save_frame(
        cx,
        window,
        directory.join(format!("sidebar-drag-{theme}-complete.png")),
    )?;

    set_drop_group_collapsed(cx, window, false)?;
    let before = read_drag_host(cx, services, WORKBENCH_UNGROUPED_HOST_ID, false).await?;
    drag_selector(
        cx,
        window,
        &ungrouped_selector,
        "host-navigation-root-drop",
        None,
    )
    .await?;
    let after = wait_drag_host(
        cx,
        services,
        WORKBENCH_UNGROUPED_HOST_ID,
        false,
        None,
        &before.revision,
    )
    .await?;
    steps.push(serde_json::json!({
        "source": WORKBENCH_UNGROUPED_HOST_ID,
        "destination": null,
        "beforeRevision": before.revision,
        "afterRevision": after.revision,
        "emptyRootDrop": true,
    }));

    click_selector(cx, window, "host-navigation-search-frame")?;
    let before_escape = read_drag_host(cx, services, WORKBENCH_PROFILE_ID, true).await?;
    let escape_release =
        start_drag_selector(cx, window, &local_selector, "host-navigation-root-drop").await?;
    press_key(cx, window, "escape")?;
    let (active, preview_visible) = window.update(cx, |_, window, app| {
        window.refresh();
        draw_acceptance_frame(window, app);
        (
            app.has_active_drag(),
            window
                .debug_selector_bounds("host-navigation-drag-preview")
                .is_some(),
        )
    })?;
    release_drag_point(cx, window, escape_release)?;
    cx.background_executor()
        .timer(Duration::from_millis(100))
        .await;
    let after_escape = read_drag_host(cx, services, WORKBENCH_PROFILE_ID, true).await?;
    let escape_ok = !active
        && !preview_visible
        && after_escape.group_id == before_escape.group_id
        && after_escape.revision == before_escape.revision;
    if !escape_ok {
        return Err(io::Error::other("Escape did not cancel the host drag cleanly").into());
    }

    restore_drag_host(
        cx,
        services,
        WORKBENCH_PROFILE_ID,
        true,
        Some(WORKBENCH_GROUP_ID),
    )
    .await?;
    let terminals_after = drag_terminal_count(cx, services).await?;
    if terminals_after != terminals_before {
        return Err(io::Error::other(format!(
            "host drag changed terminal count: {terminals_before} -> {terminals_after}"
        ))
        .into());
    }
    fs::write(
        directory.join(format!("sidebar-drag-{theme}.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "theme": theme,
            "scaleFactor": scale_factor,
            "targetGroup": WORKBENCH_DROP_GROUP_ID,
            "steps": steps,
            "escape": {
                "activeAfterEscape": active,
                "previewVisibleAfterEscape": preview_visible,
                "committed": false,
                "groupUnchanged": true,
                "revisionUnchanged": true,
            },
            "terminalCountBefore": terminals_before,
            "terminalCountAfter": terminals_after,
        }))?,
    )?;
    Ok(())
}

/// 通过真实滚轮把主机树恢复到顶部，避免后续既有验收看到溢出后的滚动偏移。
///
/// @author kongweiguang
async fn scroll_host_tree_to_top(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let selector = format!("host-navigation-profile-{WORKBENCH_PROFILE_ID}");
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let done = handle.update(cx, |_, window, app| {
            window.refresh();
            draw_acceptance_frame(window, app);
            if selector_bounds(window, &selector).is_ok() {
                return Ok::<_, io::Error>(true);
            }
            let viewport = selector_bounds(window, "host-navigation-tree")?;
            window.dispatch_event(
                ScrollWheelEvent {
                    position: viewport.center(),
                    delta: ScrollDelta::Pixels(point(px(0.), px(2_000.))),
                    touch_phase: TouchPhase::Moved,
                    ..ScrollWheelEvent::default()
                }
                .to_platform_input(),
                app,
            );
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(false)
        })??;
        if done {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("host tree did not return to its top").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 在同一隔离窗口顺序创建 24 个空组，真实等待边缘滚动后投到末组并恢复主机。
///
/// @author kongweiguang
pub(super) async fn capture_drag_overflow(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let host_service = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host management service is unavailable"))?;
    let mut groups_revision = services
        .executor
        .clone()
        .run({
            let hosts = host_service.clone();
            move |_| async move { hosts.load_groups().await }
        })
        .await?
        .revision;
    for index in 0..24 {
        let hosts = host_service.clone();
        let snapshot = services
            .executor
            .clone()
            .run(move |_| async move {
                hosts
                    .save_group(
                        HostGroupDraft {
                            id: Some(format!("acceptance-overflow-{index:02}")),
                            name: format!("acceptance-overflow-{index:02}"),
                            sort_order: 100 + index,
                        },
                        groups_revision,
                    )
                    .await
            })
            .await?;
        groups_revision = snapshot.revision;
    }

    let terminals_before = drag_terminal_count(cx, services).await?;
    let before = read_drag_host(cx, services, WORKBENCH_HOST_ID, false).await?;
    if before.group_id.as_deref() != Some(WORKBENCH_GROUP_ID) {
        return Err(io::Error::other("overflow fixture host is not in its original group").into());
    }
    let source = format!("host-navigation-host-{WORKBENCH_HOST_ID}");
    wait_drag_source_ready(cx, window, &source).await?;
    let target_selector = "host-navigation-section-acceptance-overflow-23";
    let viewport = window.update(cx, |_, window, app| {
        window.refresh();
        draw_acceptance_frame(window, app);
        selector_bounds(window, "host-navigation-tree")
    })??;
    start_drag_point(
        cx,
        window,
        &source,
        point(viewport.center().x, viewport.bottom() - px(2.)),
    )?;

    let deadline = Instant::now() + ACTION_TIMEOUT;
    let target = loop {
        let current = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            let viewport = selector_bounds(window, "host-navigation-tree")?;
            let target = selector_bounds(window, target_selector).ok();
            let visible = target.is_some_and(|target| {
                target.origin.y >= viewport.origin.y && target.bottom() <= viewport.bottom()
            });
            Ok::<_, io::Error>((target, visible, app.has_active_drag()))
        })??;
        if let (Some(target), true) = (current.0, current.1) {
            break target;
        }
        if !current.2 {
            return Err(io::Error::other("overflow host drag ended before the last group").into());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other("edge scroll did not reveal acceptance-overflow-23").into(),
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(60))
            .await;
    };
    let _ = save_frame(
        cx,
        window,
        directory.join("sidebar-drag-overflow-target.png"),
    )?;
    release_drag_point(cx, window, target.center())?;
    let after = wait_drag_host(
        cx,
        services,
        WORKBENCH_HOST_ID,
        false,
        Some("acceptance-overflow-23"),
        &before.revision,
    )
    .await?;
    let restored = restore_drag_host(
        cx,
        services,
        WORKBENCH_HOST_ID,
        false,
        Some(WORKBENCH_GROUP_ID),
    )
    .await?;
    let terminals_after = drag_terminal_count(cx, services).await?;
    if terminals_after != terminals_before {
        return Err(io::Error::other("overflow drag created a terminal").into());
    }
    fs::write(
        directory.join("sidebar-drag-overflow.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "source": WORKBENCH_HOST_ID,
            "destination": "acceptance-overflow-23",
            "overflowGroupCount": 24,
            "beforeRevision": before.revision,
            "afterRevision": after.revision,
            "restoredRevision": restored.revision,
            "restoredGroup": restored.group_id,
            "edgeScrollReachedTarget": true,
            "terminalCountBefore": terminals_before,
            "terminalCountAfter": terminals_after,
        }))?,
    )?;
    scroll_host_tree_to_top(cx, window).await?;
    Ok(())
}
