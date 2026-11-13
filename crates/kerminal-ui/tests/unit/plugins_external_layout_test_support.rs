//! `plugins::external::layout` 的测试专用几何读取入口。
//!
//! 回归测试需要区分「矩阵边长」与「含静区绘制范围」，并核对原点是否对齐设备像素。
//! 这些方法放在 `tests/unit` 是因为该文件作为 `layout` 的子模块编译，可以读取父模块
//! 私有字段；`src` 中只保留 `#[cfg(test)] #[path] mod` 引用。
//!
//! @author kongweiguang

use super::QrLayout;

impl<'a> QrLayout<'a> {
    /// 原始矩阵边长（不含静区），只用于解释插件数据，不参与绘制循环。
    ///
    /// 保留独立命名是为了让回归测试能明确区分「矩阵边长」与「含静区绘制范围」，
    /// 这正是此前绘制被截断的根因。
    ///
    /// @author kongweiguang
    pub(in crate::plugins::external) fn matrix_side(&self) -> usize {
        self.rows.len()
    }

    /// 返回按设备像素对齐、且保证不越出画布的绝对左上角逻辑坐标；绘制段已含该原点，
    /// 此处仅供测试核对对齐结果。
    ///
    /// @author kongweiguang
    pub(in crate::plugins::external) fn origin(&self) -> (f32, f32) {
        (self.origin_x, self.origin_y)
    }
}
