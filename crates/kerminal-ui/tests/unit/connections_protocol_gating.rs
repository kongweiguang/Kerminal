//! 禁用协议在连接面板内部直接启动与新建表单入口的门禁回归。
//!
//! 这些入口不经过 Root 的 ConnectRequested，因此必须在本层复查冻结启用视图。
//!
//! @author kongweiguang

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use kerminal_application::{
    AppError,
    connections::{
        CredentialResolver, HostConnectionRepository, RdpLaunchReceipt, RdpLaunchService,
        RdpLaunchTarget, RdpPlatformLauncher, ResolvedCredential,
    },
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder},
};
use kerminal_domain::connections::{CredentialRef, HostConnectionProfile, RemoteProtocol};
use tokio_util::sync::CancellationToken;

use crate::features::connections::ConnectionsEvent;

use super::*;

/// 构造只关闭指定内置单元的冻结目录，其余单元保持生产默认启用。
/// @author kongweiguang
fn registry_disabling(units: &[&str]) -> Arc<PluginRegistry> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states = units
        .iter()
        .map(|unit| (PluginId::new(*unit).expect("测试单元 ID 应合法"), false))
        .collect();
    Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("内置状态应可冻结"),
    )
}

/// 只在被真正调用时累加，门禁回归据此证明禁用 RDP 没有创建启动请求。
struct CountingRdpLauncher(Arc<AtomicUsize>);

#[async_trait]
impl RdpPlatformLauncher for CountingRdpLauncher {
    async fn launch(
        &self,
        _: RdpLaunchTarget,
        _: CancellationToken,
    ) -> AppResult<RdpLaunchReceipt> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(AppError::infrastructure("禁用 RDP 时平台启动器不得被调用"))
    }
}

/// 启动前读取主机同样应被门禁挡住；计数让测试能区分“未启动”和“读取后失败”。
struct CountingHostConnections(Arc<AtomicUsize>);

#[async_trait]
impl HostConnectionRepository for CountingHostConnections {
    async fn load(&self, _: &HostId) -> AppResult<HostConnectionProfile> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(AppError::infrastructure("禁用 RDP 时不应读取连接主机"))
    }
}

/// 禁用路径不需要解析凭据；真正被调用说明门禁发生得太晚。
struct UnusedCredentialResolver;

#[async_trait]
impl CredentialResolver for UnusedCredentialResolver {
    async fn resolve(
        &self,
        _: &CredentialRef,
        _: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        Err(AppError::infrastructure("禁用 RDP 时不应解析凭据"))
    }
}

/// 复制现有禁用协议主机不能打开新编辑器，避免生成永远无法连接的副本。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_protocol_duplicate_is_rejected(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.ssh"]);
    let host = existing_host();
    let host_id = host.id.clone();
    let (_runtime, panel, visual) =
        fixture_with_services(app, vec![host], Vec::new(), Some(registry), None);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.duplicate_saved_host(&host_id, window, cx);
        assert!(panel.editor.is_none(), "禁用协议不能打开复制草稿");
        assert!(
            panel.status.as_ref().is_some_and(|status| {
                status.contains("SSH is disabled") || status.contains("SSH 已禁用")
            }),
            "复制拒绝应复用状态栏说明原因"
        );
    });
}

/// “复制为 SSH”会创建 SSH 草稿；SSH 禁用时即使源 SFTP 主机可用也必须拒绝。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_ssh_copy_as_ssh_is_rejected(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.ssh"]);
    let mut host = existing_host();
    host.protocol = RemoteProtocol::Sftp;
    host.sftp_options.enabled = true;
    let host_id = host.id.clone();
    let (_runtime, panel, visual) =
        fixture_with_services(app, vec![host], Vec::new(), Some(registry), None);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.duplicate_saved_host_as_ssh(&host_id, window, cx);
        assert!(panel.editor.is_none(), "SSH 禁用不能打开转换后的草稿");
        assert!(
            panel.status.as_ref().is_some_and(|status| {
                status.contains("SSH is disabled") || status.contains("SSH 已禁用")
            }),
            "复制为 SSH 拒绝应复用状态栏"
        );
    });
}

/// 表单打开后协议被外部禁用，保存必须在本地停住，不分配新主机 ID、不写仓储。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_protocol_save_keeps_editor_without_persisting(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.ssh"]);
    let host = existing_host();
    let host_id = host.id.clone();
    let (_runtime, panel, visual) =
        fixture_with_services(app, vec![host], Vec::new(), Some(registry), None);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.edit_host(&host_id, window, cx);
        assert!(panel.editor.is_some(), "配置查看/编辑仍允许打开");
        panel.save_editor(window, cx);
        assert!(panel.editor.is_some(), "拒绝保存不得关闭原编辑器");
        assert!(!panel.saving);
        assert!(
            panel.status.as_ref().is_some_and(|status| {
                status.contains("SSH is disabled") || status.contains("SSH 已禁用")
            }),
            "保存拒绝应复用状态栏说明原因"
        );
    });
}

/// 默认新建主机跳过禁用协议，避免表单初值指向无法连接的传输。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_protocols_redirect_new_host_to_first_enabled(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.ssh", "kerminal.sftp"]);
    let (_runtime, panel, visual) =
        fixture_with_services(app, Vec::new(), Vec::new(), Some(registry), None);
    update_panel(&panel, visual, |panel, window, cx| {
        assert!(panel.create_host(window, cx));
        assert_eq!(
            panel.editor.as_ref().expect("应打开新主机表单").protocol,
            RemoteProtocol::Telnet
        );
    });
}

/// 全部远端协议禁用时没有可创建目标，不能打开一个随后无法保存或连接的表单。
/// @author kongweiguang
#[gpui_kit::test]
fn all_disabled_protocols_do_not_open_the_editor(app: &mut TestAppContext) {
    let registry = registry_disabling(&[
        "kerminal.ssh",
        "kerminal.sftp",
        "kerminal.telnet",
        "kerminal.serial",
        "kerminal.rdp",
    ]);
    let (_runtime, panel, visual) =
        fixture_with_services(app, Vec::new(), Vec::new(), Some(registry), None);
    update_panel(&panel, visual, |panel, window, cx| {
        assert!(!panel.create_host(window, cx));
        assert!(panel.editor.is_none());
        assert!(
            panel.status.as_ref().is_some_and(|status| {
                status.contains("All remote protocols are disabled")
                    || status.contains("所有远端协议均已禁用")
            }),
            "全部禁用应复用状态栏说明原因"
        );
    });
}

/// RDP 禁用时面板直接启动分支必须在生成请求和启动进程前返回。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_rdp_never_reaches_launcher(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.rdp"]);
    let calls = Arc::new(AtomicUsize::new(0));
    let rdp = Arc::new(RdpLaunchService::new(
        Arc::new(CountingHostConnections(calls.clone())),
        Arc::new(UnusedCredentialResolver),
        Arc::new(CountingRdpLauncher(calls.clone())),
    ));
    let mut host = existing_host();
    host.protocol = RemoteProtocol::Rdp;
    host.endpoint.port = 3389;
    let host_id = host.id.clone();
    let (runtime, panel, visual) =
        fixture_with_services(app, vec![host], Vec::new(), Some(registry), Some(rdp));

    update_panel(&panel, visual, |panel, _, cx| {
        panel.select_host(host_id, cx);
        panel.connect_selected(false, cx);
        assert_eq!(
            panel.rdp_request_generation, 0,
            "禁用协议不得生成新的 RDP 请求代次"
        );
        assert!(
            panel.status.as_ref().is_some_and(|status| {
                status.contains("RDP is disabled") || status.contains("RDP 已禁用")
            }),
            "拒绝应写入现有状态栏"
        );
    });
    pump_runtime(&runtime, visual);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "禁用 RDP 不得读取主机或调用平台启动器"
    );
}

/// SFTP 单元禁用时共享工作台的选择必须在面板内停住：不占用 request id、不投递
/// SftpSelectionReady，Root 队列可立即推进到后续意图，远端目录也不会被读取。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_sftp_selection_is_rejected_before_pending(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.sftp"]);
    let mut host = existing_host();
    host.protocol = RemoteProtocol::Sftp;
    host.sftp_options.enabled = true;
    let host_id = host.id.clone();
    let (_runtime, panel, visual) =
        fixture_with_services(app, vec![host], Vec::new(), Some(registry), None);
    let events = Rc::new(RefCell::new(Vec::new()));
    let captured = events.clone();
    let subscription = visual.update(|_, app| {
        app.subscribe(&panel, move |_, event: &ConnectionsEvent, _| {
            captured.borrow_mut().push(event.clone())
        })
    });

    update_panel(&panel, visual, |panel, _, cx| {
        panel.request_sftp_selection(&host_id, "request-sftp-disabled".into(), cx);
        assert!(
            panel.sftp_selection_request_id.is_none(),
            "拒绝不得占用文件选择请求通道"
        );
    });

    let events = events.borrow();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ConnectionsEvent::SftpSelectionReady { .. })),
        "禁用后不得交给 Root 读取远端目录"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ConnectionsEvent::SftpSelectionFinished {
                message: Some(message),
                interrupted: false,
                ..
            } if message.contains("SFTP")
        )),
        "拒绝应通过原回执出口说明 SFTP 已禁用"
    );
    drop(subscription);
}

/// SSH 主机上的 SFTP 还依赖 SSH 连接能力；只有 SFTP 启用时也必须拒绝，避免面板
/// 把无法连接的远程目录放进待办队列。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_ssh_selection_on_ssh_host_is_rejected(app: &mut TestAppContext) {
    let registry = registry_disabling(&["kerminal.ssh"]);
    let host = existing_host();
    let host_id = host.id.clone();
    let (_runtime, panel, visual) =
        fixture_with_services(app, vec![host], Vec::new(), Some(registry), None);
    let events = Rc::new(RefCell::new(Vec::new()));
    let captured = events.clone();
    let subscription = visual.update(|_, app| {
        app.subscribe(&panel, move |_, event: &ConnectionsEvent, _| {
            captured.borrow_mut().push(event.clone())
        })
    });

    update_panel(&panel, visual, |panel, _, cx| {
        panel.request_sftp_selection(&host_id, "request-ssh-disabled".into(), cx);
        assert!(
            panel.sftp_selection_request_id.is_none(),
            "SSH 禁用时不得进入 pending"
        );
    });

    let events = events.borrow();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ConnectionsEvent::SftpSelectionReady { .. })),
        "SSH 禁用时不得创建远端目录读取"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ConnectionsEvent::SftpSelectionFinished {
                message: Some(message),
                interrupted: false,
                ..
            } if message.contains("SSH")
        )),
        "拒绝应通过原回执出口说明 SSH 已禁用"
    );
    drop(subscription);
}
