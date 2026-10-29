//! 仅供隔离验收调用 Commands 生产目录、Fill 与 Run 路径。
//!
//! @author kongweiguang

use std::future::Future;

use gpui_kit::{Context, Window};
use kerminal_application::commands::SnippetCatalogSection;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::commands::{SnippetCatalogOrigin, SnippetPreferenceKey};

use super::{CommandsSection, CommandsView, snippet_transfer_ui::SnippetImportPhase};

const CANCELLED_RUN_SNIPPET_ID: &str = "snippet.builtin.core.find_large_files";
const PING_SNIPPET_ID: &str = "snippet.builtin.core.ping_host";

impl CommandsView {
    /// 校验导入预览与隔离样本完全一致后，只触发一次现有生产确认和批次提交流程。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_confirm_snippet_import(
        &mut self,
        expected_file_name: &str,
        expected_title: &str,
        expected_command: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.snippet_transfer_busy {
            return Err("片段导入仍有在途操作，不能重复提交验收样本".to_owned());
        }
        let Some(review) = self.snippet_import_review.as_ref() else {
            return Err("片段导入预览已关闭，拒绝提交过期样本".to_owned());
        };
        if review.file_name != expected_file_name
            || review.phase != SnippetImportPhase::Reviewing
            || !review.issues.is_empty()
            || review.candidates.len() != 1
            || review.selected_count() != 1
        {
            return Err("片段导入预览与隔离验收条件不一致".to_owned());
        }
        let candidate = &review.candidates[0];
        if candidate.title != expected_title
            || candidate.command != expected_command
            || !candidate.selected
        {
            return Err("片段导入候选与隔离验收样本不一致".to_owned());
        }
        self.confirm_snippet_import(cx);
        if !self.snippet_transfer_busy {
            return Err("生产片段导入确认没有启动批次提交".to_owned());
        }
        Ok(())
    }

    /// 只在原 fixture pane 的 Mine 查询完成且导入预览已清理后报告 UI 刷新就绪。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snippet_import_page_ready(
        &self,
        binding: &UiSessionBinding,
        expected_file_name: &str,
        cx: &gpui_kit::App,
    ) -> Result<bool, String> {
        if self.ui_session_binding.as_ref() != Some(binding) {
            return Ok(false);
        }
        if self.snippet_transfer_busy {
            return Ok(false);
        }
        if let Some(review) = self.snippet_import_review.as_ref() {
            if review.file_name != expected_file_name {
                return Err("片段导入验收期间出现了其它文件预览".to_owned());
            }
            if matches!(
                review.phase,
                SnippetImportPhase::NotCommitted | SnippetImportPhase::OutcomeUnknown
            ) {
                return Err(review
                    .status
                    .clone()
                    .unwrap_or_else(|| "片段导入没有完成提交".to_owned()));
            }
            return Ok(false);
        }
        Ok(self.snippet_catalog_section == SnippetCatalogSection::Mine
            && self.right_rail_acceptance_snippet_directory_ready(binding, cx))
    }

    /// 通过 CommandsService 导出真实用户目录，拥有化输入且不捕获视图引用，核对唯一标题和原始命令。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_read_imported_snippet(
        &self,
        expected_title: String,
        expected_command: String,
    ) -> impl Future<Output = Result<bool, String>> + 'static + use<> {
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        async move {
            match executor
                .run(move |_| async move { commands.export_user_snippets().await })
                .await
            {
                Ok(source) => {
                    let document = serde_json::from_str::<serde_json::Value>(&source)
                        .map_err(|_| "片段存储导出回读格式无效".to_owned())?;
                    let matches = document
                        .get("snippets")
                        .and_then(serde_json::Value::as_array)
                        .map(|snippets| {
                            snippets
                                .iter()
                                .filter(|snippet| {
                                    snippet.get("title").and_then(serde_json::Value::as_str)
                                        == Some(expected_title.as_str())
                                        && snippet
                                            .get("command")
                                            .and_then(serde_json::Value::as_str)
                                            == Some(expected_command.as_str())
                                })
                                .count()
                                == 1
                        })
                        .unwrap_or(false);
                    Ok(matches)
                }
                Err(error) => Err(error.public_message().to_owned()),
            }
        }
    }

    /// 将测试目标限制到当前 fixture pane，并用生产 Library 查询路径加载精确内置片段。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_prepare_snippet_catalog(
        &mut self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.ui_session_binding.as_ref() != Some(binding) {
            return Err("Commands 当前终端绑定与 fixture pane 不一致".to_owned());
        }
        self.select_snippets(cx);
        if self.section != CommandsSection::Snippets {
            return Err("Commands 仍处于未关闭的片段编辑状态".to_owned());
        }
        self.select_snippet_catalog_section(SnippetCatalogSection::Library, window, cx);
        self.set_snippet_query(snippet_id.to_owned(), window, cx);
        Ok(true)
    }

    /// 目录必须是当前 fixture binding 下成功加载且精确包含指定 built-in 的生产页。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snippet_catalog_ready(
        &self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        cx: &gpui_kit::App,
    ) -> bool {
        self.right_rail_acceptance_snippet_directory_ready(binding, cx)
            && self
                .snippet_page_items
                .iter()
                .any(|item| item.origin == SnippetCatalogOrigin::Builtin && item.id == snippet_id)
    }

    /// 只在精确目录项已就绪后走真实详情读取，避免验收直接伪造片段定义或 revision。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_snippet(
        &mut self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.ui_session_binding.as_ref() != Some(binding) {
            return Err("Commands 当前终端绑定与 fixture pane 不一致".to_owned());
        }
        if !self.right_rail_acceptance_snippet_catalog_ready(binding, snippet_id, cx) {
            return Ok(false);
        }
        self.open_snippet_key(
            SnippetPreferenceKey {
                origin: SnippetCatalogOrigin::Builtin,
                snippet_id: snippet_id.to_owned(),
            },
            window,
            cx,
        );
        Ok(true)
    }

    /// 详情就绪同时复核选择来源、目录项和 pane generation，迟到响应不能操作其它目标。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snippet_ready(
        &self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        cx: &gpui_kit::App,
    ) -> bool {
        let key = SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::Builtin,
            snippet_id: snippet_id.to_owned(),
        };
        self.ui_session_binding.as_ref() == Some(binding)
            && self.right_rail_acceptance_snippet_catalog_ready(binding, snippet_id, cx)
            && self.selected_snippet_matches_key(&key)
    }

    /// 只允许为当前非敏感、已加载 fixture 片段填写已声明变量，保留生产插值与校验流程。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_set_snippet_variable(
        &mut self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        variable_name: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_snippet_ready(binding, snippet_id, cx) {
            return Ok(false);
        }
        let Some(field) = self
            .snippet_variables
            .iter()
            .find(|field| field.variable.name == variable_name)
        else {
            return Err("验收片段缺少请求的变量".to_owned());
        };
        if field.variable.sensitive
            || field.variable.kind == kerminal_domain::commands::SnippetVariableKind::Secret
        {
            return Err("验收不允许填写敏感片段变量".to_owned());
        }
        field.input.update(cx, |input, cx| {
            input.set_value(value.to_owned(), window, cx)
        });
        Ok(true)
    }

    /// Fill 仍调用右栏按钮所用的生产用例；generation 变化证明请求确实通过同步准入。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_fill_snippet(
        &mut self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_snippet_ready(binding, snippet_id, cx) {
            return Ok(false);
        }
        let generation = self.snippet_fill_generation;
        self.fill_snippet_terminal(window, cx);
        Ok(self.snippet_fill_generation != generation)
    }

    /// Run 仍调用生产确认策略；返回值仅表示弹框已挂起或直接写入已开始，不声称 shell 完成。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_request_snippet_run(
        &mut self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_snippet_ready(binding, snippet_id, cx) {
            return Ok(false);
        }
        let generation = self.snippet_fill_generation;
        self.request_snippet_run_terminal(window, cx);
        Ok(self.pending_snippet_run.is_some() || self.snippet_fill_generation != generation)
    }

    /// 等待器只接受当前目标的 pending 确认；无需暴露命令正文或可变确认计划。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snippet_confirmation_pending(
        &self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        cx: &gpui_kit::App,
    ) -> bool {
        self.right_rail_acceptance_snippet_ready(binding, snippet_id, cx)
            && self.pending_snippet_run.is_some()
    }

    /// 取消回读要求原 binding 与片段仍选中，并确认生产 Dialog on_close 已清除 pending run。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snippet_confirmation_cancelled(
        &self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        cx: &gpui_kit::App,
    ) -> bool {
        self.right_rail_acceptance_snippet_ready(binding, snippet_id, cx)
            && self.pending_snippet_run.is_none()
            && !self.snippet_terminal_fill_in_flight()
    }

    /// 只为已取消高 IO 样例到 ping 的精确 builtin intent 复用生产“放弃并继续”转换。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_discard_cancelled_run_transition_to_ping(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.ui_session_binding.as_ref() != Some(binding) {
            return Err("Commands 当前终端绑定与 fixture pane 不一致".to_owned());
        }
        let cancelled_source = SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::Builtin,
            snippet_id: CANCELLED_RUN_SNIPPET_ID.to_owned(),
        };
        if !self.selected_snippet_matches_key(&cancelled_source) {
            return Err("拒绝丢弃非已取消验收片段的运行变量".to_owned());
        }
        if self.pending_snippet_run.is_some() || self.snippet_terminal_fill_in_flight() {
            return Err("已取消验收片段仍有未完成的终端 Run".to_owned());
        }
        let expected_transition = super::EditorTransition::OpenKey(SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::Builtin,
            snippet_id: PING_SNIPPET_ID.to_owned(),
        });
        if self.pending_snippet_transition.as_ref() != Some(&expected_transition) {
            return Err("拒绝丢弃非 ping builtin 的片段切换意图".to_owned());
        }

        self.discard_snippet_transition(window, cx);
        Ok(self.pending_snippet_transition.is_none())
    }

    /// 回执等待以原 pane 和片段身份为界；未知/迟到结果不会解锁验收到另一目标。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snippet_action_idle(
        &self,
        binding: &UiSessionBinding,
        snippet_id: &str,
        cx: &gpui_kit::App,
    ) -> bool {
        self.right_rail_acceptance_snippet_ready(binding, snippet_id, cx)
            && !self.snippet_terminal_fill_in_flight()
    }
}
