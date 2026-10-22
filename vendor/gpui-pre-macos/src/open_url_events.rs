// @author kongweiguang

use std::collections::VecDeque;

/// 保存尚未交付的 AppKit URL 事件，并串行化回调中的同步重入。
#[derive(Default)]
pub(crate) struct OpenUrlEventQueue {
    pending: VecDeque<Vec<String>>,
    dispatching: bool,
}

impl OpenUrlEventQueue {
    /// 保留一个完整 URL 事件批次，维持 AppKit 回调的原始批次边界与顺序。
    pub(crate) fn enqueue(&mut self, urls: Vec<String>) {
        self.pending.push_back(urls);
    }

    /// 仅当没有其它回调正在运行时，开始交付最早的待处理事件。
    pub(crate) fn begin_dispatch(&mut self) -> Option<Vec<String>> {
        if self.dispatching {
            return None;
        }

        let urls = self.pending.pop_front()?;
        self.dispatching = true;
        Some(urls)
    }

    /// 回调完成后取下一个事件；队列排空时释放串行化标记。
    pub(crate) fn next_batch(&mut self) -> Option<Vec<String>> {
        debug_assert!(self.dispatching);
        match self.pending.pop_front() {
            Some(urls) => Some(urls),
            None => {
                self.dispatching = false;
                None
            }
        }
    }
}
