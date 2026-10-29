//! 本地终端 Profile 列表与编辑表单的原生交互。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, StatefulInteractiveElement, Window,
    assets::IconName,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::{Input, Textarea},
        select::Select,
    },
    div,
    prelude::*,
    px,
};

use crate::theme::ThemeTokens;

use super::{
    ProfileDraft,
    panel::{PreferencesPanel, ProfileGroupOption},
    render::settings_error_banner,
};

impl PreferencesPanel {
    /// Profile 列表选中态采用中性次级表面，避免与保存等主操作争夺视觉层级。
    /// 悬停由 Kit 按钮内部管理，不能在外层重复安装 GPUI hover 样式。
    ///
    /// @author kongweiguang
    pub(super) fn render_profiles(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.ensure_profile_groups_loaded(cx);
        let readonly =
            self.recovery.mode == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery;
        let draft = self
            .profile_draft
            .as_ref()
            .map(|draft| draft.value().clone());
        let saving = self
            .profile_draft
            .as_ref()
            .is_some_and(|draft| draft.status() == super::DraftStatus::Saving);
        let operation_running = self.profile_operation_running;
        let outcome_unknown = self
            .profile_draft
            .as_ref()
            .is_some_and(|draft| draft.status() == super::DraftStatus::OutcomeUnknown);
        let controls_locked =
            saving || operation_running || outcome_unknown || self.loading_profiles;
        let form_locked = readonly || controls_locked || draft.is_none();
        let selected_group_id = draft
            .as_ref()
            .and_then(|profile| profile.sidebar_group_id.as_deref());
        let group_invalid = selected_group_id.is_some_and(|id| {
            self.profile_groups_load_started
                && !self.profile_group_loading
                && !self.profile_groups.iter().any(|group| group.id == id)
        });
        let selected_id = self.selected_profile_id.clone();
        let rows = self.profile_rows.iter().map(|row| {
            let id = row.profile.id.clone();
            let selected = selected_id.as_deref() == Some(id.as_str());
            let label = if row.profile.is_default {
                crate::locale::text(
                    format!("{}  ·  默认", row.profile.name),
                    format!("{}  ·  Default", row.profile.name),
                )
            } else {
                row.profile.name.clone().into()
            };
            let mut button = Button::new(format!("profile-select-{}", row.profile.id))
                .w_full()
                .compact()
                .label(label)
                .disabled(controls_locked)
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.request_profile_selection(Some(id.clone()), window, cx);
                }));
            if selected {
                button = button
                    .secondary()
                    .bg(theme.secondary)
                    .text_color(theme.foreground);
            } else {
                button = button.ghost().text_color(theme.foreground);
            }
            div().w_full().child(button)
        });
        let can_delete = self.selected_profile_id.is_some()
            && self
                .profile_draft
                .as_ref()
                .is_some_and(|draft| draft.revision().is_some())
            && !controls_locked
            && self
                .profile_draft
                .as_ref()
                .is_some_and(|draft| !draft.is_dirty())
            && !readonly;
        let actions = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                Button::new("profile-save")
                    .compact()
                    .primary()
                    .label(if saving {
                        crate::locale::text("保存中…", "Saving…")
                    } else {
                        crate::locale::text("保存", "Save")
                    })
                    .disabled(form_locked || group_invalid || !self.profile_input_errors.is_empty())
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.save_profile(cx))),
            )
            .child(
                Button::new("profile-set-default")
                    .compact()
                    .label(if self.selected_profile_is_default {
                        crate::locale::text("当前默认", "Current default")
                    } else {
                        crate::locale::text("设为默认", "Set as default")
                    })
                    .disabled(
                        readonly
                            || controls_locked
                            || self.selected_profile_id.is_none()
                            || self.selected_profile_is_default,
                    )
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| panel.set_default_profile(cx)),
                    ),
            )
            .child(
                Button::new("profile-delete")
                    .compact()
                    .label(crate::locale::text("删除", "Delete"))
                    .disabled(!can_delete)
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.request_delete_profile(cx)
                        }),
                    ),
            )
            .child(
                Button::new("profile-reload")
                    .compact()
                    .icon(IconName::RefreshCw)
                    .tooltip(crate::locale::text("重新读取 Profile", "Reload profiles"))
                    .accessibility_label(crate::locale::text("重新读取 Profile", "Reload profiles"))
                    .disabled(saving || operation_running || self.loading_profiles)
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| panel.reload_profiles(cx)),
                    ),
            );
        let form = div()
            .id("preferences-profiles")
            .flex_1()
            .min_h(px(0.))
            .flex()
            .overflow_hidden()
            .bg(theme.background)
            .child(
                div()
                    .w(px(180.))
                    .flex_shrink_0()
                    .h_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.sidebar)
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(crate::locale::text("本地 Profile", "Profiles")),
                    )
                    .child(
                        Button::new("profile-create")
                            .w_full()
                            .compact()
                            .primary()
                            .label(crate::locale::text("新建 Profile", "New profile"))
                            .disabled(readonly || controls_locked)
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.request_profile_selection(None, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("preferences-profile-list-scroll")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(rows),
                    )
                    .when(self.loading_profiles, |list| {
                        list.child(
                            div()
                                .w_full()
                                .min_w(px(0.))
                                .px_2()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "正在读取 Profile…",
                                    "Loading profiles…",
                                )),
                        )
                    })
                    .when(
                        !self.loading_profiles && self.profile_rows.is_empty(),
                        |list| {
                            list.child(issue_hint(
                                &crate::locale::text("暂无 Profile", "No profiles yet"),
                                theme,
                            ))
                        },
                    ),
            )
            .child(
                div()
                    .id("preferences-profile-form-scroll")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .overflow_y_scroll()
                    .track_scroll(&self.content_scroll)
                    .child(profile_heading(draft.as_ref(), theme))
                    .when_some(self.status.clone(), |form, status| {
                        form.child(issue_hint(&status, theme))
                    })
                    .child(actions)
                    .child(profile_fields(ProfileFieldSet {
                        name: &self.profile_inputs.name,
                        group: &self.profile_group_select,
                        shell: &self.profile_inputs.shell,
                        working_directory: &self.profile_inputs.working_directory,
                        disabled: form_locked,
                        group_disabled: self.profile_group_loading
                            || self.host_management.is_none(),
                        theme,
                    }))
                    .child(
                        Button::new("profile-advanced-toggle")
                            .compact()
                            .ghost()
                            .icon(if self.profile_advanced_open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .label(crate::locale::text("高级选项", "Advanced options"))
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.profile_advanced_open = !panel.profile_advanced_open;
                                cx.notify();
                            })),
                    )
                    .when(self.profile_advanced_open, |form| {
                        form.child(profile_advanced_fields(
                            &self.profile_inputs.arguments,
                            &self.profile_inputs.environment,
                            form_locked,
                            theme,
                        ))
                    })
                    .when(self.profile_unknown_readback.is_some(), |form| {
                        form.child(
                            div()
                                .w_full()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .when(
                                    self.profile_unknown_readback
                                        .as_ref()
                                        .is_some_and(Option::is_some),
                                    |row| {
                                        row.child(
                                            Button::new("profile-use-readback")
                                                .compact()
                                                .label(crate::locale::text(
                                                    "采用当前版本",
                                                    "Use current version",
                                                ))
                                                .on_click(cx.listener(
                                                    |panel, _: &ClickEvent, window, cx| {
                                                        panel.use_profile_unknown_readback(
                                                            window, cx,
                                                        )
                                                    },
                                                )),
                                        )
                                    },
                                )
                                .child(
                                    Button::new("profile-keep-draft")
                                        .compact()
                                        .label(crate::locale::text(
                                            "保留草稿并继续",
                                            "Keep draft and continue",
                                        ))
                                        .disabled(readonly)
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.keep_profile_unknown_draft(cx)
                                        })),
                                ),
                        )
                    })
                    .children(
                        self.profile_issues
                            .iter()
                            .map(|issue| issue_row(&issue.message, theme)),
                    )
                    .children(
                        self.profile_input_errors
                            .values()
                            .map(|message| issue_row(message, theme)),
                    )
                    .when(self.profile_group_loading, |form| {
                        form.child(issue_hint(
                            &crate::locale::text("正在读取主机分组…", "Loading host groups…"),
                            theme,
                        ))
                    })
                    .when_some(self.profile_group_error.clone(), |form, error| {
                        form.child(issue_row(&error, theme))
                    })
                    .children(settings_error_banner(
                        self.profile_list_error.as_deref(),
                        theme,
                    )),
            );
        let _ = window;
        form
    }
}

/// 用紧凑标题标识当前编辑对象；无选择时保持页面可读而不伪装成已创建项目。
///
/// @author kongweiguang
fn profile_heading(draft: Option<&ProfileDraft>, theme: ThemeTokens) -> impl IntoElement {
    let title = draft
        .map(|profile| profile.name.as_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crate::locale::text("选择或新建 Profile", "Select or create a profile").to_string()
        });
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(17.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .text_color(theme.foreground)
}

/// 聚合本地 Profile 表单控件与禁用状态，避免调用处错配字段或只读约束。
///
/// @author kongweiguang
struct ProfileFieldSet<'a> {
    name: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    group: &'a gpui_kit::Entity<gpui_kit::component::select::SelectState<Vec<ProfileGroupOption>>>,
    shell: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    working_directory: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    disabled: bool,
    group_disabled: bool,
    theme: ThemeTokens,
}

/// 本地 Profile 字段使用 Kit 输入控件；外层观察标记让原生验收通过真实点击填写，
/// 不依赖输入控件的内部路径，也不显示、读取或写入 vault 凭据。
///
/// @author kongweiguang
fn profile_fields(fields: ProfileFieldSet<'_>) -> impl IntoElement {
    let ProfileFieldSet {
        name,
        group,
        shell,
        working_directory,
        disabled,
        group_disabled,
        theme,
    } = fields;
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(profile_field(
            crate::locale::text("名称", "Name"),
            "",
            div()
                .id("profile-name-field")
                .debug_selector(|| "profile-name-field".into())
                .w_full()
                .child(Input::new(name).disabled(disabled).w_full()),
            theme,
        ))
        .child(profile_field(
            crate::locale::text("分组", "Group"),
            "",
            Select::new(group)
                .w_full()
                .h(px(36.))
                .disabled(disabled || group_disabled),
            theme,
        ))
        .child(profile_field(
            crate::locale::text("Shell", "Shell"),
            "",
            div()
                .id("profile-shell-field")
                .debug_selector(|| "profile-shell-field".into())
                .w_full()
                .child(Input::new(shell).disabled(disabled).w_full()),
            theme,
        ))
        .child(profile_field(
            crate::locale::text("工作目录", "Working directory"),
            "",
            Input::new(working_directory).disabled(disabled).w_full(),
            theme,
        ))
}

/// 高级启动参数仅在展开后呈现，仍复用原控件和同一草稿避免切换时丢失输入。
///
/// @author kongweiguang
fn profile_advanced_fields(
    arguments: &gpui_kit::Entity<gpui_kit::component::input::TextareaState>,
    environment: &gpui_kit::Entity<gpui_kit::component::input::TextareaState>,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(profile_field(
            crate::locale::text("启动参数", "Arguments"),
            crate::locale::text(
                "每行一个参数；空格会保留在参数内部。",
                "Enter one argument per line; spaces within an argument are preserved.",
            ),
            Textarea::new(arguments).disabled(disabled).w_full(),
            theme,
        ))
        .child(profile_field(
            crate::locale::text("环境变量", "Environment variables"),
            crate::locale::text(
                "每行一个 KEY=VALUE；值中可以包含等号。",
                "Enter one KEY=VALUE pair per line; values may contain equals signs.",
            ),
            Textarea::new(environment).disabled(disabled).w_full(),
            theme,
        ))
}

/// 每个 Profile 字段共用清晰 label 和约束说明，缩短表单路径并减少误填。
/// Profile 提示与主字段使用统一辅助字号，窄表单仍由布局自然换行。
///
/// @author kongweiguang
fn profile_field(
    label: impl Into<gpui_kit::SharedString>,
    hint: impl Into<gpui_kit::SharedString>,
    input: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    let label = label.into();
    let hint = hint.into();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(13.)).child(label))
        .when(!hint.is_empty(), |field| {
            field.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(hint),
            )
        })
        .child(input)
}

/// Profile 字段错误使用主题 error token，和 settings 表单呈现一致。
fn issue_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(theme.error)
        .child(message.to_owned())
}

/// 分组读取提示使用 muted 主题文字，不伪装成已保存状态或表单错误。
/// @author kongweiguang
fn issue_hint(message: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(message.to_owned())
}
