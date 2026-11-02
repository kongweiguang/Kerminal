//! 终端建议与关键词规则的 typed 草稿编辑。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::{
    AppContext as _, Context, Entity, ScrollHandle, SharedString, Subscription, Task, Window,
    component::{
        Colorize as _,
        color_picker::{ColorPickerEvent, ColorPickerState},
        input::{InputEvent, InputState, TextareaState},
        select::{SelectEvent, SelectState},
    },
    rgb,
};
use kerminal_application::preferences::{
    SettingsSnapshot, TerminalCommandSuggestionPresentation,
    TerminalCommandSuggestionRemoteRefresh, TerminalInlineSuggestionAcceptKey,
    TerminalKeywordHighlightCustomColors, TerminalKeywordHighlightMatchMode,
    TerminalKeywordHighlightRule, TerminalKeywordHighlightStyle,
};

use super::forms::{set_input, set_textarea};
use super::{PreferencesEvent, PreferencesPanel};

pub(in crate::features::preferences) const KEYWORD_RULE_LIMIT: usize = 64;
const KEYWORD_PATTERN_LIMIT: usize = 256;
const KEYWORD_NOTE_LIMIT: usize = 160;
const KEYWORD_DELETE_UNDO_WINDOW: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
struct SuggestionSelectOptionSpec {
    value: &'static str,
    label_zh: &'static str,
    label_en: &'static str,
}

const SUGGESTION_PRESENTATION_OPTIONS: [SuggestionSelectOptionSpec; 3] = [
    SuggestionSelectOptionSpec {
        value: "inlineAndMenu",
        label_zh: "灰色提示和候选列表",
        label_en: "Ghost text and candidate list",
    },
    SuggestionSelectOptionSpec {
        value: "inline",
        label_zh: "仅灰色提示",
        label_en: "Ghost text only",
    },
    SuggestionSelectOptionSpec {
        value: "off",
        label_zh: "关闭",
        label_en: "Off",
    },
];

const SUGGESTION_ACCEPT_KEY_OPTIONS: [SuggestionSelectOptionSpec; 2] = [
    SuggestionSelectOptionSpec {
        value: "rightArrow",
        label_zh: "右方向键",
        label_en: "Right arrow",
    },
    SuggestionSelectOptionSpec {
        value: "disabled",
        label_zh: "不绑定",
        label_en: "Disabled",
    },
];

const SUGGESTION_REMOTE_REFRESH_OPTIONS: [SuggestionSelectOptionSpec; 2] = [
    SuggestionSelectOptionSpec {
        value: "safe",
        label_zh: "安全后台刷新",
        label_en: "Safe background refresh",
    },
    SuggestionSelectOptionSpec {
        value: "off",
        label_zh: "仅使用已有缓存",
        label_en: "Use cached results only",
    },
];

#[derive(Clone)]
pub(in crate::features::preferences) struct SuggestionSelectOption {
    value: String,
    label: SharedString,
}

impl SearchableListItem for SuggestionSelectOption {
    type Value = String;

    /// 下拉项以 main 的本地化标签显示，value 则保持配置枚举的稳定编码。
    fn title(&self) -> SharedString {
        self.label.clone()
    }

    /// 选择回调只传递稳定设置值，不从用户可见文案反向解析配置。
    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// 三个标准下拉共用此拥有者，选项确认后由 PreferencesPanel 更新单份设置草稿。
///
/// @author kongweiguang
pub(in crate::features::preferences) struct SuggestionSelects {
    pub(in crate::features::preferences) presentation:
        Entity<SelectState<Vec<SuggestionSelectOption>>>,
    pub(in crate::features::preferences) accept_key:
        Entity<SelectState<Vec<SuggestionSelectOption>>>,
    pub(in crate::features::preferences) remote_refresh:
        Entity<SelectState<Vec<SuggestionSelectOption>>>,
    _subscriptions: Vec<Subscription>,
}

impl SuggestionSelects {
    /// 先创建无业务副作用的 Select 状态，真实快照加载后由 `sync_from_settings` 选择对应值。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn new(
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) -> Self {
        let presentation = cx.new(|cx| {
            SelectState::new(
                suggestion_select_options(&SUGGESTION_PRESENTATION_OPTIONS),
                None,
                window,
                cx,
            )
        });
        let accept_key = cx.new(|cx| {
            SelectState::new(
                suggestion_select_options(&SUGGESTION_ACCEPT_KEY_OPTIONS),
                None,
                window,
                cx,
            )
        });
        let remote_refresh = cx.new(|cx| {
            SelectState::new(
                suggestion_select_options(&SUGGESTION_REMOTE_REFRESH_OPTIONS),
                None,
                window,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe(
                &presentation,
                |panel, _, event: &SelectEvent<Vec<SuggestionSelectOption>>, cx| {
                    let SelectEvent::Confirm(Some(value)) = event else {
                        return;
                    };
                    let presentation = match value.as_str() {
                        "inlineAndMenu" => TerminalCommandSuggestionPresentation::InlineAndMenu,
                        "inline" => TerminalCommandSuggestionPresentation::Inline,
                        "off" => TerminalCommandSuggestionPresentation::Off,
                        _ => return,
                    };
                    panel.edit_settings(
                        |settings| {
                            let suggestion = &mut settings.terminal.inline_suggestion;
                            suggestion.presentation = presentation;
                            suggestion.enabled =
                                presentation != TerminalCommandSuggestionPresentation::Off;
                        },
                        cx,
                    );
                },
            ),
            cx.subscribe(
                &accept_key,
                |panel, _, event: &SelectEvent<Vec<SuggestionSelectOption>>, cx| {
                    let SelectEvent::Confirm(Some(value)) = event else {
                        return;
                    };
                    let accept_key = match value.as_str() {
                        "rightArrow" => TerminalInlineSuggestionAcceptKey::RightArrow,
                        "disabled" => TerminalInlineSuggestionAcceptKey::Disabled,
                        _ => return,
                    };
                    panel.edit_settings(
                        |settings| {
                            settings.terminal.inline_suggestion.accept_key = accept_key;
                        },
                        cx,
                    );
                },
            ),
            cx.subscribe(
                &remote_refresh,
                |panel, _, event: &SelectEvent<Vec<SuggestionSelectOption>>, cx| {
                    let SelectEvent::Confirm(Some(value)) = event else {
                        return;
                    };
                    let remote_refresh = match value.as_str() {
                        "safe" => TerminalCommandSuggestionRemoteRefresh::Safe,
                        "off" => TerminalCommandSuggestionRemoteRefresh::Off,
                        _ => return,
                    };
                    panel.edit_settings(
                        |settings| {
                            let suggestion = &mut settings.terminal.inline_suggestion;
                            suggestion.remote_refresh = remote_refresh;
                            suggestion.remote_probe_enabled =
                                remote_refresh != TerminalCommandSuggestionRemoteRefresh::Off;
                        },
                        cx,
                    );
                },
            ),
        ];

        Self {
            presentation,
            accept_key,
            remote_refresh,
            _subscriptions: subscriptions,
        }
    }

    /// 读回或并发确认设置后同步三个控件，避免显示状态与持久快照分叉。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn sync_from_settings(
        &self,
        settings: &SettingsSnapshot,
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) {
        let suggestion = &settings.terminal.inline_suggestion;
        sync_suggestion_select(
            &self.presentation,
            presentation_option_value(suggestion.presentation).to_owned(),
            window,
            cx,
        );
        sync_suggestion_select(
            &self.accept_key,
            accept_key_option_value(suggestion.accept_key).to_owned(),
            window,
            cx,
        );
        sync_suggestion_select(
            &self.remote_refresh,
            remote_refresh_option_value(suggestion.remote_refresh).to_owned(),
            window,
            cx,
        );
    }
}

/// 把静态协议选项投影成 Select 可复用的数据项。
///
/// @author kongweiguang
fn suggestion_select_options(specs: &[SuggestionSelectOptionSpec]) -> Vec<SuggestionSelectOption> {
    specs
        .iter()
        .map(|spec| SuggestionSelectOption {
            value: spec.value.to_owned(),
            label: crate::locale::text(spec.label_zh, spec.label_en),
        })
        .collect()
}

/// 仅在选中值变化时同步下拉，避免快照读回反复重置焦点和菜单状态。
///
/// @author kongweiguang
fn sync_suggestion_select(
    select: &Entity<SelectState<Vec<SuggestionSelectOption>>>,
    value: String,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    if select
        .read(cx)
        .selected_value()
        .is_some_and(|selected| selected == &value)
    {
        return;
    }
    select.update(cx, |select, cx| {
        select.set_selected_value(&value, window, cx)
    });
}

/// 将呈现模式映射到无文案的设置值，用于快照回读同步。
///
/// @author kongweiguang
fn presentation_option_value(value: TerminalCommandSuggestionPresentation) -> &'static str {
    match value {
        TerminalCommandSuggestionPresentation::Off => "off",
        TerminalCommandSuggestionPresentation::Inline => "inline",
        TerminalCommandSuggestionPresentation::InlineAndMenu => "inlineAndMenu",
    }
}

/// 将接受按键映射到无文案的设置值，用于快照回读同步。
///
/// @author kongweiguang
fn accept_key_option_value(value: TerminalInlineSuggestionAcceptKey) -> &'static str {
    match value {
        TerminalInlineSuggestionAcceptKey::Disabled => "disabled",
        TerminalInlineSuggestionAcceptKey::RightArrow => "rightArrow",
    }
}

/// 将远端刷新策略映射到无文案的设置值，用于快照回读同步。
///
/// @author kongweiguang
fn remote_refresh_option_value(value: TerminalCommandSuggestionRemoteRefresh) -> &'static str {
    match value {
        TerminalCommandSuggestionRemoteRefresh::Off => "off",
        TerminalCommandSuggestionRemoteRefresh::Safe => "safe",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::features::preferences) enum KeywordColorTarget {
    LightForeground,
    LightBackground,
    DarkForeground,
    DarkBackground,
}

impl KeywordColorTarget {
    const ALL: [Self; 4] = [
        Self::LightForeground,
        Self::LightBackground,
        Self::DarkForeground,
        Self::DarkBackground,
    ];

    fn is_foreground(self) -> bool {
        matches!(self, Self::LightForeground | Self::DarkForeground)
    }

    fn is_light(self) -> bool {
        matches!(self, Self::LightForeground | Self::LightBackground)
    }

    fn fallback(self) -> &'static str {
        if self.is_foreground() {
            "#E5E7EB"
        } else {
            "#374151"
        }
    }

    pub(in crate::features::preferences) fn setting_value(
        self,
        colors: Option<&TerminalKeywordHighlightCustomColors>,
    ) -> Option<String> {
        let pair = if self.is_light() {
            colors.map(|colors| &colors.light)
        } else {
            colors.map(|colors| &colors.dark)
        };
        pair.and_then(|pair| {
            if self.is_foreground() {
                pair.foreground.clone()
            } else {
                pair.background.clone()
            }
        })
    }

    fn update(self, colors: &mut TerminalKeywordHighlightCustomColors, value: Option<String>) {
        let pair = if self.is_light() {
            &mut colors.light
        } else {
            &mut colors.dark
        };
        if self.is_foreground() {
            pair.foreground = value;
        } else {
            pair.background = value;
        }
    }

    pub(in crate::features::preferences) fn id_part(self) -> &'static str {
        match self {
            Self::LightForeground => "light-foreground",
            Self::LightBackground => "light-background",
            Self::DarkForeground => "dark-foreground",
            Self::DarkBackground => "dark-background",
        }
    }
}

pub(in crate::features::preferences) struct KeywordColorField {
    pub(in crate::features::preferences) target: KeywordColorTarget,
    pub(in crate::features::preferences) input: Entity<InputState>,
    pub(in crate::features::preferences) picker: Entity<ColorPickerState>,
    pub(in crate::features::preferences) picker_view:
        Entity<super::super::color_picker::ColorPickerView>,
}

#[derive(Clone)]
pub(in crate::features::preferences) struct KeywordEditSession {
    pub(in crate::features::preferences) base: Option<TerminalKeywordHighlightRule>,
    pub(in crate::features::preferences) draft: TerminalKeywordHighlightRule,
    pub(in crate::features::preferences) is_new: bool,
    return_rule_id: Option<String>,
    generation: u64,
    last_submitted: Option<TerminalKeywordHighlightRule>,
}

/// 保存收据必须同时对应规则身份和编辑会话，避免迟到回执覆盖后来选中的草稿。
pub(in crate::features::preferences) struct KeywordPendingSave {
    rule: TerminalKeywordHighlightRule,
    draft: TerminalKeywordHighlightRule,
    generation: u64,
}

impl KeywordPendingSave {
    /// 回执仅属于同一规则和会话代次，不能把其他暂存草稿视为已提交。
    ///
    /// @author kongweiguang
    fn owns_session(&self, session: &KeywordEditSession) -> bool {
        self.generation == session.generation && self.rule.id == session.draft.id
    }
}

pub(in crate::features::preferences) struct KeywordDeleteUndoReceipt {
    pub(in crate::features::preferences) expires_at: Instant,
    pub(in crate::features::preferences) index: usize,
    pub(in crate::features::preferences) rule: TerminalKeywordHighlightRule,
    session: KeywordEditSession,
    generation: u64,
}

/// 将关键词列表、单规则编辑草稿和可撤销删除状态收敛到设置面板的唯一拥有者。
///
/// @author kongweiguang
pub(in crate::features::preferences) struct KeywordEditorState {
    pub(in crate::features::preferences) search_input: Entity<InputState>,
    pub(in crate::features::preferences) pattern_input: Entity<InputState>,
    pub(in crate::features::preferences) note_input: Entity<TextareaState>,
    pub(in crate::features::preferences) colors: Vec<KeywordColorField>,
    pub(in crate::features::preferences) query: String,
    pub(in crate::features::preferences) session: Option<KeywordEditSession>,
    pub(in crate::features::preferences) save_pending: Option<KeywordPendingSave>,
    pub(in crate::features::preferences) error: Option<String>,
    pub(in crate::features::preferences) undo_receipt: Option<KeywordDeleteUndoReceipt>,
    pub(in crate::features::preferences) list_scroll: ScrollHandle,
    pub(in crate::features::preferences) editor_scroll: ScrollHandle,
    pub(in crate::features::preferences) wide_layout: Option<bool>,
    parked_sessions: BTreeMap<String, KeywordEditSession>,
    session_generation: u64,
    _subscriptions: Vec<Subscription>,
    undo_task: Option<Task<()>>,
    undo_generation: u64,
}

impl KeywordEditorState {
    /// 在面板创建时固定编辑器输入与滚动状态，避免重绘重建实体或丢失焦点。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn new(
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索关键词、备注或模式",
                "Search keywords, notes, or match modes",
            ))
        });
        let pattern_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "例如 error、java-test 或 error|exception",
                "For example error, java-test, or error|exception",
            ))
        });
        let note_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "可选，用于说明规则用途",
                    "Optional rule note",
                ))
                .auto_grow(2, 5)
        });

        let mut subscriptions = vec![
            cx.subscribe(&search_input, |panel, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    panel.keyword_editor.query = input.read(cx).value().to_string();
                    cx.notify();
                }
            }),
            cx.subscribe(&pattern_input, |panel, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    panel.update_keyword_editor_pattern(input.read(cx).value().to_string(), cx);
                }
            }),
            cx.subscribe(&note_input, |panel, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    panel.update_keyword_editor_note(input.read(cx).value().to_string(), cx);
                }
            }),
        ];

        let mut colors = Vec::with_capacity(KeywordColorTarget::ALL.len());
        for target in KeywordColorTarget::ALL {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("#RRGGBB"));
            let picker = cx.new(|cx| {
                ColorPickerState::new(window, cx)
                    .default_value(parse_hex_color(target.fallback()).unwrap())
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |panel, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        panel.update_keyword_editor_color_text(
                            target,
                            input.read(cx).value().to_string(),
                            window,
                            cx,
                        );
                    }
                },
            ));
            subscriptions.push(cx.subscribe_in(
                &picker,
                window,
                move |panel, _, event: &ColorPickerEvent, window, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    panel.update_keyword_editor_color(
                        target,
                        color.map(|color| color.to_hex()),
                        window,
                        cx,
                    );
                },
            ));
            let panel = cx.entity().downgrade();
            let on_sample: super::super::color_picker::ScreenColorPickerCallback =
                std::rc::Rc::new(move |picker, window, cx| {
                    let _ = panel.update(cx, |panel, cx| {
                        panel.sample_keyword_screen_color(picker, window, cx)
                    });
                });
            let picker_view = cx.new(|cx| {
                super::super::color_picker::ColorPickerView::new(
                    picker.clone(),
                    Some(on_sample),
                    window,
                    cx,
                )
            });
            colors.push(KeywordColorField {
                target,
                input,
                picker,
                picker_view,
            });
        }

        Self {
            search_input,
            pattern_input,
            note_input,
            colors,
            query: String::new(),
            session: None,
            save_pending: None,
            error: None,
            undo_receipt: None,
            list_scroll: ScrollHandle::default(),
            editor_scroll: ScrollHandle::default(),
            wide_layout: None,
            parked_sessions: BTreeMap::new(),
            session_generation: 0,
            _subscriptions: subscriptions,
            undo_task: None,
            undo_generation: 0,
        }
    }

    /// 每次创建新会话都分配代次，规则 ID 相同也不能复用旧异步回执。
    ///
    /// @author kongweiguang
    fn next_session_generation(&mut self) -> u64 {
        self.session_generation = self.session_generation.wrapping_add(1);
        self.session_generation
    }

    /// 仅保留有修改或待确认的会话，切换规则不会丢失未提交输入。
    ///
    /// @author kongweiguang
    fn park_current_session(&mut self) {
        if let Some(session) = self.session.take()
            && (session.is_new
                || session.base.as_ref() != Some(&session.draft)
                || self.save_pending.as_ref().is_some_and(|pending| {
                    pending.generation == session.generation && pending.rule.id == session.draft.id
                }))
        {
            self.parked_sessions
                .insert(session.draft.id.clone(), session);
        }
    }

    /// 优先恢复同一规则的本地草稿，清洁会话则以最新已保存值建立基线。
    ///
    /// @author kongweiguang
    fn session_for_rule(&mut self, rule: TerminalKeywordHighlightRule) -> KeywordEditSession {
        if let Some(mut session) = self.parked_sessions.remove(&rule.id) {
            if !session.is_new && session.base.as_ref() == Some(&session.draft) {
                session.base = Some(rule.clone());
                session.draft = rule;
            }
            session
        } else {
            let generation = self.next_session_generation();
            existing_session(rule, generation)
        }
    }

    /// 新建草稿可在切回已保存规则后恢复，按钮据此显示真实后续操作。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn has_parked_new(&self) -> bool {
        self.parked_sessions.values().any(|session| session.is_new)
    }

    /// 关闭门禁覆盖所有局部草稿，但不把已提交且尚未收到回执的快照误判为未提交。
    ///
    /// @author kongweiguang
    pub(super) fn has_unsaved_drafts(&self) -> bool {
        self.session
            .iter()
            .chain(self.parked_sessions.values())
            .any(|session| {
                let dirty = session.is_new || session.base.as_ref() != Some(&session.draft);
                let submitted = self.save_pending.as_ref().is_some_and(|pending| {
                    pending.owns_session(session) && pending.draft == session.draft
                });
                dirty && !submitted
            })
    }
}

impl PreferencesPanel {
    /// 关闭只丢弃未提交编辑；已发出的 CAS 保留所属会话，以便迟到回执仍能核对。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn discard_pending_keyword_rule(&mut self) {
        let pending = self.keyword_editor.save_pending.as_ref().map(|pending| {
            (
                pending.rule.id.clone(),
                pending.generation,
                pending.draft.clone(),
            )
        });
        if let Some((id, generation, draft)) = pending {
            if let Some(session) = self.keyword_editor.session.as_mut() {
                if session.generation == generation && session.draft.id == id {
                    session.draft.clone_from(&draft);
                } else {
                    self.keyword_editor.session = None;
                }
            }
            self.keyword_editor.parked_sessions.retain(|_, session| {
                if session.generation == generation && session.draft.id == id {
                    session.draft.clone_from(&draft);
                    true
                } else {
                    false
                }
            });
        } else {
            self.keyword_editor.session = None;
            self.keyword_editor.parked_sessions.clear();
        }
        self.keyword_editor.error = None;
    }

    /// 已保存规则改变时仅同步干净草稿；用户正在编辑的草稿不被后台刷新覆盖。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_keyword_editor(
        &mut self,
        rules: &[TerminalKeywordHighlightRule],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing_id = self
            .keyword_editor
            .session
            .as_ref()
            .filter(|session| !session.is_new)
            .map(|session| session.draft.id.clone());
        let selected = existing_id
            .as_deref()
            .and_then(|id| rules.iter().find(|rule| rule.id == id));

        let mut replacement = None;
        match self.keyword_editor.session.as_mut() {
            Some(session) if session.is_new => return,
            Some(session) if session.base.as_ref() != Some(&session.draft) => return,
            Some(session) => {
                if let Some(rule) = selected {
                    session.base = Some(rule.clone());
                    session.draft = rule.clone();
                } else {
                    replacement = rules.first().cloned();
                }
            }
            None => replacement = rules.first().cloned(),
        }
        if let Some(rule) = replacement {
            self.keyword_editor.session = Some(self.keyword_editor.session_for_rule(rule));
        } else if selected.is_none()
            && self
                .keyword_editor
                .session
                .as_ref()
                .is_some_and(|session| !session.is_new)
        {
            self.keyword_editor.session = None;
        }
        self.sync_keyword_editor_inputs(window, cx);
    }

    /// 从单条规则加载编辑字段；无已选规则时保持输入空白，供新建规则使用。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn sync_keyword_editor_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self
            .keyword_editor
            .session
            .as_ref()
            .map(|session| session.draft.clone());
        let pattern = draft
            .as_ref()
            .map(|rule| rule.pattern.clone())
            .unwrap_or_default();
        let note = draft
            .as_ref()
            .map(|rule| rule.note.clone())
            .unwrap_or_default();
        set_input(&self.keyword_editor.pattern_input, pattern, window, cx);
        set_textarea(&self.keyword_editor.note_input, note, window, cx);
        for color in &self.keyword_editor.colors {
            let value = KeywordColorTarget::setting_value(
                color.target,
                draft.as_ref().and_then(|rule| rule.custom_colors.as_ref()),
            );
            set_input(&color.input, value.clone().unwrap_or_default(), window, cx);
            let picker_value = value
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or_else(|| parse_hex_color(color.target.fallback()).unwrap());
            color
                .picker
                .update(cx, |state, cx| state.set_value(picker_value, window, cx));
        }
    }

    /// 恢复已有草稿不占用新容量；真正创建或保存新规则仍受数量上限保护。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn start_keyword_rule(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.screen_color_sampling.cancel();
        if self.settings_controls_locked() {
            return;
        }
        if self
            .keyword_editor
            .session
            .as_ref()
            .is_some_and(|session| session.is_new)
        {
            return;
        }
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value()) else {
            return;
        };
        if settings.terminal.keyword_highlights.rules.len() >= KEYWORD_RULE_LIMIT
            && !self.keyword_editor.has_parked_new()
        {
            self.keyword_editor.error = Some(
                crate::locale::text(
                    "关键词规则最多保存 64 条。",
                    "At most 64 keyword rules can be saved.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let return_rule_id = self
            .keyword_editor
            .session
            .as_ref()
            .map(|session| session.draft.id.clone())
            .or_else(|| {
                settings
                    .terminal
                    .keyword_highlights
                    .rules
                    .first()
                    .map(|rule| rule.id.clone())
            });
        self.keyword_editor.park_current_session();
        let pending_new = self
            .keyword_editor
            .parked_sessions
            .iter()
            .find_map(|(id, session)| session.is_new.then(|| id.clone()));
        self.keyword_editor.session = if let Some(id) = pending_new {
            self.keyword_editor.parked_sessions.remove(&id)
        } else {
            let generation = self.keyword_editor.next_session_generation();
            Some(KeywordEditSession {
                base: None,
                draft: TerminalKeywordHighlightRule::new("", ""),
                is_new: true,
                return_rule_id,
                generation,
                last_submitted: None,
            })
        };
        self.keyword_editor.error = None;
        self.keyword_editor
            .editor_scroll
            .set_offset(Default::default());
        self.sync_keyword_editor_inputs(window, cx);
        cx.notify();
    }

    /// 切换规则时按稳定 ID 暂存当前草稿，避免点击列表或新建时丢失编辑。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn select_keyword_rule(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.screen_color_sampling.cancel();
        if self
            .keyword_editor
            .session
            .as_ref()
            .is_some_and(|session| session.draft.id == id)
        {
            return;
        }
        let Some(rule) = self.settings.as_ref().and_then(|draft| {
            draft
                .value()
                .terminal
                .keyword_highlights
                .rules
                .iter()
                .find(|rule| rule.id == id)
                .cloned()
        }) else {
            return;
        };
        self.keyword_editor.park_current_session();
        self.keyword_editor.session = Some(self.keyword_editor.session_for_rule(rule));
        self.keyword_editor.error = None;
        self.keyword_editor
            .editor_scroll
            .set_offset(Default::default());
        self.sync_keyword_editor_inputs(window, cx);
        cx.notify();
    }

    /// 输入变更只修改当前草稿，长度按 Unicode 字符计数并在界面上立即反馈。
    fn update_keyword_editor_pattern(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(session) = self.keyword_editor.session.as_mut() {
            session.draft.pattern = truncate_chars(value, KEYWORD_PATTERN_LIMIT);
            self.keyword_editor.error = None;
            cx.notify();
        }
    }

    /// 备注只保存在当前草稿，限制与 application 校验保持一致。
    fn update_keyword_editor_note(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(session) = self.keyword_editor.session.as_mut() {
            session.draft.note = truncate_chars(value, KEYWORD_NOTE_LIMIT);
            self.keyword_editor.error = None;
            cx.notify();
        }
    }

    /// HEX 字段接受编辑中的部分文本；仅合法颜色同步到色盘，其余留在草稿供校验。
    fn update_keyword_editor_color_text(
        &mut self,
        target: KeywordColorTarget,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = value.trim().to_ascii_uppercase();
        let setting_value = (!value.is_empty()).then_some(value.clone());
        self.apply_keyword_editor_color(target, setting_value, cx);
        if let (Some(color), Some(field)) = (
            parse_hex_color(&value),
            self.keyword_editor
                .colors
                .iter()
                .find(|field| field.target == target),
        ) {
            field
                .picker
                .update(cx, |state, cx| state.set_value(color, window, cx));
        }
    }

    /// 色盘选择与 HEX 输入使用同一草稿更新入口，避免主题颜色状态分叉。
    pub(in crate::features::preferences) fn update_keyword_editor_color(
        &mut self,
        target: KeywordColorTarget,
        value: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = value.map(|value| value.to_ascii_uppercase());
        self.apply_keyword_editor_color(target, value.clone(), cx);
        if let Some(field) = self
            .keyword_editor
            .colors
            .iter()
            .find(|field| field.target == target)
        {
            set_input(&field.input, value.unwrap_or_default(), window, cx);
        }
    }

    /// 将单个色值写入两个主题之一，保留另一主题与同主题另一颜色。
    fn apply_keyword_editor_color(
        &mut self,
        target: KeywordColorTarget,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.keyword_editor.session.as_mut() {
            target.update(
                session
                    .draft
                    .custom_colors
                    .get_or_insert_with(Default::default),
                value,
            );
            self.keyword_editor.error = None;
            cx.notify();
        }
    }

    /// 自定义色板切换时复制当前预设的双主题颜色；再次进入时保留用户已有颜色。
    pub(in crate::features::preferences) fn set_keyword_rule_style(
        &mut self,
        style: TerminalKeywordHighlightStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.keyword_editor.session.as_mut() {
            if style == TerminalKeywordHighlightStyle::Custom
                && session.draft.custom_colors.is_none()
            {
                session.draft.custom_colors = Some(preset_custom_colors(session.draft.style));
            }
            session.draft.style = style;
            self.keyword_editor.error = None;
            self.sync_keyword_editor_inputs(window, cx);
            cx.notify();
        }
    }

    /// 匹配方式与大小写开关只改变规则草稿，列表中的现行规则继续保持不变。
    pub(in crate::features::preferences) fn update_keyword_rule_draft(
        &mut self,
        update: impl FnOnce(&mut TerminalKeywordHighlightRule),
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.keyword_editor.session.as_mut() {
            update(&mut session.draft);
            self.keyword_editor.error = None;
            cx.notify();
        }
    }

    /// 保存先执行同一规则校验，并冻结会话代次和原草稿以隔离后续规则切换。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn save_keyword_rule(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        if self.keyword_editor.save_pending.is_some() {
            return;
        }
        let Some(session) = self.keyword_editor.session.as_ref() else {
            return;
        };
        let mut candidate = session.draft.clone();
        let is_new = session.is_new;
        let draft = session.draft.clone();
        let generation = session.generation;
        let base = session.base.clone();
        let last_submitted = session.last_submitted.clone();
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        let rules = &settings.terminal.keyword_highlights.rules;
        let existing = rules.iter().find(|rule| rule.id == candidate.id);
        let retrying = existing.is_some() && existing == last_submitted.as_ref();
        if is_new && existing.is_some() && !retrying {
            self.keyword_editor.error = Some(
                crate::locale::text("规则 ID 冲突。", "The rule ID conflicts with another rule.")
                    .to_string(),
            );
            cx.notify();
            return;
        }
        let error = validate_keyword_rule(&candidate, rules, is_new && !retrying);
        if let Some(error) = error {
            self.keyword_editor.error = Some(error);
            cx.notify();
            return;
        }

        candidate.pattern = candidate.pattern.trim().to_owned();
        candidate.note = candidate.note.trim().to_owned();
        normalize_rule_colors(&mut candidate);
        if !is_new && !retrying && base.as_ref() == Some(&candidate) && existing == Some(&candidate)
        {
            if let Some(session) = self.keyword_editor.session.as_mut() {
                session.draft = candidate;
            }
            self.keyword_editor.error = None;
            self.sync_keyword_editor_inputs(window, cx);
            cx.notify();
            return;
        }
        let mut next = settings.terminal.keyword_highlights.clone();
        if is_new && !retrying {
            next.rules.push(candidate.clone());
        } else if let Some(rule) = next.rules.iter_mut().find(|rule| rule.id == candidate.id) {
            if base.as_ref() != Some(&*rule) && !retrying {
                self.keyword_editor.error = Some(crate::locale::text(
                    "规则已在外部更改，草稿仍保留；请核对后取消或重新编辑。",
                    "This rule changed externally. The draft is retained; review it before cancelling or editing again.",
                ).to_string());
                cx.notify();
                return;
            }
            *rule = candidate.clone();
        } else {
            self.keyword_editor.error = Some(
                crate::locale::text(
                    "规则已从设置中移除，未保存编辑内容。",
                    "This rule was removed from settings; the draft was not saved.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if let Some(session) = self.keyword_editor.session.as_mut() {
            session.last_submitted = Some(candidate.clone());
        }
        self.keyword_editor.save_pending = Some(KeywordPendingSave {
            rule: candidate,
            draft,
            generation,
        });
        self.edit_settings(|settings| settings.terminal.keyword_highlights = next, cx);
        self.keyword_editor.error = None;
        if !self.save_settings(cx) {
            self.fail_keyword_rule_save(cx);
        }
        let _ = window;
        cx.notify();
    }

    /// 回执仅更新同一规则和会话代次，切换后的编辑器及较新的草稿保持原状。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn confirm_keyword_rule_saved(
        &mut self,
        saved: &SettingsSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.keyword_editor.save_pending.as_ref() else {
            return;
        };
        let Some(rule) = saved
            .terminal
            .keyword_highlights
            .rules
            .iter()
            .find(|rule| rule.id == pending.rule.id)
        else {
            return;
        };
        if rule != &pending.rule {
            return;
        }
        let Some(pending) = self.keyword_editor.save_pending.take() else {
            return;
        };
        let active_matches = self.keyword_editor.session.as_ref().is_some_and(|session| {
            session.generation == pending.generation && session.draft.id == rule.id
        });
        if active_matches {
            if let Some(session) = self.keyword_editor.session.as_mut() {
                apply_keyword_save_receipt(session, &pending, rule);
            }
            self.keyword_editor.error = None;
            self.sync_keyword_editor_inputs(window, cx);
        } else if let Some(session) = self.keyword_editor.parked_sessions.get_mut(&rule.id)
            && session.generation == pending.generation
        {
            apply_keyword_save_receipt(session, &pending, rule);
        }
        cx.notify();
    }

    /// 已知 CAS 失败时保留原编辑草稿并解锁保存重试；未知结果继续等待回读核对。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn fail_keyword_rule_save(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.keyword_editor.save_pending.take() {
            self.restore_failed_keyword_projection(&pending.rule, cx);
            if self.keyword_editor.session.as_ref().is_some_and(|session| {
                session.generation == pending.generation && session.draft.id == pending.rule.id
            }) {
                self.keyword_editor.error = Some(
                    crate::locale::text(
                        "规则未保存，编辑内容仍保留；修正后可以重试。",
                        "The rule was not saved. The draft is retained and can be retried.",
                    )
                    .to_string(),
                );
            }
            cx.notify();
        }
    }

    /// 仅回撤仍等于失败提交的规则；保存中直接修正当前投影，不重放写入或覆盖其他编辑。
    ///
    /// @author kongweiguang
    fn restore_failed_keyword_projection(
        &mut self,
        submitted: &TerminalKeywordHighlightRule,
        cx: &mut Context<Self>,
    ) {
        let rollback = self.settings.as_ref().and_then(|settings| {
            let current = settings
                .value()
                .terminal
                .keyword_highlights
                .rules
                .iter()
                .find(|rule| rule.id == submitted.id);
            (current == Some(submitted)).then(|| {
                let confirmed = settings
                    .baseline_value()
                    .terminal
                    .keyword_highlights
                    .rules
                    .iter()
                    .find(|rule| rule.id == submitted.id)
                    .cloned();
                (settings.value().clone(), confirmed, settings.status())
            })
        });
        let Some((mut value, confirmed, status)) = rollback else {
            return;
        };
        let rules = &mut value.terminal.keyword_highlights.rules;
        if let Some(index) = rules.iter().position(|rule| rule.id == submitted.id) {
            if let Some(confirmed) = confirmed {
                rules[index] = confirmed;
            } else {
                rules.remove(index);
            }
        }
        if status == super::super::DraftStatus::Saving {
            if self
                .settings
                .as_mut()
                .is_some_and(|draft| draft.replace_value_during_save(value.clone()).is_ok())
            {
                self.refresh_settings_issues();
                cx.emit(PreferencesEvent::SettingsPreview(value));
                cx.notify();
            }
        } else if !self.settings_controls_locked() {
            self.edit_settings(|settings| *settings = value, cx);
        }
    }

    /// 取消只丢弃当前会话，返回先前规则时恢复它独立保存的草稿。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn cancel_keyword_rule(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.keyword_editor.save_pending.is_some() {
            return;
        }
        self.screen_color_sampling.cancel();
        let failed_submission = self
            .keyword_editor
            .session
            .as_ref()
            .and_then(|session| session.last_submitted.as_ref().cloned());
        if let Some(submitted) = failed_submission {
            self.restore_failed_keyword_projection(&submitted, cx);
        }
        let return_rule_id = self
            .keyword_editor
            .session
            .as_ref()
            .and_then(|session| session.return_rule_id.clone());
        let current_id = self
            .keyword_editor
            .session
            .as_ref()
            .filter(|session| !session.is_new)
            .map(|session| session.draft.id.clone());
        let settings = self.settings.as_ref().map(|draft| draft.value().clone());
        let rule = settings.as_ref().and_then(|settings| {
            let rules = &settings.terminal.keyword_highlights.rules;
            current_id
                .as_deref()
                .or(return_rule_id.as_deref())
                .and_then(|id| rules.iter().find(|rule| rule.id == id))
                .or_else(|| rules.first())
                .cloned()
        });
        self.keyword_editor.session = None;
        self.keyword_editor.session = rule.map(|rule| {
            if current_id.is_some() {
                self.keyword_editor.parked_sessions.remove(&rule.id);
            }
            self.keyword_editor.session_for_rule(rule)
        });
        self.keyword_editor.error = None;
        self.keyword_editor
            .editor_scroll
            .set_offset(Default::default());
        self.sync_keyword_editor_inputs(window, cx);
        cx.notify();
    }

    /// 切换规则启用状态同步已暂存草稿，避免恢复旧草稿时回滚列表操作。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn toggle_keyword_rule(
        &mut self,
        id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() || self.keyword_editor.save_pending.is_some() {
            return;
        }
        self.edit_settings(
            |settings| {
                if let Some(rule) = settings
                    .terminal
                    .keyword_highlights
                    .rules
                    .iter_mut()
                    .find(|rule| rule.id == id)
                {
                    rule.enabled = enabled;
                }
            },
            cx,
        );
        if let Some(session) = self
            .keyword_editor
            .session
            .as_mut()
            .filter(|session| session.draft.id == id)
        {
            session.draft.enabled = enabled;
            if let Some(base) = session.base.as_mut() {
                base.enabled = enabled;
            }
        }
        if let Some(session) = self.keyword_editor.parked_sessions.get_mut(id) {
            session.draft.enabled = enabled;
            if let Some(base) = session.base.as_mut() {
                base.enabled = enabled;
            }
        }
    }

    /// 保存期间禁止重排，避免列表操作和待确认的规则快照相互覆盖。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn move_keyword_rule(
        &mut self,
        id: &str,
        offset: isize,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() || self.keyword_editor.save_pending.is_some() {
            return;
        }
        self.edit_settings(
            |settings| {
                let rules = &mut settings.terminal.keyword_highlights.rules;
                let Some(index) = rules.iter().position(|rule| rule.id == id) else {
                    return;
                };
                let target = index
                    .saturating_add_signed(offset)
                    .min(rules.len().saturating_sub(1));
                if target < rules.len() {
                    let rule = rules.remove(index);
                    rules.insert(target, rule);
                }
            },
            cx,
        );
    }

    /// 拖放按稳定 ID 定位并服从保存门禁，不依赖筛选结果的临时位置。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn move_keyword_rule_before(
        &mut self,
        source_id: &str,
        target_id: &str,
        cx: &mut Context<Self>,
    ) {
        if source_id == target_id
            || self.settings_controls_locked()
            || self.keyword_editor.save_pending.is_some()
        {
            return;
        }
        self.edit_settings(
            |settings| {
                let rules = &mut settings.terminal.keyword_highlights.rules;
                let Some(source_index) = rules.iter().position(|rule| rule.id == source_id) else {
                    return;
                };
                let Some(target_index) = rules.iter().position(|rule| rule.id == target_id) else {
                    return;
                };
                let source = rules.remove(source_index);
                let adjusted_target = if source_index < target_index {
                    target_index - 1
                } else {
                    target_index
                };
                rules.insert(adjusted_target, source);
            },
            cx,
        );
    }

    /// 删除保留原顺序和当前草稿，撤销时两者一起恢复。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn delete_selected_keyword_rule(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() || self.keyword_editor.save_pending.is_some() {
            return;
        }
        let Some(session) = self
            .keyword_editor
            .session
            .as_ref()
            .filter(|session| !session.is_new)
        else {
            return;
        };
        let id = session.draft.id.clone();
        let deleted_session = session.clone();
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        let Some(index) = settings
            .terminal
            .keyword_highlights
            .rules
            .iter()
            .position(|rule| rule.id == id)
        else {
            return;
        };
        let deleted = settings.terminal.keyword_highlights.rules[index].clone();
        let mut rules = settings.terminal.keyword_highlights.rules.clone();
        rules.remove(index);
        self.edit_settings(
            |settings| settings.terminal.keyword_highlights.rules = rules.clone(),
            cx,
        );
        let next = rules.get(index.min(rules.len().saturating_sub(1))).cloned();
        self.keyword_editor.session = next.map(|rule| self.keyword_editor.session_for_rule(rule));
        self.keyword_editor.error = None;
        self.keyword_editor
            .editor_scroll
            .set_offset(Default::default());
        self.sync_keyword_editor_inputs(window, cx);
        self.keyword_editor.undo_generation = self.keyword_editor.undo_generation.wrapping_add(1);
        let generation = self.keyword_editor.undo_generation;
        self.keyword_editor.undo_receipt = Some(KeywordDeleteUndoReceipt {
            expires_at: Instant::now() + KEYWORD_DELETE_UNDO_WINDOW,
            index,
            rule: deleted,
            session: deleted_session,
            generation,
        });
        let timer_generation = generation;
        self.keyword_editor.undo_task = Some(cx.spawn(async move |panel_entity, cx| {
            cx.background_executor()
                .timer(KEYWORD_DELETE_UNDO_WINDOW)
                .await;
            let _ = panel_entity.update(cx, |panel, cx| {
                if panel.keyword_editor.undo_generation == timer_generation {
                    panel.keyword_editor.undo_receipt = None;
                    panel.keyword_editor.undo_task = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// 撤销仅在原规则 ID 尚未出现时恢复，先暂存当前草稿以免覆盖后续编辑。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn undo_keyword_rule_delete(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let Some(receipt) = self.keyword_editor.undo_receipt.take() else {
            return;
        };
        // 计时器可能尚未取得 UI 线程；仍以截止时间与删除代次检查，防止迟到点击复活旧规则。
        if receipt.generation != self.keyword_editor.undo_generation
            || receipt.expires_at <= Instant::now()
        {
            self.keyword_editor.undo_task = None;
            cx.notify();
            return;
        }
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            self.keyword_editor.undo_receipt = Some(receipt);
            return;
        };
        if settings
            .terminal
            .keyword_highlights
            .rules
            .iter()
            .any(|rule| rule.id == receipt.rule.id)
        {
            self.keyword_editor.undo_task = None;
            cx.notify();
            return;
        }
        if settings.terminal.keyword_highlights.rules.len() >= KEYWORD_RULE_LIMIT {
            self.keyword_editor.undo_receipt = Some(receipt);
            self.status = Some(
                crate::locale::text(
                    "规则已达 64 条，无法撤销删除。",
                    "The 64-rule limit prevents restoring this rule.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let mut rules = settings.terminal.keyword_highlights.rules;
        rules.insert(receipt.index.min(rules.len()), receipt.rule.clone());
        self.edit_settings(
            |settings| settings.terminal.keyword_highlights.rules = rules,
            cx,
        );
        self.keyword_editor.park_current_session();
        self.keyword_editor.session = Some(receipt.session);
        self.keyword_editor.undo_generation = self.keyword_editor.undo_generation.wrapping_add(1);
        self.keyword_editor.undo_task = None;
        self.keyword_editor.error = None;
        self.keyword_editor
            .editor_scroll
            .set_offset(Default::default());
        self.sync_keyword_editor_inputs(window, cx);
        cx.notify();
    }
}

/// 在重载时恢复已有规则的清洁编辑基线，规则身份始终由 application 构造器生成。
///
/// @author kongweiguang
fn existing_session(rule: TerminalKeywordHighlightRule, generation: u64) -> KeywordEditSession {
    KeywordEditSession {
        base: Some(rule.clone()),
        draft: rule,
        is_new: false,
        return_rule_id: None,
        generation,
        last_submitted: None,
    }
}

/// 只推进已提交的基线；若会话已产生更新输入，回执不能覆盖这些输入。
///
/// @author kongweiguang
fn apply_keyword_save_receipt(
    session: &mut KeywordEditSession,
    pending: &KeywordPendingSave,
    rule: &TerminalKeywordHighlightRule,
) {
    if session.draft == pending.draft {
        session.draft = rule.clone();
    }
    session.base = Some(rule.clone());
    session.is_new = false;
    session.last_submitted = None;
}

/// 以 Unicode 字符而非 UTF-8 字节截断，和 main 的字符计数约束保持一致。
///
/// @author kongweiguang
fn truncate_chars(value: String, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// 只接受 #RRGGBB，避免把无效或带透明度的值传入终端颜色映射。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn parse_hex_color(value: &str) -> Option<gpui_kit::Hsla> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(rgb(u32::from_str_radix(digits, 16).ok()?).into())
}

/// 把八个 main 预设映射到两套主题的完整前景/背景对。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn preset_custom_colors(
    style: TerminalKeywordHighlightStyle,
) -> TerminalKeywordHighlightCustomColors {
    use TerminalKeywordHighlightStyle as Style;
    let (light_foreground, light_background, dark_foreground, dark_background) = match style {
        Style::Red => ("#991B1B", "#FEE2E2", "#FCA5A5", "#450A0A"),
        Style::Orange => ("#9A3412", "#FFEDD5", "#FDBA74", "#431407"),
        Style::Yellow | Style::Custom => ("#854D0E", "#FEF9C3", "#FDE047", "#422006"),
        Style::Green => ("#166534", "#DCFCE7", "#86EFAC", "#052E16"),
        Style::Cyan => ("#155E75", "#CFFAFE", "#67E8F9", "#083344"),
        Style::Blue => ("#1E40AF", "#DBEAFE", "#93C5FD", "#172554"),
        Style::Purple => ("#6B21A8", "#F3E8FF", "#D8B4FE", "#3B0764"),
        Style::Pink => ("#9D174D", "#FCE7F3", "#F9A8D4", "#500724"),
    };
    TerminalKeywordHighlightCustomColors {
        light: kerminal_application::preferences::TerminalKeywordHighlightColorPair {
            foreground: Some(light_foreground.to_owned()),
            background: Some(light_background.to_owned()),
        },
        dark: kerminal_application::preferences::TerminalKeywordHighlightColorPair {
            foreground: Some(dark_foreground.to_owned()),
            background: Some(dark_background.to_owned()),
        },
    }
}

/// 保存前校验规则上限、文本长度、正则子集与自定义颜色完整性。
///
/// @author kongweiguang
fn validate_keyword_rule(
    rule: &TerminalKeywordHighlightRule,
    stored: &[TerminalKeywordHighlightRule],
    is_new: bool,
) -> Option<String> {
    let pattern = rule.pattern.trim();
    if pattern.is_empty() || pattern.chars().count() > KEYWORD_PATTERN_LIMIT {
        return Some(
            crate::locale::text(
                "关键词不能为空且最多 256 个字符。",
                "A keyword is required and must be at most 256 characters.",
            )
            .to_string(),
        );
    }
    if rule.note.chars().count() > KEYWORD_NOTE_LIMIT {
        return Some(
            crate::locale::text(
                "备注最多 160 个字符。",
                "Notes must be at most 160 characters.",
            )
            .to_string(),
        );
    }
    if is_new && stored.len() >= KEYWORD_RULE_LIMIT {
        return Some(
            crate::locale::text(
                "关键词规则最多保存 64 条。",
                "At most 64 keyword rules can be saved.",
            )
            .to_string(),
        );
    }
    if stored
        .iter()
        .filter(|candidate| candidate.id == rule.id)
        .count()
        > 1
    {
        return Some(
            crate::locale::text("规则 ID 冲突。", "The rule ID conflicts with another rule.")
                .to_string(),
        );
    }
    if rule.match_mode == TerminalKeywordHighlightMatchMode::Regex {
        if ["(?=", "(?!", "(?<=", "(?<!", "\\1", "\\2", "\\k"]
            .iter()
            .any(|unsupported| pattern.contains(unsupported))
        {
            return Some(
                crate::locale::text(
                    "正则不支持查找断言或回溯引用。",
                    "Regular expressions do not support lookaround or backreferences.",
                )
                .to_string(),
            );
        }
        let regex = match regex::RegexBuilder::new(pattern)
            .case_insensitive(!rule.case_sensitive)
            .size_limit(1_000_000)
            .dfa_size_limit(1_000_000)
            .build()
        {
            Ok(regex) => regex,
            Err(_) => {
                return Some(
                    crate::locale::text("正则格式无效。", "The regular expression is invalid.")
                        .to_string(),
                );
            }
        };
        if ["", "a", "0", " ", "中", "ERROR"].iter().any(|probe| {
            regex
                .find_iter(probe)
                .any(|matched| matched.start() == matched.end())
        }) {
            return Some(
                crate::locale::text(
                    "正则不能匹配空文本。",
                    "The regular expression cannot match empty text.",
                )
                .to_string(),
            );
        }
    }
    if rule.style == TerminalKeywordHighlightStyle::Custom {
        let Some(colors) = rule.custom_colors.as_ref() else {
            return Some(
                crate::locale::text(
                    "自定义色板需要配置浅色和深色主题颜色。",
                    "Custom colors are required for both light and dark themes.",
                )
                .to_string(),
            );
        };
        for pair in [&colors.light, &colors.dark] {
            for value in [&pair.foreground, &pair.background].into_iter().flatten() {
                if parse_hex_color(value).is_none() {
                    return Some(
                        crate::locale::text(
                            "颜色格式应为 #RRGGBB。",
                            "Colors must use #RRGGBB format.",
                        )
                        .to_string(),
                    );
                }
            }
        }
        if [&colors.light, &colors.dark]
            .iter()
            .any(|pair| pair.foreground.is_none() && pair.background.is_none())
        {
            return Some(
                crate::locale::text(
                    "自定义色板的浅色和深色主题均需至少设置文字或背景色。",
                    "Each theme needs at least a foreground or background color.",
                )
                .to_string(),
            );
        }
    }
    None
}

/// 入库前统一自定义颜色大小写，避免 Hex 大小写造成无意义的设置差异。
///
/// @author kongweiguang
fn normalize_rule_colors(rule: &mut TerminalKeywordHighlightRule) {
    let Some(colors) = rule.custom_colors.as_mut() else {
        return;
    };
    for value in [
        &mut colors.light.foreground,
        &mut colors.light.background,
        &mut colors.dark.foreground,
        &mut colors.dark.background,
    ]
    .into_iter()
    .flatten()
    {
        *value = value.trim().to_ascii_uppercase();
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_suggestion_select_mapping.rs"]
mod suggestion_select_mapping_tests;
