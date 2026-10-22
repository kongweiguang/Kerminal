// @author kongweiguang

use std::cell::Cell;

/// 只有双方都有焦点身份且身份一致，才保留当前组合绑定。
pub(crate) fn same_composition_focus<T: PartialEq>(
    composition_focus: Option<T>,
    committed_focus: Option<T>,
) -> bool {
    matches!((composition_focus, committed_focus), (Some(composition), Some(committed)) if composition == committed)
}

/// 保存 IME 组合期间的原始输入处理器，避免按后续焦点重新选取目标。
pub(crate) struct ImeCompositionHandler<T> {
    active_epoch: Cell<Option<u64>>,
    next_epoch: Cell<u64>,
    handler: Cell<Option<(u64, T)>>,
}

impl<T> Default for ImeCompositionHandler<T> {
    /// 创建未绑定输入处理器的组合状态；窗口销毁时其字段自然释放。
    fn default() -> Self {
        Self {
            active_epoch: Cell::new(None),
            next_epoch: Cell::new(0),
            handler: Cell::new(None),
        }
    }
}

impl<T: Clone> ImeCompositionHandler<T> {
    /// 克隆组合起始处理器并恢复当前槽，借助同版 GPUI 的共享 handler 保留正常输入和缓存帧。
    pub(crate) fn begin(&self, current: &Cell<Option<T>>) -> bool {
        if self.active_epoch.get().is_some() {
            return false;
        }
        let Some(epoch) = self.next_epoch.get().checked_add(1) else {
            // 计数耗尽时拒绝建立新组合，避免回绕后复用仍可能迟到的旧 token。
            return false;
        };
        self.next_epoch.set(epoch);
        self.active_epoch.set(Some(epoch));
        if let Some(handler) = current.take() {
            self.handler.set(Some((epoch, handler.clone())));
            current.set(Some(handler));
        }
        true
    }

    /// 返回是否仍有尚未结束的 native composition，用于区分 END 回调中的新 START。
    pub(crate) fn is_active(&self) -> bool {
        self.active_epoch.get().is_some()
    }

    /// 仅对组合开始时捕获的处理器执行 IME 回调，不回退到实时焦点处理器。
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let epoch = self.active_epoch.get()?;
        let (handler_epoch, mut handler) = self.handler.take()?;
        if handler_epoch != epoch {
            return None;
        }
        let result = f(&mut handler);
        // 只在原 epoch 仍有效时归还 handler；嵌套 END+START 已建立的新目标不被旧回调覆盖。
        if self.active_epoch.get() == Some(epoch) {
            self.handler.set(Some((epoch, handler)));
        }
        Some(result)
    }

    /// 清除原目标的预编辑状态但保留槽位，供取消消息和结束消息安全收尾。
    pub(crate) fn cancel(&self, f: impl FnOnce(&mut T)) -> bool {
        self.with(f).is_some()
    }

    /// 先解除组合锁，再清理并释放固定处理器，避免清理回调期间的迟到消息继续匹配旧组合。
    pub(crate) fn end(&self, f: impl FnOnce(&mut T)) {
        self.active_epoch.set(None);
        let handler = self.handler.take();
        if let Some((_, mut handler)) = handler {
            f(&mut handler);
        }
    }

    /// 判断组合开始时是否捕获到处理器；无目标的组合仍保持活动以拒绝晚到消息。
    pub(crate) fn has_handler(&self) -> bool {
        let Some(active_epoch) = self.active_epoch.get() else {
            return false;
        };
        let Some((epoch, handler)) = self.handler.take() else {
            return false;
        };
        if epoch != active_epoch {
            return false;
        }
        self.handler.set(Some((epoch, handler)));
        true
    }
}
