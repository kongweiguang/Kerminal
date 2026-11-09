//! 外部插件验收只借用生产导航和只读实体状态，不创建验收工厂或改写插件数据。
//! @author kongweiguang

use super::*;
use crate::plugins::{
    actions::OpenContentTab, content::ContentScope, external::renderer::NativeRenderer,
};
use serde_json::{Value, json};

const NOTES_CONTENT: &str = "acme.notes/notes-editor";
const NOTES_TOOL: &str = "acme.notes/notes-panel";

impl RootView {
    /// 快照来自实际外部工厂创建的实体；缺失或 downcast 失败保留 null，使验收失败。
    pub fn external_plugin_acceptance_snapshot(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Value {
        let mut snapshot = self.plugin_acceptance_snapshot(cx);
        if let Some(tabs) = snapshot["tabs"].as_array_mut() {
            for tab in tabs {
                let renderer = tab["tabId"]
                    .as_str()
                    .and_then(|id| TabId::parse(id).ok())
                    .and_then(|id| self.content_host.view(&ContentScope::Tab(id)))
                    .and_then(|view| view.downcast::<NativeRenderer>().ok())
                    .map(|view| view.read(cx).external_plugin_acceptance_snapshot(cx));
                tab["renderer"] = renderer.unwrap_or(Value::Null);
            }
        }
        let context = self.plugin_tool_context(cx);
        let tool = ToolRailToolId::from_id(NOTES_TOOL).ok();
        let tool_view = tool.and_then(|tool| self.plugin_tools.view(tool, &context));
        let tool_mounted = tool.is_some_and(|tool| {
            self.plugin_tools
                .view_for_mount(
                    tool,
                    &context,
                    window.is_window_active() || self.right_rail_acceptance_active_override,
                )
                .is_some()
        });
        let tool_snapshot = tool_view
            .and_then(|view| view.downcast::<NativeRenderer>().ok())
            .map(|view| view.read(cx).external_plugin_acceptance_snapshot(cx));
        snapshot["tool"] = tool_snapshot.unwrap_or(Value::Null);
        snapshot["toolMounted"] = json!(tool_mounted);
        snapshot["preferencesOpen"] = json!(self.preferences_modal_open);
        snapshot["settingsReady"] = json!(self.settings_snapshot.is_some());
        snapshot["settings"] = self
            .preferences_panel
            .read(cx)
            .external_plugin_acceptance_snapshot(cx);
        snapshot["windowActive"] = json!(window.is_window_active());
        snapshot["registeredContent"] =
            json!(self.services.plugins.content_type(NOTES_CONTENT).is_some());
        snapshot
    }

    /// 通过工作区事务创建正式内容标签，状态恢复和关闭仍由原宿主负责。
    pub fn external_plugin_acceptance_open_notes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<TabId> {
        self.open_plugin_tab(
            &OpenContentTab {
                content_id: NOTES_CONTENT.into(),
                title: "Workspace Notes".into(),
                state_version: 1,
                state: json!({}),
            },
            window,
            cx,
        )
    }

    /// 导航复用工具栏实际切换路径，不提前挂载假实体或改写布局快照。
    pub fn external_plugin_acceptance_toggle_notes_tool(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let tool = ToolRailToolId::from_id(NOTES_TOOL)
            .map_err(|_| kerminal_application::AppError::invalid("笔记工具身份无效"))?;
        self.toggle_tool_rail_panel(tool, window, cx);
        Ok(())
    }

    /// 设置页通过原有偏好表面打开；失败附只读诊断，防止通用导航错误掩盖页面构造原因。
    pub fn external_plugin_acceptance_open_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        self.open_preferences_modal(window, cx);
        let result = self.preferences_panel.update(cx, |panel, cx| {
            panel.external_plugin_acceptance_open("acme.notes", window, cx)
        });
        result.map_err(|error| {
            let snapshot = self
                .preferences_panel
                .read(cx)
                .external_plugin_acceptance_snapshot(cx);
            error.with_source(std::io::Error::other(format!("设置页诊断：{snapshot}")))
        })
    }
}
