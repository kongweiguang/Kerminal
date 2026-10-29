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

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, ForwardState, PortForwardConfig, ProxyApplyScope,
        ProxyProtocol, RemoteAccessScope,
    },
};
use kerminal_ui::RootView;

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

/// 通过生产新增表单依次创建 Local、Dynamic、Remote、RemoteDynamic，并验证数据面和资源回收。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
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
        ForwardCase {
            config: remote_socks_config,
            label: "dynamic-remote",
            probe: ForwardProbe::RemoteSocks {
                bind_port: remote_socks_bind,
                destination_port: capture.container_target_port,
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

/// 为每种类型使用独立名称与配置，并在任何生命周期失败后按同一 ID 清理资源。
///
/// @author kongweiguang
async fn run_forward_case(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
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
    let lifecycle = run_forward_lifecycle(cx, root, window, capture, records, &fixture).await;
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

/// 保存后启动同一 fixture 定义、验证真实协议结果、停止验证端口关闭并读回删除。
///
/// @author kongweiguang
async fn run_forward_lifecycle(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
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
    Ok(())
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
        } => {
            verify_wsl_listener_open(cx, bind_port).await?;
            verify_socks_http(cx, bind_port, destination_port, "fixture-ok").await
        }
    }
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
