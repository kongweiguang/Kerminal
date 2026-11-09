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
    rc::Rc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

use super::{renderer::NativeRenderer, state::BindingPath, view::display_value};

#[derive(Clone)]
pub(super) struct NativeChoice {
    index: String,
    label: SharedString,
}

impl SearchableListItem for NativeChoice {
    type Value = String;

    /// 值使用选项位置而不是显示文案，避免本地化改变协议值。
    fn value(&self) -> &String {
        &self.index
    }

    /// 标题只负责展示，协议 JSON 值由宿主保留。
    fn title(&self) -> SharedString {
        self.label.clone()
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
        options: Vec<SelectOption>,
    },
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
                ..
            } => self.add_input(
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
            ),
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
                let choices = options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| NativeChoice {
                        index: index.to_string(),
                        label: self.projection.text(&option.label).into(),
                    })
                    .collect::<Vec<_>>();
                let state = cx.new(|cx| SelectState::new(choices, None, window, cx));
                if let Some(index) = options
                    .iter()
                    .position(|option| self.field_value(bind) == Some(&option.value))
                {
                    state.update(cx, |state, cx| {
                        state.set_selected_value(&index.to_string(), window, cx)
                    });
                }
                let path = bind.clone();
                let values = options
                    .iter()
                    .map(|option| option.value.clone())
                    .collect::<Vec<_>>();
                let subscription = cx.subscribe(
                    &state,
                    move |renderer, _, event: &SelectEvent<Vec<NativeChoice>>, cx| {
                        if let SelectEvent::Confirm(Some(index)) = event
                            && let Some(value) = index
                                .parse::<usize>()
                                .ok()
                                .and_then(|index| values.get(index))
                        {
                            renderer.edit_field(&path, value.clone(), cx);
                        }
                    },
                );
                self.subscriptions.push(subscription);
                self.fields.insert(
                    key,
                    FieldControl::Select {
                        state,
                        bind: bind.clone(),
                        options: options.clone(),
                    },
                );
            }
            _ => {}
        }
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
        BindingPath::parse(bind)
            .ok()
            .and_then(|path| self.projection.state().value(&path))
    }

    /// 只格式化普通绑定；秘密输入禁止从数据快照回填。
    fn field_text(&self, bind: &str) -> String {
        self.field_value(bind)
            .map(display_value)
            .unwrap_or_default()
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
        for field in self.fields.values() {
            match field {
                FieldControl::Input {
                    state,
                    bind,
                    secret,
                    ..
                } if !secret && !self.field_errors.contains_key(bind) => {
                    let value = self.field_text(bind);
                    if state.read(cx).value().as_ref() != value {
                        state.update(cx, |input, cx| input.set_value(value, window, cx));
                    }
                }
                FieldControl::Textarea { state, bind, .. } => {
                    let value = self.field_text(bind);
                    if state.read(cx).value().as_ref() != value {
                        state.update(cx, |input, cx| input.set_value(value, window, cx));
                    }
                }
                FieldControl::Select {
                    state,
                    bind,
                    options,
                } => {
                    if let Some(index) = options
                        .iter()
                        .position(|option| self.field_value(bind) == Some(&option.value))
                    {
                        let value = index.to_string();
                        if state.read(cx).selected_value() != Some(&value) {
                            state.update(cx, |state, cx| {
                                state.set_selected_value(&value, window, cx)
                            });
                        }
                    }
                }
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
                FieldControl::Select { state, options, .. } => {
                    let choices = options
                        .iter()
                        .enumerate()
                        .map(|(index, option)| NativeChoice {
                            index: index.to_string(),
                            label: self.projection.text(&option.label).into(),
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
