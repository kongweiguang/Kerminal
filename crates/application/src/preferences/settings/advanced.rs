//! 通知、外部启动、传输、工具栏与 Agent Launcher 设置契约。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, AppResult};

/// 桌面通知策略；时长字段统一以毫秒表达并在保存边界限制范围。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct DesktopNotificationSettings {
    pub enabled: bool,
    pub background_only: bool,
    pub important_only: bool,
    pub min_duration_ms: u32,
    pub throttle_ms: u32,
}

impl Default for DesktopNotificationSettings {
    /// 与旧版默认通知策略相同，避免迁移时突然打断前台操作。
    fn default() -> Self {
        Self {
            enabled: false,
            background_only: true,
            important_only: false,
            min_duration_ms: 10_000,
            throttle_ms: 30_000,
        }
    }
}

impl DesktopNotificationSettings {
    /// 将通知节流限制在旧产品支持的安全区间，避免零间隔造成通知风暴。
    pub(super) fn normalized(mut self) -> Self {
        self.min_duration_ms = self.min_duration_ms.clamp(1_000, 120_000);
        self.throttle_ms = self.throttle_ms.min(600_000);
        self
    }
}

/// 可在外部 SSH 启动请求中识别的兼容客户端。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExternalLaunchToolSetting {
    Putty,
    Mobaxterm,
    Xshell,
    Securecrt,
    Openssh,
    SftpClient,
    KerminalNative,
}

/// 接收第三方终端参数与协议 URL 的用户级策略。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ExternalLaunchSettings {
    pub enabled: bool,
    pub accept_vendor_args: bool,
    pub auto_open_sftp: bool,
    pub disabled_tools: Vec<ExternalLaunchToolSetting>,
}

impl Default for ExternalLaunchSettings {
    /// 保持现有兼容入口可用，SFTP 自动打开仍须用户显式启用。
    fn default() -> Self {
        Self {
            enabled: true,
            accept_vendor_args: true,
            auto_open_sftp: false,
            disabled_tools: Vec::new(),
        }
    }
}

impl ExternalLaunchSettings {
    /// 去重被禁用的来源，保存时不重排有效用户偏好。
    pub(super) fn normalized(mut self) -> Self {
        let mut unique = Vec::with_capacity(self.disabled_tools.len());
        for tool in self.disabled_tools {
            if !unique.contains(&tool) {
                unique.push(tool);
            }
        }
        self.disabled_tools = unique;
        self
    }
}

/// Agent Launcher 中可保存的自定义启动器。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomAgentDefinition {
    pub id: String,
    pub name: String,
    pub command: String,
}

impl CustomAgentDefinition {
    /// 在应用边界生成稳定格式的自定义 Agent ID，避免 UI 自己实现身份编码。
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            command: command.into(),
        }
    }
}

/// 全局 Agent Launcher 选择状态和自定义启动命令集合。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentLauncherSettings {
    pub selected_agent_key: String,
    pub custom_agents: Vec<CustomAgentDefinition>,
}

impl Default for AgentLauncherSettings {
    /// 旧版默认选择内置 Codex，且初始不创建任何自定义命令。
    fn default() -> Self {
        Self {
            selected_agent_key: "builtin:codex".to_owned(),
            custom_agents: Vec::new(),
        }
    }
}

impl AgentLauncherSettings {
    /// 在一次设置保存前校验引用闭合，避免所选 custom key 指向缺失定义。
    pub(super) fn validated(mut self) -> AppResult<Self> {
        if self.custom_agents.len() > 32 {
            return Err(AppError::invalid("自定义 Agent 最多保存 32 个"));
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for agent in &mut self.custom_agents {
            let id = Uuid::parse_str(agent.id.trim())
                .map_err(|_| AppError::invalid("自定义 Agent id 必须是有效 UUID"))?;
            agent.id = id.to_string();
            agent.name = agent.name.trim().to_owned();
            agent.command = agent.command.trim().to_owned();
            if agent.name.is_empty() || agent.name.chars().count() > 64 {
                return Err(AppError::invalid(
                    "自定义 Agent 名称不能为空或超过 64 个字符",
                ));
            }
            if agent.command.is_empty() || agent.command.chars().count() > 4096 {
                return Err(AppError::invalid(
                    "自定义 Agent 命令不能为空或超过 4096 个字符",
                ));
            }
            if !ids.insert(agent.id.clone()) || !names.insert(agent.name.to_lowercase()) {
                return Err(AppError::invalid("自定义 Agent 的 ID 和名称都必须唯一"));
            }
        }
        self.selected_agent_key = self.selected_agent_key.trim().to_owned();
        if !matches!(
            self.selected_agent_key.as_str(),
            "builtin:codex" | "builtin:claude" | "builtin:pi"
        ) {
            let id = self
                .selected_agent_key
                .strip_prefix("custom:")
                .and_then(|value| Uuid::parse_str(value).ok())
                .map(|id| id.to_string())
                .ok_or_else(|| AppError::invalid("Agent 选择项不受支持"))?;
            if !ids.contains(&id) {
                return Err(AppError::invalid("选中的自定义 Agent 不存在"));
            }
            self.selected_agent_key = format!("custom:{id}");
        }
        Ok(self)
    }
}

/// SFTP 并发和在途窗口设置。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct SftpPerformanceSettings {
    pub global_transfers: usize,
    pub host_transfers: usize,
    pub pipeline_depth: usize,
    pub packet_bytes: u32,
    /// 磁盘契约使用 `idleTimeoutSeconds`；旧版本改名只由一次性数据迁移处理。
    pub idle_timeout_seconds: u16,
}

impl Default for SftpPerformanceSettings {
    /// 缺字段时沿用现行并发和 2 MiB 在途数据默认边界。
    fn default() -> Self {
        Self {
            global_transfers: 4,
            host_transfers: 2,
            pipeline_depth: 8,
            packet_bytes: 256 * 1024,
            idle_timeout_seconds: 180,
        }
    }
}

impl SftpPerformanceSettings {
    /// 限制并发和网络窗口，且单主机任务数不能超过全局预算。
    pub(super) fn normalized(mut self) -> Self {
        self.global_transfers = self.global_transfers.clamp(1, 16);
        self.host_transfers = self.host_transfers.clamp(1, 8).min(self.global_transfers);
        self.pipeline_depth = self.pipeline_depth.clamp(1, 256);
        self.packet_bytes = self.packet_bytes.clamp(32 * 1024, 256 * 1024);
        self.idle_timeout_seconds = self.idle_timeout_seconds.clamp(30, 3600);
        self
    }
}

/// 全局工作台的稳定布局意图；尺寸使用逻辑像素且不含分隔槽。
///
/// @author kongweiguang
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkbenchLayoutSettings {
    pub host_sidebar_collapsed: bool,
    pub host_sidebar_width: u16,
    /// 按稳定 ToolId 稀疏保存；缺少项继续使用对应工具的动态默认宽度。
    pub dock_panel_widths: BTreeMap<ToolRailToolId, u16>,
    pub bottom_panel_height: u16,
}

impl Default for WorkbenchLayoutSettings {
    /// 缺字段或新配置根沿用当前工作台首次启动布局，不在读取时写出配置。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            host_sidebar_collapsed: false,
            host_sidebar_width: 220,
            dock_panel_widths: BTreeMap::new(),
            bottom_panel_height: 240,
        }
    }
}

impl WorkbenchLayoutSettings {
    /// 将持久布局限制在工作台可交互范围；响应式夹紧尺寸仍由 Root 临时投影而不写回。
    ///
    /// @author kongweiguang
    pub(super) fn normalized(mut self) -> Self {
        self.host_sidebar_width = self.host_sidebar_width.clamp(168, 360);
        for width in self.dock_panel_widths.values_mut() {
            *width = (*width).clamp(130, 1_360);
        }
        self.bottom_panel_height = self.bottom_panel_height.clamp(180, 4_000);
        self
    }
}
/// 右栏工具标识；序列化拼写与现行 TOML 的 camelCase 值一致。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum ToolRailToolId {
    Context,
    AgentLauncher,
    Sftp,
    Containers,
    Snippets,
    Tmux,
    Ports,
    System,
    Logs,
}

/// 工具面板在工作区中的停靠方式。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ToolRailPanelPlacement {
    #[default]
    Attached,
    Left,
    Bottom,
    Center,
}

/// 工具顺序、可见性、底部固定分区和每工具的面板位置。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolRailSettings {
    pub order: Vec<ToolRailToolId>,
    pub hidden: Vec<ToolRailToolId>,
    pub bottom: Vec<ToolRailToolId>,
    pub panel_placements: BTreeMap<ToolRailToolId, ToolRailPanelPlacement>,
}

impl Default for ToolRailSettings {
    /// 沿用历史右栏顺序，并默认只将命令历史固定在底部。
    fn default() -> Self {
        let order = default_tool_rail_order();
        Self {
            panel_placements: order
                .iter()
                .copied()
                .map(|tool| (tool, ToolRailPanelPlacement::Attached))
                .collect(),
            order,
            hidden: Vec::new(),
            bottom: vec![ToolRailToolId::Logs],
        }
    }
}

impl ToolRailSettings {
    /// 存量 Files 与 Containers 停靠值继续可读，但运行时投影固定到右侧抽屉，保留配置兼容性。
    ///
    /// 这是只读有效视图，不归一化或重写持久化的每工具位置表。
    ///
    /// @author kongweiguang
    pub fn effective_panel_placement(&self, tool: ToolRailToolId) -> ToolRailPanelPlacement {
        if matches!(tool, ToolRailToolId::Sftp | ToolRailToolId::Containers) {
            return ToolRailPanelPlacement::Attached;
        }

        self.panel_placements
            .get(&tool)
            .copied()
            .unwrap_or_default()
    }

    /// 去重并补齐未来版本新增工具，同时始终保留至少一个可发现入口。
    ///
    /// @author kongweiguang
    pub(super) fn normalized(mut self) -> Self {
        let mut order = Vec::new();
        for tool in self.order {
            if !order.contains(&tool) {
                order.push(tool);
            }
        }
        for tool in default_tool_rail_order() {
            if !order.contains(&tool) {
                order.push(tool);
            }
        }
        let mut hidden = Vec::new();
        for tool in self.hidden {
            if order.contains(&tool) && !hidden.contains(&tool) {
                hidden.push(tool);
            }
        }
        if hidden.len() >= order.len()
            && let Some(first) = order.first()
        {
            hidden.retain(|tool| tool != first);
        }
        let bottom = order
            .iter()
            .copied()
            .filter(|tool| self.bottom.contains(tool))
            .collect();
        let mut placements = Self::default().panel_placements;
        for (tool, placement) in self.panel_placements {
            if order.contains(&tool) {
                placements.insert(tool, placement);
            }
        }
        self.order = order;
        self.hidden = hidden;
        self.bottom = bottom;
        self.panel_placements = placements;
        self
    }
}

/// 默认顺序保留旧版习惯，并将容器入口紧随文件工具，方便按工作场景切换。
///
/// @author kongweiguang
fn default_tool_rail_order() -> Vec<ToolRailToolId> {
    vec![
        ToolRailToolId::Context,
        ToolRailToolId::AgentLauncher,
        ToolRailToolId::Sftp,
        ToolRailToolId::Containers,
        ToolRailToolId::Snippets,
        ToolRailToolId::Tmux,
        ToolRailToolId::Ports,
        ToolRailToolId::System,
        ToolRailToolId::Logs,
    ]
}
