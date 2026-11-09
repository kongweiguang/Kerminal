//! 外部主机 schema 表单；秘密输入在保存时转换为宿主 Vault 候选。
//! @author kongweiguang

use super::{
    external_settings::{FieldKind, FieldSpec, collect_fields, set_field},
    host_forms::{HostFormFactory, HostFormInstance, HostFormPrepare, HostFormSeed, HostFormTest},
};
use gpui_kit::{
    AnyView, App, AppContext as _, Context, Entity, IntoElement, Render, SharedString, Window,
    component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputState},
    },
    div,
    prelude::*,
};
use kerminal_application::{
    AppError, AppResult,
    connections::host_types::{
        ExternalHostSecretService, PluginHost, PluginHostDraft, host_configuration_from_json,
    },
    external_contributions::PreparedPlugin,
    plugin_runtime::PluginRuntimeService,
    plugin_settings::{PluginSecretEdit, PluginSecretRepository},
};
use kerminal_domain::identity::HostId;
use kerminal_plugin_protocol::ConfigurationSchema;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

/// 自动表单不能编辑动态字段或对象秘密，提前拒绝以免静默丢失配置或展示秘密。
fn validate_host_form_schema(schema: &Value) -> AppResult<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| AppError::unsupported("主机 schema 包含不支持的结构"))?;
    if [
        "propertyNames",
        "unevaluatedProperties",
        "$dynamicAnchor",
        "$recursiveAnchor",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
        || object
            .get("additionalProperties")
            .is_some_and(|value| value == &Value::Bool(true))
        || object.get("x-secret") == Some(&Value::Bool(true))
            && object.get("type").and_then(Value::as_str) != Some("string")
    {
        return Err(AppError::unsupported(
            "主机 schema 的动态字段或秘密类型暂不支持自动表单",
        ));
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for child in properties.values() {
            validate_host_form_schema(child)?;
        }
    }
    Ok(())
}

/// 工厂只持有冻结声明与服务，窗口实例独立保存控件和草稿。
pub struct ExternalHostFormFactory {
    plugin: Arc<PreparedPlugin>,
    type_id: String,
    local_type: String,
    runtime: Arc<PluginRuntimeService>,
    title: String,
    schema: ConfigurationSchema,
    secrets: ExternalHostSecretService,
}

impl ExternalHostFormFactory {
    /// 拒绝缺失 schema；表单与配置目录使用同一个启动快照。
    pub fn new(
        plugin: Arc<PreparedPlugin>,
        local_type: &str,
        vault: Arc<dyn PluginSecretRepository>,
        runtime: Arc<PluginRuntimeService>,
        locale: &str,
    ) -> AppResult<Self> {
        let host = plugin
            .package
            .manifest
            .contributes
            .host_types
            .iter()
            .find(|host| host.id == local_type)
            .ok_or_else(|| AppError::invalid("插件主机类型未声明"))?;
        let schema = plugin
            .package
            .configuration_schemas
            .get(&host.schema)
            .cloned()
            .ok_or_else(|| AppError::invalid("插件主机 schema 缺失"))?;
        validate_host_form_schema(&schema.document)?;
        let mut fields = Vec::new();
        collect_fields(&schema.document, &mut Vec::new(), &mut fields)?;
        let title = host
            .title
            .strip_prefix('%')
            .and_then(|text| text.strip_suffix('%'))
            .and_then(|key| {
                plugin
                    .localizations
                    .get(locale)
                    .and_then(|items| items.get(key))
                    .or_else(|| {
                        plugin
                            .localizations
                            .get("en")
                            .and_then(|items| items.get(key))
                    })
            })
            .cloned()
            .unwrap_or_else(|| host.title.clone());
        let type_id = format!("{}/{}", plugin.id.as_str(), local_type);
        let secrets = ExternalHostSecretService::new(plugin.id.clone(), schema.clone(), vault);
        Ok(Self {
            plugin,
            type_id,
            local_type: local_type.to_owned(),
            runtime,
            title,
            schema,
            secrets,
        })
    }
}

impl HostFormFactory for ExternalHostFormFactory {
    /// 返回冻结的插件身份，避免表单接管其他插件的主机。
    fn provider_id(&self) -> &str {
        self.plugin.id.as_str()
    }
    /// 返回带命名空间的稳定类型身份。
    fn type_id(&self) -> &str {
        &self.type_id
    }
    /// 使用清单本地化名称，避免协议内部标识暴露为标签。
    fn display_name(&self) -> SharedString {
        self.title.clone().into()
    }
    /// 控件持有各窗口自己的输入状态，秘密字段不预填已保存内容。
    fn create(
        &self,
        seed: &HostFormSeed,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn HostFormInstance>> {
        let mut fields = Vec::new();
        collect_fields(&self.schema.document, &mut Vec::new(), &mut fields)?;
        let mut values = seed
            .original
            .as_ref()
            .and_then(|host| host.configuration.as_ref())
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| AppError::invalid("插件主机配置无效"))?
            .unwrap_or_else(|| json!({}));
        for field in &fields {
            if values.pointer(&field.pointer).is_none() {
                let default = if matches!(field.kind, FieldKind::Secret) {
                    Value::String(String::new())
                } else {
                    field.default.clone().unwrap_or_else(|| match field.kind {
                        FieldKind::Boolean => Value::Bool(false),
                        FieldKind::Number { .. } => json!(0),
                        _ => Value::String(String::new()),
                    })
                };
                set_field(&mut values, &field.segments, default)?;
            }
        }
        let original = seed.original.clone();
        let initial_name = original
            .as_ref()
            .map_or_else(String::new, |host| host.name.clone());
        let mut initial = BTreeMap::new();
        let view = cx.new(|cx| {
            let name = cx.new(|cx| InputState::new(window, cx).default_value(initial_name.clone()));
            let mut controls = BTreeMap::new();
            for field in &fields {
                let value = if matches!(field.kind, FieldKind::Secret) {
                    String::new()
                } else {
                    match values.pointer(&field.pointer) {
                        Some(Value::String(text)) => text.clone(),
                        Some(value) => value.to_string(),
                        None => String::new(),
                    }
                };
                initial.insert(field.pointer.clone(), value.clone());
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
                if matches!(field.kind, FieldKind::Secret) {
                    input.update(cx, |input, cx| input.set_masked(true, window, cx));
                }
                controls.insert(field.pointer.clone(), input);
            }
            HostFormView {
                name,
                controls,
                fields: fields.clone(),
                saved_secrets: fields
                    .iter()
                    .filter(|field| {
                        matches!(field.kind, FieldKind::Secret)
                            && original
                                .as_ref()
                                .and_then(|host| host.configuration.as_ref())
                                .and_then(|value| serde_json::to_value(value).ok())
                                .is_some_and(|value| value.pointer(&field.pointer).is_some())
                    })
                    .map(|field| field.pointer.clone())
                    .collect(),
                removed: BTreeSet::new(),
                enabled: true,
            }
        });
        Ok(Box::new(ExternalHostForm {
            view,
            plugin: self.plugin.clone(),
            type_id: self.type_id.clone(),
            local_type: self.local_type.clone(),
            runtime: self.runtime.clone(),
            original,
            default_group: seed.default_group.clone(),
            values,
            initial_name,
            initial,
            secrets: self.secrets.clone(),
        }))
    }
}

struct HostFormView {
    name: Entity<InputState>,
    controls: BTreeMap<String, Entity<InputState>>,
    fields: Vec<FieldSpec>,
    saved_secrets: BTreeSet<String>,
    removed: BTreeSet<String>,
    enabled: bool,
}
impl Render for HostFormView {
    /// 同一实体持续持有输入，异步保存期间禁用而不替换控件。
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(div().child("名称"))
            .child(Input::new(&self.name).disabled(!self.enabled).w_full());
        for field in &self.fields {
            if let Some(input) = self.controls.get(&field.pointer) {
                body = body
                    .child(div().child(field.label.clone()))
                    .child(Input::new(input).disabled(!self.enabled).w_full());
                if self.saved_secrets.contains(&field.pointer) {
                    let pointer = field.pointer.clone();
                    let removed = self.removed.contains(&pointer);
                    body = body.child(
                        Button::new(format!("host-secret-remove-{pointer}"))
                            .ghost()
                            .disabled(!self.enabled)
                            .label(if removed {
                                crate::locale::text("撤销删除", "Undo removal")
                            } else {
                                crate::locale::text("删除已保存的秘密", "Remove saved secret")
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if !view.removed.remove(&pointer) {
                                    view.removed.insert(pointer.clone());
                                }
                                cx.notify();
                            })),
                    );
                }
            }
        }
        body
    }
}

struct ExternalHostForm {
    view: Entity<HostFormView>,
    plugin: Arc<PreparedPlugin>,
    type_id: String,
    local_type: String,
    runtime: Arc<PluginRuntimeService>,
    original: Option<PluginHost>,
    default_group: Option<String>,
    values: Value,
    initial_name: String,
    initial: BTreeMap<String, String>,
    secrets: ExternalHostSecretService,
}
impl HostFormInstance for ExternalHostForm {
    /// 宿主复用同一视图，不因重绘丢失正在编辑的字段。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }
    /// 非秘密字段转成类型化配置；秘密草稿仅携带旧引用或空占位。
    fn snapshot(&self, cx: &App) -> AppResult<PluginHostDraft> {
        let view = self.view.read(cx);
        let mut values = self.values.clone();
        for field in &view.fields {
            if matches!(field.kind, FieldKind::Secret) {
                continue;
            }
            let text = view
                .controls
                .get(&field.pointer)
                .ok_or_else(|| AppError::invalid("主机表单字段缺失"))?
                .read(cx)
                .value()
                .to_string();
            let value = match &field.kind {
                FieldKind::Number { .. } | FieldKind::Boolean => serde_json::from_str(&text)
                    .map_err(|_| AppError::invalid("主机字段类型无效"))?,
                FieldKind::Choice(options) => options
                    .iter()
                    .find(|option| {
                        option.as_str() == Some(text.as_str()) || option.to_string() == text
                    })
                    .cloned()
                    .ok_or_else(|| AppError::invalid("主机选项无效"))?,
                _ => Value::String(text),
            };
            set_field(&mut values, &field.segments, value)?;
        }
        let name = view.name.read(cx).value().to_string();
        if name.trim().is_empty() {
            return Err(AppError::invalid("主机名称不能为空"));
        }
        Ok(PluginHostDraft {
            id: self.original.as_ref().map(|host| host.id.clone()),
            provider_id: self.plugin.id.as_str().to_owned(),
            type_id: self.type_id.clone(),
            name,
            group_id: self
                .original
                .as_ref()
                .map_or_else(|| self.default_group.clone(), |host| host.group_id.clone()),
            sort_order: self.original.as_ref().map_or(0, |host| host.sort_order),
            configuration_version: 1,
            configuration: host_configuration_from_json(values)?,
        })
    }
    /// 只捕获本次输入的秘密；执行器负责解密旧值、校验并建立不可变候选。
    fn begin_prepare(&mut self, cx: &App) -> AppResult<HostFormPrepare> {
        let view = self.view.read(cx);
        let mut edits = BTreeMap::new();
        let mut secret_paths = Vec::new();
        for field in &view.fields {
            if matches!(field.kind, FieldKind::Secret) {
                secret_paths.push(field.pointer.clone());
                let value = Zeroizing::new(
                    view.controls
                        .get(&field.pointer)
                        .ok_or_else(|| AppError::invalid("主机秘密字段缺失"))?
                        .read(cx)
                        .value()
                        .to_string(),
                );
                if view.removed.contains(&field.pointer) {
                    edits.insert(field.pointer.clone(), PluginSecretEdit::Remove);
                } else if !value.is_empty() {
                    edits.insert(field.pointer.clone(), PluginSecretEdit::Replace(value));
                }
            }
        }
        let secrets = self.secrets.clone();
        let previous = self
            .original
            .as_ref()
            .and_then(|host| host.configuration.clone());
        Ok(Box::new(move |mut draft, cancel| {
            Box::pin(async move {
                let host = draft
                    .id
                    .as_ref()
                    .ok_or_else(|| AppError::invalid("插件主机身份缺失"))?;
                let mut values = serde_json::to_value(&draft.configuration)
                    .map_err(|_| AppError::invalid("插件主机配置无效"))?;
                for path in &secret_paths {
                    if let Some(target) = values.pointer_mut(path) {
                        *target = Value::Null;
                    }
                }
                draft.configuration = secrets
                    .prepare(host, values, previous.as_ref(), edits, cancel)
                    .await?;
                Ok(draft)
            })
        }))
    }
    /// 只在运行时可调用的插件表单提供测试入口。
    fn supports_test(&self) -> bool {
        true
    }

    /// 使用同一草稿和 Vault 绑定执行真实探测，测试不创建秘密候选或保存主机。
    fn begin_test(&mut self, cx: &mut App) -> AppResult<HostFormTest> {
        let draft = self.snapshot(cx)?;
        let view = self.view.read(cx);
        let mut edits = BTreeMap::new();
        let mut values = serde_json::to_value(&draft.configuration)
            .map_err(|_| AppError::invalid("插件主机配置无效"))?;
        for field in &view.fields {
            if matches!(field.kind, FieldKind::Secret) {
                if let Some(target) = values.pointer_mut(&field.pointer) {
                    *target = Value::Null;
                }
                let secret = Zeroizing::new(
                    view.controls
                        .get(&field.pointer)
                        .ok_or_else(|| AppError::invalid("主机秘密字段缺失"))?
                        .read(cx)
                        .value()
                        .to_string(),
                );
                if view.removed.contains(&field.pointer) {
                    edits.insert(field.pointer.clone(), PluginSecretEdit::Remove);
                } else if !secret.is_empty() {
                    edits.insert(field.pointer.clone(), PluginSecretEdit::Replace(secret));
                }
            }
        }
        let previous = self
            .original
            .as_ref()
            .and_then(|host| host.configuration.clone());
        let host = draft.id.unwrap_or_else(HostId::new);
        let service = self.secrets.clone();
        let runtime = self.runtime.clone();
        let plugin = self.plugin.id.clone();
        let local_type = self.local_type.clone();
        Ok(Box::new(move |cancel| {
            Box::pin(async move {
                let (config, secrets) = service
                    .preview(&host, values, previous.as_ref(), edits, cancel.clone())
                    .await?;
                let params = kerminal_plugin_protocol::HostOperationParams {
                    host_type: local_type,
                    config,
                    secrets: Value::Object(secrets),
                };
                let params = serde_json::to_value(params)
                    .map_err(|_| AppError::invalid("插件主机测试参数无效"))?;
                let response = runtime
                    .call_plugin(
                        plugin,
                        "host.test".into(),
                        params,
                        Instant::now() + Duration::from_secs(60),
                        cancel,
                    )
                    .await?;
                let result: kerminal_plugin_protocol::HostTestResult =
                    serde_json::from_value(response)
                        .map_err(|_| AppError::invalid("插件主机测试结果无效"))?;
                if result.ok {
                    Ok(())
                } else {
                    Err(AppError::infrastructure("插件主机测试失败"))
                }
            })
        }))
    }
    /// 包含未提交秘密输入，切换或关闭时不丢弃草稿。
    fn is_dirty(&self, cx: &App) -> bool {
        let view = self.view.read(cx);
        view.name.read(cx).value().as_ref() != self.initial_name
            || !view.removed.is_empty()
            || view.controls.iter().any(|(path, input)| {
                input.read(cx).value().as_ref() != self.initial.get(path).map_or("", String::as_str)
            })
    }
    /// 保存期间禁用全部控件，防止异步回执覆盖新编辑。
    fn set_enabled(&mut self, enabled: bool, _: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, cx| {
            view.enabled = enabled;
            cx.notify();
        });
    }
}

#[cfg(test)]
#[path = "../../tests/unit/external_host_form.rs"]
mod tests;
