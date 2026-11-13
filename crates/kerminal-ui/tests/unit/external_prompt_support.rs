//! 外部启动凭据卡片的测试入口。
//!
//! 生产模块只保留真实界面行为，测试所需的读取与提交入口集中在此，避免架构门禁因
//! `src/` 内的测试专用实现报错；这些方法仅在 `cfg(test)` 下编译。
//!
//! @author kongweiguang

use gpui_kit::{Context, SharedString, Window};

use super::super::panel::{ConnectionsPanel, ExternalPromptMessage};

impl ConnectionsPanel {
    /// Root 级回归读取卡片专用消息，验证 prompt-only 下错误/进度真实发布而不是只写普通 status。
    ///
    /// @author kongweiguang
    pub(crate) fn external_prompt_message(&self) -> Option<(SharedString, bool)> {
        self.external_prompt_message
            .as_ref()
            .map(|message: &ExternalPromptMessage| (message.text.clone(), message.is_error))
    }

    /// 测试直接填充密码输入，避免依赖无障碍快照是否暴露 mask 后的值。
    ///
    /// @author kongweiguang
    pub(crate) fn seed_external_password_for_test(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.external_password_input
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    /// 读取认证中布尔值，验证密码输入禁用与取消门禁使用同一状态。
    ///
    /// @author kongweiguang
    pub(crate) fn external_authenticating_for_test(&self) -> bool {
        self.external_authenticating
    }

    /// 诊断测试断言失败时的提示归属，不暴露给生产界面。
    ///
    /// @author kongweiguang
    pub(crate) fn debug_external_prompt_state(&self) -> (bool, bool, Option<String>) {
        (
            self.pending_external_username.is_some(),
            self.pending_external_password.is_some(),
            self.status.clone(),
        )
    }

    /// Root 级测试需要直接触发提交以观察同帧进度状态，不改变生产可见性。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_external_username_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_external_username(window, cx);
    }

    /// 密码提交测试入口与用户名入口同一约束。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_external_password_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_external_password(window, cx);
    }
}
