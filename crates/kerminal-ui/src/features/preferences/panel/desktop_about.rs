//! 桌面设置与内嵌 About 状态；不复制共享配置或更新事务。
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity};

use crate::features::updates::UpdatesPanel;

use super::PreferencesPanel;

/// 只保存 Disclosure 的本地呈现状态，其余值始终从设置快照和共享更新面板读取。
#[derive(Default)]
pub(in crate::features::preferences) struct DesktopAboutState {
    pub(in crate::features::preferences) advanced_expanded: bool,
}

impl PreferencesPanel {
    /// 把已由 Root 订阅生命周期事件的 UpdatesPanel 挂到 About；observe 只触发重绘。
    ///
    /// @author kongweiguang
    pub(crate) fn attach_inline_updates(
        &mut self,
        updates: Entity<UpdatesPanel>,
        cx: &mut Context<Self>,
    ) {
        self._inline_updates_subscription = Some(cx.observe(&updates, |_, _, cx| cx.notify()));
        self.inline_updates = Some(updates);
        cx.notify();
    }

    /// 关于页的更新操作只转交唯一的 UpdatesPanel，安装仍经过 Root 退出保护。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn run_inline_update_action(
        &mut self,
        action: crate::features::updates::AboutUpdateAction,
        cx: &mut Context<Self>,
    ) {
        if let Some(updates) = self.inline_updates.clone() {
            updates.update(cx, |panel, cx| panel.settings_action(action, cx));
        }
    }
}
