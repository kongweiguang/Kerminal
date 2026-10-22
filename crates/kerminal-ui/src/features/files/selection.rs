//! 文件目录内的纯选择模型；导航刷新后由列表投影裁剪，不把选择写入远端状态。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

/// 鼠标修饰键映射为稳定的选择意图，模型本身不依赖 GPUI 事件类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionGesture {
    Replace,
    Toggle,
    Extend,
    AddRange,
}

/// 保存当前目录中的名称集合与范围锚点；文件 endpoint 由所属 BrowserPanel 隔离。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FileSelection {
    names: BTreeSet<String>,
    anchor: Option<String>,
}

impl FileSelection {
    /// 用恢复或显式定位的单项目重置目录选择，锚点与可视集合保持一致。
    ///
    /// @author kongweiguang
    pub(super) fn select_only(&mut self, name: &str) {
        self.names.clear();
        self.names.insert(name.to_owned());
        self.anchor = Some(name.to_owned());
    }

    /// 根据当前可见行顺序应用单选、修饰键切换或连续范围选择。
    ///
    /// @author kongweiguang
    pub(super) fn apply(
        &mut self,
        name: &str,
        ordered_names: &[String],
        gesture: SelectionGesture,
    ) {
        if !ordered_names.iter().any(|visible| visible == name) {
            return;
        }

        match gesture {
            SelectionGesture::Replace => {
                self.names.clear();
                self.names.insert(name.to_owned());
                self.anchor = Some(name.to_owned());
            }
            SelectionGesture::Toggle => {
                if !self.names.remove(name) {
                    self.names.insert(name.to_owned());
                }
                self.anchor = Some(name.to_owned());
            }
            SelectionGesture::Extend | SelectionGesture::AddRange => {
                let Some(target) = ordered_names.iter().position(|visible| visible == name) else {
                    return;
                };
                let anchor_name = self
                    .anchor
                    .as_deref()
                    .filter(|anchor| ordered_names.iter().any(|visible| visible == anchor))
                    .unwrap_or(name);
                let Some(anchor) = ordered_names
                    .iter()
                    .position(|visible| visible == anchor_name)
                else {
                    return;
                };
                if gesture == SelectionGesture::Extend {
                    self.names.clear();
                }
                for visible in &ordered_names[anchor.min(target)..=anchor.max(target)] {
                    self.names.insert(visible.clone());
                }
                self.anchor = Some(anchor_name.to_owned());
            }
        }
    }

    /// 只选择当前过滤并排序后的项目，避免隐藏项或过期列表项被批量操作带入。
    ///
    /// @author kongweiguang
    pub(super) fn select_all(&mut self, ordered_names: &[String]) {
        self.names = ordered_names.iter().cloned().collect();
        if self
            .anchor
            .as_ref()
            .is_none_or(|anchor| !self.names.contains(anchor))
        {
            self.anchor = ordered_names.first().cloned();
        }
    }

    /// 列表刷新或隐藏过滤变化后移除已不可见项，避免把旧目录选择误用于新端点。
    ///
    /// @author kongweiguang
    pub(super) fn retain_visible(&mut self, ordered_names: &[String]) {
        self.names
            .retain(|name| ordered_names.iter().any(|visible| visible == name));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !ordered_names.iter().any(|visible| visible == anchor))
        {
            self.anchor = None;
        }
    }

    /// 清空目录级选择与范围锚点，供成功导航、关闭与显式清除复用。
    ///
    /// @author kongweiguang
    pub(super) fn clear(&mut self) {
        self.names.clear();
        self.anchor = None;
    }

    /// 查询一行是否位于当前目录选择集合。
    ///
    /// @author kongweiguang
    pub(super) fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/files_selection.rs"]
mod tests;
