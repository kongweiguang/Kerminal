//! 根视图与桌面组合根共享的应用服务句柄。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::{
    SessionService,
    agents::AgentService,
    commands::{CommandsService, SnippetTransferFileService},
    configuration_changes::ConfigurationChangeService,
    configuration_directory::ConfigurationDirectoryService,
    configuration_sync::ConfigurationSyncService,
    connections::{
        RdpLaunchService, RdpLaunchStatusService, RemoteConnectionService, SerialPortService,
        credentials::VaultKeyManagementService, management::HostManagementService,
        probe::ConnectionProbeService,
    },
    executor::ApplicationTaskExecutor,
    files::{FileService, editor::FileEditorService},
    mcp_runtime::McpRuntimeControl,
    operations::OperationsService,
    platform::{
        DesktopNotificationService, ExternalProtocolService, external_launch::ExternalLaunchService,
    },
    preferences::{ConfigurationValidationService, PreferencesService, ProfileService},
    sessions::logging::SessionLogService,
    transfers::TransferService,
    updates::UpdateService,
    workspace::{WorkspaceService, WorkspaceSessionService},
};
use kerminal_domain::files::FileEndpoint;

/// 可克隆的界面依赖；桌面组合根持有运行时并统一注入。
#[derive(Clone)]
pub struct AppServices {
    /// 布局领域规则，不持有视图或会话资源。
    pub workspace: WorkspaceService,
    /// 与 MCP、Agent 共用的唯一会话注册表。
    pub sessions: Arc<dyn SessionService>,
    /// 把应用用例投递到唯一 Tokio runtime，避免 GPUI 线程轮询 Tokio I/O。
    pub executor: ApplicationTaskExecutor,
    /// 文件化配置与只读恢复态共用同一仓储门禁。
    pub preferences: PreferencesService,
    /// 默认 profile 和平台 shell 解析由应用用例完成，UI 不猜测启动程序。
    pub profiles: ProfileService,
    /// Vault key 管理能力只经应用用例暴露，面板不直接访问加密仓储。
    pub vault_keys: Option<VaultKeyManagementService>,
    /// 文件与传输服务按组合根结果注入；缺失时不显示不可操作的文件入口。
    pub files: Option<FileService>,
    pub transfers: Option<TransferService>,
    /// 工作区 v3 使用 CAS 仓储保存，损坏/旧版读取结果不自动覆盖。
    pub workspace_sessions: Option<WorkspaceSessionService>,
    /// 终端工具栏与 MCP 共用桌面创建的日志用例，UI 不另建日志仓储。
    pub session_logs: Option<SessionLogService>,
    /// 系统通知策略由组合根创建，前台抑制与发送门禁在同一应用 service 内共享。
    pub notifications: Option<Arc<DesktopNotificationService>>,
    /// 外部启动入口复用组合根维护的 allowlist 与临时目标生命周期。
    pub external_launch: Option<ExternalLaunchService>,
    /// kerminal:// 状态由应用端口读取；当前设置页不在构造或读取时改系统注册。
    pub external_protocol: Option<ExternalProtocolService>,
    /// 文件编辑用例复用统一 FileService，脏关闭由根窗口统一拦截。
    pub file_editor: Option<FileEditorService>,
    /// 文件面板首个浏览目标由组合根显式提供，UI 不推断主机或工作目录。
    pub initial_file_endpoint: Option<FileEndpoint>,
    /// 命令历史、片段和工作流页面按组合根创建的 use case 注入。
    pub commands: Option<Arc<CommandsService>>,
    /// 片段目录入口和 JSON 文件传输都经应用端口处理，视图不持有任意路径 I/O。
    pub configuration_directory: Option<ConfigurationDirectoryService>,
    pub snippet_transfer_files: Option<SnippetTransferFileService>,
    /// Commands 与 MCP 共用同一只读配置 validator，不在 UI 复制 TOML 规则。
    pub configuration_validation: Option<ConfigurationValidationService>,
    /// 主机管理和 SSH 信任预检共用组合根的同一 service instance。
    pub hosts: Option<HostManagementService>,
    pub remote_connections: Option<Arc<dyn RemoteConnectionService>>,
    /// 未保存连接探测只执行短生命周期用例，不暴露配置或凭据仓储给界面。
    pub connection_probe: Option<Arc<ConnectionProbeService>>,
    /// 串口枚举通过应用端口注入；缺失时连接表单仍允许手工填写设备路径。
    pub serial_ports: Option<Arc<SerialPortService>>,
    pub rdp_launcher: Option<Arc<RdpLaunchService>>,
    /// RDP 请求状态复用同一 launcher 发布源，避免面板自行创建第二个状态 hub。
    pub rdp_launch_status: Option<Arc<dyn RdpLaunchStatusService>>,
    /// 运维 use case 在面板内按已选 HostId 显式作用于目标。
    pub operations: Option<Arc<OperationsService>>,
    /// Agent 会话用例只管理 Agent 生命周期；CLI terminal 仍经共享 SessionService 操作。
    pub agents: Option<Arc<dyn AgentService>>,
    /// MCP listener 控制器复用桌面 composition 的唯一 owner，不在 UI 创建第二个 runtime。
    pub mcp_runtime: Option<Arc<dyn McpRuntimeControl>>,
    /// Git 配置同步缺失时偏好页可明确显示不可用，不伪造同步成功状态。
    pub configuration_sync: Option<ConfigurationSyncService>,
    /// 有正式 feed 与签名适配器的构建才启用更新；缺失时保持 None。
    pub update_service: Option<UpdateService>,
    /// 多个配置页复用桌面唯一 watcher；各面板只订阅自己负责的相对路径。
    pub configuration_changes: Option<ConfigurationChangeService>,
    /// 桌面平台统一解析的系统 BCP-47 locale；UI 不直接探测进程环境变量。
    pub system_locale: Option<String>,
}

impl AppServices {
    /// 要求组合根显式提供核心服务，并将可选能力初始化为空，避免界面另建执行器或探测资源。
    ///
    /// @author kongweiguang
    pub fn new(
        workspace: WorkspaceService,
        sessions: Arc<dyn SessionService>,
        executor: ApplicationTaskExecutor,
        preferences: PreferencesService,
        profiles: ProfileService,
    ) -> Self {
        Self {
            workspace,
            sessions,
            executor,
            preferences,
            profiles,
            vault_keys: None,
            files: None,
            transfers: None,
            workspace_sessions: None,
            session_logs: None,
            notifications: None,
            external_launch: None,
            external_protocol: None,
            file_editor: None,
            initial_file_endpoint: None,
            commands: None,
            configuration_directory: None,
            snippet_transfer_files: None,
            configuration_validation: None,
            hosts: None,
            remote_connections: None,
            connection_probe: None,
            serial_ports: None,
            rdp_launcher: None,
            rdp_launch_status: None,
            operations: None,
            agents: None,
            mcp_runtime: None,
            configuration_sync: None,
            update_service: None,
            configuration_changes: None,
            system_locale: None,
        }
    }

    /// 一次装配文件浏览、传输和编辑用例，避免三个入口各自创建不同的文件服务实例。
    pub fn with_files(
        mut self,
        files: FileService,
        transfers: TransferService,
        initial_file_endpoint: FileEndpoint,
    ) -> Self {
        self.file_editor = Some(FileEditorService::new(files.clone()));
        self.files = Some(files);
        self.transfers = Some(transfers);
        self.initial_file_endpoint = Some(initial_file_endpoint);
        self
    }

    /// 接入唯一工作区 session 仓储；应用重启恢复和后续布局保存共享同一 revision 门禁。
    pub fn with_workspace_sessions(mut self, service: WorkspaceSessionService) -> Self {
        self.workspace_sessions = Some(service);
        self
    }

    /// 让可见终端日志与 MCP 复用同一运行态日志服务实例。
    pub fn with_session_logs(mut self, service: SessionLogService) -> Self {
        self.session_logs = Some(service);
        self
    }

    /// 注入组合根创建的通知 service，确保 UI 事件复用同一节流与展示状态。
    pub fn with_notifications(mut self, service: Arc<DesktopNotificationService>) -> Self {
        self.notifications = Some(service);
        self
    }

    /// 让 UI 事件使用组合根的外部启动白名单和临时目标仓储。
    pub fn with_external_launch(mut self, service: ExternalLaunchService) -> Self {
        self.external_launch = Some(service);
        self
    }

    /// 注入协议关联的只读状态用例；注册变更仍需独立的显式产品入口。
    ///
    /// @author kongweiguang
    pub fn with_external_protocol(mut self, service: ExternalProtocolService) -> Self {
        self.external_protocol = Some(service);
        self
    }

    /// 只注入具备真实更新端口的服务；None 显示构建缺少更新源的真实状态。
    pub fn with_update_service(mut self, service: Option<UpdateService>) -> Self {
        self.update_service = service;
        self
    }

    /// 复用桌面组合根创建的命令 use case，避免 UI 另建历史仓储或工作流执行器。
    pub fn with_commands(mut self, commands: Arc<CommandsService>) -> Self {
        self.commands = Some(commands);
        self
    }

    /// 将用户显式请求打开配置目录的能力交给 Commands，避免界面启动任意命令。
    ///
    /// @author kongweiguang
    pub fn with_configuration_directory(mut self, service: ConfigurationDirectoryService) -> Self {
        self.configuration_directory = Some(service);
        self
    }

    /// 注入受限的片段 JSON 文件端口，系统对话框只负责选择路径。
    ///
    /// @author kongweiguang
    pub fn with_snippet_transfer_files(mut self, service: SnippetTransferFileService) -> Self {
        self.snippet_transfer_files = Some(service);
        self
    }

    /// 让 Commands 使用组合根共享的只读 validator，而不是复刻配置校验逻辑。
    ///
    /// @author kongweiguang
    pub fn with_configuration_validation(
        mut self,
        service: ConfigurationValidationService,
    ) -> Self {
        self.configuration_validation = Some(service);
        self
    }

    /// 由桌面组合根注入共享 vault key 用例，避免偏好面板另开存储实例。
    pub fn with_vault_keys(mut self, vault_keys: VaultKeyManagementService) -> Self {
        self.vault_keys = Some(vault_keys);
        self
    }

    /// 让连接面板复用已建立的 host/configuration、SSH trust 与可用 RDP 用例。
    pub fn with_connections(
        mut self,
        hosts: HostManagementService,
        remote_connections: Arc<dyn RemoteConnectionService>,
        rdp_launcher: Option<Arc<RdpLaunchService>>,
    ) -> Self {
        self.hosts = Some(hosts);
        self.remote_connections = Some(remote_connections);
        self.rdp_launcher = rdp_launcher;
        self
    }

    /// 注入组合根创建的只读连接探测用例；界面不保存草稿或写入主机信任记录。
    ///
    /// @author kongweiguang
    pub fn with_connection_probe_service(mut self, service: Arc<ConnectionProbeService>) -> Self {
        self.connection_probe = Some(service);
        self
    }

    /// 注入组合根创建的串口枚举用例，避免 UI 直接访问平台 repository。
    ///
    /// @author kongweiguang
    pub fn with_serial_ports(mut self, service: Arc<SerialPortService>) -> Self {
        self.serial_ports = Some(service);
        self
    }

    /// 复用桌面唯一 RDP 状态发布源，让 launch receipt 与状态订阅属于同一运行时。
    pub fn with_rdp_launch_status(mut self, service: Arc<dyn RdpLaunchStatusService>) -> Self {
        self.rdp_launch_status = Some(service);
        self
    }

    /// 把组合根创建的运维服务交给工作台，避免 RootView 按当前标签推断远端主机。
    pub fn with_operations(mut self, operations: Arc<OperationsService>) -> Self {
        self.operations = Some(operations);
        self
    }

    /// 让右栏 Agent 与 MCP、可见终端共用相同的运行态 session registry。
    pub fn with_agents(mut self, agents: Arc<dyn AgentService>) -> Self {
        self.agents = Some(agents);
        self
    }

    /// 注入桌面唯一 MCP runtime control，使状态面板和 Agent 创建共享同一 listener owner。
    ///
    /// @author kongweiguang
    pub fn with_mcp_runtime(mut self, runtime: Arc<dyn McpRuntimeControl>) -> Self {
        self.mcp_runtime = Some(runtime);
        self
    }

    /// 注入配置同步用例；面板只允许触发显式 fetch/pull/commit/push 操作。
    pub fn with_configuration_sync(mut self, sync: ConfigurationSyncService) -> Self {
        self.configuration_sync = Some(sync);
        self
    }

    /// 将同一配置变更流交给各面板，由面板按路径过滤并在销毁时取消订阅。
    pub fn with_configuration_changes(mut self, changes: ConfigurationChangeService) -> Self {
        self.configuration_changes = Some(changes);
        self
    }

    /// 注入宿主系统 locale，System 语言模式据此选择产品支持的双语之一。
    pub fn with_system_locale(mut self, locale: String) -> Self {
        self.system_locale = Some(locale);
        self
    }
}
