//! 将输入法组合与不可变终端绑定关联，避免文本回调根据当前焦点猜目标。
//!
//! @author kongweiguang

use kerminal_application::sessions::UiSessionBinding;

/// 一次组合文本的身份；组合代次区分同一会话中的连续输入法提交。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImeCompositionBinding {
    session: UiSessionBinding,
    generation: u64,
}

impl ImeCompositionBinding {
    /// 返回组合开始时的完整会话、owner 与重连代次，调用方据此验证提交目标。
    pub fn session(&self) -> &UiSessionBinding {
        &self.session
    }

    /// 返回当前 handler 内单调递增的组合代次，用于拒绝旧组合回调。
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// 通过绑定校验的文本，仍由唯一现有 SessionService 输入路径负责发送。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImeBoundText {
    pub session: UiSessionBinding,
    pub text: String,
    pub source: ImeTextSource,
}

/// 文本来源用于让调用方保留输入协议语义，不从屏幕内容推断 IME 状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImeTextSource {
    Plain,
    Composition { generation: u64 },
}

/// 输入回调经过明确绑定检查后的决策；Stale/Unbound 均不得投递到任何会话。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImeInputDecision {
    Accepted(ImeBoundText),
    Stale { expected: UiSessionBinding },
    Unbound,
    CompositionActive,
}

/// 仅属于一个不可变 UI session binding 的 IME handler 状态。
///
/// 每次 pane owner 或会话代次变化都应创建新的状态实例；不要把该状态作为全局焦点的
/// 可变别名复用，否则平台迟到回调仍可能被错误解释为新焦点的普通文本。
#[derive(Debug)]
pub struct TerminalImeState {
    handler_session: UiSessionBinding,
    next_generation: u64,
    active: Option<ImeCompositionBinding>,
}

impl TerminalImeState {
    /// 创建只服务于一个 session/pane/generation 的输入状态，不在提交时推测当前目标。
    pub fn new(handler_session: UiSessionBinding) -> Self {
        Self {
            handler_session,
            next_generation: 0,
            active: None,
        }
    }

    /// 返回该 handler 创建时绑定的目标，Root 可用它验证仍然存活的 pane。
    pub fn handler_session(&self) -> &UiSessionBinding {
        &self.handler_session
    }

    /// 开始组合或更新同一次组合；相同 handler 的连续 preedit 共用 generation。
    pub fn begin_or_update_composition(&mut self) -> ImeCompositionBinding {
        if let Some(active) = &self.active {
            return active.clone();
        }

        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let binding = ImeCompositionBinding {
            session: self.handler_session.clone(),
            generation: self.next_generation,
        };
        self.active = Some(binding.clone());
        binding
    }

    /// 按平台/窗口确认的当前 binding 提交组合；迟到、旧代或无来源文本一律不路由。
    pub fn commit_composition(
        &mut self,
        token: Option<&ImeCompositionBinding>,
        current_session: Option<&UiSessionBinding>,
        text: &str,
    ) -> ImeInputDecision {
        let Some(token) = token else {
            return ImeInputDecision::Unbound;
        };

        let is_current_composition = self.active.as_ref() == Some(token);
        let targets_current_session = token.session() == self.handler_session()
            && current_session == Some(&self.handler_session);
        if !is_current_composition || !targets_current_session {
            if is_current_composition {
                self.active = None;
            }
            return ImeInputDecision::Stale {
                expected: token.session().clone(),
            };
        }

        self.active = None;
        ImeInputDecision::Accepted(ImeBoundText {
            session: token.session().clone(),
            text: text.to_owned(),
            source: ImeTextSource::Composition {
                generation: token.generation,
            },
        })
    }

    /// 接受普通平台文本时仍要求精确 handler binding，组合期间拒绝可能重复的文本事件。
    pub fn accept_plain_text(
        &self,
        current_session: Option<&UiSessionBinding>,
        text: &str,
    ) -> ImeInputDecision {
        if current_session != Some(&self.handler_session) {
            return ImeInputDecision::Stale {
                expected: self.handler_session.clone(),
            };
        }
        if self.active.is_some() {
            return ImeInputDecision::CompositionActive;
        }

        ImeInputDecision::Accepted(ImeBoundText {
            session: self.handler_session.clone(),
            text: text.to_owned(),
            source: ImeTextSource::Plain,
        })
    }

    /// 焦点离开固定 handler 时撤销当前组合；单独命名以免调用方把 blur 当作关闭会话。
    ///
    /// @author kongweiguang
    pub fn cancel_for_focus_loss(&mut self) -> Option<ImeCompositionBinding> {
        self.active.take()
    }

    /// 关闭绑定的 pane 时撤销组合，返回 token 供 owner 在资源清理期间继续识别旧回调。
    ///
    /// @author kongweiguang
    pub fn cancel_for_close(&mut self) -> Option<ImeCompositionBinding> {
        self.active.take()
    }
}
