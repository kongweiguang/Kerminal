//! SSH 分屏中的 ZMODEM 用户交互；实际协议与文件操作由会话服务持有。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet};

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, PathPromptOptions, Window,
    component::button::{Button, ButtonVariants as _},
    div,
    prelude::*,
};
use kerminal_application::sessions::{
    UiSessionBinding, ZmodemDecision, ZmodemDirection, ZmodemPhase, ZmodemStatus,
};
use kerminal_domain::{
    identity::{PaneId, SessionId},
    sessions::SessionOwner,
};

use super::RootView;
use crate::{locale, theme::ThemeTokens};

/// 选择器和状态均绑定会话代次，旧选择不能作用于重新连接后的新传输。
#[derive(Default)]
pub(super) struct ZmodemUiState {
    statuses: HashMap<SessionId, (UiSessionBinding, ZmodemStatus)>,
    pickers: HashSet<(SessionId, u64, u64)>,
}

impl ZmodemUiState {
    /// 关闭分屏时释放对应投影；仍打开的后台标签保留自己的传输状态。
    pub(super) fn retain_panes(&mut self, panes: &HashSet<PaneId>) {
        self.statuses.retain(|_, (binding, _)| matches!(&binding.owner, SessionOwner::Ui { pane_id, .. } if panes.contains(pane_id)));
        self.pickers.retain(|(session_id, generation, id)| {
            self.statuses
                .get(session_id)
                .is_some_and(|(binding, status)| {
                    binding.session_generation == *generation && status.id == *id
                })
        });
    }

    /// 状态来源于同一快照请求，消失时移除旧投影以免显示已结束传输。
    pub(super) fn update(
        &mut self,
        binding: UiSessionBinding,
        status: Option<ZmodemStatus>,
    ) -> Option<u64> {
        let finished = status
            .as_ref()
            .filter(|status| status.phase == ZmodemPhase::Finished)
            .filter(|status| {
                self.status(&binding).is_none_or(|current| {
                    current.id != status.id || current.phase != ZmodemPhase::Finished
                })
            })
            .map(|status| status.id);
        if let Some(status) = status {
            self.statuses
                .insert(binding.session_id.clone(), (binding, status));
        } else if self
            .statuses
            .get(&binding.session_id)
            .is_some_and(|(current, _)| current == &binding)
        {
            self.statuses.remove(&binding.session_id);
        }
        finished
    }

    /// 只暴露精确匹配的代次，重连之后立即隐藏旧传输。
    fn status(&self, binding: &UiSessionBinding) -> Option<&ZmodemStatus> {
        self.statuses
            .get(&binding.session_id)
            .and_then(|(current, status)| (current == binding).then_some(status))
    }

    /// 仅等待选择的当前传输可接受原生选择器回执。
    fn accepts_selection(&self, binding: &UiSessionBinding, id: u64) -> bool {
        self.status(binding).is_some_and(|status| {
            status.id == id && status.phase == ZmodemPhase::WaitingForSelection
        })
    }

    /// Esc 和无选区 Ctrl+C 共用同一阶段门，避免拦截普通 shell 输入。
    pub(super) fn cancellable_id(&self, binding: &UiSessionBinding) -> Option<u64> {
        self.status(binding)
            .filter(|status| {
                matches!(
                    status.phase,
                    ZmodemPhase::WaitingForSelection | ZmodemPhase::Transferring
                )
            })
            .map(|status| status.id)
    }
}

impl RootView {
    /// 已完成提示短暂保留打开目录的入口；计时结束时再次校验身份，不能关闭新传输。
    pub(super) fn schedule_zmodem_finished_dismiss(
        &mut self,
        binding: UiSessionBinding,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(8))
                .await;
            let _ =
                this.update(cx, |root, cx| {
                    if root.zmodem_ui.status(&binding).is_some_and(|status| {
                        status.id == id && status.phase == ZmodemPhase::Finished
                    }) {
                        root.respond_zmodem(binding, id, ZmodemDecision::Dismiss, cx);
                    }
                });
        })
        .detach();
    }

    /// 传输浮层绝对定位在所属画布内，不改变终端网格或其他分屏几何。
    pub(super) fn render_zmodem_overlay(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let status = self.zmodem_ui.status(binding)?.clone();
        let theme = ThemeTokens::current(window, cx);
        let title = match (status.direction, status.phase) {
            (ZmodemDirection::Upload, ZmodemPhase::WaitingForSelection) => {
                locale::text("上传文件", "Upload files")
            }
            (ZmodemDirection::Download, ZmodemPhase::WaitingForSelection) => {
                locale::text("下载文件", "Download files")
            }
            (_, ZmodemPhase::Transferring) => locale::text("正在传输", "Transferring"),
            (_, ZmodemPhase::Cancelling) => locale::text("正在取消", "Cancelling"),
            (_, ZmodemPhase::Finished) => locale::text("传输完成", "Transfer complete"),
            (_, ZmodemPhase::Cancelled) => locale::text("传输已取消", "Transfer cancelled"),
            (_, ZmodemPhase::Failed) => locale::text("传输失败", "Transfer failed"),
            (_, ZmodemPhase::NeedsReconnect) => {
                locale::text("远端未确认退出", "Remote did not confirm exit")
            }
        };
        let detail = match status.phase {
            ZmodemPhase::WaitingForSelection => match status.direction {
                ZmodemDirection::Upload => {
                    locale::text("选择要上传到远端的文件", "Choose files to upload")
                }
                ZmodemDirection::Download => {
                    locale::text("选择本机保存位置", "Choose a local destination")
                }
            }
            .to_string(),
            ZmodemPhase::Transferring => {
                let progress = status.total.map_or_else(
                    || format!("{} B", status.bytes),
                    |total| format!("{} / {} B", status.bytes, total),
                );
                status
                    .file_name
                    .as_ref()
                    .map_or(progress.clone(), |name| format!("{name} · {progress}"))
            }
            ZmodemPhase::Cancelling => {
                locale::text("正在等待远端退出…", "Waiting for the remote to stop…").to_string()
            }
            ZmodemPhase::Finished | ZmodemPhase::Cancelled => format!(
                "{} {}",
                locale::text("已完成文件：", "Files completed:"),
                status.completed
            ),
            ZmodemPhase::Failed | ZmodemPhase::NeedsReconnect => {
                status.error.unwrap_or("请检查连接并重试。").to_owned()
            }
        };
        let mut actions = div().flex().items_center().gap_2();
        if status.phase == ZmodemPhase::WaitingForSelection {
            let selected_binding = binding.clone();
            let id = status.id;
            let direction = status.direction;
            actions = actions.child(
                Button::new(format!("zmodem-select-{}", binding.session_id.as_str()))
                    .primary()
                    .label(match direction {
                        ZmodemDirection::Upload => locale::text("选择文件…", "Choose files…"),
                        ZmodemDirection::Download => {
                            locale::text("选择保存位置…", "Choose destination…")
                        }
                    })
                    .on_click(cx.listener(move |root, _: &ClickEvent, _, cx| {
                        root.select_zmodem_paths(selected_binding.clone(), id, direction, cx);
                    })),
            );
        }
        if matches!(
            status.phase,
            ZmodemPhase::WaitingForSelection | ZmodemPhase::Transferring
        ) {
            let cancel_binding = binding.clone();
            let id = status.id;
            actions = actions.child(
                Button::new(format!("zmodem-cancel-{}", binding.session_id.as_str()))
                    .ghost()
                    .label(locale::text("取消", "Cancel"))
                    .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                        root.respond_zmodem(cancel_binding.clone(), id, ZmodemDecision::Cancel, cx);
                        root.restore_zmodem_focus(&cancel_binding, window, cx);
                    })),
            );
        }
        if status.phase == ZmodemPhase::NeedsReconnect {
            let reconnect_binding = binding.clone();
            actions = actions.child(
                Button::new(format!("zmodem-reconnect-{}", binding.session_id.as_str()))
                    .primary()
                    .label(locale::text("重新连接", "Reconnect"))
                    .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                        if let SessionOwner::Ui { tab_id, pane_id } = &reconnect_binding.owner {
                            root.reconnect_workspace_pane(
                                tab_id.clone(),
                                pane_id.clone(),
                                reconnect_binding.clone(),
                                window,
                                cx,
                            );
                        }
                    })),
            );
        }
        if matches!(
            status.phase,
            ZmodemPhase::Finished | ZmodemPhase::Cancelled | ZmodemPhase::Failed
        ) {
            if status.phase == ZmodemPhase::Finished
                && let Some(directory) = status.save_directory.clone()
            {
                actions = actions.child(
                    Button::new(format!("zmodem-open-{}", binding.session_id.as_str()))
                        .ghost()
                        .label(locale::text("打开文件夹", "Open folder"))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.open_with_system(&directory)
                        })),
                );
            }
            let dismiss_binding = binding.clone();
            let id = status.id;
            actions = actions.child(
                Button::new(format!("zmodem-dismiss-{}", binding.session_id.as_str()))
                    .ghost()
                    .label(locale::text("关闭", "Dismiss"))
                    .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                        root.respond_zmodem(
                            dismiss_binding.clone(),
                            id,
                            ZmodemDecision::Dismiss,
                            cx,
                        );
                        root.restore_zmodem_focus(&dismiss_binding, window, cx);
                    })),
            );
        }
        let key_binding = binding.clone();
        Some(
            div()
                .id(format!("zmodem-overlay-{}", binding.session_id.as_str()))
                .absolute()
                .top_2()
                .right_2()
                .max_w_full()
                .p_3()
                .gap_2()
                .flex()
                .flex_col()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.floating_surface)
                .text_color(theme.foreground)
                .on_key_down(cx.listener(
                    move |root, event: &gpui_kit::KeyDownEvent, window, cx| {
                        if !event.is_held
                            && !event.keystroke.modifiers.modified()
                            && event.keystroke.key.eq_ignore_ascii_case("escape")
                            && root.cancel_zmodem_for_binding(&key_binding, cx)
                        {
                            root.restore_zmodem_focus(&key_binding, window, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .child(
                    div()
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(div().text_color(theme.muted).child(detail))
                .child(actions)
                .into_any_element(),
        )
    }

    /// 系统选择器只由显式点击打开；重复点击和迟到结果都不能抢占另一代会话。
    fn select_zmodem_paths(
        &mut self,
        binding: UiSessionBinding,
        id: u64,
        direction: ZmodemDirection,
        cx: &mut Context<Self>,
    ) {
        let key = (binding.session_id.clone(), binding.session_generation, id);
        if !self.zmodem_ui.accepts_selection(&binding, id)
            || !self.zmodem_ui.pickers.insert(key.clone())
        {
            return;
        }
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: direction == ZmodemDirection::Upload,
            directories: direction == ZmodemDirection::Download,
            multiple: direction == ZmodemDirection::Upload,
            prompt: Some(match direction {
                ZmodemDirection::Upload => {
                    locale::text("选择要上传的文件", "Choose files to upload")
                }
                ZmodemDirection::Download => locale::text("选择保存位置", "Choose a destination"),
            }),
        });
        cx.spawn(async move |this, cx| {
            let result = selected.await;
            let _ = this.update(cx, |root, cx| {
                root.zmodem_ui.pickers.remove(&key);
                if root.terminal_view_bindings.get(&binding.session_id) != Some(&binding)
                    || !root.zmodem_ui.accepts_selection(&binding, id)
                {
                    return;
                }
                let decision = match result {
                    Ok(Ok(Some(paths))) if !paths.is_empty() => match direction {
                        ZmodemDirection::Upload => ZmodemDecision::Upload(paths),
                        ZmodemDirection::Download => ZmodemDecision::Download(paths[0].clone()),
                    },
                    Ok(Ok(None)) => ZmodemDecision::Cancel,
                    _ => {
                        root.status = Some(locale::text(
                            "无法打开文件选择器，传输已取消。",
                            "Could not open the file picker; transfer cancelled.",
                        ));
                        ZmodemDecision::Cancel
                    }
                };
                root.respond_zmodem(binding, id, decision, cx);
            });
        })
        .detach();
    }

    /// 同一按钮和键盘路径都向后端提交带代次的意图，结果不能写回其他分屏。
    pub(super) fn respond_zmodem(
        &mut self,
        binding: UiSessionBinding,
        id: u64,
        decision: ZmodemDecision,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_view_bindings.get(&binding.session_id) != Some(&binding)
            || self
                .zmodem_ui
                .status(&binding)
                .is_none_or(|status| status.id != id)
        {
            return;
        }
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let callback_binding = binding.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { sessions.respond_zmodem(binding, id, decision).await })
                .await;
            let _ = this.update(cx, |root, cx| {
                if root
                    .terminal_view_bindings
                    .get(&callback_binding.session_id)
                    != Some(&callback_binding)
                {
                    return;
                }
                if let Err(error) = result {
                    root.status = Some(error.public_message().into());
                }
                if let SessionOwner::Ui { tab_id, pane_id } = &callback_binding.owner {
                    root.request_workspace_pane_snapshot(tab_id.clone(), pane_id.clone(), cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 中断手势只作用于活动传输；普通终端和已完成传输继续使用原有键盘语义。
    pub(super) fn cancel_zmodem_for_binding(
        &mut self,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.zmodem_ui.cancellable_id(binding) else {
            return false;
        };
        self.respond_zmodem(binding.clone(), id, ZmodemDecision::Cancel, cx);
        true
    }

    /// 只有原标签仍可见且会话绑定未改变时回焦，避免跨标签劫持用户操作。
    fn restore_zmodem_focus(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_view_bindings.get(&binding.session_id) != Some(binding) {
            return;
        }
        if let SessionOwner::Ui { tab_id, pane_id } = &binding.owner
            && self.workspace_tab_is_visible(tab_id)
            && window.is_window_active()
            && self
                .workspace_controller
                .as_ref()
                .is_some_and(|controller| {
                    controller.snapshot().focused_pane_id.as_ref() == Some(pane_id)
                })
        {
            self.request_workspace_pane_focus(tab_id.clone(), pane_id.clone(), window, cx);
        }
    }

    /// 私有桌面验收只观察真实 fixture 分屏的传输投影和终端输出，禁止跨会话取样。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn right_rail_acceptance_zmodem_observe(
        &self,
        host_id: &kerminal_domain::identity::HostId,
    ) -> Result<Option<(UiSessionBinding, Option<ZmodemStatus>, String)>, String> {
        let Some(binding) = self.right_rail_acceptance_fixture_binding(host_id)? else {
            return Ok(None);
        };
        let Some(snapshot) = self.terminal_snapshot_for_binding(&binding) else {
            return Ok(None);
        };
        let text = snapshot
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
        Ok(Some((
            binding.clone(),
            self.zmodem_ui.status(&binding).cloned(),
            text,
        )))
    }

    /// 验收焦点经过真实 pane 绑定入口，键盘事件仍通过生产处理链分发。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn right_rail_acceptance_zmodem_focus(
        &mut self,
        host_id: &kerminal_domain::identity::HostId,
        expected: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .as_ref()
            != Some(expected)
        {
            return Ok(false);
        }
        let SessionOwner::Ui { tab_id, pane_id } = &expected.owner else {
            return Ok(false);
        };
        self.request_workspace_pane_focus(tab_id.clone(), pane_id.clone(), window, cx);
        Ok(self
            .pane_input_handlers
            .get(&expected.session_id)
            .is_some_and(|registration| {
                registration.binding == *expected
                    && registration
                        .entity
                        .read(cx)
                        .focus_handle()
                        .is_focused(window)
            }))
    }

    /// 验收通过同一生产确认入口传递隔离目录，避免系统对话框依赖当前可见桌面。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn right_rail_acceptance_zmodem_download(
        &mut self,
        host_id: &kerminal_domain::identity::HostId,
        expected: &UiSessionBinding,
        directory: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .as_ref()
            != Some(expected)
        {
            return Ok(false);
        }
        let Some(status) = self.zmodem_ui.status(expected) else {
            return Ok(false);
        };
        if status.direction != ZmodemDirection::Download
            || status.phase != ZmodemPhase::WaitingForSelection
        {
            return Ok(false);
        }
        let id = status.id;
        self.respond_zmodem(
            expected.clone(),
            id,
            ZmodemDecision::Download(directory),
            cx,
        );
        Ok(true)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_zmodem.rs"]
mod tests;
