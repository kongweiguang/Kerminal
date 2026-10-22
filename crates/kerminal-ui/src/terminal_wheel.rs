//! 将 GPUI 滚轮增量按终端网格行归一化，并保留触控板的小数余量。
//!
//! @author kongweiguang

use gpui_kit::{Pixels, ScrollDelta, TouchPhase};

/// 单次滚轮事件累计出的有符号滚动步数。
///
/// 纵向正值表示向上，横向正值表示向右。终端未启用鼠标报告时只消费 `vertical`；
/// 不能把横向滚动伪造成纵向滚动。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalWheelSteps {
    pub vertical: i32,
    pub horizontal: i32,
}

impl TerminalWheelSteps {
    /// 让调用方在没有完整滚动步时跳过队列写入。
    ///
    /// @author kongweiguang
    pub(crate) const fn is_empty(self) -> bool {
        self.vertical == 0 && self.horizontal == 0
    }
}

/// 每个 pane 独立持有的小数滚动状态，避免连续的 macOS 触控板事件被逐次舍入丢弃。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TerminalWheelAccumulator {
    vertical_fraction: f64,
    horizontal_fraction: f64,
}

impl TerminalWheelAccumulator {
    /// 清除未满一行的增量；pane/session 更换和新触控滚动开始时都应清空旧余量。
    ///
    /// @author kongweiguang
    pub(crate) fn reset(&mut self) {
        self.vertical_fraction = 0.0;
        self.horizontal_fraction = 0.0;
    }

    /// 将 GPUI 的逻辑像素或行增量转换成完整滚轮步，保留余数供后续事件继续累计。
    ///
    /// Started 表示新手势边界：先丢弃旧余量，再消费本事件，避免首次滚轮增量丢失。
    /// Ended/Cancelled 只清理手势余量，不提交终止事件的 delta；无效几何或非有限 delta
    /// 不会送入终端。
    ///
    /// @author kongweiguang
    pub(crate) fn push(
        &mut self,
        delta: &ScrollDelta,
        phase: TouchPhase,
        line_height: Pixels,
    ) -> TerminalWheelSteps {
        match phase {
            TouchPhase::Started => self.reset(),
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.reset();
                return TerminalWheelSteps::default();
            }
            TouchPhase::Moved => {}
        }

        let line_height = line_height.as_f32();
        if !line_height.is_finite() || line_height <= 0.0 {
            return TerminalWheelSteps::default();
        }

        let (horizontal, vertical) = match delta {
            ScrollDelta::Lines(lines) => (lines.x, lines.y),
            ScrollDelta::Pixels(pixels) => (
                pixels.x.as_f32() / line_height,
                pixels.y.as_f32() / line_height,
            ),
        };

        TerminalWheelSteps {
            vertical: take_whole_steps(&mut self.vertical_fraction, vertical),
            horizontal: take_whole_steps(&mut self.horizontal_fraction, horizontal),
        }
    }
}

/// 按向零截断提取完整步数，与 Zed 的触控板余量算法一致，并隔离异常大 delta。
///
/// @author kongweiguang
fn take_whole_steps(remainder: &mut f64, delta: f32) -> i32 {
    if !delta.is_finite() {
        return 0;
    }

    let total = *remainder + f64::from(delta);
    let whole = total.trunc();
    if whole > f64::from(i32::MAX) {
        *remainder = 0.0;
        return i32::MAX;
    }
    if whole < f64::from(i32::MIN) {
        *remainder = 0.0;
        return i32::MIN;
    }

    let steps = whole as i32;
    *remainder = total - f64::from(steps);
    steps
}

#[cfg(test)]
#[path = "../tests/unit/terminal_wheel.rs"]
mod tests;
