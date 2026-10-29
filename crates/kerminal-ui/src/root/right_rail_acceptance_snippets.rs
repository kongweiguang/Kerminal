//! 只在右栏验收 feature 下桥接 fixture host 与 Commands/终端生产视图。
//!
//! @author kongweiguang

use std::future::Future;

use gpui_kit::{Context, Entity, Window};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::identity::HostId;
use kerminal_terminal::snapshot::RenderSnapshot;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

use super::RootView;
use crate::features::commands::CommandsView;

impl RootView {
    /// 验收只替代系统文件选择结果，仍由 Commands 的真实读取和校验流程生成预览。
    /// 先校验活动 fixture 目标，避免隐藏测试误触其它工作区的片段视图。
    ///
    /// @author kongweiguang
    pub fn preview_right_rail_acceptance_snippet_import(
        &mut self,
        host_id: &HostId,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self
            .right_rail_acceptance_snippets_binding(host_id)?
            .is_none()
        {
            return Ok(false);
        }
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.preview_snippet_import_path_for_acceptance(path, cx)
            })
    }

    /// 同时等待真实候选与 Root 模态 owner，不能将异步读取受理当作弹框已挂载。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_import_ready(
        &self,
        host_id: &HostId,
        expected_file_name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self
            .right_rail_acceptance_snippets_binding(host_id)?
            .is_none()
        {
            return Ok(false);
        }
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .snippet_import_review_ready_for_acceptance(expected_file_name)?
            && self.snippet_editor_modal_open)
    }

    /// 仅在当前 Snippets fixture 预览与样本字段完全一致时调用生产的一次性确认入口。
    ///
    /// @author kongweiguang
    pub fn confirm_right_rail_acceptance_snippet_import(
        &mut self,
        host_id: &HostId,
        expected_file_name: &str,
        expected_title: &str,
        expected_command: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self
            .right_rail_acceptance_snippets_binding(host_id)?
            .is_none()
        {
            return Err("片段导入验收 fixture SSH 目标尚未就绪".to_owned());
        }
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_confirm_snippet_import(
                    expected_file_name,
                    expected_title,
                    expected_command,
                    cx,
                )
            })
    }

    /// 等待生产确认成功后的弹框收起与 Mine 目录刷新，不执行写入或再次提交。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_import_committed(
        &self,
        host_id: &HostId,
        expected_file_name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_snippet_editor_closed(cx) {
            return Ok(false);
        }
        let commands = self.right_rail_acceptance_commands_view()?;
        commands
            .read(cx)
            .right_rail_acceptance_snippet_import_page_ready(&binding, expected_file_name, cx)
    }

    /// 构造拥有所有输入的存储回查 future，显式排除 2024 隐式引用捕获；执行仍交给共享应用 executor。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_import_readback(
        &self,
        host_id: &HostId,
        expected_title: String,
        expected_command: String,
        cx: &Context<Self>,
    ) -> Result<impl Future<Output = Result<bool, String>> + 'static + use<>, String> {
        if self
            .right_rail_acceptance_snippets_binding(host_id)?
            .is_none()
        {
            return Err("片段导入存储回读的 fixture SSH 目标已失效".to_owned());
        }
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .right_rail_acceptance_read_imported_snippet(expected_title, expected_command))
    }

    /// 只有活动 Snippets 工具已绑定可信 fixture SSH pane 时，验收 hook 才能触达 Commands。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_snippets_binding(
        &self,
        host_id: &HostId,
    ) -> Result<Option<UiSessionBinding>, String> {
        if self.right_rail_acceptance_active_tool()
            != Some(kerminal_application::preferences::settings::ToolRailToolId::Snippets)
        {
            return Err("Snippets 工具不是当前验收面板".to_owned());
        }
        self.right_rail_acceptance_fixture_binding(host_id)
    }

    /// Commands Entity 在 Root 内唯一持有；缺失即为验收架构错误，不静默跳过操作。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_commands_view(&self) -> Result<Entity<CommandsView>, String> {
        self.commands_view
            .clone()
            .ok_or_else(|| "Commands 片段视图不可用".to_owned())
    }

    /// 通过生产的 Library 区段和查询订阅加载 fixture 片段，不直接写目录投影。
    ///
    /// @author kongweiguang
    pub fn prepare_right_rail_acceptance_snippet(
        &mut self,
        host_id: &HostId,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_prepare_snippet_catalog(&binding, snippet_id, window, cx)
            })
    }

    /// 页面 readiness 同时要求目标 binding 与精确 built-in 目录行已由应用服务返回。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_catalog_ready(
        &self,
        host_id: &HostId,
        snippet_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .right_rail_acceptance_snippet_catalog_ready(&binding, snippet_id, cx))
    }

    /// 目录确认后复用真实详情读取，拒绝以伪造选择状态绕开来源/revision 边界。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_snippet(
        &mut self,
        host_id: &HostId,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_open_snippet(&binding, snippet_id, window, cx)
            })
    }

    /// 片段选择必须仍匹配当前 host、session generation 和实际已读详情。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_ready(
        &self,
        host_id: &HostId,
        snippet_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .right_rail_acceptance_snippet_ready(&binding, snippet_id, cx))
    }

    /// 运行时变量只写入当前 snippet 表单 Entity，实际插值仍由生产 render 与应用用例完成。
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_snippet_variable(
        &mut self,
        host_id: &HostId,
        snippet_id: &str,
        variable_name: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_set_snippet_variable(
                    &binding,
                    snippet_id,
                    variable_name,
                    value,
                    window,
                    cx,
                )
            })
    }

    /// 使用右栏的生产 Fill 用例；操作开始后由终端 cursor line 回读是否确实未提交。
    ///
    /// @author kongweiguang
    pub fn fill_right_rail_acceptance_snippet(
        &mut self,
        host_id: &HostId,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_fill_snippet(&binding, snippet_id, window, cx)
            })
    }

    /// 使用右栏 Run 生产路径，并只报告确认已挂起或写入操作已开始。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_snippet_run(
        &mut self,
        host_id: &HostId,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_request_snippet_run(&binding, snippet_id, window, cx)
            })
    }

    /// 取消派发生产 Dialog 的 Cancel action，使 on_close 清理冻结命令；通用 close_dialog 只弹栈，不能替代取消。
    ///
    /// @author kongweiguang
    pub fn cancel_right_rail_acceptance_snippet_confirmation(
        &mut self,
        host_id: &HostId,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        let commands = self.right_rail_acceptance_commands_view()?;
        if !commands
            .read(cx)
            .right_rail_acceptance_snippet_confirmation_pending(&binding, snippet_id, cx)
        {
            return Ok(false);
        }
        window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
        Ok(true)
    }

    /// 取消回执确认生产 Cancel/on_close 已撤销 pending run，且原 binding 仍保持空闲。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_confirmation_cancelled(
        &self,
        host_id: &HostId,
        snippet_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .right_rail_acceptance_snippet_confirmation_cancelled(&binding, snippet_id, cx))
    }

    /// 只在原 fixture binding 下委托 Commands 核对已取消片段到 ping 的精确待处理转换。
    ///
    /// @author kongweiguang
    pub fn discard_right_rail_acceptance_cancelled_snippet_transition_to_ping(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        self.right_rail_acceptance_commands_view()?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_discard_cancelled_run_transition_to_ping(
                    &binding, window, cx,
                )
            })
    }

    /// 只暴露确认是否仍挂起，不读取冻结命令正文或绕过生产 Dialog 的 close 回调。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_confirmation_pending(
        &self,
        host_id: &HostId,
        snippet_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .right_rail_acceptance_snippet_confirmation_pending(&binding, snippet_id, cx))
    }

    /// Run 回执仅表示当前操作不再占用 writer；shell 是否完成仍由终端输出标记单独回读。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_action_idle(
        &self,
        host_id: &HostId,
        snippet_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(false);
        };
        let commands = self.right_rail_acceptance_commands_view()?;
        Ok(commands
            .read(cx)
            .right_rail_acceptance_snippet_action_idle(&binding, snippet_id, cx))
    }

    /// 读回 fixture pane 当前 render revision，取消确认前后相等可证明没有终端输入/输出变化。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_terminal_revision(
        &self,
        host_id: &HostId,
    ) -> Result<Option<u64>, String> {
        Ok(self
            .right_rail_acceptance_terminal_snapshot(host_id)?
            .map(|snapshot| snapshot.revision))
    }

    /// 只在精确 binding 的可见 grid 中搜索输出；不触碰 session 状态。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_terminal_contains(
        &self,
        host_id: &HostId,
        expected: &str,
    ) -> Result<bool, String> {
        Ok(self
            .right_rail_acceptance_terminal_text(host_id)?
            .is_some_and(|text| text.contains(expected)))
    }

    /// 失败证据只导出隔离 fixture 的当前可见 grid，避免超时后仅留下接口受理记录。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_terminal_text(
        &self,
        host_id: &HostId,
    ) -> Result<Option<String>, String> {
        let Some(snapshot) = self.right_rail_acceptance_terminal_snapshot(host_id)? else {
            return Ok(None);
        };
        let visible_text = snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Some(visible_text))
    }

    /// 证明 Fill 后 cursor 仍停在同一未提交命令行，而不根据按钮回执推断没有发送 Enter。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_terminal_cursor_line_ends_with(
        &self,
        host_id: &HostId,
        expected: &str,
    ) -> Result<bool, String> {
        let Some(snapshot) = self.right_rail_acceptance_terminal_snapshot(host_id)? else {
            return Ok(false);
        };
        let Some(row) = snapshot
            .rows
            .iter()
            .find(|row| row.viewport_row == snapshot.cursor.row)
        else {
            return Ok(false);
        };
        let line = row
            .runs
            .iter()
            .map(|run| run.text.as_ref())
            .collect::<String>();
        let line = line.trim_end();
        Ok(line.ends_with(expected)
            && UnicodeWidthStr::width(line) == usize::from(snapshot.cursor.column))
    }

    /// 所有终端读回都重新验证当前 Snippets 工具、fixture host、binding 与活跃 render snapshot。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_terminal_snapshot(
        &self,
        host_id: &HostId,
    ) -> Result<Option<&RenderSnapshot>, String> {
        let Some(binding) = self.right_rail_acceptance_snippets_binding(host_id)? else {
            return Ok(None);
        };
        Ok(self.terminal_snapshot_for_binding(&binding))
    }
}
