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
            && (self.current_terminal_binding.is_none()
                || !self.compact_layout
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
                    self.follow_terminal_directory_to(endpoint, false, cx);
                }
            }
        } else {
            self.cancel_terminal_directory_follow_request(cx);
        }
        cx.notify();
    }

    /// 只给本次自动读取登记取消资格；已经指向相同目录的在途请求继续完成，避免不断重启慢连接。
    /// `arm_entrance` 只用于分屏切换带来的真实新目录读取：它在请求发起前推进代次并登记目标，
    /// 让迟到结果按原目标身份失效；同路径请求提前返回时不武装，避免等待一个永远不会消费的窗口。
    ///
    /// @author kongweiguang
    pub(super) fn follow_terminal_directory_to(
        &mut self,
        endpoint: FileEndpoint,
        arm_entrance: bool,
        cx: &mut Context<Self>,
    ) {
        if self.panels[1].loading && self.panels[1].requested_endpoint.as_ref() == Some(&endpoint) {
            return;
        }
        if arm_entrance {
            self.begin_target_data_entrance(endpoint.clone(), cx);
        }
        self.open_endpoint_on_side(1, endpoint, cx);
        self.terminal_directory_follow_generation = Some(self.panels[1].generation);
    }

    /// 同时推进代次和取消 provider，迟到成功与失败都不能在关闭跟随后改变目录或错误状态。
    /// 树沿用已提交缓存，只重新绑定代次，不因取消自动导航丢失用户展开位置。
    /// disarm 先于 take 的提前返回：目标读取失败时回执已清空取消资格，
    /// 若只在仍能取消时撤销入场，残留的等待会被之后的普通刷新借用补播。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_terminal_directory_follow_request(&mut self, cx: &mut Context<Self>) {
        // 关闭跟随后不再有属于该目标的“首次成功”；无论取消资格是否还在，等待都必须落位。
        self.disarm_target_data_entrance(cx);
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
