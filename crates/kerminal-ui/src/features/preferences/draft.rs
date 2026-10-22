//! 编辑草稿生命周期；远端 revision 由应用用例生成并原样回传。
//!
//! @author kongweiguang

/// 保存后的业务快照；revision 保持不透明，由应用层解释并执行 CAS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSnapshot<T, R> {
    /// 当前配置值。
    pub value: T,
    /// 配置读取时由仓储产生的并发版本。
    pub revision: R,
}

/// 草稿当前的写入状态，用于让界面区分可保存、保存中和外部冲突。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftStatus {
    /// 草稿可编辑，且当前没有未完成写入。
    Ready,
    /// 应用层正在执行写入，编辑与重复保存必须暂时锁定。
    Saving,
    /// 上次写入失败；草稿保留，可修正后重试。
    Failed,
    /// CAS 发现外部版本已变化，必须先重新读取后才可继续保存。
    ExternalConflict,
    /// 写入结果无法确认；禁止重试或丢弃，必须先从仓储读回当前版本。
    OutcomeUnknown,
}

/// 离开页面时需要关闭、展示丢弃确认，或等待保存完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseRequest {
    /// 没有未保存修改，可以关闭。
    CloseNow,
    /// 存在未保存修改，需要由用户决定保留还是丢弃。
    ConfirmDiscard,
    /// 当前保存尚未返回，不能假定草稿已经写入。
    WaitForSave,
}

/// 非法草稿状态迁移；用显式错误避免界面把冲突误当成保存成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftTransitionError {
    /// 保存期间再次启动保存。
    SaveAlreadyRunning,
    /// 无变更时发起保存。
    NoChanges,
    /// 保存结果回调没有对应的进行中写入。
    SaveNotRunning,
    /// 并发冲突后必须先加载最新版本。
    ReloadRequired,
    /// 上次写入结果未知时必须显式读回，不能把它当普通可重试失败。
    ReadBackRequired,
    /// 当前没有等待处理的关闭确认。
    NoCloseConfirmation,
    /// 写入期间禁止替换草稿。
    CannotEditWhileSaving,
}

/// 维护一份不可变基线和可编辑副本，关闭、失败与冲突路径共用同一状态机。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDraft<T, R> {
    baseline: T,
    value: T,
    revision: R,
    status: DraftStatus,
    close_confirmation_open: bool,
}

impl<T: Clone + PartialEq, R: Clone> ConfigDraft<T, R> {
    /// 从应用层返回的快照建立草稿，避免界面默认值覆盖实际文件内容。
    pub fn new(snapshot: DraftSnapshot<T, R>) -> Self {
        Self {
            baseline: snapshot.value.clone(),
            value: snapshot.value,
            revision: snapshot.revision,
            status: DraftStatus::Ready,
            close_confirmation_open: false,
        }
    }

    /// 读取编辑值的只读引用，调用方必须经 `replace_value` 才能触发状态校验。
    pub fn value(&self) -> &T {
        &self.value
    }

    /// 读取打开或最近成功写入时的 revision，保存时必须原样传给应用用例。
    pub fn revision(&self) -> &R {
        &self.revision
    }

    /// 读取当前写入状态以控制保存按钮和冲突说明。
    pub const fn status(&self) -> DraftStatus {
        self.status
    }

    /// 仅当编辑值与最近一次基线不同才算 dirty，避免无操作关闭弹出确认。
    pub fn is_dirty(&self) -> bool {
        self.value != self.baseline
    }

    /// 替换整个表单快照；保存进行中不可编辑，外部冲突状态则必须显式重载。
    pub fn replace_value(&mut self, value: T) -> Result<(), DraftTransitionError> {
        match self.status {
            DraftStatus::Saving => return Err(DraftTransitionError::CannotEditWhileSaving),
            DraftStatus::ExternalConflict => return Err(DraftTransitionError::ReloadRequired),
            DraftStatus::OutcomeUnknown => return Err(DraftTransitionError::ReadBackRequired),
            DraftStatus::Failed | DraftStatus::Ready => self.status = DraftStatus::Ready,
        }
        self.value = value;
        Ok(())
    }

    /// 只允许有变化且未冲突的草稿开始保存，阻止重复提交与过期 revision 重写。
    pub fn begin_save(&mut self) -> Result<(), DraftTransitionError> {
        match self.status {
            DraftStatus::Saving => return Err(DraftTransitionError::SaveAlreadyRunning),
            DraftStatus::ExternalConflict => return Err(DraftTransitionError::ReloadRequired),
            DraftStatus::OutcomeUnknown => return Err(DraftTransitionError::ReadBackRequired),
            DraftStatus::Ready | DraftStatus::Failed => {}
        }
        if !self.is_dirty() {
            return Err(DraftTransitionError::NoChanges);
        }
        self.status = DraftStatus::Saving;
        Ok(())
    }

    /// 保存成功后用服务端确认的快照替换基线，不能用提交前的 UI 值推断 revision。
    pub fn save_succeeded(
        &mut self,
        snapshot: DraftSnapshot<T, R>,
    ) -> Result<(), DraftTransitionError> {
        self.require_saving()?;
        self.replace_snapshot(snapshot);
        self.status = DraftStatus::Ready;
        Ok(())
    }

    /// 普通失败保留用户草稿，允许查看错误并修正或重试。
    pub fn save_failed(&mut self) -> Result<(), DraftTransitionError> {
        self.require_saving()?;
        self.status = DraftStatus::Failed;
        Ok(())
    }

    /// CAS 冲突保留草稿供用户查看，但禁止静默覆盖另一个进程的新版本。
    pub fn save_conflicted(&mut self) -> Result<(), DraftTransitionError> {
        self.require_saving()?;
        self.status = DraftStatus::ExternalConflict;
        Ok(())
    }

    /// 写入已越过边界但未能确认结果时保留原草稿，要求显式读回再决定后续操作。
    pub fn save_outcome_unknown(&mut self) -> Result<(), DraftTransitionError> {
        self.require_saving()?;
        self.status = DraftStatus::OutcomeUnknown;
        Ok(())
    }

    /// 多文件事务结果未知时锁住当前草稿，要求用户刷新完整列表后再继续操作。
    pub fn mark_outcome_unknown(&mut self) {
        self.status = DraftStatus::OutcomeUnknown;
    }

    /// 外部 watcher 发现配置变化时锁住 dirty 草稿；写入中和结果未知仍由 CAS/读回状态机处理。
    pub fn mark_external_change(&mut self) -> bool {
        if !self.is_dirty() || !matches!(self.status, DraftStatus::Ready | DraftStatus::Failed) {
            return false;
        }
        self.status = DraftStatus::ExternalConflict;
        true
    }

    /// 重新读取后接受最新应用快照，明确清除本地旧草稿并更新并发版本。
    pub fn reload(&mut self, snapshot: DraftSnapshot<T, R>) -> Result<(), DraftTransitionError> {
        if self.status == DraftStatus::Saving {
            return Err(DraftTransitionError::CannotEditWhileSaving);
        }
        self.replace_snapshot(snapshot);
        self.status = DraftStatus::Ready;
        self.close_confirmation_open = false;
        Ok(())
    }

    /// 在用户主动放弃时恢复最近一次成功读取或保存的值。
    pub fn discard_changes(&mut self) -> Result<(), DraftTransitionError> {
        if self.status == DraftStatus::Saving {
            return Err(DraftTransitionError::CannotEditWhileSaving);
        }
        if self.status == DraftStatus::OutcomeUnknown {
            return Err(DraftTransitionError::ReadBackRequired);
        }
        self.value.clone_from(&self.baseline);
        if self.status != DraftStatus::ExternalConflict {
            self.status = DraftStatus::Ready;
        }
        Ok(())
    }

    /// 根据 dirty 和保存状态返回关闭决定，不把尚未落盘的内容报告为已保存。
    pub fn request_close(&mut self) -> CloseRequest {
        if matches!(
            self.status,
            DraftStatus::Saving | DraftStatus::OutcomeUnknown
        ) {
            return CloseRequest::WaitForSave;
        }
        if self.is_dirty() {
            self.close_confirmation_open = true;
            CloseRequest::ConfirmDiscard
        } else {
            CloseRequest::CloseNow
        }
    }

    /// 关闭确认中的“继续编辑”只收起弹框，保留原有草稿。
    pub fn cancel_close(&mut self) -> Result<(), DraftTransitionError> {
        if !self.close_confirmation_open {
            return Err(DraftTransitionError::NoCloseConfirmation);
        }
        self.close_confirmation_open = false;
        Ok(())
    }

    /// 关闭确认中的“丢弃并关闭”恢复基线并关闭提示。
    pub fn discard_and_close(&mut self) -> Result<(), DraftTransitionError> {
        if !self.close_confirmation_open {
            return Err(DraftTransitionError::NoCloseConfirmation);
        }
        self.discard_changes()?;
        self.close_confirmation_open = false;
        Ok(())
    }

    /// 读取关闭确认是否可见，UI 不维护第二份容易失步的 dirty 状态。
    pub const fn close_confirmation_open(&self) -> bool {
        self.close_confirmation_open
    }

    /// 只有进行中的请求才允许结束写入状态，拒绝过期异步回调改变新草稿。
    fn require_saving(&self) -> Result<(), DraftTransitionError> {
        if self.status == DraftStatus::Saving {
            Ok(())
        } else {
            Err(DraftTransitionError::SaveNotRunning)
        }
    }

    /// 一次性替换值和 revision，确保字段与 CAS 版本来自同一个应用快照。
    fn replace_snapshot(&mut self, snapshot: DraftSnapshot<T, R>) {
        self.baseline.clone_from(&snapshot.value);
        self.value = snapshot.value;
        self.revision = snapshot.revision;
    }
}
