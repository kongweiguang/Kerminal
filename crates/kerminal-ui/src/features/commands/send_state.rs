//! 普通命令与广播共享的单一发送生命周期，隔离目标切换后的迟到回执。
//!
//! @author kongweiguang

/// Commands 面板允许同时存在的唯一发送操作类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SendOperationKind {
    Command,
    Broadcast,
}

/// 绑定发送种类和单调代次，旧异步回执不能完成后来启动的操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SendOperationToken {
    generation: u64,
    pub(super) kind: SendOperationKind,
}

/// 保存唯一活动回执；面板只需要一个 token 槽来隔离重连与会话切换的旧任务。
#[derive(Default)]
pub(super) struct SendOperationState {
    generation: u64,
    pub(super) active: Option<SendOperationToken>,
}

impl SendOperationState {
    /// 忙时拒绝第二个发送；唯一活动 token 同时保护 busy 和取消句柄归属。
    ///
    /// @author kongweiguang
    pub(super) fn begin(&mut self, kind: SendOperationKind) -> Option<SendOperationToken> {
        if self.active.is_some() {
            return None;
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        let token = SendOperationToken {
            generation: self.generation,
            kind,
        };
        self.active = Some(token);
        Some(token)
    }

    /// 换 session 时作废旧回执；同 session 重连不调用它，需等旧写入结果收敛。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.active = None;
    }

    /// 仅精确活动 token 可清空 busy；过期回执不能触碰新 token 或新取消句柄。
    ///
    /// @author kongweiguang
    pub(super) fn finish(&mut self, token: SendOperationToken) -> bool {
        if self.active != Some(token) {
            return false;
        }
        self.active = None;
        true
    }
}
