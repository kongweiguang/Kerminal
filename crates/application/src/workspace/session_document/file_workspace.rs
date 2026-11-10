//! 文件工作区将目录和文档放在同一持久 owner 内，未知字段仍由原始文档按稳定 ID 合并。
//!
//! @author kongweiguang

use super::*;

/// 内部标签与旧来源别名分开，关闭文档不会丢失仍需核对的传输来源。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileWorkspaceState {
    pub items: Vec<FileWorkspaceItem>,
    pub active_item_id: Option<TabId>,
    pub source_tab_ids: Vec<TabId>,
}

/// 文档保留自己的权限与入口语义，不能因为同机聚合而扩大写权限。
#[derive(Clone, Debug, PartialEq)]
pub enum FileWorkspaceItem {
    Directory {
        id: TabId,
        path: String,
    },
    Document {
        id: TabId,
        title: String,
        path: String,
        root_path: Option<String>,
        access: WorkspaceFileAccess,
        source: WorkspaceFileSource,
    },
}

impl FileWorkspaceItem {
    /// 让运行态按内部身份持有资源，避免标签重排后误操作另一个文档。
    /// @author kongweiguang
    pub fn id(&self) -> &TabId {
        match self {
            Self::Directory { id, .. } | Self::Document { id, .. } => id,
        }
    }

    /// 路径只是持久描述，实际访问必须由 owner 的精确 target 解析。
    /// @author kongweiguang
    pub fn path(&self) -> &str {
        match self {
            Self::Directory { path, .. } | Self::Document { path, .. } => path,
        }
    }
}

impl FileWorkspaceState {
    /// 只返回显式活动项，缺失选择时不猜测首个文档或改变恢复焦点。
    /// @author kongweiguang
    pub fn active_item(&self) -> Option<&FileWorkspaceItem> {
        let id = self.active_item_id.as_ref()?;
        self.items.iter().find(|item| item.id() == id)
    }

    /// 目录面板和检索共用同一活动引用，不各自维护索引产生漂移。
    /// @author kongweiguang
    pub fn active_path(&self) -> Option<&str> {
        self.active_item().map(FileWorkspaceItem::path)
    }

    /// 宿主必须保留目录入口且引用指向本 owner；损坏描述拒绝恢复，codec 不凭空创建资源。
    /// @author kongweiguang
    pub fn validate(&self) -> Result<(), WorkspaceSessionInvalidReason> {
        if !self
            .items
            .iter()
            .any(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
        {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
        let mut ids = BTreeSet::new();
        let mut document_paths = BTreeSet::new();
        for item in &self.items {
            if matches!(item, FileWorkspaceItem::Document { .. })
                && !document_paths.insert(item.path())
            {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
            if !ids.insert(item.id().clone())
                || item.path().is_empty()
                || item.path().chars().any(char::is_control)
            {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
            if let FileWorkspaceItem::Document {
                title, root_path, ..
            } = item
                && (title.is_empty()
                    || title.chars().any(char::is_control)
                    || root_path
                        .as_ref()
                        .is_some_and(|path| path.is_empty() || path.chars().any(char::is_control)))
            {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
        }
        if self
            .active_item_id
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
            || self.source_tab_ids.iter().collect::<BTreeSet<_>>().len()
                != self.source_tab_ids.len()
        {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
        Ok(())
    }
}

/// 版本必须显式声明，未知状态不降级为空工作区后覆盖原始材料。
/// @author kongweiguang
pub(super) fn decode_state(
    object: &Map<String, Value>,
) -> Result<FileWorkspaceState, WorkspaceSessionInvalidReason> {
    if object.get("stateVersion").and_then(Value::as_u64) != Some(1) {
        return Err(WorkspaceSessionInvalidReason::InvalidSchema);
    }
    let state = object
        .get("state")
        .and_then(Value::as_object)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let items = state
        .get("items")
        .and_then(Value::as_array)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let items = items
        .iter()
        .map(|value| {
            let item = value
                .as_object()
                .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
            let id = parse_tab_id(item.get("id"))?;
            let path = required_string(item.get("path"))?;
            match item.get("kind").and_then(Value::as_str) {
                Some("directory") => Ok(FileWorkspaceItem::Directory { id, path }),
                Some("document") => Ok(FileWorkspaceItem::Document {
                    id,
                    path,
                    title: required_string(item.get("title"))?,
                    root_path: optional_string(item.get("rootPath"))?,
                    access: decode_file_access(item.get("access"))?,
                    source: decode_file_source(item.get("source"))?,
                }),
                _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let active_item_id = state
        .get("activeItemId")
        .filter(|value| !value.is_null())
        .map(|value| parse_tab_id(Some(value)))
        .transpose()?;
    let source_tab_ids = state
        .get("sourceTabIds")
        .and_then(Value::as_array)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?
        .iter()
        .map(|value| parse_tab_id(Some(value)))
        .collect::<Result<Vec<_>, _>>()?;
    let state = FileWorkspaceState {
        items,
        active_item_id,
        source_tab_ids,
    };
    state.validate()?;
    Ok(state)
}

/// 依稳定 ID 而非数组位置合并未知字段，重排和关闭内部标签不会串用扩展信息。
/// @author kongweiguang
pub(super) fn merge_state(
    existing: Option<&Value>,
    state: &FileWorkspaceState,
) -> Result<Value, WorkspaceSessionInvalidReason> {
    state.validate()?;
    let mut fields = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let old = fields
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::with_capacity(state.items.len());
    for item in &state.items {
        let mut fields = old
            .iter()
            .find(|value| value.get("id").and_then(Value::as_str) == Some(item.id().as_str()))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        fields.insert("id".into(), json!(item.id().as_str()));
        fields.insert("path".into(), json!(item.path()));
        match item {
            FileWorkspaceItem::Directory { .. } => {
                fields.insert("kind".into(), json!("directory"));
            }
            FileWorkspaceItem::Document {
                title,
                root_path,
                access,
                source,
                ..
            } => {
                fields.insert("kind".into(), json!("document"));
                fields.insert("title".into(), json!(title));
                set_optional_string(&mut fields, "rootPath", root_path.as_deref());
                fields.insert("access".into(), json!(file_access_str(*access)));
                fields.insert("source".into(), json!(file_source_str(*source)));
            }
        }
        items.push(Value::Object(fields));
    }
    fields.insert("items".into(), Value::Array(items));
    set_optional_string(
        &mut fields,
        "activeItemId",
        state.active_item_id.as_ref().map(TabId::as_str),
    );
    fields.insert(
        "sourceTabIds".into(),
        Value::Array(
            state
                .source_tab_ids
                .iter()
                .map(|id| json!(id.as_str()))
                .collect(),
        ),
    );
    Ok(Value::Object(fields))
}
