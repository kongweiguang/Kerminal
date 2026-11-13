//! `plugins::tools` 的测试专用读取入口。
//!
//! 这些方法只用于断言投影代次和已暂存投影，不参与生产调度。放在 `tests/unit`
//! 是因为该文件作为 `tools` 的子模块编译，可以读取父模块私有字段；`src` 中只保留
//! `#[cfg(test)] #[path] mod` 引用，避免生产模块出现 test-only 项。
//!
//! @author kongweiguang

use super::{PluginTools, ProjectionTicket, ToolProjection};

impl ProjectionTicket {
    /// 测试只读代次；生产调度由当前投影比较决定，不暴露可变构造入口。
    ///
    /// @author kongweiguang
    pub(crate) fn generation(self) -> u64 {
        self.generation
    }
}

impl PluginTools {
    /// 测试读取真实发起过生命周期的代次；仅观察，不参与调度。
    ///
    /// @author kongweiguang
    pub(crate) fn attempted_projection_generations_for_test(&self) -> Vec<u64> {
        self.attempted_projection_generations.clone()
    }

    /// 测试读取当前已暂存投影，用于构造值相同的旧代次票据；生产路径不暴露投影快照。
    ///
    /// @author kongweiguang
    pub(crate) fn current_projection_for_test(&self) -> Option<ToolProjection> {
        self.projection.clone()
    }
}
