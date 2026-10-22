//! 文件工作区的纯展示模式；真实文件操作仍由同一 FileService/TransferService 执行。
//!
//! @author kongweiguang

use kerminal_domain::files::FileEntryKind;

/// 列表、惰性树与编辑器工作区概览共享文件端点与传输队列，不复制业务状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum FilesViewMode {
    #[default]
    List,
    Tree,
    Workspace,
}

/// 文件列表过滤只控制可见集合，不改变目录读取结果或 endpoint 选择身份。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FileEntryFilter {
    #[default]
    All,
    Files,
    Directories,
}

impl FileEntryFilter {
    /// 特殊项仍归入“全部”；文件与目录计数只包含其精确类型。
    ///
    /// @author kongweiguang
    pub(super) fn matches(self, kind: FileEntryKind) -> bool {
        match self {
            Self::All => true,
            Self::Files => kind == FileEntryKind::File,
            Self::Directories => kind == FileEntryKind::Directory,
        }
    }
}
