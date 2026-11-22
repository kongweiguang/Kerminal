//! 原生输入控件与绑定草稿；秘密只在控件和短生命周期 Vault 请求中出现。
//!
//! @author kongweiguang

#[cfg(test)]
#[path = "../../../tests/unit/external_native_inputs.rs"]
mod tests;

use crate::gpui::Focusable as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EntityInputHandler as _, SharedString, Window,
    component::{
        input::{EditorState, InputEvent, InputState, TextareaState},
        searchable_list::SearchableListItem,
        select::{SelectEvent, SelectState},
    },
};
use kerminal_plugin_protocol::native::{NativeNode, NativeNodeKind, SelectOption};
use serde_json::{Number, Value};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

use super::{
    renderer::NativeRenderer,
    state::BindingPath,
    view::{NativeViewProjection, display_value},
};

/// 运行时选项来自实例数据而非静态视图，因此必须在渲染前限制条数和单值大小，
/// 避免插件用超大补丁撑爆下拉列表或把草稿写出协议允许的范围。
const RUNTIME_SELECT_OPTIONS_EXTENSION: &str = "x-kerminal-select-options";
const MAX_RUNTIME_SELECT_OPTIONS: usize = 500;
const MAX_RUNTIME_OPTION_LABEL_CHARS: usize = 512;
const MAX_RUNTIME_OPTION_VALUE_BYTES: usize = 512;

#[derive(Clone, PartialEq)]
pub(super) struct NativeChoice {
    // 静态选项用位置字符串保证重复值也能分别选中；运行时选项直接保存原始值，
    // 确认事件因此无需再按可能已经更新的选项表回查，避免使用过期的捕获索引。
    index: String,
    label: SharedString,
}

impl SearchableListItem for NativeChoice {
    type Value = String;

    /// 静态选项保留位置字符串，运行时选项就是协议值；确认事件无需按可变列表回查。
    fn value(&self) -> &String {
        &self.index
    }

    /// 标题只负责展示，协议 JSON 值由宿主保留。
    fn title(&self) -> SharedString {
        self.label.clone()
    }
}

/// 控件同步需要同时借用 `fields` 和投影，因此按键查找做成自由函数，
/// 让调用方显式只借用 `projection` 字段，避免再次借用整个渲染器。
fn bound_value<'a>(projection: &'a NativeViewProjection, bind: &str) -> Option<&'a Value> {
    BindingPath::parse(bind)
        .ok()
        .and_then(|path| projection.state().value(&path))
}

/// 文本控件与选择控件共享同一展示格式，缺失值保持空串而非 `null` 文本。
fn bound_text(projection: &NativeViewProjection, bind: &str) -> String {
    bound_value(projection, bind)
        .map(display_value)
        .unwrap_or_default()
}

/// 只在路径真实存在时替换为 null；父级不是对象或键缺失都保持原快照，
/// 避免为了隐藏一个失效值而凭空制造字段。
fn null_out_path(root: &mut Value, segments: &[String]) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = root;
    for segment in parents {
        let Some(next) = current.get_mut(segment.as_str()) else {
            return;
        };
        current = next;
    }
    let Some(object) = current.as_object_mut() else {
        return;
    };
    if object.contains_key(last) {
        object.insert(last.clone(), Value::Null);
    }
}

#[derive(Clone, Copy)]
pub(super) struct NumberLimits {
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
}

pub(super) enum FieldControl {
    Input {
        state: Entity<InputState>,
        bind: String,
        secret: bool,
        secret_write: Option<Rc<SecretWriteState>>,
        placeholder: Option<String>,
    },
    Textarea {
        state: Entity<TextareaState>,
        bind: String,
        placeholder: Option<String>,
    },
    Select {
        state: Entity<SelectState<Vec<NativeChoice>>>,
        bind: String,
        source: SelectSource,
    },
}

/// 静态选项在视图装载时冻结；运行时选项引用实例数据，补丁到达后必须重新投影，
/// 但两者共用同一个 Select 实体以保留焦点和交互状态。
pub(super) enum SelectSource {
    Static(Vec<SelectOption>),
    Runtime {
        binding: String,
        choices: Vec<NativeChoice>,
        // 当前非空值无法在下拉候选中找到（包括候选缺失或暂时为空）时，
        // 动作快照必须隐藏该值，避免提交用户已经看不见的旧值。
        stale: bool,
    },
}

/// 只接受显式的只读绑定；格式不符时回退普通文本输入，
/// 让旧宿主和新宿主都不会因扩展字段写错而丢失可编辑能力。
fn runtime_select_binding(extensions: &BTreeMap<String, Value>) -> Option<String> {
    let binding = extensions
        .get(RUNTIME_SELECT_OPTIONS_EXTENSION)?
        .as_str()?
        .strip_prefix('$')?;
    BindingPath::parse(binding).ok()?;
    Some(binding.to_owned())
}

/// 运行时选项只接受 `{label, value}` 字符串对象；空值、重复值和超限项会被跳过，
/// 不生成占位选项，也不把非字符串 JSON 隐式转成表单值。
fn runtime_select_choices(value: Option<&Value>) -> Vec<NativeChoice> {
    let Some(rows) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    rows.iter()
        .take(MAX_RUNTIME_SELECT_OPTIONS)
        .filter_map(|row| {
            let row = row.as_object()?;
            let label = row.get("label")?.as_str()?;
            let value = row.get("value")?.as_str()?;
            if label.is_empty()
                || label.chars().count() > MAX_RUNTIME_OPTION_LABEL_CHARS
                || value.is_empty()
                || value.len() > MAX_RUNTIME_OPTION_VALUE_BYTES
                || !seen.insert(value.to_owned())
            {
                return None;
            }
            Some(NativeChoice {
                index: value.to_owned(),
                label: label.into(),
            })
        })
        .collect()
}

/// 窗口线程只保留写入和编辑标志，不为比较回执再次保存秘密明文。
#[derive(Default)]
pub(super) struct SecretWriteState {
    pending: Cell<bool>,
    changed: Cell<bool>,
}

impl SecretWriteState {
    /// 同一输入只允许一个 Vault 写入，防止乱序回执覆盖较新的引用。
    fn begin(&self) -> bool {
        if self.pending.replace(true) {
            return false;
        }
        self.changed.set(false);
        true
    }

    /// 写入期间的任何编辑都必须保留，即使随后又改回原值。
    fn edited(&self) {
        if self.pending.get() {
            self.changed.set(true);
        }
    }

    /// 回执结束门禁，并告知调用方是否仍可安全清空输入。
    fn finish(&self) -> bool {
        self.pending.set(false);
        !self.changed.get()
    }

    /// 关闭和动作需要等待 Vault 回执，避免使用未确认的引用。
    pub(super) fn is_pending(&self) -> bool {
        self.pending.get()
    }
}

/// 各输入变体共享窗口实体构造，但仅携带各自声明的约束。
struct InputSpec<'a> {
    key: String,
    bind: &'a str,
    placeholder: Option<&'a str>,
    number: Option<NumberLimits>,
    secret: bool,
    on_submit: Option<String>,
}

impl NativeRenderer {
    /// 输入实体只在装载视图时创建，后续补丁不会重建焦点或撤销栈。
    pub(super) fn prepare_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.projection.view().root.clone();
        self.prepare_node_controls(&root, "root".into(), window, cx);
    }

    /// 静态树使用结构路径稳定索引，重复绑定也保持各自的原生输入实体。
    fn prepare_node_controls(
        &mut self,
        node: &NativeNode,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use NativeNodeKind::*;
        match &node.kind {
            Stack { children, .. }
            | Row { children, .. }
            | Section { children, .. }
            | Split { children, .. }
            | Toolbar { children, .. } => {
                for (index, child) in children.iter().enumerate() {
                    self.prepare_node_controls(child, format!("{key}.{index}"), window, cx);
                }
            }
            Scroll { child, .. } => {
                self.prepare_node_controls(child, format!("{key}.0"), window, cx)
            }
            Tabs { tabs, .. } => {
                for (index, tab) in tabs.iter().enumerate() {
                    self.prepare_node_controls(&tab.content, format!("{key}.{index}"), window, cx);
                }
            }
            Form { schema, bind, .. } => {
                if self
                    .embedded_form
                    .as_ref()
                    .is_some_and(|(expected, _)| schema == expected && bind.is_none())
                {
                    return;
                }
                let form = super::forms::NativeForm::from_resource(
                    &self.prepared,
                    schema,
                    bind.as_deref(),
                )
                .map_err(|error| error.public_message().to_owned());
                if let Ok(form) = &form {
                    self.prepare_node_controls(&form.root, format!("{key}.form"), window, cx);
                }
                self.forms.insert(key, form);
            }
            Code { code, language, .. } => {
                let initial = self.projection.text(code);
                let language = language.clone();
                let state = cx.new(|cx| {
                    let mut state = EditorState::new(window, cx).default_value(initial);
                    state.set_readonly(true, cx);
                    if let Some(language) = language {
                        state.set_highlighter(language, cx);
                    }
                    state
                });
                self.code_blocks.insert(key, (state, code.clone()));
            }
            Input {
                bind,
                placeholder,
                on_submit,
                extensions,
                ..
            } => {
                if let Some(binding) = runtime_select_binding(extensions) {
                    let choices = self.runtime_select_choices(&binding);
                    self.add_select(
                        key,
                        bind,
                        SelectSource::Runtime {
                            binding,
                            choices,
                            stale: false,
                        },
                        window,
                        cx,
                    );
                } else {
                    self.add_input(
                        InputSpec {
                            key,
                            bind,
                            placeholder: placeholder.as_deref(),
                            number: None,
                            secret: false,
                            on_submit: on_submit.clone(),
                        },
                        window,
                        cx,
                    );
                }
            }
            Number {
                bind,
                min,
                max,
                step,
                ..
            } => self.add_input(
                InputSpec {
                    key,
                    bind,
                    placeholder: None,
                    number: Some(NumberLimits {
                        min: *min,
                        max: *max,
                        step: *step,
                    }),
                    secret: false,
                    on_submit: None,
                },
                window,
                cx,
            ),
            Path { bind, .. } => self.add_input(
                InputSpec {
                    key,
                    bind,
                    placeholder: None,
                    number: None,
                    secret: false,
                    on_submit: None,
                },
                window,
                cx,
            ),
            Secret { bind, .. } => self.add_input(
                InputSpec {
                    key,
                    bind,
                    placeholder: None,
                    number: None,
                    secret: true,
                    on_submit: None,
                },
                window,
                cx,
            ),
            Textarea {
                bind, placeholder, ..
            } => {
                let initial = self.field_text(bind);
                let placeholder_template = placeholder.clone();
                let placeholder = placeholder
                    .as_deref()
                    .map(|text| self.projection.text(text))
                    .unwrap_or_default();
                let state = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .placeholder(placeholder)
                        .auto_grow(2, 8)
                });
                state.update(cx, |input, cx| input.set_value(initial, window, cx));
                let path = bind.clone();
                let subscription =
                    cx.subscribe(&state, move |renderer, input, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            renderer.edit_field(
                                &path,
                                Value::String(input.read(cx).value().to_string()),
                                cx,
                            );
                        }
                    });
                self.subscriptions.push(subscription);
                self.fields.insert(
                    key,
                    FieldControl::Textarea {
                        state,
                        bind: bind.clone(),
                        placeholder: placeholder_template,
                    },
                );
            }
            Select { bind, options, .. } => {
                self.add_select(key, bind, SelectSource::Static(options.clone()), window, cx)
            }
            _ => {}
        }
    }

    /// 静态和运行时选项共用 Select 实体；运行时源只保存绑定路径和当前投影，
    /// 确认事件直接携带真实值，避免补丁替换列表后仍按旧索引写回表单。
    fn add_select(
        &mut self,
        key: String,
        bind: &str,
        source: SelectSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choices = match &source {
            SelectSource::Static(options) => self.static_choices(options),
            SelectSource::Runtime { choices, .. } => choices.clone(),
        };
        let selected = self.select_selection(&source, bind);
        let state = cx.new(|cx| SelectState::new(choices, None, window, cx));
        if let Some(selected) = selected {
            state.update(cx, |state, cx| {
                state.set_selected_value(&selected, window, cx)
            });
        }
        let path = bind.to_owned();
        let node_key = key.clone();
        // 静态选项在视图装载后不再变化，捕获其值到索引的映射是安全的；
        // 运行时选项不捕获列表，确认值本身就是需要写回绑定的字符串。
        let static_values = match &source {
            SelectSource::Static(options) => Some(
                options
                    .iter()
                    .map(|option| option.value.clone())
                    .collect::<Vec<_>>(),
            ),
            SelectSource::Runtime { .. } => None,
        };
        let subscription = cx.subscribe(
            &state,
            move |renderer, _, event: &SelectEvent<Vec<NativeChoice>>, cx| {
                let SelectEvent::Confirm(Some(choice)) = event else {
                    return;
                };
                let value = match &static_values {
                    Some(values) => choice
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| values.get(index))
                        .cloned(),
                    None => {
                        // 确认时按当前列表核对，而不是相信装载时的索引；确认值有效
                        // 同时解除补丁造成的不可用标记，让 Start 等动作恢复可用。
                        let Some(FieldControl::Select {
                            source: SelectSource::Runtime { choices, stale, .. },
                            ..
                        }) = renderer.fields.get_mut(&node_key)
                        else {
                            return;
                        };
                        if !choices.iter().any(|candidate| candidate.index == *choice) {
                            *stale = true;
                            return;
                        }
                        *stale = false;
                        Some(Value::String(choice.clone()))
                    }
                };
                if let Some(value) = value {
                    renderer.edit_field(&path, value, cx);
                }
            },
        );
        self.subscriptions.push(subscription);
        self.fields.insert(
            key,
            FieldControl::Select {
                state,
                bind: bind.to_owned(),
                source,
            },
        );
    }

    /// 静态选项的位置与文案只在视图或语言变化时重建，值和选中身份保持稳定。
    fn static_choices(&self, options: &[SelectOption]) -> Vec<NativeChoice> {
        options
            .iter()
            .enumerate()
            .map(|(index, option)| NativeChoice {
                index: index.to_string(),
                label: self.projection.text(&option.label).into(),
            })
            .collect()
    }

    /// 初始选中只接受当前控件能表达的值；运行时候选尚未返回时保持空选择，
    /// 由后续补丁到达后的同步补齐，不能伪造当前值选项。
    fn select_selection(&self, source: &SelectSource, bind: &str) -> Option<String> {
        let current = bound_value(&self.projection, bind);
        match source {
            SelectSource::Static(options) => options
                .iter()
                .position(|option| current == Some(&option.value))
                .map(|index| index.to_string()),
            SelectSource::Runtime { choices, .. } => current
                .and_then(Value::as_str)
                .filter(|value| choices.iter().any(|choice| choice.index == *value))
                .map(str::to_owned),
        }
    }

    /// 选项绑定只读取实例数据；缺失或类型错误时返回空列表，等待下一次补丁重试。
    fn runtime_select_choices(&self, binding: &str) -> Vec<NativeChoice> {
        bound_value(&self.projection, binding)
            .map(|value| runtime_select_choices(Some(value)))
            .unwrap_or_default()
    }

    /// 动作快照不得携带下拉里已经不可见的旧值；只把失效绑定投影为 null，
    /// 其他字段和动作保持原样，避免一个失效下拉阻断停止、菜单等无关操作。
    pub(super) fn action_data(&self) -> Value {
        let mut data = self.projection.state().data().clone();
        for field in self.fields.values() {
            if let FieldControl::Select {
                bind,
                source: SelectSource::Runtime { stale: true, .. },
                ..
            } = field
                && let Ok(path) = BindingPath::parse(bind)
            {
                null_out_path(&mut data, path.segments());
            }
        }
        data
    }

    /// `x-kerminal-requires-text` 等显式依赖只把失效下拉当成缺值，
    /// 这样 Start 会被禁用，而其他没有声明依赖的动作继续可用。
    pub(super) fn is_stale_runtime_binding(&self, bind: &str) -> bool {
        self.fields.values().any(|field| {
            matches!(
                field,
                FieldControl::Select {
                    bind: field_bind,
                    source: SelectSource::Runtime { stale: true, .. },
                    ..
                } if field_bind == bind
            )
        })
    }

    /// 普通值直接更新草稿，数字仅在完整且满足边界时进入动作快照。
    fn add_input(&mut self, spec: InputSpec<'_>, window: &mut Window, cx: &mut Context<Self>) {
        let InputSpec {
            key,
            bind,
            placeholder,
            number,
            secret,
            on_submit,
        } = spec;
        let initial = if secret {
            String::new()
        } else {
            self.field_text(bind)
        };
        let placeholder_template = placeholder.map(str::to_owned);
        let placeholder = placeholder
            .map(|text| self.projection.text(text))
            .unwrap_or_default();
        let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        state.update(cx, |input, cx| {
            input.set_value(initial, window, cx);
            if secret {
                input.set_masked(true, window, cx);
            }
        });
        let path = bind.to_owned();
        let node_key = key.clone();
        // 同一绑定可出现在多个节点；共享写入门禁避免这些入口互相覆盖。
        let secret_write = secret.then(|| {
            self.fields
                .values()
                .find_map(|field| match field {
                    FieldControl::Input {
                        bind: existing,
                        secret_write: Some(write),
                        ..
                    } if existing == bind => Some(Rc::clone(write)),
                    _ => None,
                })
                .unwrap_or_else(|| Rc::new(SecretWriteState::default()))
        });
        let secret_events = secret_write.clone();
        // 输入法确认键可能先结束组合再产生 Enter；在输入控件分发前记录这一拍。
        let suppress_enter = Rc::new(Cell::new(false));
        let window_handle = window.window_handle();
        let composition_input = state.clone();
        if on_submit.is_some() {
            let focus = state.read(cx).focus_handle(cx);
            let input = state.clone();
            let suppress = Rc::clone(&suppress_enter);
            self.subscriptions
                .push(cx.intercept_keystrokes(move |event, window, app| {
                    if !event.keystroke.key.eq_ignore_ascii_case("enter")
                        || !focus.is_focused(window)
                    {
                        return;
                    }
                    if !input.update(app, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    }) {
                        return;
                    }
                    suppress.set(true);
                    let cleanup = Rc::clone(&suppress);
                    window.defer(app, move |_, _| cleanup.set(false));
                }));
        }
        let subscription =
            cx.subscribe(
                &state,
                move |renderer, input, event: &InputEvent, cx| match event {
                    InputEvent::Change if secret => {
                        if let Some(write) = &secret_events {
                            write.edited();
                        }
                    }
                    InputEvent::Change if !secret => {
                        let text = input.read(cx).value().to_string();
                        if let Some(limits) = number {
                            let parsed = text
                                .parse::<f64>()
                                .ok()
                                .filter(|value| value.is_finite())
                                .filter(|value| limits.min.is_none_or(|min| *value >= min))
                                .filter(|value| limits.max.is_none_or(|max| *value <= max))
                                .filter(|value| {
                                    limits.step.is_none_or(|step| {
                                        step > 0.0
                                            && (((*value - limits.min.unwrap_or(0.0)) / step)
                                                .round()
                                                - ((*value - limits.min.unwrap_or(0.0)) / step))
                                                .abs()
                                                < 1e-9
                                    })
                                })
                                .and_then(Number::from_f64);
                            if let Some(value) = parsed {
                                renderer.field_errors.remove(&path);
                                renderer.edit_field(&path, Value::Number(value), cx);
                            } else {
                                renderer.field_errors.insert(
                                    path.clone(),
                                    crate::locale::text(
                                        "数字无效或超出允许范围",
                                        "Invalid or out-of-range number",
                                    )
                                    .to_string(),
                                );
                                cx.notify();
                            }
                        } else {
                            renderer.edit_field(&path, Value::String(text), cx);
                        }
                    }
                    InputEvent::PressEnter {
                        secondary: false,
                        shift: false,
                    } if !secret => {
                        if suppress_enter.replace(false) {
                            return;
                        }
                        let composing = window_handle.update(cx, |_, window, app| {
                            composition_input.update(app, |input, cx| {
                                input.marked_text_range(window, cx).is_some()
                            })
                        });
                        if matches!(composing, Ok(false))
                            && let Some(action) = &on_submit
                        {
                            renderer.action(node_key.clone(), action.clone(), None, cx);
                        }
                    }
                    _ => {}
                },
            );
        self.subscriptions.push(subscription);
        self.fields.insert(
            key,
            FieldControl::Input {
                state,
                bind: bind.to_owned(),
                secret,
                secret_write,
                placeholder: placeholder_template,
            },
        );
    }

    /// 缺失值以空输入展示，不把 JSON null 当成文字写回。
    pub(super) fn field_value(&self, bind: &str) -> Option<&Value> {
        bound_value(&self.projection, bind)
    }

    /// 只格式化普通绑定；秘密输入禁止从数据快照回填。
    fn field_text(&self, bind: &str) -> String {
        bound_text(&self.projection, bind)
    }

    /// 有效编辑进入既有工作区保存通知；秘密和无效输入不进入草稿快照，也不发额外 RPC。
    pub(super) fn edit_field(&mut self, bind: &str, value: Value, cx: &mut Context<Self>) {
        match BindingPath::parse(bind)
            .and_then(|path| self.projection.state_mut().edit(&path, value))
        {
            Ok(changed) => {
                self.field_errors.remove(bind);
                if changed {
                    self.notify_content_state_changed(cx);
                }
            }
            Err(_) => {
                self.field_errors.insert(
                    bind.to_owned(),
                    crate::locale::text("字段无法更新", "Cannot update field").to_string(),
                );
            }
        }
        cx.notify();
    }

    /// 只同步非秘密且当前没有无效输入的字段，避免后台补丁覆盖用户输入。
    pub(super) fn sync_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in self.fields.values_mut() {
            match field {
                FieldControl::Input {
                    state,
                    bind,
                    secret,
                    ..
                } if !*secret && !self.field_errors.contains_key(bind.as_str()) => {
                    let value = bound_text(&self.projection, bind.as_str());
                    if state.read(cx).value().as_ref() != value {
                        state.update(cx, |input, cx| input.set_value(value, window, cx));
                    }
                }
                FieldControl::Textarea { state, bind, .. } => {
                    let value = bound_text(&self.projection, bind.as_str());
                    if state.read(cx).value().as_ref() != value {
                        state.update(cx, |input, cx| input.set_value(value, window, cx));
                    }
                }
                FieldControl::Select {
                    state,
                    bind,
                    source,
                } => match source {
                    SelectSource::Static(options) => {
                        let current = bound_value(&self.projection, bind.as_str());
                        if let Some(index) = options
                            .iter()
                            .position(|option| current == Some(&option.value))
                        {
                            let value = index.to_string();
                            if state.read(cx).selected_value() != Some(&value) {
                                state.update(cx, |state, cx| {
                                    state.set_selected_value(&value, window, cx)
                                });
                            }
                        }
                    }
                    SelectSource::Runtime {
                        binding,
                        choices,
                        stale,
                    } => {
                        let raw = bound_value(&self.projection, bind.as_str());
                        let current = raw.and_then(Value::as_str).map(str::to_owned);
                        let next = bound_value(&self.projection, binding.as_str())
                            .map(|value| runtime_select_choices(Some(value)))
                            .unwrap_or_default();
                        // 下拉只能表达候选中的字符串；当前值不是字符串，或非空字符串
                        // 不在候选里（包括候选尚未返回或暂时为空）时，动作快照都不能
                        // 继续带出这个看不见的值。null 和空串视为未选择。
                        let unrepresentable =
                            raw.is_some_and(|value| !value.is_null() && !value.is_string());
                        let current_value = current.as_deref().filter(|value| !value.is_empty());
                        let matched = current_value.is_some_and(|value| {
                            next.iter().any(|candidate| candidate.index == value)
                        });
                        *stale = unrepresentable || (current_value.is_some() && !matched);
                        if next != *choices {
                            // 先替换选项再按当前草稿值重选：值仍存在时保留选择，
                            // 已被移除时只清空下拉显示，不丢弃用户尚未提交的草稿。
                            *choices = next.clone();
                            state.update(cx, |state, cx| {
                                state.set_items(next, window, cx);
                                match &current {
                                    Some(value) => state.set_selected_value(value, window, cx),
                                    None => state.set_selected_index(None, window, cx),
                                }
                            });
                        } else {
                            let selected = state.read(cx).selected_value().cloned();
                            if selected != current {
                                state.update(cx, |state, cx| match &current {
                                    Some(value) => state.set_selected_value(value, window, cx),
                                    None => state.set_selected_index(None, window, cx),
                                });
                            }
                        }
                    }
                },
                _ => {}
            }
        }
        for (state, source) in self.code_blocks.values() {
            let value = self.projection.text(source);
            if state.read(cx).value().as_ref() != value {
                state.update(cx, |editor, cx| editor.set_value(value, window, cx));
            }
        }
    }

    /// 语言变化只替换提示和选项文案，保留输入实体、选中值与草稿。
    pub(super) fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in self.fields.values() {
            match field {
                FieldControl::Input {
                    state,
                    placeholder: Some(text),
                    ..
                } => {
                    let text = self.projection.text(text);
                    state.update(cx, |state, cx| state.set_placeholder(text, window, cx));
                }
                FieldControl::Textarea {
                    state,
                    placeholder: Some(text),
                    ..
                } => {
                    let text = self.projection.text(text);
                    state.update(cx, |state, cx| state.set_placeholder(text, window, cx));
                }
                FieldControl::Select { state, source, .. } => {
                    // 静态选项文案来自本地化键，需要重建；运行时选项已经是插件给出的
                    // 展示文本，语言切换后由图中的字段标签和无障碍名称更新。
                    if let SelectSource::Static(options) = source {
                        let choices = self.static_choices(options);
                        let selected = state.read(cx).selected_value().cloned();
                        state.update(cx, |state, cx| {
                            state.set_items(choices, window, cx);
                            if let Some(selected) = selected {
                                state.set_selected_value(&selected, window, cx);
                            }
                        });
                    }
                }
                _ => {}
            }
        }
        self.sync_controls(window, cx);
    }

    /// 明文只交给 Vault，成功后清空控件并向视图提供引用；失败保留输入供用户重试。
    pub(super) fn save_secret(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(FieldControl::Input {
            state,
            bind,
            secret: true,
            secret_write: Some(write),
            ..
        }) = self.fields.get(key)
        else {
            return;
        };
        let state = state.clone();
        let bind = bind.clone();
        let write = Rc::clone(write);
        let secret = Zeroizing::new(state.read(cx).value().to_string());
        if secret.is_empty() || !write.begin() {
            return;
        }
        cx.notify();
        let identity = self.identity.clone();
        let runtime = self.runtime.clone();
        let executor = self.executor.clone();
        let cancellation = self.cancellation.child_token();
        let deadline = Instant::now() + Duration::from_secs(15);
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    runtime
                        .store_secret(identity, bind.clone(), secret, deadline, token)
                        .await
                        .map(|reference| (bind, reference))
                })
                .await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                let unchanged = write.finish();
                match result {
                    Ok((bind, reference)) => {
                        if unchanged {
                            state.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        renderer.edit_field(&bind, Value::String(reference), cx);
                    }
                    Err(error) => {
                        renderer.error = Some(error.public_message().to_owned());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}
