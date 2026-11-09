//! 原生视图只描述语义组件，结构静态且不能注入样式或代码。
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    ProtocolError, invalid,
    manifest::{valid_local_id, valid_text},
    parse_json, validate_extensions,
};

pub const VIEW_SCHEMA_VERSION: u32 = 1;
pub const MAX_VIEW_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_VIEW_NODES: usize = 512;
pub const MAX_VIEW_DEPTH: usize = 24;
pub const MAX_INLINE_ROWS: usize = 500;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NativeView {
    pub schema: u32,
    #[serde(default)]
    pub data: Map<String, Value>,
    pub root: NativeNode,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Gap {
    Sm,
    Md,
    Lg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Default,
    Muted,
    Success,
    Warning,
    Danger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CollectionSource {
    Rpc,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NativeNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<String>,
    #[serde(flatten)]
    pub kind: NativeNodeKind,
}

macro_rules! native_nodes {
    ($( $name:ident { required { $( $required:ident : $required_type:ty ),* $(,)? } optional { $( $optional:ident : $optional_type:ty ),* $(,)? } } ),* $(,)?) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "type", rename_all = "lowercase")]
        pub enum NativeNodeKind {
            $( $name {
                $( $required: $required_type, )*
                $( #[serde(default, skip_serializing_if = "Option::is_none")] $optional: Option<$optional_type>, )*
                #[serde(flatten)] extensions: BTreeMap<String, Value>,
            }, )*
        }

        impl NativeNodeKind {
            /// 全部节点使用相同的扩展字段策略，未知样式不会被静默接受。
            fn extensions(&self) -> &BTreeMap<String, Value> {
                match self { $( Self::$name { extensions, .. } => extensions, )* }
            }
        }
    };
}

native_nodes! {
    Stack { required { children: Vec<NativeNode> } optional { gap: Gap } },
    Row { required { children: Vec<NativeNode> } optional { gap: Gap } },
    Section { required { title: String, children: Vec<NativeNode> } optional { gap: Gap } },
    Scroll { required { child: Box<NativeNode> } optional {} },
    Split { required { children: Vec<NativeNode> } optional {} },
    Tabs { required { tabs: Vec<TabPane> } optional {} },
    Text { required { text: String } optional { tone: Tone } },
    Markdown { required { text: String } optional {} },
    Badge { required { text: String } optional { tone: Tone } },
    Icon { required { icon: String } optional {} },
    Code { required { code: String } optional { language: String } },
    Progress { required { value: Value } optional { max: f64 } },
    Empty { required { title: String } optional { icon: String, description: String } },
    Notice { required { text: String } optional { tone: Tone } },
    Input { required { bind: String } optional { label: String, placeholder: String, on_submit: String } },
    Textarea { required { bind: String } optional { label: String, placeholder: String } },
    Number { required { bind: String } optional { label: String, min: f64, max: f64, step: f64 } },
    Select { required { bind: String, options: Vec<SelectOption> } optional { label: String } },
    Switch { required { bind: String } optional { label: String } },
    Checkbox { required { bind: String } optional { label: String } },
    Path { required { bind: String } optional { label: String, mode: PickerMode } },
    Secret { required { bind: String } optional { label: String } },
    Button { required { action: String } optional { label: String, icon: String, variant: ButtonVariant, busy: String } },
    Toolbar { required { children: Vec<NativeNode> } optional { gap: Gap } },
    Menu { required { items: Vec<MenuItem> } optional {} },
    List { required { key: String, title: String } optional { items: String, source: CollectionSource, subtitle: String, on_activate: String, menu: Vec<MenuItem>, empty: EmptyState } },
    Table { required { key: String, columns: Vec<TableColumn> } optional { items: String, source: CollectionSource, on_activate: String, menu: Vec<MenuItem>, empty: EmptyState } },
    Tree { required { key: String, title: String } optional { items: String, source: CollectionSource, on_activate: String, menu: Vec<MenuItem>, empty: EmptyState } },
    Form { required { schema: String } optional { bind: String } },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PickerMode {
    Open,
    Save,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TabPane {
    pub id: String,
    pub title: String,
    pub content: Box<NativeNode>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelectOption {
    pub label: String,
    pub value: Value,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MenuItem {
    pub label: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger: Option<bool>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EmptyState {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TableColumn {
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sortable: Option<bool>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// 视图文件单独解析，保证打包检查和宿主加载采用相同的结构上限。
pub fn parse_native_view(bytes: &[u8]) -> Result<NativeView, ProtocolError> {
    if bytes.len() > MAX_VIEW_BYTES {
        return Err(invalid("view", "视图文件超过大小上限"));
    }
    let value = parse_json(bytes, "view")?;
    let view: NativeView = serde_json::from_value(value).map_err(|source| ProtocolError::Json {
        location: "view".into(),
        source,
    })?;
    view.validate()?;
    Ok(view)
}

impl NativeView {
    /// 在进入渲染器前一次检查全树，跨分支共享节点和深度预算。
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != VIEW_SCHEMA_VERSION {
            return Err(invalid("view.schema", "不支持的视图版本"));
        }
        validate_extensions(&self.extensions, "view")?;
        let mut budget = MAX_VIEW_NODES;
        let mut ids = BTreeSet::new();
        self.root.validate(1, &mut budget, &mut ids, &self.data)
    }
}

impl NativeNode {
    /// 深度与数量按整棵树计算，避免大量同级组件绕过限制。
    fn validate<'a>(
        &'a self,
        depth: usize,
        budget: &mut usize,
        ids: &mut BTreeSet<&'a str>,
        data: &Map<String, Value>,
    ) -> Result<(), ProtocolError> {
        if depth > MAX_VIEW_DEPTH || *budget == 0 {
            return Err(invalid("view.root", "视图节点数量或深度超限"));
        }
        *budget -= 1;
        validate_extensions(self.kind.extensions(), "view.node")?;
        if let Some(id) = &self.id
            && (!valid_local_id(id) || !ids.insert(id))
        {
            return Err(invalid("view.node.id", "节点 ID 无效或重复"));
        }
        if self
            .visible
            .as_ref()
            .is_some_and(|value| !valid_binding(value, true))
        {
            return Err(invalid("view.node.visible", "visible 必须是只读绑定"));
        }
        use NativeNodeKind::*;
        let action = match &self.kind {
            Input { on_submit, .. } => on_submit.as_deref(),
            List { on_activate, .. } | Table { on_activate, .. } | Tree { on_activate, .. } => {
                on_activate.as_deref()
            }
            _ => None,
        };
        if action.is_some_and(|action| !valid_local_id(action)) {
            return Err(invalid("view.action", "动作标识无效"));
        }
        match &self.kind {
            Stack { children, .. }
            | Row { children, .. }
            | Toolbar { children, .. }
            | Section { children, .. }
            | Split { children, .. } => {
                if matches!(&self.kind, Split { .. }) && children.len() < 2 {
                    return Err(invalid("view.split", "分栏至少需要两个子节点"));
                }
                for child in children {
                    child.validate(depth + 1, budget, ids, data)?;
                }
            }
            Scroll { child, .. } => child.validate(depth + 1, budget, ids, data)?,
            Tabs { tabs, .. } => {
                let mut pane_ids = BTreeSet::new();
                for tab in tabs {
                    validate_extensions(&tab.extensions, "view.tabs")?;
                    if !valid_local_id(&tab.id)
                        || !valid_text(&tab.title, 512)
                        || !pane_ids.insert(&tab.id)
                    {
                        return Err(invalid("view.tabs", "页签 ID 或标题无效"));
                    }
                    tab.content.validate(depth + 1, budget, ids, data)?;
                }
            }
            Input { bind, .. }
            | Textarea { bind, .. }
            | Number { bind, .. }
            | Select { bind, .. }
            | Switch { bind, .. }
            | Checkbox { bind, .. }
            | Path { bind, .. }
            | Secret { bind, .. } => {
                if !valid_binding(bind, false) {
                    return Err(invalid("view.bind", "双向绑定必须是对象键链"));
                }
            }
            Button {
                action,
                label,
                icon,
                busy,
                ..
            } => {
                if !valid_local_id(action)
                    || (label.is_none() && icon.is_none())
                    || busy
                        .as_ref()
                        .is_some_and(|value| !valid_binding(value, true))
                {
                    return Err(invalid("view.button", "按钮动作、名称或忙碌绑定无效"));
                }
            }
            Menu { items, .. } => validate_menu(items)?,
            List {
                items,
                source,
                key,
                title,
                menu,
                empty,
                ..
            }
            | Tree {
                items,
                source,
                key,
                title,
                menu,
                empty,
                ..
            } => {
                validate_collection(self.id.as_deref(), items.as_deref(), *source, key, data)?;
                if !valid_binding(title, false) {
                    return Err(invalid("view.collection.title", "集合标题字段无效"));
                }
                validate_collection_details(menu.as_deref(), empty.as_ref())?;
            }
            Table {
                items,
                source,
                key,
                columns,
                menu,
                empty,
                ..
            } => {
                validate_collection(self.id.as_deref(), items.as_deref(), *source, key, data)?;
                if columns.is_empty() {
                    return Err(invalid("view.table", "表格需要列定义"));
                }
                for column in columns {
                    validate_extensions(&column.extensions, "view.table.column")?;
                    if !valid_binding(&column.key, false) || !valid_text(&column.title, 512) {
                        return Err(invalid("view.table.column", "列键或标题无效"));
                    }
                }
                validate_collection_details(menu.as_deref(), empty.as_ref())?;
            }
            Form { schema, bind, .. } => {
                crate::validate_package_path(schema)?;
                if bind
                    .as_ref()
                    .is_some_and(|value| !valid_binding(value, false))
                {
                    return Err(invalid("view.form.bind", "表单绑定无效"));
                }
            }
            _ => {}
        }
        if let Select { options, .. } = &self.kind {
            for option in options {
                validate_extensions(&option.extensions, "view.select.option")?;
                if !valid_text(&option.label, 512) {
                    return Err(invalid("view.select.option", "选项文案无效"));
                }
            }
        }
        Ok(())
    }
}

/// 双向绑定不带 $，只读绑定带 $；数组索引和表达式均不执行。
fn valid_binding(path: &str, read_only: bool) -> bool {
    let path = if read_only {
        match path.strip_prefix('$') {
            Some(path) => path,
            None => return false,
        }
    } else {
        path
    };
    !path.is_empty()
        && path.len() <= 256
        && path.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

/// 集合必须选择唯一来源；RPC 分页需要稳定 ID，内联初值受行数阈值约束。
fn validate_collection(
    id: Option<&str>,
    items: Option<&str>,
    source: Option<CollectionSource>,
    key: &str,
    data: &Map<String, Value>,
) -> Result<(), ProtocolError> {
    if !valid_binding(key, false)
        || items.is_some_and(|value| !valid_binding(value, true))
        || (source == Some(CollectionSource::Rpc) && (id.is_none() || items.is_some()))
        || (source.is_none() && items.is_none())
    {
        return Err(invalid("view.collection", "集合来源、键或分页节点 ID 无效"));
    }
    if source.is_none()
        && let Some(path) = items.and_then(|value| value.strip_prefix('$'))
    {
        let mut current = data.get(path.split('.').next().unwrap_or_default());
        for segment in path.split('.').skip(1) {
            current = current.and_then(|value| value.get(segment));
        }
        if current
            .and_then(Value::as_array)
            .is_some_and(|rows| rows.len() > MAX_INLINE_ROWS)
        {
            return Err(invalid(
                "view.collection",
                "超过 500 行的集合必须使用 RPC 分页",
            ));
        }
    }
    Ok(())
}

/// 集合与菜单共享相同的扩展和文案约束。
fn validate_collection_details(
    menu: Option<&[MenuItem]>,
    empty: Option<&EmptyState>,
) -> Result<(), ProtocolError> {
    if let Some(menu) = menu {
        validate_menu(menu)?;
    }
    if let Some(empty) = empty {
        validate_extensions(&empty.extensions, "view.empty")?;
        if !valid_text(&empty.title, 512) {
            return Err(invalid("view.empty", "空状态标题无效"));
        }
    }
    Ok(())
}

/// 菜单动作只传递稳定身份，不接受可执行脚本。
fn validate_menu(items: &[MenuItem]) -> Result<(), ProtocolError> {
    for item in items {
        validate_extensions(&item.extensions, "view.menu")?;
        if !valid_text(&item.label, 512) || !valid_local_id(&item.action) {
            return Err(invalid("view.menu", "菜单项无效"));
        }
    }
    Ok(())
}
