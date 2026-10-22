//! 文件编辑会话的领域状态与保存不变量。
//!
//! @author kongweiguang

use super::{
    FileEndpoint, FileRevision, LineEnding, TextDocument, WriteTextResult, WriteTextStrategy,
};

/// 编辑区最大字节数与冻结版严格文本读取上限保持一致，避免悄然缩小现有能力。
pub const MAX_EDITOR_BYTES: usize = 10 * 1024 * 1024;

/// 文件编辑会话明确区分可保存、保存中、冲突、未知提交和普通失败状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorPhase {
    Ready,
    Saving,
    Conflict,
    OutcomeUnknown,
    Failed,
}

/// 覆盖仅在一次 revision 比较已发生冲突后允许，避免普通保存绕过并发保护。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveResolution {
    CompareRevision,
    OverwriteAfterConflict,
}

/// 编辑器向应用交付的写入意图；正文已按行尾策略编码，不含协议或存储对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSaveCommand {
    pub endpoint: FileEndpoint,
    pub content: String,
    pub strategy: WriteTextStrategy,
}

/// 领域状态转换失败原因由 UI 映射为可操作说明，避免暴露底层 adapter 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorTransitionError {
    ReadOnly,
    ContentTooLarge,
    SaveInProgress,
    NothingToSave,
    ConflictResolutionRequired,
    OutcomeUnknownRequiresReload,
    OverwriteRequiresConflict,
    MissingPendingSave,
    ReloadWhileSaving,
}

/// 一个打开文件的编辑聚合持有基线 revision 和保存状态，文本缓冲由 GPUI Rope 管理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditorSession {
    endpoint: FileEndpoint,
    saved_content: String,
    revision: FileRevision,
    line_ending: LineEnding,
    readonly: bool,
    truncated: bool,
    phase: EditorPhase,
    pending_editor_content: Option<String>,
}

impl FileEditorSession {
    /// 从文件读取契约建立编辑缓冲基线并记住原始行尾策略；截断内容仍保持只读。
    pub fn open(document: TextDocument) -> Self {
        Self {
            endpoint: document.endpoint,
            saved_content: document.content,
            revision: document.revision,
            line_ending: document.line_ending,
            readonly: document.readonly,
            truncated: document.truncated,
            phase: EditorPhase::Ready,
            pending_editor_content: None,
        }
    }

    /// 对比完整文本而非仅比较长度，避免等长替换被误判为已保存。
    pub fn is_dirty(&self, current_content: &str) -> bool {
        current_content != self.saved_content
    }

    /// 返回 GPUI 编辑缓冲的已保存快照；磁盘行尾编码由保存命令单独处理。
    pub fn saved_content(&self) -> &str {
        &self.saved_content
    }

    /// 返回当前文件 revision，CompareRevision 必须使用打开或最近保存返回的值。
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }

    /// 返回原始行尾策略，均一行尾会在保存时延续，混合行尾逐段保持原样。
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// 返回文件身份；端点值同时区分本机、SSH 主机及容器文件系统。
    pub fn endpoint(&self) -> &FileEndpoint {
        &self.endpoint
    }

    /// 只读或截断文件仍可选择和复制，但禁止编辑器提交写入。
    pub fn is_readonly(&self) -> bool {
        self.readonly || self.truncated
    }

    /// 返回打开的文件是否因读取上限而不完整，供 UI 显示可执行的重新打开提示。
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// 暴露状态机只读快照，供保存按钮、冲突提示和关闭确认使用。
    pub fn phase(&self) -> EditorPhase {
        self.phase
    }

    /// 校验保存意图并冻结编辑器快照，再按原文件行尾生成磁盘正文以保留 dirty 基线。
    pub fn begin_save(
        &mut self,
        current_content: &str,
        resolution: SaveResolution,
    ) -> Result<EditorSaveCommand, EditorTransitionError> {
        if self.phase == EditorPhase::Saving {
            return Err(EditorTransitionError::SaveInProgress);
        }
        if self.phase == EditorPhase::OutcomeUnknown {
            return Err(EditorTransitionError::OutcomeUnknownRequiresReload);
        }
        if self.is_readonly() {
            return Err(EditorTransitionError::ReadOnly);
        }
        match resolution {
            SaveResolution::CompareRevision if self.phase == EditorPhase::Conflict => {
                return Err(EditorTransitionError::ConflictResolutionRequired);
            }
            SaveResolution::OverwriteAfterConflict if self.phase != EditorPhase::Conflict => {
                return Err(EditorTransitionError::OverwriteRequiresConflict);
            }
            SaveResolution::CompareRevision if !self.is_dirty(current_content) => {
                return Err(EditorTransitionError::NothingToSave);
            }
            _ => {}
        }

        let strategy = match resolution {
            SaveResolution::CompareRevision => {
                WriteTextStrategy::CompareRevision(self.revision.clone())
            }
            SaveResolution::OverwriteAfterConflict => WriteTextStrategy::ExplicitOverwrite,
        };
        let serialized_content = encode_line_endings(current_content, self.line_ending);
        if serialized_content.len() > MAX_EDITOR_BYTES {
            return Err(EditorTransitionError::ContentTooLarge);
        }
        let command = EditorSaveCommand {
            endpoint: self.endpoint.clone(),
            content: serialized_content,
            strategy,
        };
        self.pending_editor_content = Some(current_content.to_owned());
        self.phase = EditorPhase::Saving;
        Ok(command)
    }

    /// 保存结果推进 revision 与编辑器快照基线；异步期间后续输入仍会保持 dirty。
    pub fn save_succeeded(&mut self, result: WriteTextResult) -> Result<(), EditorTransitionError> {
        if self.phase != EditorPhase::Saving {
            return Err(EditorTransitionError::MissingPendingSave);
        }
        let content = self
            .pending_editor_content
            .take()
            .ok_or(EditorTransitionError::MissingPendingSave)?;
        self.saved_content = content;
        self.revision = result.revision;
        self.phase = EditorPhase::Ready;
        Ok(())
    }

    /// 保存失败保留用户正文，并把 revision 冲突提升为必须显式重载或覆盖的状态。
    pub fn save_failed(&mut self, conflict: bool) -> Result<(), EditorTransitionError> {
        if self.phase != EditorPhase::Saving || self.pending_editor_content.take().is_none() {
            return Err(EditorTransitionError::MissingPendingSave);
        }
        self.phase = if conflict {
            EditorPhase::Conflict
        } else {
            EditorPhase::Failed
        };
        Ok(())
    }

    /// 未知提交结果保留原 revision 并禁止再次保存，必须先由用户读回文件确认真实状态。
    pub fn save_outcome_unknown(&mut self) -> Result<(), EditorTransitionError> {
        if self.phase != EditorPhase::Saving || self.pending_editor_content.take().is_none() {
            return Err(EditorTransitionError::MissingPendingSave);
        }
        self.phase = EditorPhase::OutcomeUnknown;
        Ok(())
    }

    /// 重新读取只允许在没有在途保存时替换基线，关闭保护由 UI 先确认丢弃脏内容。
    pub fn reload(&mut self, document: TextDocument) -> Result<(), EditorTransitionError> {
        if self.phase == EditorPhase::Saving {
            return Err(EditorTransitionError::ReloadWhileSaving);
        }
        *self = Self::open(document);
        Ok(())
    }
}

/// 将均一行尾文件的编辑器输入转回原风格；混合行尾直接保留每个已有分隔符。
fn encode_line_endings(content: &str, line_ending: LineEnding) -> String {
    match line_ending {
        LineEnding::Mixed => content.to_owned(),
        LineEnding::Lf => content.replace("\r\n", "\n"),
        LineEnding::Crlf => {
            let mut encoded = String::with_capacity(content.len());
            let mut characters = content.chars().peekable();
            while let Some(character) = characters.next() {
                match character {
                    '\r' if characters.peek() == Some(&'\n') => {
                        characters.next();
                        encoded.push_str("\r\n");
                    }
                    '\n' => encoded.push_str("\r\n"),
                    other => encoded.push(other),
                }
            }
            encoded
        }
    }
}

/// 脏标签关闭必须明确选择保存、丢弃或取消，保存中的标签等待完成后再关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    CloseNow,
    ConfirmUnsavedChanges,
    WaitForSave,
}

impl FileEditorSession {
    /// 关闭决策复用聚合的 dirty 与 phase，保证标签关闭和窗口退出遵循同一规则。
    pub fn close_decision(&self, current_content: &str) -> CloseDecision {
        if self.phase == EditorPhase::Saving {
            CloseDecision::WaitForSave
        } else if self.is_dirty(current_content) {
            CloseDecision::ConfirmUnsavedChanges
        } else {
            CloseDecision::CloseNow
        }
    }
}
