//! 设置防抖保存的代次与单飞协调，不持有 UI 或应用服务。
//!
//! @author kongweiguang

/// 一次已启动 settings 写入的身份，拒绝重载前任务的迟到回执。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SettingsSaveTicket {
    request_id: u64,
    edit_generation: u64,
}

/// 防抖到期或显式 flush 请求的处理结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsSaveRequest {
    Started(SettingsSaveTicket),
    Queued,
    Stale,
}

/// 用单调代次丢弃旧防抖任务，以单飞 ticket 串行复用同一 CAS revision。
#[derive(Debug, Default)]
pub(super) struct SettingsAutosaveState {
    edit_generation: u64,
    request_generation: u64,
    in_flight: Option<SettingsSaveTicket>,
    queued_generation: Option<u64>,
}

impl SettingsAutosaveState {
    /// 每次有效编辑推进代次并使旧计时器、旧排队请求失效。
    pub(super) fn edited(&mut self) -> u64 {
        self.edit_generation = next_generation(self.edit_generation);
        self.queued_generation = None;
        self.edit_generation
    }

    /// 返回当前编辑代次，供主动保存和关闭时的立即 flush 使用。
    pub(super) const fn current_generation(&self) -> u64 {
        self.edit_generation
    }

    /// 只接受最近一次编辑安排的计时器，重载后旧任务不再能提交草稿。
    pub(super) fn is_current(&self, generation: u64) -> bool {
        self.edit_generation == generation
    }

    /// watcher 或未知结果回读后作废旧防抖请求；安全确认的 dirty 草稿可立即用新代次续存。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate_pending(&mut self) {
        self.edit_generation = next_generation(self.edit_generation);
        self.queued_generation = None;
    }

    /// 同一时间只允许一个 CAS 请求；保存期间到期的较新编辑只排队最新代次。
    pub(super) fn request_save(&mut self, generation: u64) -> SettingsSaveRequest {
        if !self.is_current(generation) {
            return SettingsSaveRequest::Stale;
        }
        if let Some(active) = self.in_flight {
            if active.edit_generation != generation {
                self.queued_generation = Some(generation);
            }
            return SettingsSaveRequest::Queued;
        }

        self.request_generation = next_generation(self.request_generation);
        let ticket = SettingsSaveTicket {
            request_id: self.request_generation,
            edit_generation: generation,
        };
        self.in_flight = Some(ticket);
        SettingsSaveRequest::Started(ticket)
    }

    /// 确认回执仍属于当前请求，避免旧响应完成新一轮保存。
    pub(super) fn is_active(&self, ticket: SettingsSaveTicket) -> bool {
        self.in_flight == Some(ticket)
    }

    /// 完成当前请求后只释放已到期的最新排队编辑；冲突或未知结果可关闭续写。
    pub(super) fn finish_save(
        &mut self,
        ticket: SettingsSaveTicket,
        allow_follow_up: bool,
    ) -> Option<u64> {
        if !self.is_active(ticket) {
            return None;
        }
        self.in_flight = None;
        let queued = self.queued_generation.take();
        if allow_follow_up
            && queued.is_some_and(|generation| {
                generation == self.edit_generation && generation != ticket.edit_generation
            })
        {
            queued
        } else {
            None
        }
    }

    /// 草稿状态拒绝启动保存时释放预留 ticket，避免协调器永久停在单飞状态。
    pub(super) fn cancel_save(&mut self, ticket: SettingsSaveTicket) {
        if self.is_active(ticket) {
            self.in_flight = None;
            self.queued_generation = None;
        }
    }

    /// 新配置安装后失效旧任务；保留请求序号，防止迟到回执与新请求身份重合。
    pub(super) fn reset(&mut self) {
        self.edit_generation = next_generation(self.edit_generation);
        self.in_flight = None;
        self.queued_generation = None;
    }
}

/// 代次从 1 开始且不回到哨兵 0，便于默认状态和有效编辑区分。
fn next_generation(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_settings_autosave.rs"]
mod tests;
