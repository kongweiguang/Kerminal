//! 目录跟随只消费会话已有的目录上报，不修改远端 Shell 启动文件。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_domain::files::FileEndpoint;

use super::FilesPane;

impl FilesPane {
    /// 显式开启立即同步最新 SSH 目录；关闭仅取消自动跟随读取，保留用户手动导航与现有清单。
    ///
    /// @author kongweiguang
    pub(crate) fn set_follow_terminal_directory(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled
            && (!self.compact_layout
                || !matches!(
                    self.current_terminal_endpoint.as_ref(),
                    Some(FileEndpoint::Remote { .. })
                ))
        {
            return;
        }
        if self.follow_terminal_directory == enabled {
            return;
        }
        self.follow_terminal_directory = enabled;
        if enabled {
            if let Some(endpoint) = self.current_terminal_endpoint.clone() {
                let panel = &self.panels[1];
                if panel.endpoint != endpoint
                    || panel
                        .requested_endpoint
                        .as_ref()
                        .is_some_and(|pending| pending != &endpoint)
                    || panel.failed_directory.is_some()
                    || panel.listing.is_none()
                {
                    self.follow_terminal_directory_to(endpoint, cx);
                }
            }
        } else {
            self.cancel_terminal_directory_follow_request();
        }
        cx.notify();
    }

    /// 只给本次自动读取登记取消资格；已经指向相同目录的在途请求继续完成，避免不断重启慢连接。
    ///
    /// @author kongweiguang
    pub(super) fn follow_terminal_directory_to(
        &mut self,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        if self.panels[1].loading && self.panels[1].requested_endpoint.as_ref() == Some(&endpoint) {
            return;
        }
        self.open_endpoint_on_side(1, endpoint, cx);
        self.terminal_directory_follow_generation = Some(self.panels[1].generation);
    }

    /// 同时推进代次和取消 provider，迟到成功与失败都不能在关闭跟随后改变目录或错误状态。
    /// 树沿用已提交缓存，只重新绑定代次，不因取消自动导航丢失用户展开位置。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_terminal_directory_follow_request(&mut self) {
        let Some(generation) = self.terminal_directory_follow_generation.take() else {
            return;
        };
        let panel = &mut self.panels[1];
        if panel.generation != generation || !panel.loading {
            return;
        }
        panel.cancel_root_request_cancellation();
        panel.generation = panel.generation.wrapping_add(1);
        panel.loading = false;
        panel.requested_endpoint = None;
        panel.pending_selection = None;
        panel.tree.rebind_generation(panel.generation);
        panel.keyboard_focus.reset_for_directory(&panel.endpoint);
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/files_directory_follow.rs"]
mod tests;
