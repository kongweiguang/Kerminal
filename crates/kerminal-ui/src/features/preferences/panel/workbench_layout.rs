//! 工作台布局的有界末意图队列；写入仍由同一个 settings 草稿及 CAS owner 承担。
//!
//! @author kongweiguang

use super::{PreferencesEvent, PreferencesPanel};
use crate::features::preferences::DraftStatus;
use gpui_kit::Context;
use kerminal_application::{PublicErrorCode, preferences::WorkbenchLayoutSettings};
use kerminal_domain::preferences::ConfigurationMode;

/// request 只关联原意图和回执，不表示保存已经开始或成功。
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkbenchLayoutIntent {
    request_id: u64,
    layout: WorkbenchLayoutSettings,
}

/// 最多保留一个在途值和一个末意图；保存资格始终读取现有 ConfigDraft 状态。
#[derive(Default)]
pub(super) struct WorkbenchLayoutRequests {
    next_request_id: u64,
    pending: Option<WorkbenchLayoutIntent>,
    in_flight: Option<WorkbenchLayoutIntent>,
    /// 读回 Ready 不是重放授权；只有新显式意图或相等事实核对可以解除此 fence。
    blocked_after_failure: bool,
}

impl WorkbenchLayoutRequests {
    /// 新显式手势覆盖尚未发送的旧意图，但绝不改变原在途请求的关联。
    ///
    /// @author kongweiguang
    fn request(&mut self, layout: WorkbenchLayoutSettings) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        self.pending = Some(WorkbenchLayoutIntent { request_id, layout });
        self.blocked_after_failure = false;
        request_id
    }

    /// 取走末意图之前同时检查在途 owner 和失败 fence，避免创建并行写或自动重试。
    ///
    /// @author kongweiguang
    fn begin(&mut self) -> Option<WorkbenchLayoutIntent> {
        if self.in_flight.is_some() || self.blocked_after_failure {
            return None;
        }
        let intent = self.pending.take()?;
        self.in_flight = Some(intent.clone());
        Some(intent)
    }

    /// 回执只结束原在途值；失败时保留更晚意图，并锁住后续自动 flush。
    ///
    /// @author kongweiguang
    fn finish(&mut self, succeeded: bool) -> Option<WorkbenchLayoutIntent> {
        let intent = self.in_flight.take()?;
        if !succeeded {
            if self.pending.is_none() {
                self.pending = Some(intent.clone());
            }
            self.blocked_after_failure = true;
        }
        Some(intent)
    }

    /// 普通设置写入的未知或冲突同样保护尚未发送的布局，读回不得变成隐式提交授权。
    ///
    /// @author kongweiguang
    fn block_on_settings_error(&mut self, code: PublicErrorCode) {
        if self.has_pending()
            && matches!(
                code,
                PublicErrorCode::Conflict | PublicErrorCode::OutcomeUnknown
            )
        {
            self.blocked_after_failure = true;
        }
    }

    /// 明确核对只确认当前读回事实与末意图相等，差异不触发写入或清除失败 fence。
    ///
    /// @author kongweiguang
    fn confirm_readback(
        &mut self,
        layout: &WorkbenchLayoutSettings,
    ) -> Option<WorkbenchLayoutIntent> {
        if !self.blocked_after_failure
            || self.in_flight.is_some()
            || self
                .pending
                .as_ref()
                .is_none_or(|intent| &intent.layout != layout)
        {
            return None;
        }
        self.blocked_after_failure = false;
        self.pending.take()
    }

    /// 退出与更新须检查实际待办，而不能把干净草稿误认为布局队列也已落盘。
    ///
    /// @author kongweiguang
    fn has_pending(&self) -> bool {
        self.pending.is_some() || self.in_flight.is_some()
    }
}

/// 自动布局提交只接受干净 Ready 草稿，不携带其它未保存输入跨过 settings CAS。
///
/// @author kongweiguang
fn workbench_layout_save_ready(
    status: Option<DraftStatus>,
    dirty: bool,
    loading: bool,
    read_only: bool,
    input_errors: bool,
) -> bool {
    status == Some(DraftStatus::Ready) && !dirty && !loading && !read_only && !input_errors
}

impl PreferencesPanel {
    /// Root 只从显式布局手势提交逻辑内容尺寸；返回序号供 Root 拒收会回滚新布局的旧收据。
    /// busy/dirty 时仅合并末意图，不修改或提交用户草稿。
    ///
    /// @author kongweiguang
    pub fn request_workbench_layout(
        &mut self,
        layout: WorkbenchLayoutSettings,
        cx: &mut Context<Self>,
    ) -> u64 {
        let request_id = self.workbench_layout_requests.request(layout);
        self.flush_pending_workbench_layout(cx);
        cx.notify();
        request_id
    }

    /// Root 在普通保存、加载或显式丢弃后可再次尝试；失败后的 Ready 不构成重放授权。
    /// 不直接安装应用快照，避免覆盖其它未保存输入，也不建立第二个 settings 写入 owner。
    ///
    /// @author kongweiguang
    pub fn flush_pending_workbench_layout(&mut self, cx: &mut Context<Self>) {
        let ready = workbench_layout_save_ready(
            self.settings.as_ref().map(|draft| draft.status()),
            self.settings.as_ref().is_some_and(|draft| draft.is_dirty()),
            self.loading_settings,
            self.recovery.mode == ConfigurationMode::ReadOnlyRecovery,
            !self.settings_input_errors.is_empty(),
        );
        if !ready {
            return;
        }
        let Some(original) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        let Some(intent) = self.workbench_layout_requests.begin() else {
            return;
        };
        if original.workbench_layout == intent.layout {
            self.finish_workbench_layout_saved(original.workbench_layout, cx);
            return;
        }

        let mut candidate = original.clone();
        candidate.workbench_layout = intent.layout;
        let replaced = self
            .settings
            .as_mut()
            .is_some_and(|draft| draft.replace_value(candidate).is_ok());
        if replaced && self.save_settings(cx) {
            return;
        }
        if replaced {
            if let Some(draft) = self.settings.as_mut() {
                let _ = draft.replace_value(original);
            }
            self.refresh_settings_issues();
        }
        let message = self.status.clone().unwrap_or_else(|| {
            crate::locale::text(
                "布局尚未保存，请核对设置状态后重试。",
                "The layout was not saved. Review Settings before retrying.",
            )
            .to_string()
        });
        self.fail_workbench_layout_save(PublicErrorCode::Invalid, message, cx);
    }

    /// Root 必须在普通退出与更新安装的最终 Preferences 门禁前检查，不能静默丢弃队列。
    ///
    /// @author kongweiguang
    pub fn has_pending_workbench_layout(&self) -> bool {
        self.workbench_layout_requests.has_pending()
    }

    /// 退出只等待实际布局写入的收据；未发送的 dirty/失败队列仍须经过原草稿门禁。
    /// 此处只投影唯一 owner 的在途事实，不在 Root 复制保存状态。
    ///
    /// @author kongweiguang
    pub fn is_workbench_layout_save_in_flight(&self) -> bool {
        self.workbench_layout_requests.in_flight.is_some()
    }

    /// 仅供用户明确读回核对后的动作调用，不能由 watcher 自动调用或在差异时重放。
    /// 相等只确认当前配置已具备目标事实，不宣称原未知写入必然成功。
    ///
    /// @author kongweiguang
    pub fn confirm_workbench_layout_readback(&mut self, cx: &mut Context<Self>) -> bool {
        if !workbench_layout_save_ready(
            self.settings.as_ref().map(|draft| draft.status()),
            self.settings.as_ref().is_some_and(|draft| draft.is_dirty()),
            self.loading_settings,
            self.recovery.mode == ConfigurationMode::ReadOnlyRecovery,
            !self.settings_input_errors.is_empty(),
        ) {
            return false;
        }
        let Some(layout) = self
            .settings
            .as_ref()
            .map(|draft| &draft.value().workbench_layout)
        else {
            return false;
        };
        let Some(intent) = self.workbench_layout_requests.confirm_readback(layout) else {
            return false;
        };
        cx.emit(PreferencesEvent::WorkbenchLayoutPersisted {
            request_id: intent.request_id,
            layout: intent.layout,
        });
        cx.notify();
        true
    }

    /// 成功路径使用应用确认的布局值，允许后端校验规范化；Root 自行按序号保护更晚 RAM 意图。
    ///
    /// @author kongweiguang
    pub(super) fn finish_workbench_layout_saved(
        &mut self,
        layout: WorkbenchLayoutSettings,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(intent) = self.workbench_layout_requests.finish(true) else {
            return false;
        };
        cx.emit(PreferencesEvent::WorkbenchLayoutPersisted {
            request_id: intent.request_id,
            layout,
        });
        true
    }

    /// 失败只回报原 request 并保留末意图；现有草稿继续负责 Failed/Conflict/Unknown 门禁。
    ///
    /// @author kongweiguang
    pub(super) fn fail_workbench_layout_save(
        &mut self,
        code: PublicErrorCode,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.workbench_layout_requests.block_on_settings_error(code);
        if let Some(intent) = self.workbench_layout_requests.finish(false) {
            cx.emit(PreferencesEvent::WorkbenchLayoutSaveFailed {
                request_id: intent.request_id,
                code,
                message,
            });
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_workbench_layout.rs"]
mod tests;
