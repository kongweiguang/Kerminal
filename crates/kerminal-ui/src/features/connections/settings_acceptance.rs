//! 隔离原生验收所需的只读表单裁切区域。
//!
//! @author kongweiguang

use super::ConnectionsPanel;

impl ConnectionsPanel {
    /// 读取实际滚动裁切区，确保验收只点击完整可见的生产控件。
    ///
    /// @author kongweiguang
    pub(crate) fn settings_acceptance_local_editor_bounds(
        &self,
    ) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
        self.local_hosts
            .editor
            .as_ref()
            .map(|editor| editor.scroll_handle.bounds())
    }
}
