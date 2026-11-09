//! 普通原生表单把安全支持的 schema 投影成已有控件，并在动作前校验数据。
//!
//! @author kongweiguang

use kerminal_application::{AppError, AppResult, external_contributions::PreparedPlugin};
use kerminal_plugin_protocol::{
    ConfigurationSchema,
    native::{NativeNode, NativeNodeKind},
};
use serde_json::{Value, json};

use super::{renderer::NativeRenderer, state::BindingPath};
use crate::plugins::external_settings::{FieldKind, collect_fields};

/// schema 与绑定随视图冻结；生成的节点复用原有草稿和控件生命周期。
pub(super) struct NativeForm {
    pub root: NativeNode,
    schema: ConfigurationSchema,
    bind: Option<BindingPath>,
}

impl NativeForm {
    /// 只生成可无损绑定的字段；秘密和复杂 schema 必须由专用表单处理。
    pub fn from_resource(
        prepared: &PreparedPlugin,
        resource: &str,
        bind: Option<&str>,
    ) -> AppResult<Self> {
        let bytes = prepared
            .resources
            .get(resource)
            .ok_or_else(|| AppError::not_found("插件表单 schema 资源"))?;
        let schema = ConfigurationSchema::parse(bytes)
            .map_err(|_| AppError::invalid("插件表单 schema 无效"))?;
        Self::from_schema(schema, bind)
    }

    /// 先验证每个字段的可表达性，再一次构造控件树，避免留下半个表单。
    fn from_schema(schema: ConfigurationSchema, bind: Option<&str>) -> AppResult<Self> {
        let binding = bind
            .map(BindingPath::parse)
            .transpose()
            .map_err(|_| AppError::invalid("插件表单绑定无效"))?;
        let mut fields = Vec::new();
        collect_fields(&schema.document, &mut Vec::new(), &mut fields)?;
        let mut children = Vec::new();
        for field in fields {
            let path = binding
                .as_ref()
                .map(|binding| {
                    binding
                        .segments()
                        .iter()
                        .cloned()
                        .chain(field.segments.iter().cloned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| field.segments.clone())
                .join(".");
            let parsed = BindingPath::parse(&path)
                .map_err(|_| AppError::unsupported("表单字段名无法表示为绑定路径"))?;
            if parsed.segments().len()
                != binding
                    .as_ref()
                    .map_or(0, |binding| binding.segments().len())
                    + field.segments.len()
            {
                return Err(AppError::unsupported("表单字段名无法表示为绑定路径"));
            }
            let node = match field.kind {
                FieldKind::Text => json!({"type":"input", "bind":path, "label":field.label}),
                FieldKind::Number { .. } => {
                    json!({"type":"number", "bind":path, "label":field.label})
                }
                FieldKind::Boolean => json!({"type":"switch", "bind":path, "label":field.label}),
                FieldKind::Choice(values) => {
                    let options = values
                        .into_iter()
                        .map(|value| {
                            let label = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            json!({"label":label, "value":value})
                        })
                        .collect::<Vec<_>>();
                    json!({"type":"select", "bind":path, "label":field.label, "options":options})
                }
                FieldKind::Secret => {
                    return Err(AppError::unsupported(
                        "含秘密字段的普通表单需要专用设置表单",
                    ));
                }
            };
            children.push(node);
            if let Some(description) = field.description {
                children.push(json!({"type":"text", "text":description, "tone":"muted"}));
            }
        }
        let root = serde_json::from_value(json!({"type":"stack", "children":children}))
            .map_err(|_| AppError::invalid("插件表单无法构造控件"))?;
        Ok(Self {
            root,
            schema,
            bind: binding,
        })
    }

    /// 拍摄发起动作时的值；后台校验不得借用之后变化的窗口草稿。
    fn validation_snapshot(&self, data: &Value) -> (ConfigurationSchema, Value) {
        let value = self
            .bind
            .as_ref()
            .map(|bind| {
                bind.segments()
                    .iter()
                    .try_fold(data, |value, key| value.get(key))
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            })
            .unwrap_or_else(|| data.clone());
        (self.schema.clone(), value)
    }
}

impl NativeRenderer {
    /// 只校验当前可见分支，隐藏表单和未选择页签不应阻止其他操作。
    pub(super) fn visible_form_snapshots(&self) -> AppResult<Vec<(ConfigurationSchema, Value)>> {
        let mut snapshots = Vec::new();
        self.collect_visible_forms(
            &self.projection.view().root,
            "root".to_owned(),
            &mut snapshots,
        )?;
        Ok(snapshots)
    }

    /// 绑定路径与控件树使用同一结构索引，避免在不同页签间混淆同名表单。
    fn collect_visible_forms(
        &self,
        node: &NativeNode,
        path: String,
        out: &mut Vec<(ConfigurationSchema, Value)>,
    ) -> AppResult<()> {
        if !self.projection.is_visible(node) {
            return Ok(());
        }
        use NativeNodeKind::*;
        match &node.kind {
            Stack { children, .. }
            | Row { children, .. }
            | Section { children, .. }
            | Split { children, .. }
            | Toolbar { children, .. } => {
                for (index, child) in children.iter().enumerate() {
                    self.collect_visible_forms(child, format!("{path}.{index}"), out)?;
                }
            }
            Scroll { child, .. } => self.collect_visible_forms(child, format!("{path}.0"), out)?,
            Tabs { tabs, .. } => {
                let selected = self
                    .active_tabs
                    .get(&path)
                    .map(String::as_str)
                    .or_else(|| tabs.first().map(|tab| tab.id.as_str()));
                if let Some((index, tab)) = tabs
                    .iter()
                    .enumerate()
                    .find(|(_, tab)| Some(tab.id.as_str()) == selected)
                {
                    self.collect_visible_forms(&tab.content, format!("{path}.{index}"), out)?;
                }
            }
            Form { schema, bind, .. } => {
                if self
                    .embedded_form
                    .as_ref()
                    .is_some_and(|(expected, _)| schema == expected && bind.is_none())
                {
                    return Ok(());
                }
                let form = self
                    .forms
                    .get(&path)
                    .and_then(|result| result.as_ref().ok())
                    .ok_or_else(|| AppError::unsupported("当前视图的表单无法校验"))?;
                out.push(form.validation_snapshot(self.projection.state().data()));
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/external_native_forms.rs"]
mod tests;
