// @author kongweiguang

use std::collections::VecDeque;

const MAX_IME_COMMIT_EPOCHS: usize = 64;
const MAX_IME_COMMIT_EPOCH_SPAN: u32 = 1 << 20;
const MAX_IME_BATCH_EVENTS: usize = 128;

/// 将文本输入事件批次与 Done(serial) 对应的 client commit 状态分开保存。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ImeBatchEvent {
    Commit(Option<String>),
    Preedit(Option<String>),
}

/// Done 批次按 text-input-v3 规定整理为“已提交文本先于最新预编辑文本”。
pub(super) struct ImeBatchContents {
    pub(super) commits: Vec<String>,
    pub(super) preedit: Option<Option<String>>,
    pub(super) had_commit: bool,
}

/// 对 Wayland 输入法的事件批次设定上限，避免 compositor 未发 Done 时持续占用内存。
#[derive(Default)]
pub(super) struct ImeBatch {
    events: Vec<ImeBatchEvent>,
    overflowed: bool,
}

impl ImeBatch {
    /// 超出单批上限时丢弃整批，避免只应用部分提交而制造错误文本。
    pub(super) fn push(&mut self, event: ImeBatchEvent) {
        if self.overflowed {
            return;
        }
        if self.events.len() == MAX_IME_BATCH_EVENTS {
            self.events.clear();
            self.overflowed = true;
            return;
        }
        self.events.push(event);
    }

    /// Done 后一次性取走当前批次，重复 Done 不会重放上次内容。
    pub(super) fn take(&mut self) -> Option<ImeBatchContents> {
        let overflowed = std::mem::replace(&mut self.overflowed, false);
        let events = std::mem::take(&mut self.events);
        if overflowed {
            return None;
        }
        let mut contents = ImeBatchContents {
            commits: Vec::new(),
            preedit: None,
            had_commit: false,
        };
        for event in events {
            match event {
                ImeBatchEvent::Commit(text) => {
                    contents.had_commit = true;
                    if let Some(text) = text {
                        contents.commits.push(text);
                    }
                }
                ImeBatchEvent::Preedit(text) => contents.preedit = Some(text),
            }
        }
        Some(contents)
    }

    /// Leave 或输入对象重建时同时清除未完成内容与溢出状态。
    pub(super) fn clear(&mut self) {
        self.events.clear();
        self.overflowed = false;
    }
}

struct ImeCommitEpoch<K, T> {
    first_serial: u32,
    last_serial: u32,
    focus_key: Option<K>,
    target: Option<T>,
}

/// 用受限的 focus epoch 历史将 compositor 的 Done(serial) 关联回提交时的焦点。
pub(super) struct ImeCommitHistory<K, T> {
    next_serial: u32,
    epochs: VecDeque<ImeCommitEpoch<K, T>>,
}

impl<K, T> Default for ImeCommitHistory<K, T> {
    fn default() -> Self {
        Self {
            next_serial: 0,
            epochs: VecDeque::new(),
        }
    }
}

impl<K, T> ImeCommitHistory<K, T>
where
    K: Clone + Eq,
    T: Clone,
{
    /// 每次实际发送 text_input.commit 都递增序号；只合并相邻且可识别的相同焦点。
    pub(super) fn record_commit(&mut self, focus_key: Option<K>, target: Option<T>) -> u32 {
        self.next_serial = self.next_serial.wrapping_add(1);
        let serial = self.next_serial;

        if let Some(focus_key) = focus_key.as_ref() {
            if let Some(epoch) = self.epochs.back_mut()
                && epoch.focus_key.as_ref() == Some(focus_key)
                && serial.wrapping_sub(epoch.first_serial) < MAX_IME_COMMIT_EPOCH_SPAN
            {
                epoch.last_serial = serial;
                return serial;
            }
        }

        self.epochs.push_back(ImeCommitEpoch {
            first_serial: serial,
            last_serial: serial,
            focus_key,
            target,
        });
        while self.epochs.len() > MAX_IME_COMMIT_EPOCHS {
            self.epochs.pop_front();
        }
        serial
    }

    /// 只接受保留窗口内精确命中的 serial；未知序号不回退到当前窗口。
    pub(super) fn resolve(&self, serial: u32) -> Option<T> {
        self.epochs
            .iter()
            .rev()
            .find(|epoch| serial_in_wrapping_range(serial, epoch.first_serial, epoch.last_serial))
            .and_then(|epoch| epoch.target.clone())
    }

    /// Leave 使旧 serial 路由失效，但保留对象内计数，避免旧序号在下一次 Enter 被复用。
    pub(super) fn clear(&mut self) {
        self.epochs.clear();
    }

    /// text_input 对象销毁并重建后协议计数重新开始。
    pub(super) fn reset(&mut self) {
        self.next_serial = 0;
        self.epochs.clear();
    }

    #[cfg(test)]
    fn epoch_count(&self) -> usize {
        self.epochs.len()
    }
}

/// Wayland serial 是 wrapping u32；历史区间受限，故用模数距离判定跨零区间。
fn serial_in_wrapping_range(serial: u32, first: u32, last: u32) -> bool {
    serial.wrapping_sub(first) <= last.wrapping_sub(first)
}

#[cfg(test)]
mod tests {
    use super::{ImeBatch, ImeBatchEvent, ImeCommitHistory};

    /// 连续同焦点提交共用一个有界区间，同时可解析区间内每个 serial。
    #[test]
    fn same_focus_commits_are_compressed_into_one_epoch() {
        let mut history = ImeCommitHistory::<u8, &'static str>::default();
        let first = history.record_commit(Some(1), Some("old focus"));
        let second = history.record_commit(Some(1), Some("old focus"));

        assert_eq!(history.epoch_count(), 1);
        assert_eq!(history.resolve(first), Some("old focus"));
        assert_eq!(history.resolve(second), Some("old focus"));
    }

    /// 旧 Done 即使在焦点改变后到达，也只能匹配历史 handler 而不能读取新焦点。
    #[test]
    fn delayed_done_keeps_the_focus_recorded_for_its_serial() {
        let mut history = ImeCommitHistory::<u8, &'static str>::default();
        let old_serial = history.record_commit(Some(1), Some("old focus"));
        let new_serial = history.record_commit(Some(2), Some("new focus"));

        assert_eq!(history.resolve(old_serial), Some("old focus"));
        assert_eq!(history.resolve(new_serial), Some("new focus"));
    }

    /// u32 serial 跨零时保留正确区间归属，避免回绕后误投其它焦点。
    #[test]
    fn commit_history_resolves_across_u32_wraparound() {
        let mut history = ImeCommitHistory::<u8, &'static str>::default();
        history.next_serial = u32::MAX - 1;

        let before_wrap = history.record_commit(Some(1), Some("same focus"));
        let after_wrap = history.record_commit(Some(1), Some("same focus"));
        let after_zero = history.record_commit(Some(1), Some("same focus"));

        assert_eq!(before_wrap, u32::MAX);
        assert_eq!(after_wrap, 0);
        assert_eq!(after_zero, 1);
        assert_eq!(history.resolve(0), Some("same focus"));
        assert_eq!(history.resolve(1), Some("same focus"));
    }

    /// 聚焦切换后即使当前已有新目标，未留存的旧 serial 也必须失败关闭。
    #[test]
    fn unknown_and_expired_serials_never_resolve_to_a_new_focus() {
        let mut history = ImeCommitHistory::<u8, u8>::default();
        for focus in 0..=super::MAX_IME_COMMIT_EPOCHS as u8 {
            history.record_commit(Some(focus), Some(focus));
        }

        assert_eq!(history.resolve(0), None);
        assert_eq!(history.resolve(1), None);
        assert_eq!(
            history.resolve(super::MAX_IME_COMMIT_EPOCHS as u32 + 2),
            None
        );
    }

    /// 每个 Done 消费一次批次；相同 serial 可承载后续独立批次而不做全局去重。
    #[test]
    fn done_batch_is_drained_once_and_keeps_commit_before_preedit_data() {
        let mut history = ImeCommitHistory::<u8, &'static str>::default();
        let serial = history.record_commit(Some(1), Some("same focus"));
        let mut batch = ImeBatch::default();
        batch.push(ImeBatchEvent::Commit(Some("汉".to_owned())));
        batch.push(ImeBatchEvent::Preedit(Some("字".to_owned())));

        assert_eq!(history.resolve(serial), Some("same focus"));
        let contents = batch.take().unwrap();
        assert_eq!(contents.commits, ["汉"]);
        assert_eq!(contents.preedit, Some(Some("字".to_owned())));
        assert!(contents.had_commit);
        assert_eq!(batch.take().unwrap().commits, Vec::<String>::new());

        batch.push(ImeBatchEvent::Commit(Some("后".to_owned())));
        assert_eq!(history.resolve(serial), Some("same focus"));
        assert_eq!(batch.take().unwrap().commits, ["后"]);
    }

    /// Leave 清除未完成 batch 与旧路由，同时保持对象内 serial 单调推进。
    #[test]
    fn clearing_leave_state_invalidates_routes_and_pending_events() {
        let mut history = ImeCommitHistory::<u8, u8>::default();
        let mut batch = ImeBatch::default();
        let serial = history.record_commit(Some(1), Some(7));
        batch.push(ImeBatchEvent::Commit(Some("stale".to_owned())));

        history.clear();
        batch.clear();

        assert_eq!(history.resolve(serial), None);
        assert_eq!(history.record_commit(Some(2), Some(8)), serial + 1);
        assert!(batch.take().unwrap().commits.is_empty());
    }

    /// 超过批次上限后整批拒绝，不能部分插入组合文本。
    #[test]
    fn oversized_batch_is_discarded_atomically() {
        let mut batch = ImeBatch::default();
        for _ in 0..=super::MAX_IME_BATCH_EVENTS {
            batch.push(ImeBatchEvent::Commit(Some("x".to_owned())));
        }

        assert!(batch.take().is_none());
        assert!(batch.take().unwrap().commits.is_empty());
    }
}
