//! 插件设置表单：使用声明的 schema 和应用层 CAS，不在窗口线程访问配置文件。
//!
//! @author kongweiguang

use crate::{
    PluginUiRegistration,
    plugins::external::{
        factories::ExternalViewServices,
        renderer::{NativeRenderer, NativeRendererConfig},
    },
};
use gpui_kit::{
    AnyView, App, AppContext as _, Context, Entity, IntoElement, Render, SharedString,
    Subscription, Window,
    component::{
        ActiveTheme as _, Disableable as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        searchable_list::SearchableListItem,
        select::{Select, SelectEvent, SelectState},
        switch::Switch,
    },
    div,
    prelude::*,
};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    executor::ApplicationTaskExecutor,
    plugin_ports::{PluginViewIdentity, PreparedPlugin},
    plugin_settings::{
        PluginSecretEdit, PluginSettingsSave, PluginSettingsService, PluginSettingsSnapshot,
    },
};
use kerminal_domain::plugins::PluginId;
use kerminal_plugin_protocol::{ConfigurationSchema, SettingsContribution, ValidatedPackage};
use serde_json::{Map, Number, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Clone)]
pub(super) enum FieldKind {
    Text,
    Number { integer: bool },
    Boolean,
    Choice(Vec<Value>),
    Secret,
}
#[derive(Clone)]
pub(super) struct FieldSpec {
    pub(super) segments: Vec<String>,
    pub(super) pointer: String,
    pub(super) label: String,
    pub(super) description: Option<String>,
    pub(super) default: Option<Value>,
    pub(super) kind: FieldKind,
}
#[derive(Clone)]
struct Choice {
    index: String,
    label: SharedString,
}
impl SearchableListItem for Choice {
    type Value = String;
    /// 选项值与文案分离，切换语言不会改变保存的 JSON。
    fn value(&self) -> &String {
        &self.index
    }
    /// 未提供单独标签时展示协议枚举值。
    fn title(&self) -> SharedString {
        self.label.clone()
    }
}
enum Control {
    Text(Entity<InputState>),
    Choice(Entity<SelectState<Vec<Choice>>>),
}

/// 设置 schema 在包校验时已冻结，原生插件不要求将它重复放入网页资源快照。
fn settings_schema(package: &ValidatedPackage, path: &str) -> AppResult<ConfigurationSchema> {
    package
        .configuration_schemas
        .get(path)
        .cloned()
        .ok_or_else(|| AppError::not_found("插件设置 schema 资源"))
}

/// 删除标记代表最后一次按钮操作；后续输入事件会撤销标记并恢复替换意图。
fn secret_edit(secret: Zeroizing<String>, removed: bool) -> Option<PluginSecretEdit> {
    if removed {
        Some(PluginSecretEdit::Remove)
    } else if secret.is_empty() {
        None
    } else {
        Some(PluginSecretEdit::Replace(secret))
    }
}

/// 先保留有符号范围，再接受 JSON 数字支持的较大无符号整数，避免浮点精度损失。
fn parse_integer(text: &str) -> Option<Value> {
    text.parse::<i64>()
        .map(Number::from)
        .or_else(|_| text.parse::<u64>().map(Number::from))
        .ok()
        .map(Value::Number)
}

/// 收集表单可以准确编辑的结构；复杂组合显式报错而不是悄悄丢弃字段。
pub(super) fn collect_fields(
    schema: &Value,
    segments: &mut Vec<String>,
    fields: &mut Vec<FieldSpec>,
) -> AppResult<()> {
    let Some(object) = schema.as_object() else {
        return Err(AppError::unsupported("设置 schema 包含不支持的结构"));
    };
    if [
        "$ref",
        "$dynamicRef",
        "$recursiveRef",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
        "prefixItems",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
    {
        return Err(AppError::unsupported(
            "设置 schema 的组合或引用暂不支持自动表单",
        ));
    }
    if object
        .get("additionalProperties")
        .is_some_and(Value::is_object)
    {
        return Err(AppError::unsupported(
            "设置 schema 的动态字段暂不支持自动表单",
        ));
    }
    if object.get("type").and_then(Value::as_str) == Some("object")
        || (segments.is_empty() && object.contains_key("properties"))
    {
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| AppError::unsupported("设置对象缺少属性声明"))?;
        for (name, child) in properties {
            segments.push(name.clone());
            collect_fields(child, segments, fields)?;
            segments.pop();
        }
        return Ok(());
    }
    if segments.is_empty() {
        return Err(AppError::unsupported("插件设置必须声明对象字段"));
    }
    let secret = object
        .get("x-secret")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let kind = if secret {
        if object.get("type").and_then(Value::as_str) != Some("string") {
            return Err(AppError::unsupported("秘密设置必须为字符串"));
        }
        FieldKind::Secret
    } else if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if values.is_empty()
            || values
                .iter()
                .any(|value| value.is_object() || value.is_array())
        {
            return Err(AppError::unsupported("设置枚举类型暂不支持"));
        }
        FieldKind::Choice(values.clone())
    } else {
        match object.get("type").and_then(Value::as_str) {
            Some("string") => FieldKind::Text,
            Some("number") => FieldKind::Number { integer: false },
            Some("integer") => FieldKind::Number { integer: true },
            Some("boolean") => FieldKind::Boolean,
            _ => {
                return Err(AppError::unsupported(
                    "设置 schema 字段类型暂不支持自动表单",
                ));
            }
        }
    };
    let pointer = segments
        .iter()
        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
        .collect();
    let label = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| segments.last().cloned().unwrap_or_default());
    fields.push(FieldSpec {
        segments: segments.clone(),
        pointer,
        label,
        description: object
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        default: object.get("default").cloned(),
        kind,
    });
    Ok(())
}

/// 只为缺失的对象键创建父对象，遇到非对象拒绝覆盖已有设置。
pub(super) fn set_field(value: &mut Value, segments: &[String], new_value: Value) -> AppResult<()> {
    let Some((last, parents)) = segments.split_last() else {
        return Err(AppError::invalid("设置字段路径为空"));
    };
    let mut current = value;
    for part in parents {
        let object = current
            .as_object_mut()
            .ok_or_else(|| AppError::invalid("设置字段父级不是对象"))?;
        current = object
            .entry(part.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| AppError::invalid("设置字段父级不是对象"))?
        .insert(last.clone(), new_value);
    Ok(())
}

/// 翻译只解析完整占位符，不把设置字段值当作翻译键。
fn localized(prepared: &PreparedPlugin, text: &str) -> String {
    let Some(key) = text
        .strip_prefix('%')
        .and_then(|text| text.strip_suffix('%'))
    else {
        return text.to_owned();
    };
    let locale = gpui_kit::component::locale();
    prepared
        .localizations
        .get(&*locale)
        .and_then(|items| items.get(key))
        .or_else(|| {
            prepared
                .localizations
                .get("en")
                .and_then(|items| items.get(key))
        })
        .cloned()
        .unwrap_or_else(|| text.to_owned())
}

/// 冲突和未知提交都必须读回，但前者不能被错误报告成已发生未知提交。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsReadbackReason {
    Conflict,
    OutcomeUnknown,
}

/// 实际表单实体可嵌在插件自定义视图，也可作为独立默认页面。
struct SettingsForm {
    prepared: Arc<PreparedPlugin>,
    schema: ConfigurationSchema,
    fields: Vec<FieldSpec>,
    service: PluginSettingsService,
    executor: ApplicationTaskExecutor,
    controls: BTreeMap<String, Control>,
    subscriptions: Vec<Subscription>,
    snapshot: Option<PluginSettingsSnapshot>,
    draft: Value,
    errors: BTreeMap<String, String>,
    removed: BTreeSet<String>,
    cancellation: CancellationToken,
    busy: bool,
    dirty: bool,
    require_readback: Option<SettingsReadbackReason>,
    locale: String,
    error: Option<String>,
}

impl SettingsForm {
    /// 加载前不展示伪造默认快照；完成后才创建控件并允许用户保存。
    fn new(
        prepared: Arc<PreparedPlugin>,
        schema: ConfigurationSchema,
        fields: Vec<FieldSpec>,
        service: PluginSettingsService,
        executor: ApplicationTaskExecutor,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut form = Self {
            prepared,
            schema,
            fields,
            service,
            executor,
            controls: BTreeMap::new(),
            subscriptions: Vec::new(),
            snapshot: None,
            draft: json!({}),
            errors: BTreeMap::new(),
            removed: BTreeSet::new(),
            cancellation: CancellationToken::new(),
            busy: false,
            dirty: false,
            require_readback: None,
            locale: gpui_kit::component::locale().to_string(),
            error: None,
        };
        form.load(cx);
        form
    }

    /// 读取使用共享执行器；未知提交和冲突之后必须由用户显式要求读回。
    fn load(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let plugin = self.prepared.id.clone();
        let schema = self.schema.clone();
        let service = self.service.clone();
        let executor = self.executor.clone();
        let cancellation = self.cancellation.child_token();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    service.load(&plugin, &schema, token).await
                })
                .await;
            let _ = this.update_in(cx, |form, window, cx| {
                form.busy = false;
                match result {
                    Ok(snapshot) => form.apply_snapshot(snapshot, window, cx),
                    Err(error) => {
                        form.error = Some(error.public_message().to_owned());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 读取确认后才替换草稿与控件；schema 默认值属于展示基线，不能被误判为用户编辑。
    /// 秘密回显为空，已保存状态只显示标记；真正输入仍由编辑事件单独建立脏态。
    fn apply_snapshot(
        &mut self,
        snapshot: PluginSettingsSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut draft = snapshot.values.clone();
        for field in &self.fields {
            if !matches!(field.kind, FieldKind::Secret)
                && draft.pointer(&field.pointer).is_none()
                && let Some(default) = &field.default
                && let Err(error) = set_field(&mut draft, &field.segments, default.clone())
            {
                self.error = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        }
        self.draft = draft;
        self.snapshot = Some(snapshot);
        self.controls.clear();
        self.subscriptions.clear();
        self.errors.clear();
        self.removed.clear();
        self.require_readback = None;
        self.dirty = false;
        self.error = None;
        for field in self.fields.clone() {
            match &field.kind {
                FieldKind::Boolean => {}
                FieldKind::Choice(values) => {
                    let choices = values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| Choice {
                            index: index.to_string(),
                            label: match value {
                                Value::String(text) => localized(&self.prepared, text),
                                _ => value.to_string(),
                            }
                            .into(),
                        })
                        .collect::<Vec<_>>();
                    let state = cx.new(|cx| SelectState::new(choices, None, window, cx));
                    if let Some(index) = values
                        .iter()
                        .position(|value| self.draft.pointer(&field.pointer) == Some(value))
                    {
                        state.update(cx, |state, cx| {
                            state.set_selected_value(&index.to_string(), window, cx)
                        });
                    }
                    let values = values.clone();
                    let spec = field.clone();
                    self.subscriptions.push(cx.subscribe(
                        &state,
                        move |form, _, event: &SelectEvent<Vec<Choice>>, cx| {
                            if let SelectEvent::Confirm(Some(index)) = event
                                && let Some(value) = index
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|index| values.get(index))
                            {
                                form.edit(&spec, value.clone(), cx);
                            }
                        },
                    ));
                    self.controls
                        .insert(field.pointer.clone(), Control::Choice(state));
                }
                _ => {
                    let secret = matches!(field.kind, FieldKind::Secret);
                    let value = if secret {
                        String::new()
                    } else {
                        self.draft
                            .pointer(&field.pointer)
                            .map(super_value)
                            .unwrap_or_default()
                    };
                    let state = cx.new(|cx| InputState::new(window, cx));
                    state.update(cx, |input, cx| {
                        input.set_value(value, window, cx);
                        if secret {
                            input.set_masked(true, window, cx);
                        }
                    });
                    let spec = field.clone();
                    self.subscriptions.push(cx.subscribe(
                        &state,
                        move |form, input, event: &InputEvent, cx| {
                            if !matches!(event, InputEvent::Change) {
                                return;
                            }
                            if secret {
                                form.dirty = true;
                                form.removed.remove(&spec.pointer);
                                cx.notify();
                                return;
                            }
                            let text = input.read(cx).value().to_string();
                            let value = match spec.kind {
                                FieldKind::Number { integer: true } => parse_integer(&text),
                                FieldKind::Number { integer: false } => text
                                    .parse::<f64>()
                                    .ok()
                                    .and_then(Number::from_f64)
                                    .map(Value::Number),
                                _ => Some(Value::String(text)),
                            };
                            if let Some(value) = value {
                                form.errors.remove(&spec.pointer);
                                form.edit(&spec, value, cx);
                            } else {
                                form.errors.insert(
                                    spec.pointer.clone(),
                                    crate::locale::text("请输入有效数字", "Enter a valid number")
                                        .to_string(),
                                );
                                cx.notify();
                            }
                        },
                    ));
                    self.controls
                        .insert(field.pointer.clone(), Control::Text(state));
                }
            }
        }
        cx.notify();
    }

    /// 更新枚举文案时保留同一控件和原选中值，语言不参与设置数据。
    fn refresh_translations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in &self.fields {
            let (FieldKind::Choice(values), Some(Control::Choice(state))) =
                (&field.kind, self.controls.get(&field.pointer))
            else {
                continue;
            };
            let choices = values
                .iter()
                .enumerate()
                .map(|(index, value)| Choice {
                    index: index.to_string(),
                    label: match value {
                        Value::String(text) => localized(&self.prepared, text),
                        _ => value.to_string(),
                    }
                    .into(),
                })
                .collect::<Vec<_>>();
            let selected = state.read(cx).selected_value().cloned();
            state.update(cx, |state, cx| {
                state.set_items(choices, window, cx);
                if let Some(selected) = selected {
                    state.set_selected_value(&selected, window, cx);
                }
            });
        }
    }

    /// 普通编辑只修改内存草稿，最终合法性仍交由完整 JSON Schema 验证。
    fn edit(&mut self, field: &FieldSpec, value: Value, cx: &mut Context<Self>) {
        match set_field(&mut self.draft, &field.segments, value) {
            Ok(()) => {
                self.errors.remove(&field.pointer);
                self.dirty = true;
            }
            Err(error) => {
                self.errors
                    .insert(field.pointer.clone(), error.public_message().to_owned());
            }
        }
        cx.notify();
    }

    /// 明文只在提交时进入 Zeroizing，用户未改的秘密继续沿用已保存引用。
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.require_readback.is_some() || !self.errors.is_empty() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let mut values = self.draft.clone();
        let mut secrets = BTreeMap::new();
        for field in &self.fields {
            if !matches!(field.kind, FieldKind::Secret) {
                continue;
            }
            let Some(Control::Text(input)) = self.controls.get(&field.pointer) else {
                continue;
            };
            let secret = Zeroizing::new(input.read(cx).value().to_string());
            if let Some(edit) = secret_edit(secret, self.removed.contains(&field.pointer)) {
                if let Err(error) = set_field(&mut values, &field.segments, Value::Null) {
                    self.error = Some(error.public_message().to_owned());
                    cx.notify();
                    return;
                }
                secrets.insert(field.pointer.clone(), edit);
            }
        }
        let request = PluginSettingsSave {
            values,
            secrets,
            expected_revision: snapshot.revision.clone(),
        };
        let plugin = self.prepared.id.clone();
        let schema = self.schema.clone();
        let service = self.service.clone();
        let executor = self.executor.clone();
        let cancellation = self.cancellation.child_token();
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    service.save(&plugin, &schema, request, token).await
                })
                .await;
            let _ = this.update_in(cx, |form, window, cx| {
                form.busy = false;
                match result {
                    Ok(snapshot) => form.apply_snapshot(snapshot, window, cx),
                    Err(error) => {
                        form.require_readback = match error.public_code() {
                            PublicErrorCode::Conflict => Some(SettingsReadbackReason::Conflict),
                            PublicErrorCode::OutcomeUnknown => {
                                Some(SettingsReadbackReason::OutcomeUnknown)
                            }
                            _ => None,
                        };
                        form.error = Some(error.public_message().to_owned());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

impl Drop for SettingsForm {
    /// 释放页面时取消未提交的读取；已开始提交的真实结果由应用仓储判定。
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl Render for SettingsForm {
    /// 表单内容稳定保留，保存期间冻结控件并显示可恢复的局部错误。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locale = gpui_kit::component::locale().to_string();
        if locale != self.locale {
            self.locale = locale;
            self.refresh_translations(window, cx);
        }
        let disabled = self.busy || self.require_readback.is_some() || self.snapshot.is_none();
        let mut body = div().flex().flex_col().gap_3().w_full();
        for field in &self.fields {
            let mut row = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(localized(&self.prepared, &field.label));
            if let Some(description) = &field.description {
                row = row.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(localized(&self.prepared, description)),
                );
            }
            match &field.kind {
                FieldKind::Boolean => {
                    let checked = self
                        .draft
                        .pointer(&field.pointer)
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let spec = field.clone();
                    row = row.child(
                        Switch::new(format!("plugin-setting-{}", field.pointer))
                            .checked(checked)
                            .disabled(disabled)
                            .on_change(cx.listener(move |form, checked: &bool, _, cx| {
                                form.edit(&spec, Value::Bool(*checked), cx)
                            })),
                    );
                }
                _ => {
                    if let Some(control) = self.controls.get(&field.pointer) {
                        match control {
                            Control::Text(input) => {
                                row = row.child(Input::new(input).disabled(disabled).w_full());
                                if matches!(field.kind, FieldKind::Secret)
                                    && self.snapshot.as_ref().is_some_and(|snapshot| {
                                        snapshot.secret_paths.contains(&field.pointer)
                                    })
                                {
                                    let pointer = field.pointer.clone();
                                    let removed = self.removed.contains(&pointer);
                                    row = row.child(
                                        Button::new(format!("plugin-secret-remove-{pointer}"))
                                            .ghost()
                                            .disabled(disabled)
                                            .label(if removed {
                                                crate::locale::text("撤销删除", "Undo removal")
                                            } else {
                                                crate::locale::text(
                                                    "删除已保存的秘密",
                                                    "Remove saved secret",
                                                )
                                            })
                                            .on_click(cx.listener(move |form, _, _, cx| {
                                                if !form.removed.remove(&pointer) {
                                                    form.removed.insert(pointer.clone());
                                                }
                                                form.dirty = true;
                                                cx.notify();
                                            })),
                                    );
                                }
                            }
                            Control::Choice(state) => {
                                row = row.child(Select::new(state).disabled(disabled).w_full());
                            }
                        }
                    }
                }
            }
            if let Some(error) = self.errors.get(&field.pointer) {
                row = row.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                );
            }
            body = body.child(row);
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        let mut actions = div().flex().gap_2().child(
            Button::new("plugin-settings-save")
                .primary()
                .label(crate::locale::text("保存", "Save"))
                .loading(self.busy)
                .disabled(disabled || !self.dirty || !self.errors.is_empty())
                .on_click(cx.listener(|form, _, _, cx| form.save(cx))),
        );
        if self.require_readback.is_some() || self.snapshot.is_none() && self.error.is_some() {
            actions = actions.child(
                Button::new("plugin-settings-reload")
                    .label(crate::locale::text("重新读取", "Reload"))
                    .disabled(self.busy)
                    .on_click(cx.listener(|form, _, _, cx| form.load(cx))),
            );
        }
        body.child(actions)
    }
}

/// 标量保持原有 JSON 类型，仅转换为输入可编辑的展示文本。
fn super_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

/// 设置页面负责组合自定义原生视图与同一个应用层设置表单。
pub struct ExternalSettingsPage {
    form: Entity<SettingsForm>,
    custom: Option<Entity<NativeRenderer>>,
    _registration: Option<PluginUiRegistration>,
}

/// 设置页关闭前区分在途工作、未知写入和未保存草稿，供宿主选择等待或确认丢弃。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalSettingsCloseState {
    Ready,
    Working,
    ReadbackRequired,
    OutcomeUnknown,
    UnsavedChanges,
}

impl ExternalSettingsCloseState {
    /// 优先保留在途写入和未知结果；只有普通草稿允许用户显式选择丢弃。
    fn for_form(
        busy: bool,
        require_readback: Option<SettingsReadbackReason>,
        dirty: bool,
        has_errors: bool,
    ) -> Self {
        if busy {
            return Self::Working;
        }
        match require_readback {
            Some(SettingsReadbackReason::Conflict) => Self::ReadbackRequired,
            Some(SettingsReadbackReason::OutcomeUnknown) => Self::OutcomeUnknown,
            None if dirty || has_errors => Self::UnsavedChanges,
            None => Self::Ready,
        }
    }

    /// 丢弃草稿不能绕过在途事务或未知提交回读，避免把未确认写入当成已关闭。
    fn ensure_allowed(self, discard_draft: bool) -> AppResult<()> {
        match self {
            Self::Ready => Ok(()),
            Self::UnsavedChanges if discard_draft => Ok(()),
            Self::UnsavedChanges => Err(AppError::conflict("插件设置有未保存的更改")),
            Self::Working => Err(AppError::conflict("插件设置操作尚未完成")),
            Self::ReadbackRequired => Err(AppError::conflict("插件设置冲突，请重新读取")),
            Self::OutcomeUnknown => {
                Err(AppError::outcome_unknown("插件设置需要重新读取后才能关闭"))
            }
        }
    }
}

impl ExternalSettingsPage {
    /// 验收观察真实表单与自定义视图；秘密字段只暴露类型，不返回输入或引用。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn external_plugin_acceptance_snapshot(&self, cx: &App) -> Value {
        let form = self.form.read(cx);
        let mut draft = form.draft.clone();
        for field in &form.fields {
            if matches!(field.kind, FieldKind::Secret) && draft.pointer(&field.pointer).is_some() {
                let _ = set_field(&mut draft, &field.segments, Value::Null);
            }
        }
        let fields: Vec<Value> = form
            .fields
            .iter()
            .map(|field| {
                let secret = matches!(field.kind, FieldKind::Secret);
                let value = if secret {
                    Value::Null
                } else {
                    form.draft
                        .pointer(&field.pointer)
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                let input = if secret {
                    Value::Null
                } else {
                    match form.controls.get(&field.pointer) {
                        Some(Control::Text(state)) => json!(state.read(cx).value().to_string()),
                        Some(Control::Choice(state)) => json!(state.read(cx).selected_value()),
                        None => Value::Null,
                    }
                };
                json!({"pointer": field.pointer, "secret": secret, "value": value, "input": input})
            })
            .collect();
        json!({
            "ready": form.snapshot.is_some(), "loaded": form.snapshot.is_some(),
            "draft": draft, "busy": form.busy, "dirty": form.dirty,
            "closeState": format!("{:?}", self.close_state(cx)), "error": form.error,
            "fields": fields,
            "custom": self.custom.as_ref().map(|view| view.read(cx).external_plugin_acceptance_snapshot(cx)),
        })
    }

    /// schema 和视图均从已冻结资源构造，无法正确表示的 schema 明确拒绝。
    pub fn new(
        prepared: Arc<PreparedPlugin>,
        contribution: SettingsContribution,
        service: PluginSettingsService,
        services: ExternalViewServices,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Entity<Self>> {
        if prepared.package.manifest.contributes.settings.as_ref() != Some(&contribution) {
            return Err(AppError::invalid("插件设置贡献不属于清单"));
        }
        let schema = settings_schema(&prepared.package, &contribution.schema)?;
        let mut fields = Vec::new();
        collect_fields(&schema.document, &mut Vec::new(), &mut fields)?;
        let form = cx.new(|cx| {
            SettingsForm::new(
                prepared.clone(),
                schema,
                fields,
                service,
                services.executor.clone(),
                cx,
            )
        });
        let (custom, registration) = if let Some(view_id) = &contribution.view {
            let view = super::external::factories::native_view(&prepared, view_id)?;
            let identity = PluginViewIdentity {
                plugin: prepared.id.clone(),
                view: view_id.clone(),
                instance: uuid::Uuid::new_v4().to_string(),
                generation: 1,
            };
            let registration = services
                .ui_bridge
                .as_ref()
                .map(|bridge| bridge.register_instance(identity.clone()))
                .transpose()?;
            let renderer = NativeRenderer::mount(
                NativeRendererConfig {
                    prepared: prepared.clone(),
                    view,
                    identity,
                    scope: json!({"kind":"settings"}),
                    saved_state: None,
                    initial_data: None,
                    runtime: services.runtime.clone(),
                    executor: services.executor.clone(),
                    locale: services.locale.clone(),
                    content_identity: None,
                    embedded_form: Some((contribution.schema.clone(), form.clone().into())),
                },
                window,
                cx,
            )?;
            (Some(renderer), registration)
        } else {
            (None, None)
        };
        Ok(cx.new(|_| Self {
            form,
            custom,
            _registration: registration,
        }))
    }

    /// 页面切换时复用实例并通知插件实际显隐，隐藏页面不能保留焦点状态。
    pub fn visibility_changed(&self, visible: bool, focused: bool, cx: &mut App) {
        if let Some(renderer) = &self.custom {
            renderer.update(cx, |renderer, cx| {
                renderer.visibility_changed(visible, visible && focused, cx)
            });
        }
    }

    /// 对外暴露表单的关闭门禁，宿主不得在回执前丢弃页面或其秘密草稿。
    pub fn close_state(&self, cx: &App) -> ExternalSettingsCloseState {
        let form = self.form.read(cx);
        let custom = self.custom.as_ref().map(|renderer| renderer.read(cx));
        ExternalSettingsCloseState::for_form(
            form.busy || custom.is_some_and(|renderer| renderer.secret_write_pending()),
            form.require_readback,
            form.dirty || custom.is_some_and(|renderer| renderer.has_uncommitted_inputs(cx)),
            !form.errors.is_empty(),
        )
    }

    /// 普通关闭保留草稿；宿主须在回执前保留页面并冻结该页交互。
    pub fn begin_close(
        &self,
        deadline: std::time::Instant,
        cx: &App,
    ) -> super::external::runtime::RuntimeFuture<()> {
        self.begin_close_with_policy(deadline, false, cx)
    }

    /// 用户确认丢弃后才关闭；在途事务和未知提交仍必须先完成或读回。
    pub fn begin_close_discarding_draft(
        &self,
        deadline: std::time::Instant,
        cx: &App,
    ) -> super::external::runtime::RuntimeFuture<()> {
        self.begin_close_with_policy(deadline, true, cx)
    }

    /// 表单检查发生在创建关闭 future 时，避免先发送关闭再发现无法安全释放页面。
    fn begin_close_with_policy(
        &self,
        deadline: std::time::Instant,
        discard_draft: bool,
        cx: &App,
    ) -> super::external::runtime::RuntimeFuture<()> {
        if let Err(error) = self.close_state(cx).ensure_allowed(discard_draft) {
            return Box::pin(async move { Err(error) });
        }
        match &self.custom {
            Some(renderer) => renderer.read(cx).close(deadline),
            None => Box::pin(async { Ok(()) }),
        }
    }
}
impl Render for ExternalSettingsPage {
    /// 自定义视图通过 Form 节点使用同一实体；缺省页面直接显示该表单。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyView = self
            .custom
            .as_ref()
            .map(|view| view.clone().into())
            .unwrap_or_else(|| self.form.clone().into());
        div().size_full().child(content)
    }
}

/// 页面缓存属于偏好设置宿主；跨导航保留草稿和实例，不能用临时渲染值持有。
#[derive(Default)]
pub struct ExternalSettingsPages {
    pages: BTreeMap<PluginId, SettingsPageEntry>,
    next_close: u64,
}

struct SettingsPageEntry {
    page: Entity<ExternalSettingsPage>,
    closing: Option<u64>,
}

/// 关闭令牌绑定插件和本次操作；迟到回执不能释放后续创建的页面。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalSettingsCloseTicket {
    plugin: PluginId,
    sequence: u64,
}

impl ExternalSettingsPages {
    /// 按冻结的插件身份复用页面；关闭进行中拒绝重新展示同一实例。
    pub fn get_or_open(
        &mut self,
        prepared: Arc<PreparedPlugin>,
        service: PluginSettingsService,
        services: ExternalViewServices,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Entity<ExternalSettingsPage>> {
        if let Some(entry) = self.pages.get(&prepared.id) {
            if entry.closing.is_some() {
                return Err(AppError::conflict("插件设置页面正在关闭"));
            }
            return Ok(entry.page.clone());
        }
        let contribution = prepared
            .package
            .manifest
            .contributes
            .settings
            .clone()
            .ok_or_else(|| AppError::not_found("插件未声明设置页面"))?;
        let plugin = prepared.id.clone();
        let page =
            ExternalSettingsPage::new(prepared, contribution, service, services, window, cx)?;
        self.pages.insert(
            plugin,
            SettingsPageEntry {
                page: page.clone(),
                closing: None,
            },
        );
        Ok(page)
    }

    /// 关闭操作不可交互；宿主重绘时不再获取该实例作为活动页面。
    pub fn page(&self, plugin: &PluginId) -> Option<Entity<ExternalSettingsPage>> {
        self.pages
            .get(plugin)
            .filter(|entry| entry.closing.is_none())
            .map(|entry| entry.page.clone())
    }

    /// 宿主退出时枚举所有仍持有的页面，逐个执行相同截止时间的关闭流程。
    pub fn mounted_plugins(&self) -> Vec<PluginId> {
        self.pages.keys().cloned().collect()
    }

    /// 表单门禁通过后才登记关闭；调用方须立即撤下活动页面并等待对应回执。
    pub fn begin_close(
        &mut self,
        plugin: &PluginId,
        deadline: std::time::Instant,
        discard_draft: bool,
        cx: &App,
    ) -> AppResult<(
        ExternalSettingsCloseTicket,
        super::external::runtime::RuntimeFuture<()>,
    )> {
        let entry = self
            .pages
            .get_mut(plugin)
            .ok_or_else(|| AppError::not_found("插件设置页面不存在"))?;
        if entry.closing.is_some() {
            return Err(AppError::conflict("插件设置页面正在关闭"));
        }
        let page = entry.page.read(cx);
        page.close_state(cx).ensure_allowed(discard_draft)?;
        let sequence = self
            .next_close
            .checked_add(1)
            .ok_or_else(|| AppError::infrastructure("插件设置关闭代次已耗尽"))?;
        let future = page.begin_close_with_policy(deadline, discard_draft, cx);
        self.next_close = sequence;
        entry.closing = Some(sequence);
        Ok((
            ExternalSettingsCloseTicket {
                plugin: plugin.clone(),
                sequence,
            },
            future,
        ))
    }

    /// 成功回执才释放页面；失败保留同一实体，由宿主提示并决定恢复路径。
    pub fn finish_close(
        &mut self,
        ticket: &ExternalSettingsCloseTicket,
        result: &AppResult<()>,
    ) -> AppResult<()> {
        let entry = self
            .pages
            .get_mut(&ticket.plugin)
            .filter(|entry| entry.closing == Some(ticket.sequence))
            .ok_or_else(|| AppError::conflict("插件设置关闭回执已过期"))?;
        if result.is_ok() {
            self.pages.remove(&ticket.plugin);
        } else {
            entry.closing = None;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/external_plugin_settings.rs"]
mod tests;
