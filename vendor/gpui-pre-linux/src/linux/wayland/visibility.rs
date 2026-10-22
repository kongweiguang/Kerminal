// @author kongweiguang

/// 记录 XDG toplevel 的手动映射阶段，避免把空提交误报为已显示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SurfaceMapping {
    Mapped,
    Hidden,
    Remapping,
}

impl SurfaceMapping {
    /// 隐藏或取消尚未完成的 remap；重复隐藏保持幂等。
    pub(super) fn hide(&mut self) -> bool {
        if *self == Self::Hidden {
            return false;
        }
        *self = Self::Hidden;
        true
    }

    /// 只允许从明确 unmap 状态启动新的 configure 流程。
    pub(super) fn show(&mut self) -> bool {
        if *self != Self::Hidden {
            return false;
        }
        *self = Self::Remapping;
        true
    }

    /// compositor 仍报告 suspended 时，不在 remap 首帧前提交缓冲区。
    pub(super) fn can_render(self, compositor_visible: bool) -> bool {
        match self {
            Self::Mapped => true,
            Self::Hidden => false,
            Self::Remapping => compositor_visible,
        }
    }

    /// 只有 remap 后首个可呈现帧提交成功，才切回 mapped 并允许报告可见。
    pub(super) fn presented(&mut self, compositor_visible: bool) -> bool {
        if *self != Self::Remapping || !compositor_visible {
            return false;
        }
        *self = Self::Mapped;
        true
    }
}

// 测试实现独立放在 crate 的 tests 目录，生产源码只保留测试模块引用。
#[cfg(test)]
#[path = "../../../tests/wayland_visibility.rs"]
mod tests;
