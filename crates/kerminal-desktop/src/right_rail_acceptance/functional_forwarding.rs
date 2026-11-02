//! 验证生产 SSH 隧道四种方向的新增、取消、真实数据面、停止和删除闭环。
//!
//! @author kongweiguang

use std::{
    error::Error,
    ffi::OsStr,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use gpui_kit::{AnyWindowHandle, AsyncApp, Bounds, ElementId, Entity, Pixels};
use gpui_pre::{InputEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};
use kerminal_application::{
    preferences::ThemeMode,
    sessions::{
        ShellCommandOutcome, ShellCommandRequest, TerminalCommandAvailability, UiSessionBinding,
    },
};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, ForwardState, PortForwardConfig, ProxyApplyScope,
        ProxyProtocol, RemoteAccessScope,
    },
};
use kerminal_ui::{
    AppServices, RootView,
    features::operations::proxy_panel::{ProxyCurrentApplication, ProxyCurrentAvailability},
};

use super::{
    RightRailCaptureContext, ToolRailToolId, activate_dialog_tool,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

const FIXTURE_WSL_DISTRIBUTION: &str = "Ubuntu-22.04";
const FIXTURE_CONNECT_TIMEOUT: Duration = Duration::from_millis(600);
const STOPPED_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const FIXTURE_READ_TIMEOUT: Duration = Duration::from_secs(2);
const FIXTURE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(12);
const FIXTURE_RETRY_INTERVAL: Duration = Duration::from_millis(150);
const WSL_COMMAND_TIMEOUT: Duration = Duration::from_secs(12);

/// Keeps the configuration and protocol probe from different forwarding cases from being mixed.
struct ForwardCase {
    config: PortForwardConfig,
    label: &'static str,
    probe: ForwardProbe,
}

struct ForwardFixture {
    config: PortForwardConfig,
    forward_id: String,
    label: &'static str,
    probe: ForwardProbe,
}

#[derive(Clone, Copy)]
enum ForwardProbe {
    LocalHttp {
        bind_port: u16,
    },
    LocalSocks {
        bind_port: u16,
        destination_port: u16,
    },
    RemoteHttp {
        bind_port: u16,
        expected_body: &'static str,
    },
    RemoteSocks {
        bind_port: u16,
        destination_port: u16,
        expected_body: &'static str,
    },
}

struct LocalHttpFixture {
    port: u16,
    stopping: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Result<(), io::Error>>>,
}

enum CleanupReadback {
    Deleted,
    State(ForwardState),
}

/// 通过生产新增表单依次验证四种隧道，远端 SOCKS 还须在同一 fixture shell 回读代理变量。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &kerminal_ui::AppServices,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    let run_id = capture
        .output_directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("验收输出目录缺少唯一名称"))?;
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Ports,
        host_id,
        "ports functional",
    )
    .await?;
    verify_add_form_cancel(cx, root, window, capture, records).await?;

    let local_bind = unused_loopback_port()?;
    let local_config = acceptance_config(
        host_id,
        &fixture_forward_name(run_id, "local")?,
        ForwardKind::Local,
        local_bind,
        Some(capture.container_target_port),
    )?;
    run_forward_case(
        cx,
        root,
        window,
        capture,
        records,
        services,
        ForwardCase {
            config: local_config,
            label: "local",
            probe: ForwardProbe::LocalHttp {
                bind_port: local_bind,
            },
        },
    )
    .await?;

    let socks_bind = unused_loopback_port()?;
    let socks_config = acceptance_config(
        host_id,
        &fixture_forward_name(run_id, "dynamic-local")?,
        ForwardKind::Dynamic,
        socks_bind,
        None,
    )?;
    run_forward_case(
        cx,
        root,
        window,
        capture,
        records,
        services,
        ForwardCase {
            config: socks_config,
            label: "dynamic-local",
            probe: ForwardProbe::LocalSocks {
                bind_port: socks_bind,
                destination_port: capture.container_target_port,
            },
        },
    )
    .await?;

    let local_http = LocalHttpFixture::start()?;
    let remote_bind = unused_remote_loopback_port(cx).await?;
    let remote_config = acceptance_config(
        host_id,
        &fixture_forward_name(run_id, "remote")?,
        ForwardKind::Remote,
        remote_bind,
        Some(local_http.port),
    )?;
    run_forward_case(
        cx,
        root,
        window,
        capture,
        records,
        services,
        ForwardCase {
            config: remote_config,
            label: "remote",
            probe: ForwardProbe::RemoteHttp {
                bind_port: remote_bind,
                expected_body: "fixture-local-ok",
            },
        },
    )
    .await?;

    verify_client_only_fixture(cx, local_http.port).await?;
    let remote_socks_bind = unused_remote_loopback_port(cx).await?;
    let remote_socks_config = acceptance_config(
        host_id,
        &fixture_forward_name(run_id, "dynamic-remote")?,
        ForwardKind::RemoteDynamic,
        remote_socks_bind,
        None,
    )?;
    run_forward_case(
        cx,
        root,
        window,
        capture,
        records,
        services,
        ForwardCase {
            config: remote_socks_config,
            label: "dynamic-remote",
            probe: ForwardProbe::RemoteSocks {
                bind_port: remote_socks_bind,
                destination_port: local_http.port,
                expected_body: "fixture-local-ok",
            },
        },
    )
    .await?;
    drop(local_http);
    Ok(())
}

/// 记录正式表单实际打开和取消回读，后续各方向仍各自走同一生产 Add 保存路径。
///
/// @author kongweiguang
async fn verify_add_form_cancel(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let dialog_open = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_forward_add(window, root_cx)
        })
    })?;
    if !dialog_open {
        return Err(io::Error::other("SSH 隧道新增表单生产入口未能打开").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "forward-add-dialog",
        "production_local_forward_form_opened_before_cancel",
    )
    .await?;
    verify_forward_form_feedback(cx, root, window, capture, records).await?;
    poll_root(
        cx,
        root,
        window,
        "取消 SSH 隧道新增表单",
        |root, _, cx| {
            Ok(root
                .close_right_rail_acceptance_forward_add(cx)
                .then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "新增表单取消回读",
        |root, _, cx| {
            Ok(root
                .right_rail_acceptance_forward_add_closed(cx)
                .then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "forward-add-cancelled",
        "form_and_root_confirmation_owner_closed_after_cancel",
    )
    .await
}

/// 三主题都通过真实提交按钮触发端口错误，表单保留且 toast 不遮挡底部操作。
///
/// @author kongweiguang
async fn verify_forward_form_feedback(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let input_id = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.right_rail_acceptance_invalid_forward_port(host, window, cx)
        })
    })??;
    let submit_id: ElementId = "forward-start".into();
    let cancel_id: ElementId = "forward-cancel".into();
    for (mode, operation) in [
        (ThemeMode::Light, "forward-form-error-light"),
        (ThemeMode::Dark, "forward-form-error-dark"),
        (ThemeMode::System, "forward-form-error-system"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx);
                root.clear_right_rail_acceptance_forward_form_status(host, cx)
            })
        })??;
        let _ = super::frame_signature(cx, window)?;
        let before = (
            observed_bounds(cx, window, &input_id)?,
            observed_bounds(cx, window, &submit_id)?,
            observed_bounds(cx, window, &cancel_id)?,
        );
        click_observed_control(cx, window, &submit_id)?;
        poll_root(
            cx,
            root,
            window,
            "隧道表单校验错误",
            |root, _, _| {
                root.right_rail_acceptance_forward_form_status(host)
                    .map(|message| message.filter(|message| !message.trim().is_empty()))
            },
        )
        .await?;
        let _ = super::frame_signature(cx, window)?;
        let after = (
            observed_bounds(cx, window, &input_id)?,
            observed_bounds(cx, window, &submit_id)?,
            observed_bounds(cx, window, &cancel_id)?,
        );
        if !same_bounds(before.0, after.0)
            || !same_bounds(before.1, after.1)
            || !same_bounds(before.2, after.2)
        {
            return Err(io::Error::other("表单错误改变了字段或页脚按钮位置").into());
        }
        let toast = window
            .update(cx, |_, window, _| {
                window.debug_selector_bounds("rr-workbench-feedback-toast")
            })?
            .ok_or_else(|| io::Error::other("表单校验后的 toast 不在当前绘制帧"))?;
        if overlaps(toast, after.1) || overlaps(toast, after.2) {
            return Err(io::Error::other("表单错误 toast 遮挡提交或取消按钮").into());
        }
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "invalid_port_kept_form_geometry_and_visible_unobstructed_toast",
        )
        .await?;
    }
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx);
            root.clear_right_rail_acceptance_forward_form_status(host, cx)
        })
    })??;
    Ok(())
}

/// 使用 Kit 的当前帧节点识别真实输入或按钮；不可见节点不能作为布局证据。
///
/// @author kongweiguang
fn observed_bounds(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    id: &ElementId,
) -> Result<Bounds<Pixels>, Box<dyn Error>> {
    window
        .update(cx, |_, window, _| {
            let node = gpui_kit::base::test_support::find(window, &[], id)
                .ok_or_else(|| io::Error::other(format!("缺少验收控件 {id:?}")))?;
            if !node.visible() {
                return Err(io::Error::other(format!("验收控件不可见 {id:?}")));
            }
            Ok(node.bounds())
        })?
        .map_err(Into::into)
}

/// 只向私有窗口发送控件中心点击，确保校验经过真实按钮事件。
///
/// @author kongweiguang
fn click_observed_control(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    id: &ElementId,
) -> Result<(), Box<dyn Error>> {
    let position = observed_bounds(cx, window, id)?.center();
    window.update(cx, |_, window, app| {
        for event in [
            MouseMoveEvent {
                position,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
    })?;
    Ok(())
}

/// 仅容忍浮点绘制舍入，避免把可见布局跳动当作稳定。
///
/// @author kongweiguang
fn same_bounds(first: Bounds<Pixels>, second: Bounds<Pixels>) -> bool {
    (first.origin.x.as_f32() - second.origin.x.as_f32()).abs() < 0.05
        && (first.origin.y.as_f32() - second.origin.y.as_f32()).abs() < 0.05
        && (first.size.width.as_f32() - second.size.width.as_f32()).abs() < 0.05
        && (first.size.height.as_f32() - second.size.height.as_f32()).abs() < 0.05
}

/// 直接比较两矩形交叉，避免通过 toast 所在层级猜测底层按钮是否被遮挡。
///
/// @author kongweiguang
fn overlaps(first: Bounds<Pixels>, second: Bounds<Pixels>) -> bool {
    first.origin.x < second.origin.x + second.size.width
        && second.origin.x < first.origin.x + first.size.width
        && first.origin.y < second.origin.y + second.size.height
        && second.origin.y < first.origin.y + first.size.height
}

/// 为每种类型使用独立名称与配置，并在任何生命周期失败后按同一 ID 清理资源。
///
/// @author kongweiguang
async fn run_forward_case(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    case: ForwardCase,
) -> Result<(), Box<dyn Error>> {
    let ForwardCase {
        config,
        label,
        probe,
    } = case;
    poll_root(
        cx,
        root,
        window,
        "通过生产表单创建 fixture 隧道",
        |root, window, cx| {
            root.create_right_rail_acceptance_forward(&config, window, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    let forward_id = poll_root(
        cx,
        root,
        window,
        "回读 fixture 隧道 ID",
        |root, _, cx| root.right_rail_acceptance_forward_id(&config, cx),
    )
    .await?;
    let fixture = ForwardFixture {
        config,
        forward_id,
        label,
        probe,
    };
    let lifecycle =
        run_forward_lifecycle(cx, root, window, capture, records, services, &fixture).await;
    if let Err(primary_error) = lifecycle {
        if let Err(cleanup_error) = cleanup_forward(cx, root, window, &fixture).await {
            return Err(io::Error::other(format!(
                "{} 隧道验收失败：{primary_error}；清理原 ID 也失败：{cleanup_error}",
                fixture.label
            ))
            .into());
        }
        return Err(primary_error);
    }
    Ok(())
}

/// 保存后验证真实协议、停止及确认删除；远端代理恢复验证必须跨越原定义删除。
///
/// @author kongweiguang
async fn run_forward_lifecycle(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    checkpoint(
        cx,
        window,
        capture,
        records,
        operation_name(fixture.label, "created"),
        "production_snapshot_returned_unique_stopped_forward_id",
    )
    .await?;
    if fixture.label == "dynamic-remote" {
        verify_proxy_unavailable_notice(cx, root, window, capture, records, fixture).await?;
    }
    poll_root(
        cx,
        root,
        window,
        "启动 fixture SSH 隧道",
        |root, _, cx| {
            root.start_right_rail_acceptance_forward(&fixture.config, &fixture.forward_id, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "回读 fixture 隧道活动态",
        |root, _, cx| {
            root.right_rail_acceptance_forward_state_ready(
                &fixture.config,
                &fixture.forward_id,
                ForwardState::Active,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    verify_active_data_plane(cx, fixture).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        operation_name(fixture.label, "active"),
        "real_http_or_socks5_connect_returned_expected_fixture_body",
    )
    .await?;
    let proxy_binding = if fixture.label == "dynamic-remote" {
        Some(verify_proxy_application(cx, root, window, capture, records, services, fixture).await?)
    } else {
        None
    };

    poll_root(
        cx,
        root,
        window,
        "停止 fixture SSH 隧道",
        |root, _, cx| {
            root.stop_right_rail_acceptance_forward(&fixture.config, &fixture.forward_id, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "回读 fixture 隧道停止态",
        |root, _, cx| {
            root.right_rail_acceptance_forward_state_ready(
                &fixture.config,
                &fixture.forward_id,
                ForwardState::Stopped,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    verify_stopped_data_plane(cx, fixture).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        operation_name(fixture.label, "stopped"),
        "same_forward_id_stopped_and_data_plane_refused_connections",
    )
    .await?;

    if fixture.label == "local" {
        open_forward_delete_confirmation(cx, root, window, fixture).await?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            "forward-delete-confirmation",
            "production_delete_confirmation_opened_for_exact_forward",
        )
        .await?;
        poll_root(
            cx,
            root,
            window,
            "取消 fixture 隧道删除确认",
            |root, _, cx| {
                root.cancel_right_rail_acceptance_forward_delete(
                    &fixture.config,
                    &fixture.forward_id,
                    cx,
                )
                .map(|cancelled| cancelled.then_some(()))
            },
        )
        .await?;
        poll_root(
            cx,
            root,
            window,
            "确认取消后原定义仍处于停止态",
            |root, _, cx| {
                if root.right_rail_acceptance_forward_delete_confirmation_ready(
                    &fixture.config,
                    &fixture.forward_id,
                    cx,
                )? {
                    return Ok(None);
                }
                root.right_rail_acceptance_forward_state_ready(
                    &fixture.config,
                    &fixture.forward_id,
                    ForwardState::Stopped,
                    cx,
                )
                .map(|ready| ready.then_some(()))
            },
        )
        .await?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            "forward-delete-cancelled",
            "same_forward_definition_remains_stopped_after_cancel",
        )
        .await?;
    }
    open_forward_delete_confirmation(cx, root, window, fixture).await?;
    poll_root(
        cx,
        root,
        window,
        "删除 fixture SSH 隧道",
        |root, _, cx| {
            root.delete_right_rail_acceptance_forward(&fixture.config, &fixture.forward_id, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "回读 fixture 隧道删除",
        |root, _, cx| {
            root.right_rail_acceptance_forward_deleted(&fixture.config, &fixture.forward_id, cx)
                .map(|deleted| deleted.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        operation_name(fixture.label, "deleted"),
        "production_snapshot_no_longer_contains_fixture_forward_id_or_name",
    )
    .await?;
    if let Some(binding) = proxy_binding {
        verify_proxy_restoration(
            cx,
            root,
            window,
            capture,
            records,
            services,
            (fixture, binding),
        )
        .await?;
    }
    Ok(())
}

/// 停止态的明确拒绝必须产生一次新的工作区提示，不能由旧状态或卡片文案冒充。
///
/// @author kongweiguang
async fn verify_proxy_unavailable_notice(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.clear_right_rail_acceptance_forward_status(&fixture.config, cx)
        })
    })??;
    click_proxy_menu_item(
        cx,
        window,
        fixture,
        &["应用到当前终端", "Apply to current terminal"],
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "停止态代理错误提示",
        |root, _, _| {
            root.right_rail_acceptance_forward_status(&fixture.config)
                .map(|message| message.filter(|message| !message.trim().is_empty()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "forward-proxy-error-toast",
        "stopped_proxy_action_reported_in_workbench_feedback",
    )
    .await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.clear_right_rail_acceptance_forward_status(&fixture.config, cx)
        })
    })??;
    Ok(())
}

/// 在相同 SSH pane 设置互不相同的原值，再由生产菜单应用并用真实 shell 环境验证。
///
/// @author kongweiguang
async fn verify_proxy_application(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    fixture: &ForwardFixture,
) -> Result<UiSessionBinding, Box<dyn Error>> {
    let binding = wait_proxy_state(
        cx,
        root,
        window,
        fixture,
        ProxyCurrentApplication::NotApplied,
        None,
    )
    .await?;
    run_fixture_shell(
        services,
        &binding,
        "seed",
        "export ALL_PROXY='rr-original-upper' all_proxy='rr-original-lower'",
    )
    .await?;
    refresh_proxy_projection(cx, root, window, fixture)?;
    let fresh = wait_proxy_state(
        cx,
        root,
        window,
        fixture,
        ProxyCurrentApplication::NotApplied,
        None,
    )
    .await?;
    if fresh != binding {
        return Err(io::Error::other("设置原始环境期间 fixture 终端已换代").into());
    }

    for (mode, name, operation) in [
        (ThemeMode::Light, "light", "forward-proxy-light"),
        (ThemeMode::Dark, "dark", "forward-proxy-dark"),
        (ThemeMode::System, "system", "forward-proxy-system"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            })
        })?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "active_remote_socks_panel_in_selected_theme",
        )
        .await?;
        super::menus::capture_menu(
            cx,
            window,
            &capture.output_directory,
            name,
            "remote SOCKS menu",
            "forward-proxy",
            &format!("rr-forward-more-{}", fixture.forward_id),
        )
        .await?;
    }
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx)
        })
    })?;
    click_proxy_menu_item(
        cx,
        window,
        fixture,
        &["应用到当前终端", "Apply to current terminal"],
    )
    .await?;
    let applied = wait_proxy_state(
        cx,
        root,
        window,
        fixture,
        ProxyCurrentApplication::Applied,
        Some(&fixture.forward_id),
    )
    .await;
    if applied.is_err() {
        checkpoint(
            cx,
            window,
            capture,
            records,
            "forward-proxy-apply-failed",
            "fixture_terminal_after_proxy_application_timeout",
        )
        .await?;
        let endpoint = format!(
            "socks5h://{}:{}",
            fixture.config.bind.host, fixture.config.bind.port
        );
        let check = format!(
            "[ \"$(printenv ALL_PROXY)\" = '{endpoint}' ] && [ \"$(printenv all_proxy)\" = '{endpoint}' ]"
        );
        let actual = run_fixture_shell(services, &binding, "diagnostic-applied", &check).await;
        eprintln!("fixture shell proxy values after timeout: {actual:?}");
    }
    let applied = applied?;
    if applied != binding {
        return Err(io::Error::other("代理应用到了不同的 fixture 终端").into());
    }
    let endpoint = format!(
        "socks5h://{}:{}",
        fixture.config.bind.host, fixture.config.bind.port
    );
    let check = format!(
        "[ \"$(printenv ALL_PROXY)\" = '{endpoint}' ] && [ \"$(printenv all_proxy)\" = '{endpoint}' ]"
    );
    run_fixture_shell(services, &binding, "applied", &check).await?;
    let text = window.update(cx, |_, _, app| {
        root.update(app, |root, _| {
            root.right_rail_acceptance_forward_terminal_text(&fixture.config)
        })
    })??;
    if text.is_some_and(|text| text.contains("\\137\\137\\113\\105\\122")) {
        return Err(io::Error::other("终端仍显示长串八进制代理命令").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "forward-proxy-applied",
        "same_fixture_shell_exports_both_actual_socks_proxy_values",
    )
    .await?;
    capture_proxy_result_themes(cx, root, window, capture, records, fixture, true).await?;
    verify_refresh_stability(
        cx,
        root,
        window,
        capture,
        records,
        fixture,
        (&binding, false),
    )
    .await?;
    Ok(binding)
}

/// 隧道删除后仍需在原 pane 显式恢复，并用子进程环境验证大小写两项原值均已恢复。
///
/// @author kongweiguang
async fn verify_proxy_restoration(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    target: (&ForwardFixture, UiSessionBinding),
) -> Result<(), Box<dyn Error>> {
    let (fixture, binding) = target;
    let endpoint = format!(
        "socks5h://{}:{}",
        fixture.config.bind.host, fixture.config.bind.port
    );
    let still_applied = format!(
        "[ \"$(printenv ALL_PROXY)\" = '{endpoint}' ] && [ \"$(printenv all_proxy)\" = '{endpoint}' ]"
    );
    run_fixture_shell(services, &binding, "after-delete", &still_applied).await?;
    refresh_proxy_projection(cx, root, window, fixture)?;
    let current = wait_proxy_state(
        cx,
        root,
        window,
        fixture,
        ProxyCurrentApplication::Applied,
        Some(&fixture.forward_id),
    )
    .await?;
    if current != binding {
        return Err(io::Error::other("删除后代理恢复绑定发生变化").into());
    }
    verify_refresh_stability(
        cx,
        root,
        window,
        capture,
        records,
        fixture,
        (&binding, true),
    )
    .await?;
    let _ = super::frame_signature(cx, window)?;
    super::menus::click_selector(cx, window, "rr-forward-orphan-recovery")?;
    let restored = wait_proxy_state(
        cx,
        root,
        window,
        fixture,
        ProxyCurrentApplication::NotApplied,
        None,
    )
    .await?;
    if restored != binding {
        return Err(io::Error::other("代理恢复到了不同的 fixture 终端").into());
    }
    run_fixture_shell(services, &binding, "restored", "[ \"$(printenv ALL_PROXY)\" = 'rr-original-upper' ] && [ \"$(printenv all_proxy)\" = 'rr-original-lower' ]").await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "forward-proxy-restored-after-delete",
        "original_fixture_shell_environment_restored_after_forward_deletion",
    )
    .await?;
    capture_proxy_result_themes(cx, root, window, capture, records, fixture, false).await?;
    Ok(())
}

/// 同一终端和同一转发状态逐个切换主题；恢复后的原行已删除，故不伪造菜单截图。
///
/// @author kongweiguang
async fn capture_proxy_result_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    fixture: &ForwardFixture,
    applied: bool,
) -> Result<(), Box<dyn Error>> {
    let themes = if applied {
        [
            (ThemeMode::Light, "light", "forward-applied-light"),
            (ThemeMode::Dark, "dark", "forward-applied-dark"),
            (ThemeMode::System, "system", "forward-applied-system"),
        ]
    } else {
        [
            (ThemeMode::Light, "light", "forward-restored-light"),
            (ThemeMode::Dark, "dark", "forward-restored-dark"),
            (ThemeMode::System, "system", "forward-restored-system"),
        ]
    };
    for (mode, name, operation) in themes {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            })
        })?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            if applied {
                "proxy_applied_to_fixture_terminal"
            } else {
                "original_proxy_environment_restored"
            },
        )
        .await?;
        if applied {
            super::menus::capture_menu(
                cx,
                window,
                &capture.output_directory,
                name,
                "applied remote SOCKS menu",
                "forward-proxy-applied",
                &format!("rr-forward-more-{}", fixture.forward_id),
            )
            .await?;
        }
    }
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx)
        })
    })?;
    Ok(())
}

/// 点击真实刷新并立即重读代理门禁；原恢复动作在读取中只禁用，不丢失绑定或移动位置。
///
/// @author kongweiguang
async fn verify_refresh_stability(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    fixture: &ForwardFixture,
    target: (&UiSessionBinding, bool),
) -> Result<(), Box<dyn Error>> {
    let (binding, orphan) = target;
    poll_root(
        cx,
        root,
        window,
        "刷新前代理动作就绪",
        |root, _, cx| {
            let loading =
                root.right_rail_acceptance_forward_loading(&fixture.config.host_id, cx)?;
            let action = root.right_rail_acceptance_proxy_action(
                &fixture.config,
                &fixture.forward_id,
                binding,
                cx,
            )?;
            Ok((!loading && action == (false, true)).then_some(()))
        },
    )
    .await?;
    let selector = if orphan {
        "rr-forward-orphan-recovery".to_owned()
    } else {
        format!("rr-forward-more-{}", fixture.forward_id)
    };
    let _ = super::frame_signature(cx, window)?;
    let path = capture.output_directory.join(if orphan {
        "forward-orphan-refresh-loading.png"
    } else {
        "forward-refresh-loading.png"
    });
    window.update(cx, |_, window, app| {
        let before_refresh = window
            .debug_selector_bounds("rr-forwards-refresh")
            .ok_or_else(|| io::Error::other("缺少隧道刷新按钮"))?;
        let before_action = window
            .debug_selector_bounds(&selector)
            .ok_or_else(|| io::Error::other("刷新前缺少原代理动作"))?;
        let orphan_id: ElementId = "operations-forward-orphan-recovery".into();
        if orphan {
            let node = gpui_kit::base::test_support::find(window, &[], &orphan_id)
                .ok_or_else(|| io::Error::other("刷新前恢复按钮未绘制"))?;
            if !node.supports_click() {
                return Err(io::Error::other("刷新前恢复按钮不可点击"));
            }
        }
        let position = before_refresh.center();
        for event in [
            MouseMoveEvent {
                position,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        let (loading, action) = root
            .update(app, |root, cx| {
                let loading =
                    root.right_rail_acceptance_forward_loading(&fixture.config.host_id, cx)?;
                root.refresh_right_rail_acceptance_proxy(&fixture.config, cx)?;
                let action = root.right_rail_acceptance_proxy_action(
                    &fixture.config,
                    &fixture.forward_id,
                    binding,
                    cx,
                )?;
                Ok::<_, String>((loading, action))
            })
            .map_err(io::Error::other)?;
        if !loading || action != (true, true) {
            return Err(io::Error::other(format!(
                "刷新未保留原恢复动作与忙碌门禁：loading={loading}, action={action:?}"
            )));
        }
        let clear = window.draw(app);
        clear.clear(app);
        let after_refresh = window
            .debug_selector_bounds("rr-forwards-refresh")
            .ok_or_else(|| io::Error::other("刷新期间按钮消失"))?;
        let after_action = window
            .debug_selector_bounds(&selector)
            .ok_or_else(|| io::Error::other("刷新期间原代理动作消失"))?;
        if !same_bounds(before_refresh, after_refresh) || !same_bounds(before_action, after_action)
        {
            return Err(io::Error::other("刷新期间按钮或原代理动作发生位移"));
        }
        if orphan {
            let node = gpui_kit::base::test_support::find(window, &[], &orphan_id)
                .ok_or_else(|| io::Error::other("刷新期间恢复按钮未绘制"))?;
            // Kit's AccessKit node omits the disabled flag; its Click action is
            // the observable distinction between an enabled and disabled Button.
            if node.supports_click() {
                return Err(io::Error::other(format!(
                    "刷新期间恢复按钮仍支持点击：disabled={:?}",
                    node.disabled()
                )));
            }
            let position = after_action.center();
            for event in [
                MouseMoveEvent {
                    position,
                    ..MouseMoveEvent::default()
                }
                .to_platform_input(),
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..MouseDownEvent::default()
                }
                .to_platform_input(),
                MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..MouseUpEvent::default()
                }
                .to_platform_input(),
            ] {
                let _ = window.dispatch_event(event, app);
            }
            let action = root
                .update(app, |root, cx| {
                    root.right_rail_acceptance_proxy_action(
                        &fixture.config,
                        &fixture.forward_id,
                        binding,
                        cx,
                    )
                })
                .map_err(io::Error::other)?;
            if action != (true, true) {
                return Err(io::Error::other(format!(
                    "刷新期间点击改变了原恢复事务：action={action:?}"
                )));
            }
        }
        window
            .render_to_image()
            .map_err(io::Error::other)?
            .save(&path)
            .map_err(io::Error::other)?;
        Ok::<_, io::Error>(())
    })??;
    poll_root(cx, root, window, "原代理刷新回执", |root, _, cx| {
        let loading = root.right_rail_acceptance_forward_loading(&fixture.config.host_id, cx)?;
        let action = root.right_rail_acceptance_proxy_action(
            &fixture.config,
            &fixture.forward_id,
            binding,
            cx,
        )?;
        Ok((!loading && action == (false, true)).then_some(()))
    })
    .await?;
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, _| {
        window
            .debug_selector_bounds(&selector)
            .ok_or_else(|| io::Error::other("刷新回执后原代理动作消失"))
    })??;
    checkpoint(
        cx,
        window,
        capture,
        records,
        if orphan {
            "forward-orphan-refresh-stable"
        } else {
            "forward-proxy-refresh-stable"
        },
        "same_binding_recovery_action_preserved_during_and_after_refresh",
    )
    .await?;
    Ok(())
}

/// 只读状态必须同时满足提示符就绪、指定应用态和原转发身份；超时保留最近快照以区分执行与提示符问题。
///
/// @author kongweiguang
async fn wait_proxy_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: &ForwardFixture,
    application: ProxyCurrentApplication,
    expected_recovery: Option<&str>,
) -> Result<UiSessionBinding, Box<dyn Error>> {
    let mut latest = String::new();
    let result = poll_root(
        cx,
        root,
        window,
        "代理回读和终端提示符",
        |root, _, cx| {
            let state =
                root.right_rail_acceptance_proxy_session(&fixture.config, &fixture.forward_id, cx)?;
            let status = root.right_rail_acceptance_forward_status(&fixture.config)?;
            latest = format!("state={state:?}, status={status:?}");
            Ok(state.and_then(|(session, recovery, _)| {
                (session.application == application
                    && session.availability == ProxyCurrentAvailability::Ready
                    && recovery.as_deref() == expected_recovery)
                    .then_some(session.binding)
            }))
        },
    )
    .await;
    result.map_err(|error| {
        io::Error::other(format!("{error}; expected={application:?}, last={latest}")).into()
    })
}

/// 主动更新当前 panel 后等待权威快照，避免测试自身的只读 shell 命令留下旧提示符投影。
///
/// @author kongweiguang
fn refresh_proxy_projection(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.refresh_right_rail_acceptance_proxy(&fixture.config, cx)
        })
    })??;
    Ok(())
}

/// 在精确 UI binding 上执行 fixture 命令并等待新提示符；只重试注入前的明确拒绝，未知结果绝不重放。
///
/// @author kongweiguang
async fn run_fixture_shell(
    services: &AppServices,
    binding: &UiSessionBinding,
    label: &'static str,
    command: &str,
) -> Result<(), Box<dyn Error>> {
    let sessions = Arc::clone(&services.sessions);
    let binding = binding.clone();
    let command = command.to_owned();
    let outcome = services
        .executor
        .run(move |cancellation| async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            let (epoch, outcome) = loop {
                let epoch =
                    wait_fixture_prompt(sessions.as_ref(), &binding, None, deadline, &cancellation)
                        .await?;
                let request = ShellCommandRequest {
                    session_id: binding.session_id.clone(),
                    expected_ui_binding: Some(binding.clone()),
                    command_id: format!("rr-proxy-{label}"),
                    command: command.clone(),
                    timeout: Duration::from_secs(20),
                };
                match sessions
                    .execute_shell_command(request, cancellation.clone())
                    .await
                {
                    Ok(outcome) => break (epoch, outcome),
                    Err(error)
                        if matches!(
                            error.public_code(),
                            kerminal_application::PublicErrorCode::Conflict
                                | kerminal_application::PublicErrorCode::CapabilityNotSupported
                        ) && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            if matches!(outcome, ShellCommandOutcome::Finished { exit_code: 0, .. }) {
                wait_fixture_prompt(
                    sessions.as_ref(),
                    &binding,
                    Some(epoch),
                    deadline,
                    &cancellation,
                )
                .await?;
            }
            Ok(outcome)
        })
        .await?;
    if !matches!(outcome, ShellCommandOutcome::Finished { exit_code: 0, .. }) {
        return Err(
            io::Error::other(format!("fixture shell {label} 校验失败：{outcome:?}")).into(),
        );
    }
    Ok(())
}

/// 完成回执早于下个 OSC 133 提示符时等待新 epoch，期间始终验证原会话身份和截止时间。
///
/// @author kongweiguang
async fn wait_fixture_prompt(
    sessions: &dyn kerminal_application::SessionService,
    binding: &UiSessionBinding,
    previous_epoch: Option<u64>,
    deadline: tokio::time::Instant,
    cancellation: &tokio_util::sync::CancellationToken,
) -> kerminal_application::AppResult<u64> {
    loop {
        if cancellation.is_cancelled() {
            return Err(kerminal_application::AppError::cancelled(
                "fixture shell probe cancelled",
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(kerminal_application::AppError::timeout(
                "fixture shell prompt",
            ));
        }
        let context = sessions
            .terminal_command_context(binding.session_id.clone())
            .await?;
        if context.session_id != binding.session_id
            || context.owner != binding.owner
            || context.session_generation != binding.session_generation
        {
            return Err(kerminal_application::AppError::conflict(
                "fixture shell binding changed",
            ));
        }
        if context.availability == TerminalCommandAvailability::Available
            && context.prompt_ready
            && context.prompt_line_empty
            && !context.alternate_screen
            && previous_epoch.is_none_or(|epoch| context.prompt_epoch > epoch)
        {
            return Ok(context.prompt_epoch);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 通过真实菜单项的无障碍标签和窗口内鼠标事件触发，防止测试绕过产品菜单接线。
///
/// @author kongweiguang
async fn click_proxy_menu_item(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    fixture: &ForwardFixture,
    expected_labels: &[&str],
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    super::menus::click_selector(
        cx,
        window,
        &format!("rr-forward-more-{}", fixture.forward_id),
    )?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = super::frame_signature(cx, window)?;
        let result = window.update(cx, |_, window, app| {
            let items = gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .filter(|node| node.role() == Some(gpui_pre::Role::MenuItem))
                .collect::<Vec<_>>();
            if items
                .iter()
                .filter_map(|node| node.label())
                .any(|label| label.contains("上次操作") || label.contains("Last operation"))
            {
                return Err(io::Error::other("代理菜单仍包含上次操作提示"));
            }
            let Some(item) = items.iter().find(|node| {
                node.label()
                    .is_some_and(|label| expected_labels.contains(&label))
            }) else {
                return Ok(false);
            };
            if item.disabled() == Some(true) || !item.visible() {
                return Err(io::Error::other(format!(
                    "代理菜单项不可用：{expected_labels:?}"
                )));
            }
            let position = item.bounds().center();
            for event in [
                MouseMoveEvent {
                    position,
                    ..MouseMoveEvent::default()
                }
                .to_platform_input(),
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..MouseDownEvent::default()
                }
                .to_platform_input(),
                MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..MouseUpEvent::default()
                }
                .to_platform_input(),
            ] {
                let _ = window.dispatch_event(event, app);
            }
            Ok(true)
        })??;
        if result {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other(format!("等待代理菜单项超时：{expected_labels:?}")).into(),
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

/// 打开或复用原 fixture 的生产删除确认，并等待对应 Root 弹层挂载。
///
/// @author kongweiguang
async fn open_forward_delete_confirmation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    let already_open = window
        .update(cx, |_, _, app| {
            root.update(app, |root, root_cx| {
                root.right_rail_acceptance_forward_delete_confirmation_ready(
                    &fixture.config,
                    &fixture.forward_id,
                    root_cx,
                )
            })
        })?
        .map_err(io::Error::other)?;
    if !already_open {
        poll_root(
            cx,
            root,
            window,
            "打开 fixture 隧道删除确认",
            |root, _, cx| {
                root.open_right_rail_acceptance_forward_delete(
                    &fixture.config,
                    &fixture.forward_id,
                    cx,
                )
                .map(|opened| opened.then_some(()))
            },
        )
        .await?;
    }
    poll_root(
        cx,
        root,
        window,
        "回读 fixture 隧道删除确认",
        |root, _, cx| {
            root.right_rail_acceptance_forward_delete_confirmation_ready(
                &fixture.config,
                &fixture.forward_id,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 异常后先回读删除/运行状态，再按原 ID 停止失败或活动资源并确认删除，避免重放未知请求。
///
/// @author kongweiguang
async fn cleanup_forward(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    let readback = poll_root(
        cx,
        root,
        window,
        "清理前回读 fixture 隧道状态",
        |root, _, cx| {
            if root.right_rail_acceptance_forward_deleted(
                &fixture.config,
                &fixture.forward_id,
                cx,
            )? {
                return Ok(Some(CleanupReadback::Deleted));
            }
            Ok(root
                .right_rail_acceptance_forward_state(&fixture.config, &fixture.forward_id, cx)?
                .map(CleanupReadback::State))
        },
    )
    .await?;
    let CleanupReadback::State(mut state) = readback else {
        return Ok(());
    };
    if state == ForwardState::Starting {
        state = poll_root(
            cx,
            root,
            window,
            "等待失败路径中的启动操作收敛",
            |root, _, cx| {
                Ok(root
                    .right_rail_acceptance_forward_state(&fixture.config, &fixture.forward_id, cx)?
                    .filter(|state| *state != ForwardState::Starting)
                    .map(CleanupReadback::State))
            },
        )
        .await?
        .into_state()?;
    }

    if matches!(
        state,
        ForwardState::Active | ForwardState::Failed | ForwardState::Closing
    ) {
        poll_root(
            cx,
            root,
            window,
            "失败后停止原 fixture 隧道",
            |root, _, cx| {
                root.stop_right_rail_acceptance_forward(&fixture.config, &fixture.forward_id, cx)
                    .map(|started| started.then_some(()))
            },
        )
        .await?;
        poll_root(
            cx,
            root,
            window,
            "确认失败路径中的隧道停止",
            |root, _, cx| {
                root.right_rail_acceptance_forward_state_ready(
                    &fixture.config,
                    &fixture.forward_id,
                    ForwardState::Stopped,
                    cx,
                )
                .map(|stopped| stopped.then_some(()))
            },
        )
        .await?;
    } else if state != ForwardState::Stopped {
        return Err(io::Error::other("fixture 隧道状态未知；拒绝尝试删除").into());
    }

    open_forward_delete_confirmation(cx, root, window, fixture).await?;
    poll_root(
        cx,
        root,
        window,
        "失败后删除原 fixture 隧道",
        |root, _, cx| {
            root.delete_right_rail_acceptance_forward(&fixture.config, &fixture.forward_id, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "确认失败路径中的隧道删除",
        |root, _, cx| {
            root.right_rail_acceptance_forward_deleted(&fixture.config, &fixture.forward_id, cx)
                .map(|deleted| deleted.then_some(()))
        },
    )
    .await?;
    Ok(())
}

impl CleanupReadback {
    /// 删除已确认或状态已收敛后才继续清理；仍缺少回读时不猜测并继续写入。
    ///
    /// @author kongweiguang
    fn into_state(self) -> Result<ForwardState, io::Error> {
        match self {
            Self::State(state) => Ok(state),
            Self::Deleted => Err(io::Error::other("fixture 隧道已删除；不应继续状态清理")),
        }
    }
}

/// 按转发方向执行实际 HTTP 或 SOCKS5 CONNECT，不以管理快照状态代替数据面成功。
///
/// @author kongweiguang
async fn verify_active_data_plane(
    cx: &mut AsyncApp,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    match fixture.probe {
        ForwardProbe::LocalHttp { bind_port } => {
            verify_local_http(cx, bind_port, "fixture-ok").await
        }
        ForwardProbe::LocalSocks {
            bind_port,
            destination_port,
        } => verify_socks_http(cx, bind_port, destination_port, "fixture-ok").await,
        ForwardProbe::RemoteHttp {
            bind_port,
            expected_body,
        } => verify_wsl_http(cx, bind_port, expected_body).await,
        ForwardProbe::RemoteSocks {
            bind_port,
            destination_port,
            expected_body,
        } => {
            verify_wsl_listener_open(cx, bind_port).await?;
            verify_wsl_socks_http(cx, bind_port, destination_port, expected_body).await
        }
    }
}

/// 先证明本机 fixture 正常且 WSL 不能直接连接同一回环端口，避免共享 localhost 掩盖错误出口。
///
/// @author kongweiguang
async fn verify_client_only_fixture(cx: &mut AsyncApp, port: u16) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move {
            read_http_at(port, "fixture-local-ok")?;
            wsl_listener_probe(port, false).map_err(|error| {
                io::Error::other(format!("本机 fixture 不能证明与 SSH 侧隔离：{error}"))
            })
        })
        .await?;
    Ok(())
}

/// 停止后本机 listener 必须拒绝 TCP；远端 listener 必须从 fixture WSL 清单和 loopback 消失。
///
/// @author kongweiguang
async fn verify_stopped_data_plane(
    cx: &mut AsyncApp,
    fixture: &ForwardFixture,
) -> Result<(), Box<dyn Error>> {
    match fixture.probe {
        ForwardProbe::LocalHttp { bind_port } | ForwardProbe::LocalSocks { bind_port, .. } => {
            verify_local_port_refused(cx, bind_port).await
        }
        ForwardProbe::RemoteHttp { bind_port, .. }
        | ForwardProbe::RemoteSocks { bind_port, .. } => {
            verify_wsl_listener_closed(cx, bind_port).await
        }
    }
}

/// 在 GPUI background executor 中有限重试本机固定目标，并核对 HTTP 200 和 fixture 正文。
///
/// @author kongweiguang
async fn verify_local_http(
    cx: &mut AsyncApp,
    bind_port: u16,
    expected_body: &'static str,
) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(
            async move { retry_until_fixture_response(|| read_http_at(bind_port, expected_body)) },
        )
        .await?;
    Ok(())
}

/// 通过 Windows loopback 进入已由 WSL `ss` 确认的远端 SOCKS listener，再建立真实 SOCKS5 CONNECT。
///
/// @author kongweiguang
async fn verify_socks_http(
    cx: &mut AsyncApp,
    socks_port: u16,
    destination_port: u16,
    expected_body: &'static str,
) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move {
            retry_until_fixture_response(|| {
                let mut stream = connect_loopback(socks_port)?;
                socks5_connect(&mut stream, destination_port)?;
                write_http_request(&mut stream)?;
                verify_http_response(&mut stream, expected_body)
            })
        })
        .await?;
    Ok(())
}

/// 从 SSH fixture 一侧发起 SOCKS CONNECT，确保数据真正经过远端监听器返回本机目标。
///
/// @author kongweiguang
async fn verify_wsl_socks_http(
    cx: &mut AsyncApp,
    socks_port: u16,
    destination_port: u16,
    expected_body: &'static str,
) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move {
            retry_until_fixture_response(|| {
                wsl_socks_http_probe(socks_port, destination_port, expected_body)
            })
        })
        .await?;
    Ok(())
}

/// 在隔离 fixture WSL 内读取远端 HTTP 监听的实际响应，验证 -R 的回程目标也能提供正文。
///
/// @author kongweiguang
async fn verify_wsl_http(
    cx: &mut AsyncApp,
    bind_port: u16,
    expected_body: &'static str,
) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move {
            retry_until_fixture_response(|| wsl_http_probe(bind_port, expected_body))
        })
        .await?;
    Ok(())
}

/// 在 fixture WSL 清单和 loopback 均确认远端 SOCKS listener 已开放后才开始协议探测。
///
/// @author kongweiguang
async fn verify_wsl_listener_open(cx: &mut AsyncApp, bind_port: u16) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move { retry_until_fixture_response(|| wsl_listener_probe(bind_port, true)) })
        .await?;
    Ok(())
}

/// 用同一 loopback 连接路径确认停止的本机固定或 SOCKS listener 不再接受连接。
///
/// @author kongweiguang
async fn verify_local_port_refused(
    cx: &mut AsyncApp,
    bind_port: u16,
) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move { expect_local_connection_refused(bind_port) })
        .await?;
    Ok(())
}

/// 通过 fixture WSL `ss` 和实际 /dev/tcp 尝试，确认停止的 -R listener 不可连接。
///
/// @author kongweiguang
async fn verify_wsl_listener_closed(
    cx: &mut AsyncApp,
    bind_port: u16,
) -> Result<(), Box<dyn Error>> {
    cx.background_executor()
        .spawn(async move { retry_until_listener_closed(bind_port) })
        .await?;
    Ok(())
}

/// 每次保存独立临时端口候选，并让 fixture WSL `ss` 确认没有远端 listener 占用。
///
/// @author kongweiguang
async fn unused_remote_loopback_port(cx: &mut AsyncApp) -> Result<u16, Box<dyn Error>> {
    for _ in 0..32 {
        let candidate = unused_loopback_port()?;
        let occupied = cx
            .background_executor()
            .spawn(async move { wsl_listener_is_open(candidate) })
            .await?;
        if !occupied {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("无法在 fixture WSL 找到未占用的远端 loopback 端口").into())
}

/// 使用操作系统临时 loopback 端口，关闭探测 socket 后由生产隧道或本地 fixture 接管。
///
/// @author kongweiguang
fn unused_loopback_port() -> Result<u16, io::Error> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    Ok(listener.local_addr()?.port())
}

/// 将四种类型的 fixture 端点规范为产品领域配置，动态模式不携带固定 destination。
///
/// @author kongweiguang
fn acceptance_config(
    host_id: &HostId,
    name: &str,
    kind: ForwardKind,
    bind_port: u16,
    destination_port: Option<u16>,
) -> Result<PortForwardConfig, io::Error> {
    let dynamic = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
    let remote = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
    if bind_port == 0 || dynamic == destination_port.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture 隧道端口或 destination 与模式不匹配",
        ));
    }
    let destination = destination_port
        .map(|port| ForwardEndpoint::new("127.0.0.1", port))
        .transpose()
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let config = PortForwardConfig {
        host_id: host_id.clone(),
        name: name.to_owned(),
        kind,
        bind: ForwardEndpoint::new("127.0.0.1", bind_port)
            .map_err(|error| io::Error::other(format!("{error:?}")))?,
        destination,
        proxy_protocol: dynamic.then_some(ProxyProtocol::Socks5),
        remote_access_scope: remote.then_some(RemoteAccessScope::Loopback),
        proxy_apply_scope: ProxyApplyScope::None,
    };
    config
        .validate()
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    Ok(config)
}

/// 把输出目录 run ID 和方向名压成稳定、唯一且不超过产品长度限制的验收名称。
///
/// @author kongweiguang
fn fixture_forward_name(run_id: &str, label: &str) -> Result<String, io::Error> {
    let suffix = run_id
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let suffix = suffix.trim_matches('-');
    if suffix.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "验收输出目录名称不能生成有效隧道 ID 后缀",
        ));
    }
    let name = format!("rr-ui-{suffix}-{label}");
    if name.len() > 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "验收隧道名称超过产品长度限制",
        ));
    }
    Ok(name)
}

/// 方向和阶段都使用固定文件名，保持 JSON evidence 可稳定追溯到具体生命周期步骤。
///
/// @author kongweiguang
fn operation_name(label: &'static str, stage: &'static str) -> &'static str {
    match (label, stage) {
        ("local", "created") => "forward-local-created",
        ("local", "active") => "forward-local-active",
        ("local", "stopped") => "forward-local-stopped",
        ("local", "deleted") => "forward-local-deleted",
        ("dynamic-local", "created") => "forward-dynamic-local-created",
        ("dynamic-local", "active") => "forward-dynamic-local-active",
        ("dynamic-local", "stopped") => "forward-dynamic-local-stopped",
        ("dynamic-local", "deleted") => "forward-dynamic-local-deleted",
        ("remote", "created") => "forward-remote-created",
        ("remote", "active") => "forward-remote-active",
        ("remote", "stopped") => "forward-remote-stopped",
        ("remote", "deleted") => "forward-remote-deleted",
        ("dynamic-remote", "created") => "forward-dynamic-remote-created",
        ("dynamic-remote", "active") => "forward-dynamic-remote-active",
        ("dynamic-remote", "stopped") => "forward-dynamic-remote-stopped",
        ("dynamic-remote", "deleted") => "forward-dynamic-remote-deleted",
        _ => "forward-unknown-stage",
    }
}

/// 只连接本机 IPv4 loopback，连接错误不改投其它地址或已关闭的端口。
///
/// @author kongweiguang
fn connect_loopback(port: u16) -> Result<TcpStream, io::Error> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = TcpStream::connect_timeout(&address, FIXTURE_CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(FIXTURE_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(FIXTURE_READ_TIMEOUT))?;
    Ok(stream)
}

/// 在所选 loopback listener 上读取完整 HTTP/1.x 响应并要求固定 fixture 正文完全匹配。
///
/// @author kongweiguang
fn read_http_at(port: u16, expected_body: &str) -> Result<(), io::Error> {
    let mut stream = connect_loopback(port)?;
    write_http_request(&mut stream)?;
    verify_http_response(&mut stream, expected_body)
}

/// 发送关闭连接的 HTTP/1.0 请求，使固定 fixture 服务在读完正文后释放整个 channel。
///
/// @author kongweiguang
fn write_http_request(stream: &mut TcpStream) -> Result<(), io::Error> {
    stream.write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
}

/// 检查真实 HTTP 状态行、头体分隔和固定正文，不把 TCP 建连成功当作隧道成功。
///
/// @author kongweiguang
fn verify_http_response(stream: &mut TcpStream, expected_body: &str) -> Result<(), io::Error> {
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8_lossy(&response);
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "fixture HTTP 响应缺少头部边界")
    })?;
    if !headers.lines().next().is_some_and(|status| {
        status.starts_with("HTTP/1.") && status.split_whitespace().nth(1) == Some("200")
    }) || body.trim() != expected_body
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("隧道没有返回预期的 fixture HTTP 200 {expected_body} 响应"),
        ));
    }
    Ok(())
}

/// 执行无认证 SOCKS5 greeting 与 IPv4 CONNECT，再请求唯一 fixture HTTP 端点。
///
/// @author kongweiguang
fn socks5_connect(stream: &mut TcpStream, destination_port: u16) -> Result<(), io::Error> {
    stream.write_all(&[0x05, 0x01, 0x00])?;
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting)?;
    if greeting != [0x05, 0x00] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 listener 未接受无认证方式",
        ));
    }

    let [port_high, port_low] = destination_port.to_be_bytes();
    stream.write_all(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, port_high, port_low])?;
    let mut reply = [0_u8; 4];
    stream.read_exact(&mut reply)?;
    if reply[0] != 0x05 || reply[1] != 0x00 || reply[2] != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5 CONNECT 返回失败码 {:02x}", reply[1]),
        ));
    }
    match reply[3] {
        0x01 => {
            let mut endpoint = [0_u8; 6];
            stream.read_exact(&mut endpoint)?;
        }
        0x03 => {
            let mut length = [0_u8; 1];
            stream.read_exact(&mut length)?;
            let mut endpoint = vec![0_u8; usize::from(length[0]) + 2];
            stream.read_exact(&mut endpoint)?;
        }
        0x04 => {
            let mut endpoint = [0_u8; 18];
            stream.read_exact(&mut endpoint)?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SOCKS5 CONNECT 返回未知地址类型",
            ));
        }
    }
    Ok(())
}

/// 对瞬时启动延迟进行有界重试；协议错误、HTTP 错误和 fixture 正文不匹配仍会最终失败。
///
/// @author kongweiguang
fn retry_until_fixture_response(
    mut probe: impl FnMut() -> Result<(), io::Error>,
) -> Result<(), io::Error> {
    let deadline = Instant::now() + FIXTURE_RESPONSE_TIMEOUT;
    loop {
        let last_error = match probe() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        thread::sleep(FIXTURE_RETRY_INTERVAL);
    }
}

/// 本机 listener 停止后必须明确拒绝 loopback 连接，超时或仍可连通均不作为关闭证据。
/// Windows 对空 loopback 端口可能约两秒才报告拒连，关闭探测单独保留三秒预算。
///
/// @author kongweiguang
fn expect_local_connection_refused(port: u16) -> Result<(), io::Error> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match TcpStream::connect_timeout(&address, STOPPED_CONNECT_TIMEOUT) {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("停止后的本机隧道端口未明确拒绝连接：{error}"),
        )),
        Ok(_) => Err(io::Error::other("隧道停止回读后本机端口仍接受 TCP 连接")),
    }
}

/// 在隔离 fixture WSL 上以 `ss` 检查唯一端口 listener，不扫描或返回其它端口信息。
///
/// @author kongweiguang
fn wsl_listener_is_open(port: u16) -> Result<bool, io::Error> {
    let script = format!(
        r#"if output=$(ss -H -ltn 'sport = :{port}'); then
    if [[ -n "$output" ]]; then printf RR_PORT_USED; else printf RR_PORT_FREE; fi
else
    exit 70
fi"#
    );
    let output = run_fixture_wsl(&script)?;
    if output.contains("RR_PORT_USED") {
        Ok(true)
    } else if output.contains("RR_PORT_FREE") {
        Ok(false)
    } else {
        Err(io::Error::other("fixture WSL 没有返回远端端口占用结果"))
    }
}

/// 在 fixture WSL loopback 上确认远端 listener 能接收连接，随后关闭探针 socket。
///
/// @author kongweiguang
fn wsl_listener_probe(port: u16, should_be_open: bool) -> Result<(), io::Error> {
    let script = format!(
        "set -e\nif exec 3<>/dev/tcp/127.0.0.1/{port} 2>/dev/null; then printf RR_PORT_OPEN; exec 3<&-; exec 3>&-; else printf RR_PORT_CLOSED; fi"
    );
    let output = run_fixture_wsl(&script)?;
    if (should_be_open && output.contains("RR_PORT_OPEN"))
        || (!should_be_open && output.contains("RR_PORT_CLOSED"))
    {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "fixture WSL 远端端口状态错误：expected_open={should_be_open}"
        )))
    }
}

/// 在 WSL `ss` 和实际 /dev/tcp 两层都确认停止后的远端监听已释放并拒绝连接。
///
/// @author kongweiguang
fn retry_until_listener_closed(port: u16) -> Result<(), io::Error> {
    let deadline = Instant::now() + FIXTURE_RESPONSE_TIMEOUT;
    loop {
        match wsl_listener_is_open(port) {
            Ok(false) => match wsl_listener_probe(port, false) {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                }
                Err(error) => return Err(error),
            },
            Ok(true) if Instant::now() < deadline => {}
            Ok(true) => return Err(io::Error::other("停止后 fixture WSL 仍显示远端 listener")),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
            }
            Err(error) => return Err(error),
        }
        thread::sleep(FIXTURE_RETRY_INTERVAL);
    }
}

/// 从 fixture WSL loopback 请求远端 listener，检查端到端 HTTP 正文并区分 listener 存在与转发成功。
///
/// @author kongweiguang
fn wsl_http_probe(port: u16, expected_body: &str) -> Result<(), io::Error> {
    let script = format!(
        r#"set -e
exec 3<>/dev/tcp/127.0.0.1/{port}
printf 'GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3
IFS= read -r -t 4 status <&3
status=${{status%$'\r'}}
[[ "$status" == 'HTTP/1.0 200 OK' || "$status" == 'HTTP/1.1 200 OK' ]]
while IFS= read -r -t 4 header <&3; do
    header=${{header%$'\r'}}
    [[ -n "$header" ]] || break
done
IFS= read -r -t 4 body <&3
body=${{body%$'\r'}}
[[ "$body" == '{expected_body}' ]]
printf RR_HTTP_OK"#
    );
    let output = run_fixture_wsl(&script)?;
    if output.contains("RR_HTTP_OK") {
        Ok(())
    } else {
        Err(io::Error::other(
            "fixture WSL 远端 HTTP 探针没有确认预期正文",
        ))
    }
}

/// 通过 WSL 内的标准库完成 SOCKS5 协商和 HTTP 正文核对，不借用 Windows 的端口映射。
///
/// @author kongweiguang
fn wsl_socks_http_probe(
    socks_port: u16,
    destination_port: u16,
    expected_body: &str,
) -> Result<(), io::Error> {
    if expected_body != "fixture-local-ok" {
        return Err(io::Error::other("远端 SOCKS 只允许本轮本机 fixture 响应"));
    }
    let script = format!(
        r#"python3 - {socks_port} {destination_port} <<'PY'
import socket, struct, sys
def exact(sock, size):
    data = b''
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            raise RuntimeError('SOCKS stream closed')
        data += chunk
    return data
s = socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=4)
s.settimeout(4)
s.sendall(b'\x05\x01\x00')
if exact(s, 2) != b'\x05\x00':
    raise RuntimeError('SOCKS authentication rejected')
s.sendall(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + struct.pack('!H', int(sys.argv[2])))
reply = exact(s, 4)
if reply[:3] != b'\x05\x00\x00':
    raise RuntimeError('SOCKS CONNECT failed')
if reply[3] == 1:
    exact(s, 6)
elif reply[3] == 4:
    exact(s, 18)
elif reply[3] == 3:
    exact(s, exact(s, 1)[0] + 2)
else:
    raise RuntimeError('invalid SOCKS address type')
s.sendall(b'GET / HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n')
response = bytearray()
while len(response) < 16384:
    chunk = s.recv(4096)
    if not chunk:
        break
    response.extend(chunk)
s.close()
headers, body = bytes(response).split(b'\r\n\r\n', 1)
if not headers.startswith(b'HTTP/1.0 200 ') or body.strip() != b'fixture-local-ok':
    raise RuntimeError('unexpected client fixture response')
print('RR_SOCKS_HTTP_OK')
PY"#
    );
    if run_fixture_wsl(&script)?.contains("RR_SOCKS_HTTP_OK") {
        Ok(())
    } else {
        Err(io::Error::other("fixture WSL SOCKS 探针未确认本机正文"))
    }
}

/// 通过固定 fixture distro 执行只读 Bash 探针，禁止新控制台并为连接和响应设置硬超时。
///
/// @author kongweiguang
fn run_fixture_wsl(script: &str) -> Result<String, io::Error> {
    let mut command = Command::new("wsl.exe");
    command
        .args([
            "--distribution",
            FIXTURE_WSL_DISTRIBUTION,
            "--user",
            "root",
            "--exec",
            "/bin/bash",
            "-c",
            script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + WSL_COMMAND_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture WSL 远端端口探针超过时限",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "fixture WSL 探针退出码 {:?}",
            output.status.code()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 创建本机一次性 HTTP 目标，让 Remote 转发从 fixture WSL 返回独有正文。
///
/// @author kongweiguang
impl LocalHttpFixture {
    /// Bind before saving a Remote forward so the SSH client has a live loopback destination.
    ///
    /// @author kongweiguang
    fn start() -> Result<Self, io::Error> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("kerminal-right-rail-http-fixture".to_owned())
            .spawn(move || serve_local_http_fixture(listener, worker_stopping))?;
        Ok(Self {
            port,
            stopping,
            worker: Some(worker),
        })
    }
}

/// 服务每个有效 GET 并保持 listener 存活到 Remote 隧道确认释放后，避免测试时序竞争。
///
/// @author kongweiguang
fn serve_local_http_fixture(
    listener: TcpListener,
    stopping: Arc<AtomicBool>,
) -> Result<(), io::Error> {
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(FIXTURE_READ_TIMEOUT))?;
                stream.set_write_timeout(Some(FIXTURE_READ_TIMEOUT))?;
                if read_http_request(&mut stream)? {
                    stream.write_all(
                        b"HTTP/1.0 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\nfixture-local-ok\n",
                    )?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(FIXTURE_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// 只接受有限长度内的完整 HTTP 请求头，防止任意客户端占用 fixture 线程。
///
/// @author kongweiguang
fn read_http_request(stream: &mut TcpStream) -> Result<bool, io::Error> {
    let mut request = Vec::with_capacity(512);
    let mut buffer = [0_u8; 256];
    while request.len() < 8_192 && !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(false);
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request
        .windows(b"GET / HTTP/".len())
        .any(|window| window == b"GET / HTTP/"))
}

/// Drop 先发停止信号再等待唯一 worker 退出，避免 fixture server 留在进程外继续监听。
///
/// @author kongweiguang
impl Drop for LocalHttpFixture {
    /// Stop the accept loop before joining so failed probes cannot leak a listener thread.
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
