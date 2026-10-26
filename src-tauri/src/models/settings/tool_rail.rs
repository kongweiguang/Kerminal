// @author kongweiguang

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 可以由用户排序和隐藏的右侧工具栏入口。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum ToolRailToolId {
    /// 当前上下文检查器。
    Context,
    /// Agent 启动与会话面板。
    AgentLauncher,
    /// SFTP 与容器文件面板。
    Sftp,
    /// 脚本片段面板。
    Snippets,
    /// tmux session/window/pane 面板。
    Tmux,
    /// SSH 端口转发面板。
    Ports,
    /// 系统资源信息面板。
    System,
    /// 当前终端命令历史面板。
    Logs,
    /// 当前 SSH 主机的 Docker、Podman 与 Compose 容器。
    Containers,
}

/// 工具内容相对工作区的展示方式。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ToolRailPanelPlacement {
    /// 使用可调整宽度的右侧贴靠面板。
    #[default]
    Attached,
    /// 使用位于主机侧栏与终端之间的可调整宽度面板。
    Left,
    /// 使用位于终端工作区下方的可调整高度面板。
    Bottom,
    /// 初始居中、可在工作区内拖动且不阻断其它操作的浮窗。
    Center,
}

/// 右侧工具栏的全局顺序、分区、可见性与内容展示偏好。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRailSettings {
    /// 完整顺序；旧配置缺少新工具时由 `normalized` 追加。
    #[serde(default = "default_tool_rail_order")]
    pub order: Vec<ToolRailToolId>,
    /// 被隐藏的工具；至少保留一个入口可发现配置编辑器。
    #[serde(default)]
    pub hidden: Vec<ToolRailToolId>,
    /// 固定在 rail 底部的工具；缺省保留历史入口的既有位置。
    #[serde(default = "default_tool_rail_bottom")]
    pub bottom: Vec<ToolRailToolId>,
    /// 每个工具独立选择右侧、左侧、底部停靠或工作区浮窗。
    #[serde(default)]
    pub panel_placements: BTreeMap<ToolRailToolId, ToolRailPanelPlacement>,
    /// 早期单一下拉框保存字段，仅用于读入迁移；写回时固定转换为逐工具映射。
    #[serde(default, rename = "panelPlacement", skip_serializing)]
    legacy_panel_placement: Option<ToolRailPanelPlacement>,
}

impl Default for ToolRailSettings {
    fn default() -> Self {
        Self {
            order: default_tool_rail_order(),
            hidden: Vec::new(),
            bottom: default_tool_rail_bottom(),
            panel_placements: default_tool_rail_panel_placements(),
            legacy_panel_placement: None,
        }
    }
}

impl ToolRailSettings {
    /// 归一化外部配置，去重补齐目录并稳定底部分区和逐工具展示偏好。
    pub fn normalized(mut self) -> Self {
        let mut order = Vec::new();
        for tool_id in self.order {
            if !order.contains(&tool_id) {
                order.push(tool_id);
            }
        }
        for tool_id in default_tool_rail_order() {
            if !order.contains(&tool_id) {
                order.push(tool_id);
            }
        }

        let mut hidden = Vec::new();
        for tool_id in self.hidden {
            if order.contains(&tool_id) && !hidden.contains(&tool_id) {
                hidden.push(tool_id);
            }
        }
        if hidden.len() >= order.len() {
            // 全部隐藏时恢复当前排序第一项，而不是默认顺序的第一项。
            if let Some(first_ordered_tool) = order.first() {
                hidden.retain(|tool_id| tool_id != first_ordered_tool);
            }
        }

        // bottom 是成员集合而非第二套顺序；按 order 输出可稳定 TOML 往返和前端 dirty 判断。
        let bottom = order
            .iter()
            .copied()
            .filter(|tool_id| self.bottom.contains(tool_id))
            .collect();
        let requested_panel_placements = std::mem::take(&mut self.panel_placements);
        let legacy_panel_placement = self.legacy_panel_placement.take();
        let mut panel_placements = default_tool_rail_panel_placements();
        if requested_panel_placements.is_empty() {
            if let Some(legacy_placement) = legacy_panel_placement {
                // 旧版只有一个全局选项；扩展到完整目录比静默退回 attached 更符合原意。
                panel_placements
                    .values_mut()
                    .for_each(|placement| *placement = legacy_placement);
            }
        }
        for (tool_id, placement) in requested_panel_placements {
            if order.contains(&tool_id) {
                panel_placements.insert(tool_id, placement);
            }
        }

        self.order = order;
        self.hidden = hidden;
        self.bottom = bottom;
        self.panel_placements = panel_placements;
        self.legacy_panel_placement = None;
        self
    }
}

/// 返回稳定的右栏默认目录；新工具只能在归一化阶段追加，避免重排旧用户入口。
fn default_tool_rail_order() -> Vec<ToolRailToolId> {
    vec![
        ToolRailToolId::Context,
        ToolRailToolId::AgentLauncher,
        ToolRailToolId::Sftp,
        ToolRailToolId::Snippets,
        ToolRailToolId::Tmux,
        ToolRailToolId::Ports,
        ToolRailToolId::System,
        ToolRailToolId::Logs,
        ToolRailToolId::Containers,
    ]
}

/// 默认只把命令历史固定到底部，延续旧版布局但不在渲染层写死特殊工具。
fn default_tool_rail_bottom() -> Vec<ToolRailToolId> {
    vec![ToolRailToolId::Logs]
}

/// 新增工具默认贴靠右栏；显式逐工具映射避免一个选择意外改变全部工作流。
fn default_tool_rail_panel_placements() -> BTreeMap<ToolRailToolId, ToolRailPanelPlacement> {
    default_tool_rail_order()
        .into_iter()
        .map(|tool_id| (tool_id, ToolRailPanelPlacement::Attached))
        .collect()
}
