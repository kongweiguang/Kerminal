//! Workspace 的纯领域模型与递归终端布局操作。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

use crate::identity::PaneId;

/// 与已保存 v3 workspace session 中方向字符串保持一致。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// v3 使用子 pane/split ID 作为键；保留键形状以便旧布局原样读写。
pub type SplitLayoutSizes = std::collections::BTreeMap<String, f64>;

/// 终端布局只保存结构和稳定 ID，不持有 GPUI Entity 或终端会话资源。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalLayoutNode {
    Pane {
        #[serde(rename = "paneId")]
        pane_id: PaneId,
    },
    Split {
        id: String,
        direction: SplitDirection,
        children: Vec<TerminalLayoutNode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sizes: Option<SplitLayoutSizes>,
    },
}

/// 布局命令拒绝悬空 pane 和非法比例，避免持久化后无法恢复的树形结构。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceLayoutError {
    PaneNotFound(PaneId),
    DuplicatePane(PaneId),
    DuplicateSplit(String),
    InvalidStructure(String),
    SplitNotFound(String),
    InvalidSplitSizes(String),
}

impl fmt::Display for WorkspaceLayoutError {
    /// 提供可显示且不依赖 UI 的错误文本，供 application 层映射到产品消息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PaneNotFound(id) => write!(formatter, "pane not found: {id}"),
            Self::DuplicatePane(id) => write!(formatter, "pane already exists: {id}"),
            Self::DuplicateSplit(id) => write!(formatter, "split already exists: {id}"),
            Self::InvalidStructure(reason) => {
                write!(formatter, "invalid workspace layout: {reason}")
            }
            Self::SplitNotFound(id) => write!(formatter, "split not found: {id}"),
            Self::InvalidSplitSizes(id) => write!(formatter, "invalid child sizes for split: {id}"),
        }
    }
}

impl std::error::Error for WorkspaceLayoutError {}

impl TerminalLayoutNode {
    /// 创建单 pane 初始布局，保持新 Tab 与 v3 的 `{ type: pane, paneId }` 一致。
    pub fn pane(pane_id: PaneId) -> Self {
        Self::Pane { pane_id }
    }

    /// 深度优先列出 pane ID，顺序稳定且与布局中的显示顺序相同。
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.collect_pane_ids(&mut ids);
        ids
    }

    /// 检查 pane 是否被布局树引用，避免调用方复制递归遍历规则。
    pub fn contains_pane(&self, pane_id: &PaneId) -> bool {
        match self {
            Self::Pane { pane_id: current } => current == pane_id,
            Self::Split { children, .. } => {
                children.iter().any(|child| child.contains_pane(pane_id))
            }
        }
    }

    /// 验证可持久化结构，包含 split 唯一性和 child-size 映射而不只验证叶子 ID。
    pub fn validate(&self) -> Result<(), WorkspaceLayoutError> {
        validate_unique_pane_ids(self)
    }

    /// 在目标 pane 旁增加一个叶子；先拒绝重复 ID，防止两处渲染同一 session。
    pub fn split_pane(
        &self,
        target: &PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
        split_id: impl Into<String>,
        before: bool,
    ) -> Result<Self, WorkspaceLayoutError> {
        self.validate()?;
        if self.contains_pane(&new_pane) {
            return Err(WorkspaceLayoutError::DuplicatePane(new_pane));
        }
        if !self.contains_pane(target) {
            return Err(WorkspaceLayoutError::PaneNotFound(target.clone()));
        }

        let split_id = split_id.into();
        if split_id.trim().is_empty() {
            return Err(WorkspaceLayoutError::InvalidStructure(
                "split ID cannot be empty".to_owned(),
            ));
        }
        if self.contains_split(&split_id) {
            return Err(WorkspaceLayoutError::DuplicateSplit(split_id));
        }
        let next = self.split_pane_inner(target, new_pane, direction, &split_id, before);
        next.validate()?;
        Ok(next)
    }

    /// 更新一个 splitter 的持久化比例；比例键必须与当前子节点完全匹配。
    pub fn resize_split(
        &self,
        split_id: &str,
        sizes: SplitLayoutSizes,
    ) -> Result<Self, WorkspaceLayoutError> {
        self.validate()?;
        let Some(children) = self.find_split_children(split_id) else {
            return Err(WorkspaceLayoutError::SplitNotFound(split_id.to_owned()));
        };
        if !valid_child_sizes(children, &sizes) {
            return Err(WorkspaceLayoutError::InvalidSplitSizes(split_id.to_owned()));
        }
        Ok(self.resize_split_inner(split_id, &sizes))
    }

    /// 删除 pane 并折叠只有一个 child 的 split，防止空容器残留在 v3 树中。
    pub fn remove_pane(&self, pane_id: &PaneId) -> Result<Option<Self>, WorkspaceLayoutError> {
        self.validate()?;
        if !self.contains_pane(pane_id) {
            return Err(WorkspaceLayoutError::PaneNotFound(pane_id.clone()));
        }
        let next = remove_pane_inner(self, pane_id);
        if let Some(layout) = &next {
            layout.validate()?;
        }
        Ok(next)
    }

    /// 在树结构不变的情况下交换两个 pane 的位置，比例保留在原来的布局槽位。
    pub fn swap_panes(
        &self,
        first: &PaneId,
        second: &PaneId,
    ) -> Result<Self, WorkspaceLayoutError> {
        self.validate()?;
        if !self.contains_pane(first) {
            return Err(WorkspaceLayoutError::PaneNotFound(first.clone()));
        }
        if !self.contains_pane(second) {
            return Err(WorkspaceLayoutError::PaneNotFound(second.clone()));
        }
        if first == second {
            return Ok(self.clone());
        }
        let next = swap_pane_ids(self, first, second);
        next.validate()?;
        Ok(next)
    }

    /// 检查 split ID 是否已用过；v3 把它作为父布局的 child-weight key。
    fn contains_split(&self, split_id: &str) -> bool {
        match self {
            Self::Pane { .. } => false,
            Self::Split { id, children, .. } => {
                id == split_id || children.iter().any(|child| child.contains_split(split_id))
            }
        }
    }

    /// 递归收集 pane ID；调用方提供缓冲区，避免每层 split 都创建临时 Vec。
    fn collect_pane_ids(&self, ids: &mut Vec<PaneId>) {
        match self {
            Self::Pane { pane_id } => ids.push(pane_id.clone()),
            Self::Split { children, .. } => {
                for child in children {
                    child.collect_pane_ids(ids);
                }
            }
        }
    }

    /// 仅变更命中的叶子；父 split 的比例键随被替换 child 一起迁移。
    fn split_pane_inner(
        &self,
        target: &PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
        split_id: &str,
        before: bool,
    ) -> Self {
        match self {
            Self::Pane { pane_id } if pane_id == target => {
                let target = Self::Pane {
                    pane_id: pane_id.clone(),
                };
                let added = Self::Pane { pane_id: new_pane };
                let children = if before {
                    vec![added, target]
                } else {
                    vec![target, added]
                };
                Self::Split {
                    id: split_id.to_owned(),
                    direction,
                    children,
                    sizes: None,
                }
            }
            Self::Pane { .. } => self.clone(),
            Self::Split {
                id,
                direction: current_direction,
                children,
                sizes,
            } => {
                let next_children = children
                    .iter()
                    .map(|child| {
                        child.split_pane_inner(
                            target,
                            new_pane.clone(),
                            direction,
                            split_id,
                            before,
                        )
                    })
                    .collect::<Vec<_>>();
                let next_sizes = sizes
                    .as_ref()
                    .map(|map| rekey_sizes(children, &next_children, map));
                Self::Split {
                    id: id.clone(),
                    direction: *current_direction,
                    children: next_children,
                    sizes: next_sizes,
                }
            }
        }
    }

    /// 定位 split 的直接 child 集合，以便在不重建树的情况下校验比例键。
    fn find_split_children(&self, split_id: &str) -> Option<&[Self]> {
        match self {
            Self::Pane { .. } => None,
            Self::Split { id, children, .. } if id == split_id => Some(children),
            Self::Split { children, .. } => children
                .iter()
                .find_map(|child| child.find_split_children(split_id)),
        }
    }

    /// 仅更新唯一匹配节点的比例，避免重复 split ID 引发多处同时变更。
    fn resize_split_inner(&self, split_id: &str, sizes: &SplitLayoutSizes) -> Self {
        match self {
            Self::Pane { .. } => self.clone(),
            Self::Split {
                id,
                direction,
                children,
                sizes: current,
            } => Self::Split {
                id: id.clone(),
                direction: *direction,
                children: children
                    .iter()
                    .map(|child| child.resize_split_inner(split_id, sizes))
                    .collect(),
                sizes: if id == split_id {
                    Some(sizes.clone())
                } else {
                    current.clone()
                },
            },
        }
    }
}

/// 子节点大小按稳定 ID 一一映射；非有限或非正值无法安全参与 flex 布局。
fn valid_child_sizes(children: &[TerminalLayoutNode], sizes: &SplitLayoutSizes) -> bool {
    if children.len() < 2 || sizes.len() != children.len() {
        return false;
    }
    children.iter().all(|child| {
        sizes
            .get(layout_child_id(child))
            .is_some_and(|size| size.is_finite() && *size > 0.0)
    })
}

/// 返回与旧 workspace session 一致的 child map 键。
fn layout_child_id(child: &TerminalLayoutNode) -> &str {
    match child {
        TerminalLayoutNode::Pane { pane_id } => pane_id.as_str(),
        TerminalLayoutNode::Split { id, .. } => id,
    }
}

/// 按 child 的树槽位构造新比例表，避免交换两个 ID 时覆盖对方原始权重。
fn rekey_sizes(
    old_children: &[TerminalLayoutNode],
    new_children: &[TerminalLayoutNode],
    old_sizes: &SplitLayoutSizes,
) -> SplitLayoutSizes {
    old_children
        .iter()
        .zip(new_children)
        .filter_map(|(old, new)| {
            old_sizes
                .get(layout_child_id(old))
                .map(|weight| (layout_child_id(new).to_owned(), *weight))
        })
        .collect()
}

/// 删除单一叶子后收缩 split，并将 surviving child 的权重键重映射到新树槽位。
fn remove_pane_inner(node: &TerminalLayoutNode, pane_id: &PaneId) -> Option<TerminalLayoutNode> {
    match node {
        TerminalLayoutNode::Pane { pane_id: current } if current == pane_id => None,
        TerminalLayoutNode::Pane { .. } => Some(node.clone()),
        TerminalLayoutNode::Split {
            id,
            direction,
            children,
            sizes,
        } => {
            let survivors = children
                .iter()
                .filter_map(|child| remove_pane_inner(child, pane_id).map(|next| (child, next)))
                .collect::<Vec<_>>();
            match survivors.len() {
                0 => None,
                1 => survivors.into_iter().next().map(|(_, child)| child),
                _ => {
                    let next_sizes = sizes.as_ref().map(|old_sizes| {
                        survivors
                            .iter()
                            .filter_map(|(old, new)| {
                                old_sizes
                                    .get(layout_child_id(old))
                                    .map(|weight| (layout_child_id(new).to_owned(), *weight))
                            })
                            .collect()
                    });
                    Some(TerminalLayoutNode::Split {
                        id: id.clone(),
                        direction: *direction,
                        children: survivors.into_iter().map(|(_, child)| child).collect(),
                        sizes: next_sizes,
                    })
                }
            }
        }
    }
}

/// 交换 pane ID 时保持空间权重属于布局槽位，随后按新 child ID 重建比例键。
fn swap_pane_ids(node: &TerminalLayoutNode, first: &PaneId, second: &PaneId) -> TerminalLayoutNode {
    match node {
        TerminalLayoutNode::Pane { pane_id } if pane_id == first => TerminalLayoutNode::Pane {
            pane_id: second.clone(),
        },
        TerminalLayoutNode::Pane { pane_id } if pane_id == second => TerminalLayoutNode::Pane {
            pane_id: first.clone(),
        },
        TerminalLayoutNode::Pane { .. } => node.clone(),
        TerminalLayoutNode::Split {
            id,
            direction,
            children,
            sizes,
        } => {
            let next_children = children
                .iter()
                .map(|child| swap_pane_ids(child, first, second))
                .collect::<Vec<_>>();
            let next_sizes = sizes
                .as_ref()
                .map(|map| rekey_sizes(children, &next_children, map));
            TerminalLayoutNode::Split {
                id: id.clone(),
                direction: *direction,
                children: next_children,
                sizes: next_sizes,
            }
        }
    }
}

/// 验证 pane/split ID、split 结构和比例映射；恢复 v3 数据时拒绝无法安全重绘的树。
pub fn validate_unique_pane_ids(layout: &TerminalLayoutNode) -> Result<(), WorkspaceLayoutError> {
    let mut pane_ids = BTreeSet::new();
    let mut split_ids = BTreeSet::new();
    validate_node(layout, &mut pane_ids, &mut split_ids)
}

/// 递归验证所有不变量，使 resize/split/restore 共用一套持久化边界。
fn validate_node(
    node: &TerminalLayoutNode,
    pane_ids: &mut BTreeSet<PaneId>,
    split_ids: &mut BTreeSet<String>,
) -> Result<(), WorkspaceLayoutError> {
    match node {
        TerminalLayoutNode::Pane { pane_id } => {
            if !pane_ids.insert(pane_id.clone()) {
                return Err(WorkspaceLayoutError::DuplicatePane(pane_id.clone()));
            }
        }
        TerminalLayoutNode::Split {
            id,
            children,
            sizes,
            ..
        } => {
            if id.trim().is_empty() {
                return Err(WorkspaceLayoutError::InvalidStructure(
                    "split ID cannot be empty".to_owned(),
                ));
            }
            if !split_ids.insert(id.clone()) {
                return Err(WorkspaceLayoutError::DuplicateSplit(id.clone()));
            }
            if children.len() < 2 {
                return Err(WorkspaceLayoutError::InvalidStructure(format!(
                    "split {id} must contain at least two children"
                )));
            }
            let mut child_ids = BTreeSet::new();
            for child in children {
                // Report globally duplicated identities precisely before checking a sibling-key collision.
                validate_node(child, pane_ids, split_ids)?;
                if !child_ids.insert(layout_child_id(child).to_owned()) {
                    return Err(WorkspaceLayoutError::InvalidStructure(format!(
                        "split {id} has ambiguous child IDs"
                    )));
                }
            }
            if sizes
                .as_ref()
                .is_some_and(|sizes| !valid_child_sizes(children, sizes))
            {
                return Err(WorkspaceLayoutError::InvalidSplitSizes(id.clone()));
            }
        }
    }
    Ok(())
}
