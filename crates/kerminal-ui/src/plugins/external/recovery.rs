//! 原生标签的宿主恢复信封与绑定白名单；插件自有状态始终保持不透明。
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use kerminal_application::{AppError, AppResult, plugin_ports::PluginViewSavedState};
use kerminal_plugin_protocol::native::{NativeNode, NativeNodeKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::state::BindingPath;

const FORMAT: &str = "kerminal.native-content";
const MAX_RECOVERY_BYTES: usize = 1024 * 1024;
const MAX_DRAFTS: usize = 512;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeDraft {
    pub path: String,
    pub value: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginState {
    state_version: u32,
    state: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    schema: u32,
    view: String,
    plugin_state: Option<PluginState>,
    drafts: Vec<NativeDraft>,
}

/// 版本零由宿主独占（manifest 禁止插件声明零），旧插件 JSON 因此不会误判为信封。
pub(super) fn decode(
    saved: Option<PluginViewSavedState>,
    view: &str,
) -> AppResult<(Option<PluginViewSavedState>, Vec<NativeDraft>)> {
    let Some(saved) = saved else {
        return Ok((None, Vec::new()));
    };
    if saved.state_version != 0 {
        return Ok((Some(saved), Vec::new()));
    }
    let envelope = parse(saved.state, view)?;
    Ok((
        envelope.plugin_state.map(|state| PluginViewSavedState {
            state_version: state.state_version,
            state: state.state,
        }),
        envelope.drafts,
    ))
}

/// 工厂在挂载之前核对内层插件版本；未知信封或贡献版本交由内容恢复界面保留原文。
pub(crate) fn validate_native_content_state(
    value: &Value,
    view: &str,
    plugin_version: u32,
) -> AppResult<()> {
    let envelope = parse(value.clone(), view)?;
    if envelope
        .plugin_state
        .is_some_and(|state| state.state_version != plugin_version)
    {
        return Err(AppError::unsupported("插件标签状态版本不受支持"));
    }
    Ok(())
}

/// 校验完整后才交出任何字段，避免半个畸形信封导致恢复时静默覆盖原材料。
fn parse(value: Value, view: &str) -> AppResult<Envelope> {
    check_size(&value)?;
    let envelope: Envelope =
        serde_json::from_value(value).map_err(|_| AppError::invalid("插件原生恢复材料无效"))?;
    if envelope.format != FORMAT || envelope.schema != 1 {
        return Err(AppError::unsupported("插件原生恢复材料版本不受支持"));
    }
    if envelope.view != view
        || envelope.drafts.len() > MAX_DRAFTS
        || envelope
            .plugin_state
            .as_ref()
            .is_some_and(|state| state.state_version == 0)
    {
        return Err(AppError::invalid("插件原生恢复材料不匹配"));
    }
    let mut paths = BTreeSet::new();
    for draft in &envelope.drafts {
        if BindingPath::parse(&draft.path).is_err()
            || draft.path.starts_with('$')
            || draft.value.is_array()
            || draft.value.is_object()
            || draft.value.is_null()
            || !paths.insert(&draft.path)
        {
            return Err(AppError::invalid("插件原生草稿绑定无效"));
        }
    }
    Ok(envelope)
}

/// 没有插件快照也可独立保存有效草稿；没有任何恢复材料时继续返回 None。
pub(super) fn encode(
    view: &str,
    state: Option<PluginViewSavedState>,
    drafts: Vec<NativeDraft>,
) -> AppResult<Option<PluginViewSavedState>> {
    if state.is_none() && drafts.is_empty() {
        return Ok(None);
    }
    let value = serde_json::to_value(Envelope {
        format: FORMAT.into(),
        schema: 1,
        view: view.into(),
        plugin_state: state.map(|state| PluginState {
            state_version: state.state_version,
            state: state.state,
        }),
        drafts,
    })
    .map_err(|_| AppError::invalid("插件原生恢复材料无法编码"))?;
    parse(value.clone(), view)?;
    Ok(Some(PluginViewSavedState {
        state_version: 0,
        state: value,
    }))
}

/// 恢复数据和正常工作区状态使用有界材料，超限必须阻止覆盖最后一份有效状态。
fn check_size(value: &Value) -> AppResult<()> {
    let size = serde_json::to_vec(value)
        .map_err(|_| AppError::invalid("插件原生恢复材料无法编码"))?
        .len();
    if size > MAX_RECOVERY_BYTES {
        return Err(AppError::invalid("插件原生恢复材料超过大小上限"));
    }
    Ok(())
}

enum DraftKind {
    Text,
    Boolean,
    Number {
        min: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
    },
    Select(Vec<Value>),
}

#[derive(Default)]
pub(super) struct DraftPolicy {
    allowed: BTreeMap<String, DraftKind>,
    secret: BTreeSet<String>,
}

impl DraftPolicy {
    /// 恢复前展开与控件相同的表单资源，不能仅因表单尚未创建就丢弃已保存草稿。
    pub fn from_view(
        root: &NativeNode,
        prepared: &kerminal_application::plugin_ports::PreparedPlugin,
    ) -> Self {
        let mut policy = Self::default();
        policy.add_node(root);
        policy.add_forms(root, prepared);
        policy
    }

    /// 表单生成只读取冻结资源且不执行 I/O；不可表达的表单不提供可恢复绑定。
    fn add_forms(
        &mut self,
        node: &NativeNode,
        prepared: &kerminal_application::plugin_ports::PreparedPlugin,
    ) {
        use NativeNodeKind::*;
        match &node.kind {
            Stack { children, .. }
            | Row { children, .. }
            | Section { children, .. }
            | Split { children, .. }
            | Toolbar { children, .. } => {
                for child in children {
                    self.add_forms(child, prepared);
                }
            }
            Scroll { child, .. } => self.add_forms(child, prepared),
            Tabs { tabs, .. } => {
                for tab in tabs {
                    self.add_forms(&tab.content, prepared);
                }
            }
            Form { schema, bind, .. } => {
                if let Ok(form) = super::super::forms::NativeForm::from_resource(
                    prepared,
                    schema,
                    bind.as_deref(),
                ) {
                    self.add_node(&form.root);
                }
            }
            _ => {}
        }
    }

    /// 白名单来自当前冻结视图及展开表单，未知、只读或秘密路径不能借旧状态注入输入。
    pub fn add_node(&mut self, node: &NativeNode) {
        use NativeNodeKind::*;
        let field = match &node.kind {
            Stack { children, .. }
            | Row { children, .. }
            | Section { children, .. }
            | Split { children, .. }
            | Toolbar { children, .. } => {
                for child in children {
                    self.add_node(child);
                }
                None
            }
            Scroll { child, .. } => {
                self.add_node(child);
                None
            }
            Tabs { tabs, .. } => {
                for tab in tabs {
                    self.add_node(&tab.content);
                }
                None
            }
            Input { bind, .. } | Textarea { bind, .. } | Path { bind, .. } => {
                Some((bind, DraftKind::Text))
            }
            Switch { bind, .. } | Checkbox { bind, .. } => Some((bind, DraftKind::Boolean)),
            Number {
                bind,
                min,
                max,
                step,
                ..
            } => Some((
                bind,
                DraftKind::Number {
                    min: *min,
                    max: *max,
                    step: *step,
                },
            )),
            Select { bind, options, .. } => Some((
                bind,
                DraftKind::Select(options.iter().map(|option| option.value.clone()).collect()),
            )),
            Secret { bind, .. } => {
                self.secret.insert(normalize(bind));
                None
            }
            _ => None,
        };
        if let Some((bind, kind)) = field {
            self.allowed.insert(normalize(bind), kind);
        }
    }

    /// 只恢复控件能表达的合法标量；秘密路径及父子重叠一律排除，避免别名绕过隔离。
    pub fn permits(&self, path: &str, value: &Value) -> bool {
        if self.secret.iter().any(|secret| overlaps(path, secret)) {
            return false;
        }
        match self.allowed.get(path) {
            Some(DraftKind::Text) => value.is_string(),
            Some(DraftKind::Boolean) => value.is_boolean(),
            Some(DraftKind::Select(options)) => {
                !value.is_array()
                    && !value.is_object()
                    && !value.is_null()
                    && options.contains(value)
            }
            Some(DraftKind::Number { min, max, step }) => value.as_f64().is_some_and(|value| {
                value.is_finite()
                    && min.is_none_or(|min| value >= min)
                    && max.is_none_or(|max| value <= max)
                    && step.is_none_or(|step| {
                        let units = (value - min.unwrap_or(0.0)) / step;
                        step > 0.0 && (units.round() - units).abs() < 1e-9
                    })
            }),
            None => false,
        }
    }
}

/// 可选 `$` 不属于字段身份，与 ViewState 的对象键路径保持一致。
fn normalize(path: &str) -> String {
    path.strip_prefix('$').unwrap_or(path).to_owned()
}

/// 点分边界阻止 secret 与 secret2 混淆，同时拒绝导出秘密对象的父字段。
fn overlaps(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|rest| rest.starts_with('.'))
        || right
            .strip_prefix(left)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
#[path = "../../../tests/unit/external_native_recovery.rs"]
mod tests;
