//! 原生主窗口几何与恢复状态的应用端口。
//!
//! @author kongweiguang

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::AppResult;

const MIN_WINDOW_WIDTH: u32 = 640;
const MIN_WINDOW_HEIGHT: u32 = 420;
const MAX_WINDOW_DIMENSION: u32 = 16_384;

/// 主窗口正常状态下的屏幕坐标与尺寸；最大化和全屏时仍保留这份还原矩形。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowRect {
    /// 限定可持久化的窗口尺寸，避免损坏配置制造零尺寸或异常大的原生窗口。
    pub fn is_valid(self) -> bool {
        (MIN_WINDOW_WIDTH..=MAX_WINDOW_DIMENSION).contains(&self.width)
            && (MIN_WINDOW_HEIGHT..=MAX_WINDOW_DIMENSION).contains(&self.height)
    }
}

/// 原生窗口重开时应恢复的状态；最大化与全屏独立保存正常窗口矩形。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowStateSnapshot {
    pub normal_bounds: WindowRect,
    pub maximized: bool,
    pub fullscreen: bool,
}

/// 普通运行态只接受当前文件；历史窗口状态在启动前已由一次迁移转换。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowStateSource {
    Current,
}

/// 状态读取的显式结果；损坏、未来版本和 I/O 故障都不会被当成空状态写回。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowStateLoad {
    Missing,
    Restored {
        snapshot: WindowStateSnapshot,
        revision: Option<u64>,
        source: WindowStateSource,
    },
    Unavailable {
        reason: WindowStateUnavailableReason,
    },
}

/// 恢复不可用的原因供窗口状态提示展示；不会含绝对路径或配置正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowStateUnavailableReason {
    Corrupt,
    UnsupportedSchemaVersion(u32),
    StorageUnavailable,
    StorageLockUnavailable,
}

impl WindowStateUnavailableReason {
    /// 生成不暴露本机路径或文件内容的稳定用户提示。
    pub fn safe_message(&self) -> String {
        match self {
            Self::Corrupt => "窗口状态文件损坏，已保留原文件且本次不会写回。".to_owned(),
            Self::UnsupportedSchemaVersion(version) => {
                format!("窗口状态版本 {version} 暂不支持，原文件已保留且不会写回。")
            }
            Self::StorageUnavailable => "窗口状态存储不可用，本次将使用默认窗口布局。".to_owned(),
            Self::StorageLockUnavailable => {
                "窗口状态存储正由另一进程更新，本次不会覆盖其内容。".to_owned()
            }
        }
    }
}

/// 明确区分系统托盘已装配时的隐藏行为与无托盘时的退出门禁。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosePolicy {
    HideToTray,
    RequestQuit,
}

/// 仅在原生托盘实际安装成功时关闭窗口转为隐藏；显式退出仍由 Root 处理。
pub fn close_policy(tray_installed: bool) -> ClosePolicy {
    if tray_installed {
        ClosePolicy::HideToTray
    } else {
        ClosePolicy::RequestQuit
    }
}

/// 原生窗口状态存储端口；compare-and-swap 防止另一实例的窗口几何被静默覆盖。
#[async_trait]
pub trait WindowStateRepository: Send + Sync + 'static {
    /// 仅读取当前格式，缺失返回 Missing，不再次触碰旧版来源。
    ///
    /// @author kongweiguang
    async fn load(&self) -> AppResult<WindowStateLoad>;

    /// 以预期版本原子写入；None 表示调用方要求当前文件仍不存在。
    async fn compare_and_swap(
        &self,
        expected_revision: Option<u64>,
        snapshot: WindowStateSnapshot,
    ) -> AppResult<u64>;
}
