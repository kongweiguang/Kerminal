//! MCP adapter 注入的应用用例边界。
//!
//! @author kongweiguang

use std::{future::Future, sync::Arc};

use kerminal_application::{
    AppError, AppResult,
    agents::AgentService,
    commands::CommandsService,
    connections::{
        RemoteConnectionService, credentials::CredentialService, management::HostManagementService,
    },
    executor::ApplicationTaskExecutor,
    operations::OperationsService,
    preferences::{ConfigurationValidationService, ProfileService},
    sessions::SessionService,
    transfers::TransferService,
};
use kerminal_domain::identity::AgentSessionId;
use tokio_util::sync::CancellationToken;

/// MCP 只依赖应用服务端口，不直接持有基础设施对象或 GPUI 状态。
#[derive(Clone)]
pub struct McpServices {
    pub(crate) sessions: Arc<dyn SessionService>,
    pub(crate) agents: Option<Arc<dyn AgentService>>,
    pub(crate) remote_connections: Option<Arc<dyn RemoteConnectionService>>,
    pub(crate) host_management: Option<HostManagementService>,
    pub(crate) credentials: Option<CredentialService>,
    pub(crate) transfers: Option<TransferService>,
    pub(crate) operations: Option<OperationsService>,
    pub(crate) commands: Option<CommandsService>,
    pub(crate) profiles: Option<ProfileService>,
    pub(crate) configuration_validation: Option<ConfigurationValidationService>,
    pub(crate) task_executor: Option<ApplicationTaskExecutor>,
    pub(crate) agent_scope: Option<AgentSessionId>,
}

impl McpServices {
    /// 显式注入唯一终端会话服务，避免 MCP 自建第二套 PTY/session registry。
    pub fn new(sessions: Arc<dyn SessionService>) -> Self {
        Self {
            sessions,
            agents: None,
            remote_connections: None,
            host_management: None,
            credentials: None,
            transfers: None,
            operations: None,
            commands: None,
            profiles: None,
            configuration_validation: None,
            task_executor: None,
            agent_scope: None,
        }
    }

    /// 复用 composition root 的唯一 tracker，令 MCP 请求与应用退出共享取消和 join 生命周期。
    pub fn with_task_executor(mut self, executor: ApplicationTaskExecutor) -> Self {
        self.task_executor = Some(executor);
        self
    }

    /// 注入 Agent 生命周期用例；MCP 只调度 application port，不读取会话文件。
    pub fn with_agents(mut self, agents: Arc<dyn AgentService>) -> Self {
        self.agents = Some(agents);
        self
    }

    /// 注入 SSH/主机管理/凭据用例，保持协议 adapter 不直接访问连接基础设施。
    pub fn with_connections(
        mut self,
        remote: Arc<dyn RemoteConnectionService>,
        hosts: HostManagementService,
        credentials: CredentialService,
    ) -> Self {
        self.remote_connections = Some(remote);
        self.host_management = Some(hosts);
        self.credentials = Some(credentials);
        self
    }

    /// 只注入共享的传输队列，避免 MCP 为服务器文件操作再持有独立文件服务。
    pub fn with_transfers(mut self, transfers: TransferService) -> Self {
        self.transfers = Some(transfers);
        self
    }

    /// 注入应用的运维用例集合，以供端口转发复用唯一运行时。
    pub fn with_operations(mut self, operations: OperationsService) -> Self {
        self.operations = Some(operations);
        self
    }

    /// 注入命令历史查询与建议用例，不向 MCP 暴露历史写入或工作流 CRUD。
    pub fn with_commands(mut self, commands: CommandsService) -> Self {
        self.commands = Some(commands);
        self
    }

    /// 复用 composition root 的默认 profile 与平台 shell 解析，避免 MCP 探测环境变量或猜启动程序。
    pub fn with_profiles(mut self, profiles: ProfileService) -> Self {
        self.profiles = Some(profiles);
        self
    }

    /// 注入配置只读 validator；MCP 不解析 TOML 或读取配置文件路径。
    pub fn with_configuration_validation(
        mut self,
        validation: ConfigurationValidationService,
    ) -> Self {
        self.configuration_validation = Some(validation);
        self
    }

    /// 为受管 Agent 的 session-scoped endpoint 建立不可变调用上下文。
    pub fn for_agent_session(mut self, agent_session_id: AgentSessionId) -> Self {
        self.agent_scope = Some(agent_session_id);
        self
    }

    /// 要求 Agent 能力已由桌面 composition root 装配，避免静默退化为无 Agent 的运行态。
    pub(crate) fn agent_service(&self) -> kerminal_application::AppResult<&dyn AgentService> {
        self.agents
            .as_deref()
            .ok_or_else(|| kerminal_application::AppError::unsupported("Agent runtime unavailable"))
    }

    /// 要求 SSH 连接用例已由 composition root 装配，禁止工具自行构造协议连接。
    pub(crate) fn require_remote_connections(&self) -> AppResult<&dyn RemoteConnectionService> {
        self.remote_connections
            .as_deref()
            .ok_or_else(|| AppError::unsupported("SSH connection capability unavailable"))
    }

    /// 要求文件型主机目录管理用例存在；MCP 不直接改写 hosts/*.toml。
    pub(crate) fn require_host_management(&self) -> AppResult<&HostManagementService> {
        self.host_management
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Host management capability unavailable"))
    }

    /// 要求凭据 vault 用例存在，秘密正文只在该应用边界短暂出现。
    pub(crate) fn require_credentials(&self) -> AppResult<&CredentialService> {
        self.credentials
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Credential vault capability unavailable"))
    }

    /// 要求统一传输队列用例存在，SFTP 和容器传输不可各自维护任务状态。
    pub(crate) fn require_transfers(&self) -> AppResult<&TransferService> {
        self.transfers
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Transfer capability unavailable"))
    }

    /// 要求运维用例集合存在；工具族从中调用目标明确的 application service。
    pub(crate) fn require_operations(&self) -> AppResult<&OperationsService> {
        self.operations
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Operations capability unavailable"))
    }

    /// 要求命令服务存在；MCP 外部 adapter 只允许调用实际暴露的历史读取能力。
    pub(crate) fn require_commands(&self) -> AppResult<&CommandsService> {
        self.commands
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Command capability unavailable"))
    }

    /// 要求本地 profile/shell 解析用例已装配，terminal.create 不自行读取主机环境。
    pub(crate) fn require_profiles(&self) -> AppResult<&ProfileService> {
        self.profiles
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Local profile capability unavailable"))
    }

    /// 要求只读配置 validator 已装配，避免工具在 MCP 层复刻运行时加载规则。
    pub(crate) fn require_configuration_validation(
        &self,
    ) -> AppResult<&ConfigurationValidationService> {
        self.configuration_validation
            .as_ref()
            .ok_or_else(|| AppError::unsupported("Configuration validation capability unavailable"))
    }

    /// 在桌面启动 MCP 前一次检查所有目录工具的运行态依赖，阻止部分装配却发布完整目录。
    pub fn validate_complete(&self) -> Result<(), McpServicesValidationError> {
        let mut missing = Vec::new();
        if self.agents.is_none() {
            missing.push("agents");
        }
        if self.remote_connections.is_none() {
            missing.push("remote_connections");
        }
        if self.host_management.is_none() {
            missing.push("host_management");
        }
        if self.credentials.is_none() {
            missing.push("credentials");
        }
        if self.transfers.is_none() {
            missing.push("transfers");
        }
        if self.operations.is_none() {
            missing.push("operations");
        }
        if self.commands.is_none() {
            missing.push("commands");
        }
        if self.profiles.is_none() {
            missing.push("profiles");
        }
        if self.configuration_validation.is_none() {
            missing.push("configuration_validation");
        }
        if self.task_executor.is_none() {
            missing.push("task_executor");
        }

        if missing.is_empty() {
            Ok(())
        } else {
            Err(McpServicesValidationError { missing })
        }
    }

    /// 在应用 tracker 内执行请求，并在取消时通知 handler 后等待其协作清理完成。
    pub(crate) async fn run_request<T, Build, Fut>(
        &self,
        request_cancellation: CancellationToken,
        build: Build,
    ) -> AppResult<T>
    where
        T: Send + 'static,
        Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<T>> + Send + 'static,
    {
        match &self.task_executor {
            Some(executor) => {
                let request_cancellation = request_cancellation.clone();
                executor
                    .run(move |shutdown_cancellation| async move {
                        run_cancellable_request(request_cancellation, shutdown_cancellation, build)
                            .await
                    })
                    .await
            }
            None => {
                run_cancellable_request(request_cancellation, CancellationToken::new(), build).await
            }
        }
    }
}

/// 汇总桌面 MCP 生产装配中缺失的用例服务名称，错误文本不包含配置或凭据信息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServicesValidationError {
    missing: Vec<&'static str>,
}

impl McpServicesValidationError {
    /// 返回缺失服务名，供启动日志或 UI 以结构化方式说明装配问题。
    pub fn missing_services(&self) -> &[&'static str] {
        &self.missing
    }
}

impl std::fmt::Display for McpServicesValidationError {
    /// 用稳定的服务标识描述未完成的生产装配，避免把实现内部错误或秘密写入日志。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "MCP 服务装配不完整，缺少：{}",
            self.missing.join(", ")
        )
    }
}

impl std::error::Error for McpServicesValidationError {}

/// 将请求取消和应用关闭合并到 handler child token，并等待 handler 完成自身资源清理。
async fn run_cancellable_request<T, Build, Fut>(
    request_cancellation: CancellationToken,
    shutdown_cancellation: CancellationToken,
    build: Build,
) -> AppResult<T>
where
    Build: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = AppResult<T>>,
{
    let handler_cancellation = request_cancellation.child_token();
    let mut operation = Box::pin(build(handler_cancellation.clone()));
    tokio::select! {
        biased;
        result = operation.as_mut() => result,
        _ = request_cancellation.cancelled() => {
            handler_cancellation.cancel();
            operation.as_mut().await
        }
        _ = shutdown_cancellation.cancelled() => {
            handler_cancellation.cancel();
            operation.as_mut().await
        }
    }
}
