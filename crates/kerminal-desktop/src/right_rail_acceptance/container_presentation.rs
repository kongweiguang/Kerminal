//! 容器侧栏的只读原生交互验收，不改变夹具容器或 Compose 项目状态。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use gpui_pre::{InputEvent, KeyDownEvent, KeyUpEvent, Keystroke};
use kerminal_domain::{identity::HostId, operations::ComposeProjectTarget};
use kerminal_ui::RootView;
use serde::Serialize;

use super::{
    CaptureRecord, RIGHT_RAIL_THEMES, RightRailCaptureContext, StateCaptureMetadata,
    StateCaptureRecord, ToolRailToolId, activate_dialog_tool, capture_state, frame_signature,
    functional::poll_root, menus, wait_for_acceptance_state, wait_for_acceptance_value,
    wait_for_stable_frame,
};

/// 容器专项逐主题独立登记基线、面板、详情和菜单，不借用完整右栏验收报告。
///
/// @author kongweiguang
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ContainerThemeCaptureRecord {
    mode: &'static str,
    window_appearance: String,
    baseline: CaptureRecord,
    states: Vec<StateCaptureRecord>,
    menus: Vec<menus::MenuCaptureRecord>,
}

/// 容器专项只激活本工具及其只读菜单，三种主题均运行与完整验收相同的交互检查。
///
/// @author kongweiguang
pub(super) async fn capture_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
) -> Result<Vec<ContainerThemeCaptureRecord>, Box<dyn Error>> {
    let mut records = Vec::with_capacity(RIGHT_RAIL_THEMES.len());
    for (mode, theme_name) in RIGHT_RAIL_THEMES {
        let window_appearance = window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.close_right_rail_acceptance_panels(window, cx);
                root.set_right_rail_acceptance_theme(mode, cx);
            });
            format!("{:?}", window.appearance())
        })?;
        let baseline = wait_for_stable_frame(
            cx,
            window,
            capture
                .output_directory
                .join(format!("theme-{theme_name}-baseline.png")),
            None,
            "container theme baseline",
        )
        .await?;
        activate_dialog_tool(
            cx,
            root,
            window,
            ToolRailToolId::Containers,
            &capture.fixture_host_id,
            "containers",
        )
        .await?;
        window
            .update(cx, |_, _, app| {
                root.update(app, |root, cx| {
                    root.return_right_rail_acceptance_container_list(&capture.fixture_host_id, cx)
                })
            })?
            .map_err(io::Error::other)?;
        let mut states = vec![
            capture_state(
                cx,
                window,
                &capture.output_directory,
                &baseline.pixel_sha256,
                StateCaptureMetadata {
                    theme_name,
                    surface: "container-list",
                    host_id: capture.fixture_host_id.as_str().to_owned(),
                    target_id: None,
                    interaction: "production_container_list",
                },
            )
            .await?,
        ];
        let mut menu_records = Vec::new();
        for (surface, selector) in [
            ("containers-runtime", "rr-containers-runtime"),
            ("containers-group", "rr-containers-group"),
        ] {
            menu_records.push(
                menus::capture_menu(
                    cx,
                    window,
                    &capture.output_directory,
                    theme_name,
                    surface,
                    surface,
                    selector,
                )
                .await?,
            );
        }
        capture_container_states(
            cx,
            root,
            window,
            capture,
            theme_name,
            &baseline.pixel_sha256,
            (&mut states, &mut menu_records),
        )
        .await?;
        records.push(ContainerThemeCaptureRecord {
            mode: theme_name,
            window_appearance,
            baseline,
            states,
            menus: menu_records,
        });
    }
    if records[0].baseline.pixel_sha256 == records[1].baseline.pixel_sha256 {
        return Err(io::Error::other("容器浅色与深色主题基线相同").into());
    }
    Ok(records)
}

/// 容器详情和监控使用同一目标的生产读取；完整右栏与容器专项共享动作和截图断言。
///
/// @author kongweiguang
pub(super) async fn capture_container_states(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    theme_name: &'static str,
    baseline: &str,
    captures: (
        &mut Vec<StateCaptureRecord>,
        &mut Vec<menus::MenuCaptureRecord>,
    ),
) -> Result<(), Box<dyn Error>> {
    let (states, menu_records) = captures;
    let host = &capture.fixture_host_id;
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Containers,
        host,
        "containers",
    )
    .await?;
    let container_id =
        wait_for_acceptance_value(cx, root, window, "running container target", |root, cx| {
            root.right_rail_acceptance_container_target_id(
                host,
                &capture.fixture_container_name,
                cx,
            )
        })
        .await?;
    for (page, surface) in [
        ("details", "container-details"),
        ("stats", "container-monitor"),
    ] {
        window
            .update(cx, |_, _, app| {
                root.update(app, |root, cx| {
                    root.open_right_rail_acceptance_container_page(host, &container_id, page, cx)
                })
            })?
            .map_err(io::Error::other)?;
        wait_for_acceptance_state(cx, root, window, surface, |root, cx| {
            root.right_rail_acceptance_container_page_ready(host, &container_id, page, cx)
        })
        .await?;
        states.push(
            capture_state(
                cx,
                window,
                &capture.output_directory,
                baseline,
                StateCaptureMetadata {
                    theme_name,
                    surface,
                    host_id: host.as_str().to_owned(),
                    target_id: Some(container_id.clone()),
                    interaction: "acceptance_navigation_and_production_container_read",
                },
            )
            .await?,
        );
        if page == "details" {
            menu_records.push(
                menus::capture_menu(
                    cx,
                    window,
                    &capture.output_directory,
                    theme_name,
                    "container-actions",
                    "container-actions",
                    &format!("rr-container-more-{container_id}"),
                )
                .await?,
            );
        } else {
            check_stats_refresh(cx, root, window, host, &container_id).await?;
        }
    }
    capture_compose(
        cx,
        root,
        window,
        capture,
        theme_name,
        baseline,
        (states, menu_records),
    )
    .await
}

/// 逐帧读取真实元素位置；跳过目标消失会掩盖刷新时列表被替换的布局问题。
///
/// @author kongweiguang
fn selector_y(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<f32, Box<dyn Error>> {
    window
        .update(cx, |_, window, _| {
            window
                .debug_selector_bounds(selector)
                .map(|bounds| bounds.origin.y.as_f32())
                .ok_or_else(|| io::Error::other(format!("缺少实际界面元素 {selector}")))
        })?
        .map_err(Into::into)
}

/// 使用同一容器的实际监控框验证多行显示和刷新前后的纵向位置，不把完成回执当作像素验收。
///
/// @author kongweiguang
pub(super) async fn check_stats_refresh(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    container_id: &str,
) -> Result<(), Box<dyn Error>> {
    let _ = frame_signature(cx, window)?;
    let bounds = window.update(cx, |_, window, _| {
        window.debug_selector_bounds("rr-container-stats-raw")
    })?;
    let bounds = bounds.ok_or_else(|| io::Error::other("缺少容器监控原始 JSON 区域"))?;
    if bounds.size.height.as_f32() <= 28.0 {
        return Err(io::Error::other("容器监控原始 JSON 未呈现为多行").into());
    }
    let before = bounds.origin.y.as_f32();
    menus::click_selector(cx, window, "rr-container-refresh")?;
    let _ = frame_signature(cx, window)?;
    let after = selector_y(cx, window, "rr-container-stats-raw")?;
    if (before - after).abs() > 1.0 {
        return Err(
            io::Error::other(format!("容器监控刷新时正文位置变化：{before} -> {after}")).into(),
        );
    }
    poll_root(
        cx,
        root,
        window,
        "容器监控刷新回执",
        |root, _, cx| {
            root.right_rail_acceptance_container_page_ready(host, container_id, "stats", cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 点击目标项目并回读展开状态；连续单击都必须保持列表页且只改变一次折叠状态。
///
/// @author kongweiguang
async fn toggle_and_wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
    selector: &str,
    expected: bool,
) -> Result<(), Box<dyn Error>> {
    menus::click_selector(cx, window, selector)?;
    poll_root(cx, root, window, "Compose 折叠状态", |root, _, cx| {
        let Some((list, expanded, _, _, _)) =
            root.right_rail_acceptance_compose_presentation(host, target, cx)?
        else {
            return Ok(None);
        };
        if !list {
            return Err("点击 Compose 行意外离开了容器列表".to_owned());
        }
        Ok((expanded == expected).then_some(()))
    })
    .await
}

/// 通过窗口内键盘输入选择真实菜单项，避免在验收桥直接调用详情导航。
///
/// @author kongweiguang
fn menu_key(cx: &mut AsyncApp, window: AnyWindowHandle, key: &str) -> Result<(), Box<dyn Error>> {
    let keystroke = Keystroke::parse(key)
        .map_err(|error| io::Error::other(format!("菜单按键无效：{error}")))?;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), app);
    })?;
    let _ = frame_signature(cx, window)?;
    Ok(())
}

/// YAML 必须对应夹具真实端点、完成读取且不产生中央文件标签，等待响应时不信任旧帧。
///
/// @author kongweiguang
async fn wait_yaml(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
    tab_count: usize,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "Compose YAML 侧栏回执",
        |root, _, cx| {
            if root.right_rail_acceptance_workspace_tab_count() != Some(tab_count) {
                return Err("Compose YAML 意外改变了中央工作区标签数".to_owned());
            }
            let Some((_, _, detail, yaml, ready)) =
                root.right_rail_acceptance_compose_presentation(host, target, cx)?
            else {
                return Ok(None);
            };
            Ok((detail && yaml && ready).then_some(()))
        },
    )
    .await
}

/// 对照实际 Input 与外层视口，并要求夹具末行的字形落在可见范围内，防止空白大容器掩盖短编辑器。
///
/// @author kongweiguang
async fn check_yaml_editor_layout(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    let (input, tail) = poll_root(
        cx,
        root,
        window,
        "Compose YAML 编辑器布局",
        |root, _, cx| root.right_rail_acceptance_compose_editor_bounds(host, target, cx),
    )
    .await?;
    let frame = window
        .update(cx, |_, window, _| {
            window.debug_selector_bounds("rr-compose-yaml-editor-frame")
        })?
        .ok_or_else(|| io::Error::other("Compose YAML 编辑器视口未绘制"))?;
    let frame_height = frame.size.height.as_f32();
    let input_height = input.size.height.as_f32();
    if frame_height < 120.0 || input_height < frame_height * 0.8 {
        return Err(io::Error::other(format!(
            "Compose YAML 输入区未填满视口：输入区 {input_height:.1}px，视口 {frame_height:.1}px"
        ))
        .into());
    }
    let tail =
        tail.ok_or_else(|| io::Error::other("Compose YAML fixture 末行未进入实际绘制范围"))?;
    let within = |origin: f32, length: f32, edge: f32, span: f32| {
        origin >= edge - 1.0 && origin + length <= edge + span + 1.0
    };
    if !within(
        tail.origin.y.as_f32(),
        tail.size.height.as_f32(),
        input.origin.y.as_f32(),
        input_height,
    ) || !within(
        tail.origin.x.as_f32(),
        tail.size.width.as_f32(),
        input.origin.x.as_f32(),
        input.size.width.as_f32(),
    ) || !within(
        tail.origin.y.as_f32(),
        tail.size.height.as_f32(),
        frame.origin.y.as_f32(),
        frame_height,
    ) {
        return Err(io::Error::other("Compose YAML fixture 末行被编辑器或外层视口裁剪").into());
    }
    Ok(())
}

/// YAML 返回、项目返回均通过真实按钮，区分返回概览与返回列表两层导航。
///
/// @author kongweiguang
async fn back_to_list(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    menus::click_selector(cx, window, "rr-compose-back")?;
    poll_root(
        cx,
        root,
        window,
        "YAML 返回项目概览",
        |root, _, cx| {
            let Some((_, _, detail, yaml, _)) =
                root.right_rail_acceptance_compose_presentation(host, target, cx)?
            else {
                return Ok(None);
            };
            Ok((detail && !yaml).then_some(()))
        },
    )
    .await?;
    menus::click_selector(cx, window, "rr-compose-back")?;
    poll_root(
        cx,
        root,
        window,
        "项目返回容器列表",
        |root, _, cx| {
            let Some((list, _, _, _, _)) =
                root.right_rail_acceptance_compose_presentation(host, target, cx)?
            else {
                return Ok(None);
            };
            Ok(list.then_some(()))
        },
    )
    .await
}

/// 三主题均在原生窗口中重复验证列表点击、菜单详情和两个 YAML 入口，不向夹具发送写命令。
///
/// @author kongweiguang
pub(super) async fn capture_compose(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    theme: &'static str,
    baseline: &str,
    captures: (
        &mut Vec<StateCaptureRecord>,
        &mut Vec<menus::MenuCaptureRecord>,
    ),
) -> Result<(), Box<dyn Error>> {
    let (states, menu_records) = captures;
    let host = &capture.fixture_host_id;
    let run_id = capture
        .fixture_container_name
        .strip_prefix("kerminal-rr-")
        .ok_or_else(|| io::Error::other("容器 fixture 缺少运行 ID"))?;
    let project_name = format!("kerminal-rr-compose-{run_id}");
    let target = poll_root(cx, root, window, "Compose fixture 目标", |root, _, cx| {
        root.right_rail_acceptance_compose_target(host, &project_name, cx)
    })
    .await?;
    window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.return_right_rail_acceptance_container_list(host, cx)
            })
        })?
        .map_err(io::Error::other)?;
    let key = format!(
        "{:?}",
        (
            target.host_id.as_ref().map(|host| host.as_str()),
            target.runtime,
            &target.project,
            &target.working_dir,
            &target.config_files
        )
    );
    let toggle = format!("rr-compose-toggle-{key}");
    let more = format!("rr-compose-project-more-{key}");
    let list_yaml = format!("rr-compose-yaml-{key}");
    let initially_expanded = poll_root(cx, root, window, "Compose 列表页", |root, _, cx| {
        let Some((list, expanded, _, _, _)) =
            root.right_rail_acceptance_compose_presentation(host, &target, cx)?
        else {
            return Ok(None);
        };
        Ok(list.then_some(expanded))
    })
    .await?;
    if initially_expanded {
        toggle_and_wait(cx, root, window, host, &target, &toggle, false).await?;
    }
    let before = selector_y(cx, window, &toggle)?;
    menus::click_selector(cx, window, "rr-containers-refresh")?;
    let _ = frame_signature(cx, window)?;
    let after = selector_y(cx, window, &toggle)?;
    if (before - after).abs() > 1.0 {
        return Err(
            io::Error::other(format!("容器列表刷新时项目位置变化：{before} -> {after}")).into(),
        );
    }
    poll_root(
        cx,
        root,
        window,
        "容器列表刷新回执",
        |root, _, cx| {
            root.right_rail_acceptance_compose_presentation(host, &target, cx)
                .map(|state| state.map(|_| ()))
        },
    )
    .await?;

    for index in 0..3 {
        toggle_and_wait(cx, root, window, host, &target, &toggle, true).await?;
        if index == 0 {
            states.push(
                capture_state(
                    cx,
                    window,
                    &capture.output_directory,
                    baseline,
                    StateCaptureMetadata {
                        theme_name: theme,
                        surface: "compose-expanded",
                        host_id: host.as_str().to_owned(),
                        target_id: Some(target.project.clone()),
                        interaction: "production_pointer_click_and_target_bound_expanded_state",
                    },
                )
                .await?,
            );
        }
        toggle_and_wait(cx, root, window, host, &target, &toggle, false).await?;
    }

    menu_records.push(
        menus::capture_menu(
            cx,
            window,
            &capture.output_directory,
            theme,
            "compose-list-actions",
            "compose-list-actions",
            &more,
        )
        .await?,
    );
    menus::click_selector(cx, window, &more)?;
    poll_root(cx, root, window, "Compose 更多菜单", |_, window, _| {
        Ok(window.debug_selector_bounds("rr-popup-menu").map(|_| ()))
    })
    .await?;
    menu_key(cx, window, "down")?;
    menu_key(cx, window, "enter")?;
    poll_root(
        cx,
        root,
        window,
        "更多菜单进入 Compose 详情",
        |root, _, cx| {
            let Some((_, _, detail, yaml, _)) =
                root.right_rail_acceptance_compose_presentation(host, &target, cx)?
            else {
                return Ok(None);
            };
            Ok((detail && !yaml).then_some(()))
        },
    )
    .await?;
    states.push(
        capture_state(
            cx,
            window,
            &capture.output_directory,
            baseline,
            StateCaptureMetadata {
                theme_name: theme,
                surface: "compose-details",
                host_id: host.as_str().to_owned(),
                target_id: Some(target.project.clone()),
                interaction: "production_more_menu_view_details",
            },
        )
        .await?,
    );
    menu_records.push(
        menus::capture_menu(
            cx,
            window,
            &capture.output_directory,
            theme,
            "compose-detail-actions",
            "compose-detail-actions",
            "rr-compose-project-more",
        )
        .await?,
    );
    let container_id =
        wait_for_acceptance_value(cx, root, window, "running container target", |root, cx| {
            root.right_rail_acceptance_container_target_id(
                host,
                &capture.fixture_container_name,
                cx,
            )
        })
        .await?;
    menus::click_selector(cx, window, "rr-compose-tab-containers")?;
    poll_root(cx, root, window, "Compose 容器页签", |_, window, _| {
        Ok(window
            .debug_selector_bounds(&format!("rr-container-row-{container_id}"))
            .map(|_| ()))
    })
    .await?;
    menus::click_selector(cx, window, &format!("rr-container-row-{container_id}"))?;
    poll_root(
        cx,
        root,
        window,
        "项目容器进入详情",
        |root, _, cx| {
            root.right_rail_acceptance_container_page_ready(host, &container_id, "details", cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    menus::click_selector(cx, window, "rr-container-back")?;
    poll_root(
        cx,
        root,
        window,
        "容器返回项目容器页签",
        |root, window, cx| {
            if window
                .debug_selector_bounds(&format!("rr-container-row-{container_id}"))
                .is_none()
            {
                return Ok(None);
            }
            let Some((_, _, detail, yaml, _)) =
                root.right_rail_acceptance_compose_presentation(host, &target, cx)?
            else {
                return Ok(None);
            };
            Ok((detail && !yaml).then_some(()))
        },
    )
    .await?;

    let tabs_before = poll_root(cx, root, window, "中央标签数", |root, _, _| {
        Ok(root.right_rail_acceptance_workspace_tab_count())
    })
    .await?;
    menus::click_selector(cx, window, "rr-compose-yaml")?;
    wait_yaml(cx, root, window, host, &target, tabs_before).await?;
    check_yaml_editor_layout(cx, root, window, host, &target).await?;
    states.push(
        capture_state(
            cx,
            window,
            &capture.output_directory,
            baseline,
            StateCaptureMetadata {
                theme_name: theme,
                surface: "compose-yaml",
                host_id: host.as_str().to_owned(),
                target_id: Some(target.project.clone()),
                interaction: "production_yaml_button_side_panel_without_workspace_tab",
            },
        )
        .await?,
    );
    back_to_list(cx, root, window, host, &target).await?;
    menus::click_selector(cx, window, &list_yaml)?;
    wait_yaml(cx, root, window, host, &target, tabs_before).await?;
    check_yaml_editor_layout(cx, root, window, host, &target).await?;
    back_to_list(cx, root, window, host, &target).await
}
