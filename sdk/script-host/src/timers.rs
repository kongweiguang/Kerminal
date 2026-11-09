// @author kongweiguang

use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_TIMERS: usize = 1024;

struct Timer {
    due: Instant,
    interval: Option<Duration>,
}

#[derive(Default)]
pub(crate) struct TimerQueue {
    next_id: u32,
    timers: HashMap<u32, Timer>,
}

impl TimerQueue {
    /// 限制每个插件持有的计时器数，避免计时器绕开脚本堆上限无限增长。
    pub(crate) fn schedule(&mut self, milliseconds: u32, repeat: bool) -> Result<u32, String> {
        if self.timers.len() >= MAX_TIMERS {
            return Err("too many timers".into());
        }
        let delay = Duration::from_millis(u64::from(if repeat {
            milliseconds.max(1)
        } else {
            milliseconds
        }));
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id != 0 && !self.timers.contains_key(&self.next_id) {
                break;
            }
        }
        self.timers.insert(
            self.next_id,
            Timer {
                due: Instant::now() + delay,
                interval: repeat.then_some(delay),
            },
        );
        Ok(self.next_id)
    }

    /// 两种 clear API 共用同一计时器空间，重复取消保持幂等。
    pub(crate) fn cancel(&mut self, id: u32) {
        self.timers.remove(&id);
    }

    /// 一次只取一个到期回调，让管道消息和其他计时器有机会运行。
    pub(crate) fn take_due(&mut self) -> Option<u32> {
        let now = Instant::now();
        let id = self
            .timers
            .iter()
            .filter(|(_, timer)| timer.due <= now)
            .min_by_key(|(id, timer)| (timer.due, **id))
            .map(|(id, _)| *id)?;
        let timer = self.timers.get_mut(&id)?;
        if let Some(interval) = timer.interval {
            timer.due = now + interval;
        } else {
            self.timers.remove(&id);
        }
        Some(id)
    }

    /// 等待时间由最早计时器决定，空队列允许阻塞等待输入。
    pub(crate) fn wait_duration(&self) -> Option<Duration> {
        let now = Instant::now();
        self.timers
            .values()
            .map(|timer| timer.due.saturating_duration_since(now))
            .min()
    }
}
