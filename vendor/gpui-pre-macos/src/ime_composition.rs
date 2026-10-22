// @author kongweiguang

//! 为 AppKit marked-text 回调保留焦点绑定。

/// 区分查询、编辑、提交与取消阶段，避免把原生回调误投给新焦点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputHandlerOperation {
    Query,
    DirectEdit,
    SetMarkedText,
    InsertText,
    UnmarkText,
}

/// 标记一轮原生组合输入，供正常 unmark 与同步取消分别完成对应状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompositionEpoch(u64);

/// 把已选 handler 带出窗口锁，避免 GPUI 回调重入时覆盖当前槽位。
pub(crate) struct InputHandlerSelection<H> {
    pub(crate) handler: H,
    pub(crate) finish_unmark: Option<CompositionEpoch>,
}

#[derive(Clone)]
struct BoundHandler<B, H> {
    focus_id: Option<B>,
    handler: H,
}

#[derive(Clone)]
struct CompositionHandler<B, H> {
    focus_id: Option<B>,
    handler: H,
    epoch: CompositionEpoch,
}

enum CompositionState<B, H> {
    Idle,
    Marked(CompositionHandler<B, H>),
    Committed(CompositionHandler<B, H>),
    Cancelling(CompositionHandler<B, H>),
}

/// 单独保留组合输入 owner，同时让 GPUI 当前 handler 槽继续按焦点更新。
pub(crate) struct ImeInputHandlerState<B, H> {
    current: Option<BoundHandler<B, H>>,
    composition: CompositionState<B, H>,
    next_epoch: u64,
}

impl<B, H> ImeInputHandlerState<B, H> {
    /// 初始不暴露输入目标，确保只有 GPUI 当前焦点绘制后的 handler 能接收原生输入。
    pub(crate) fn new() -> Self {
        Self {
            current: None,
            composition: CompositionState::Idle,
            next_epoch: 0,
        }
    }

    /// GPUI 重绘会临时 take handler；组合 pin 必须独立存活，不能随当前槽一起清空。
    pub(crate) fn take_current(&mut self) -> Option<H> {
        self.current.take().map(|bound| bound.handler)
    }

    /// 安装新的当前 handler；只有焦点身份变化才取消旧组合，同焦点重绘继续使用旧 pin。
    pub(crate) fn set_current(
        &mut self,
        focus_id: Option<B>,
        handler: H,
    ) -> Option<CompositionEpoch>
    where
        B: Clone + PartialEq,
        H: Clone,
    {
        self.current = Some(BoundHandler { focus_id, handler });

        let cancellation = match &self.composition {
            CompositionState::Marked(pinned) | CompositionState::Committed(pinned)
                if !same_known_focus(
                    pinned.focus_id.as_ref(),
                    self.current.as_ref().and_then(|h| h.focus_id.as_ref()),
                ) =>
            {
                Some(pinned.clone())
            }
            CompositionState::Idle
            | CompositionState::Marked(_)
            | CompositionState::Committed(_)
            | CompositionState::Cancelling(_) => None,
        };

        cancellation.map(|pinned| {
            let epoch = pinned.epoch;
            self.composition = CompositionState::Cancelling(pinned);
            epoch
        })
    }

    /// 按回调类型选目标并记录必要状态，保证提交与 unmark 留在组合开始时的焦点。
    pub(crate) fn select(
        &mut self,
        operation: InputHandlerOperation,
    ) -> Option<InputHandlerSelection<H>>
    where
        B: Clone,
        H: Clone,
    {
        match operation {
            InputHandlerOperation::Query => self.pinned_or_current(),
            InputHandlerOperation::DirectEdit => match &self.composition {
                CompositionState::Cancelling(_) => None,
                CompositionState::Idle => self.current_selection(),
                CompositionState::Marked(_) | CompositionState::Committed(_) => {
                    self.pinned_selection(None)
                }
            },
            InputHandlerOperation::SetMarkedText => match &self.composition {
                CompositionState::Cancelling(_) => None,
                CompositionState::Marked(pinned) | CompositionState::Committed(pinned) => {
                    let pinned = pinned.clone();
                    self.composition = CompositionState::Marked(pinned.clone());
                    Some(InputHandlerSelection {
                        handler: pinned.handler,
                        finish_unmark: None,
                    })
                }
                CompositionState::Idle => {
                    let current = self.current.as_ref()?.clone();
                    let epoch = self.next_epoch();
                    let pinned = CompositionHandler {
                        focus_id: current.focus_id,
                        handler: current.handler,
                        epoch,
                    };
                    let selection = pinned.handler.clone();
                    self.composition = CompositionState::Marked(pinned);
                    Some(InputHandlerSelection {
                        handler: selection,
                        finish_unmark: None,
                    })
                }
            },
            InputHandlerOperation::InsertText => match &self.composition {
                CompositionState::Cancelling(_) => None,
                CompositionState::Marked(pinned) | CompositionState::Committed(pinned) => {
                    let pinned = pinned.clone();
                    self.composition = CompositionState::Committed(pinned.clone());
                    Some(InputHandlerSelection {
                        handler: pinned.handler,
                        finish_unmark: None,
                    })
                }
                CompositionState::Idle => self.current_selection(),
            },
            InputHandlerOperation::UnmarkText => match &self.composition {
                CompositionState::Cancelling(pinned) => Some(InputHandlerSelection {
                    handler: pinned.handler.clone(),
                    finish_unmark: None,
                }),
                CompositionState::Marked(pinned) | CompositionState::Committed(pinned) => {
                    Some(InputHandlerSelection {
                        handler: pinned.handler.clone(),
                        finish_unmark: Some(pinned.epoch),
                    })
                }
                CompositionState::Idle => self.current_selection(),
            },
        }
    }

    /// 开始焦点丢失取消；在 AppKit 同步 discard 回调结束前保留旧 handler 供清理重入使用。
    pub(crate) fn begin_cancellation(&mut self) -> Option<CompositionEpoch>
    where
        B: Clone,
        H: Clone,
    {
        let pinned = match &self.composition {
            CompositionState::Marked(pinned) | CompositionState::Committed(pinned) => {
                Some(pinned.clone())
            }
            CompositionState::Idle | CompositionState::Cancelling(_) => None,
        }?;
        let epoch = pinned.epoch;
        self.composition = CompositionState::Cancelling(pinned);
        Some(epoch)
    }

    /// 仅结束匹配代次的正常 unmark；若回调中发生焦点切换，保留取消状态供外层收尾。
    pub(crate) fn finish_unmark(&mut self, epoch: CompositionEpoch) {
        let matches = match &self.composition {
            CompositionState::Marked(pinned) | CompositionState::Committed(pinned) => {
                pinned.epoch == epoch
            }
            CompositionState::Idle | CompositionState::Cancelling(_) => false,
        };
        if matches {
            self.composition = CompositionState::Idle;
        }
    }

    /// 同步 discard 返回后释放旧 pin；AppKit 不给更晚的异步回调附带来源代次。
    pub(crate) fn finish_cancellation(&mut self, epoch: CompositionEpoch) {
        let matches = matches!(
            &self.composition,
            CompositionState::Cancelling(pinned) if pinned.epoch == epoch
        );
        if matches {
            self.composition = CompositionState::Idle;
        }
    }

    /// 窗口销毁时释放当前与组合 handler，不再向正在销毁的原生 view 发 AppKit 消息。
    pub(crate) fn clear(&mut self) {
        self.current = None;
        self.composition = CompositionState::Idle;
    }

    /// 组合活跃或同步取消期间，查询始终读取旧 owner；空闲时读取 GPUI 当前 handler。
    fn pinned_or_current(&self) -> Option<InputHandlerSelection<H>>
    where
        H: Clone,
    {
        match &self.composition {
            CompositionState::Marked(pinned)
            | CompositionState::Committed(pinned)
            | CompositionState::Cancelling(pinned) => Some(InputHandlerSelection {
                handler: pinned.handler.clone(),
                finish_unmark: None,
            }),
            CompositionState::Idle => self.current_selection(),
        }
    }

    /// 只选择仍有效的组合 pin，取消重入中的普通编辑不得落到任一 pane。
    fn pinned_selection(
        &self,
        finish_unmark: Option<CompositionEpoch>,
    ) -> Option<InputHandlerSelection<H>>
    where
        H: Clone,
    {
        let pinned = match &self.composition {
            CompositionState::Marked(pinned) | CompositionState::Committed(pinned) => pinned,
            CompositionState::Idle | CompositionState::Cancelling(_) => return None,
        };
        Some(InputHandlerSelection {
            handler: pinned.handler.clone(),
            finish_unmark,
        })
    }

    /// 克隆 GPUI 最新当前 handler，避免调用期间 take/set 覆盖其他同步重入更新。
    fn current_selection(&self) -> Option<InputHandlerSelection<H>>
    where
        H: Clone,
    {
        self.current.as_ref().map(|current| InputHandlerSelection {
            handler: current.handler.clone(),
            finish_unmark: None,
        })
    }

    /// 分配新代次，使迟到的 unmark 完成不能清除之后开始的组合 pin。
    fn next_epoch(&mut self) -> CompositionEpoch {
        self.next_epoch = self.next_epoch.wrapping_add(1).max(1);
        CompositionEpoch(self.next_epoch)
    }
}

/// 未知焦点身份按不匹配处理，避免缺失标签意外保留过期文本目标。
fn same_known_focus<B: PartialEq>(left: Option<&B>, right: Option<&B>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
}
