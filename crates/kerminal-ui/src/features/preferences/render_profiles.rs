//! 本地终端 Profile 列表与编辑表单的原生交互。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, StatefulInteractiveElement, Window,
    assets::IconName,
    component::{
        Disableable, Selectable,
        button::{Button, ButtonVariants},
        input::{Input, Textarea},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::VersionedProfile;

use crate::theme::ThemeTokens;

use super::{ProfileDraft, panel::PreferencesPanel, render::settings_error_banner};

#[path = "render_profiles/review.rs"]
mod review;

impl PreferencesPanel {
    /// 把列表操作、当前项命令和编辑内容分开，保留原表单身份与读写门禁。
    ///
    /// @author kongweiguang
    pub(super) fn render_profiles(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        let controls_locked = saving
            || operation_running
            || outcome_unknown
            || self.loading_profiles
            || self.profile_scan.requires_readback;
        let form_locked = readonly || controls_locked || draft.is_none();
        let selected_id = self.selected_profile_id.clone();
        let rows = self.profile_rows.iter().map(|row| {
            let id = row.profile.id.clone();
            profile_list_item(
                row,
                selected_id.as_deref() == Some(id.as_str()),
                controls_locked,
                theme,
            )
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.request_profile_selection(Some(id.clone()), window, cx);
            }))
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
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .when(self.selected_profile_id.is_some(), |actions| {
                actions
                    .when(!self.selected_profile_is_default, |actions| {
                        actions.child(
                            Button::new("profile-set-default")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("设为默认", "Set as default"))
                                .disabled(readonly || controls_locked)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.set_default_profile(cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("profile-delete")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("删除…", "Delete…"))
                            .disabled(!can_delete)
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.request_delete_profile(cx)
                            })),
                    )
            })
            .child(
                Button::new("profile-reload")
                    .compact()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .loading(self.loading_profiles)
                    .tooltip(crate::locale::text("重新读取 Profile", "Reload profiles"))
                    .accessibility_label(crate::locale::text("重新读取 Profile", "Reload profiles"))
                    .disabled(saving || operation_running || self.loading_profiles)
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| panel.reload_profiles(cx)),
                    ),
            );
        div()
            .id("preferences-profiles")
            .flex_1()
            .min_h(px(0.))
            .flex()
            .overflow_hidden()
            .bg(theme.background)
            .child(
                div()
                    .w(px(208.))
                    .flex_shrink_0()
                    .h_full()
                    .p_3()
                    .pt_6()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.sidebar)
                    .border_r_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .px_2()
                            .h_8()
                            .flex()
                            .items_center()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(crate::locale::text("本地 Profile", "Local profiles")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .mt_2()
                            .pb_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("profile-create")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Plus)
                                    .label(crate::locale::text("新建", "New"))
                                    .accessibility_label(crate::locale::text(
                                        "新建 Profile",
                                        "New profile",
                                    ))
                                    .disabled(readonly || controls_locked)
                                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                        panel.request_profile_selection(None, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("profile-rescan")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Search)
                                    .label(crate::locale::text("重新扫描", "Rescan"))
                                    .tooltip(crate::locale::text(
                                        "扫描已安装的 Shell",
                                        "Scan for installed shells",
                                    ))
                                    .loading(self.rescan_profiles_running())
                                    .disabled(!self.can_rescan_profiles())
                                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.rescan_profiles(cx);
                                    })),
                            ),
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
                            .children(rows)
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
                    .when(!self.profile_scan.conflicts.is_empty(), |list| {
                        let count = self.profile_scan.conflicts.len();
                        list.child(
                            Button::new("profile-scan-review")
                                .w_full()
                                .compact()
                                .label(crate::locale::text(
                                    format!("核对覆盖项（{count}）…"),
                                    format!("Review updates ({count})…"),
                                ))
                                .disabled(!self.can_rescan_profiles())
                                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                    panel.review_profile_scan(theme, window, cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("preferences-profile-form-scroll")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .overflow_y_scroll()
                    .track_scroll(&self.content_scroll)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(profile_heading(
                                draft.as_ref(),
                                self.selected_profile_is_default,
                                theme,
                            ))
                            .when(draft.is_some(), |header| {
                                header.child(
                                    Button::new("profile-save")
                                        .primary()
                                        .label(crate::locale::text("保存", "Save"))
                                        .loading(saving)
                                        .disabled(
                                            form_locked
                                                || !self.profile_input_errors.is_empty()
                                                || self
                                                    .profile_draft
                                                    .as_ref()
                                                    .is_none_or(|draft| !draft.is_dirty()),
                                        )
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.save_profile(cx)
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .pb_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(actions)
                            .when_some(self.status.as_deref(), |toolbar, status| {
                                toolbar.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted)
                                        .child(status.to_owned()),
                                )
                            }),
                    )
                    .when(draft.is_some(), |form| {
                        form.child(profile_fields(ProfileFieldSet {
                            name: &self.profile_inputs.name,
                            shell: &self.profile_inputs.shell,
                            working_directory: &self.profile_inputs.working_directory,
                            disabled: form_locked,
                            theme,
                        }))
                    })
                    .when(draft.is_some(), |form| {
                        form.child(
                            div()
                                .flex()
                                .pt_2()
                                .border_t_1()
                                .border_color(theme.border)
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
                                            panel.profile_advanced_open =
                                                !panel.profile_advanced_open;
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .when(draft.is_some() && self.profile_advanced_open, |form| {
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
                    .children(settings_error_banner(
                        self.profile_list_error.as_deref(),
                        theme,
                    )),
            )
    }
}

/// 列表继续使用 Kit 按钮的选中和焦点语义，自定义内容只负责对齐和完整名称的回退。
/// @author kongweiguang
fn profile_list_item(
    row: &VersionedProfile,
    selected: bool,
    disabled: bool,
    theme: ThemeTokens,
) -> Button {
    let profile = &row.profile;
    let accessible = if profile.is_default {
        crate::locale::text(
            format!("{}，默认", profile.name),
            format!("{}, default", profile.name),
        )
    } else {
        profile.name.clone().into()
    };
    Button::new(format!("profile-select-{}", profile.id))
        .w_full()
        .ghost()
        .selected(selected)
        .disabled(disabled)
        .tooltip(accessible.clone())
        .accessibility_label(accessible)
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    gpui_kit::component::Icon::new(IconName::Terminal)
                        .size_4()
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_ellipsis()
                        .when(selected, |label| {
                            label.font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        })
                        .child(profile.name.clone()),
                )
                .when(profile.is_default, |row| {
                    row.child(profile_default_tag(theme))
                }),
        )
}

/// 默认状态使用中性文字标记，与选中态或保存主操作区分。
/// @author kongweiguang
fn profile_default_tag(theme: ThemeTokens) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_1p5()
        .py_0p5()
        .rounded(theme.radius_sm)
        .bg(theme.secondary)
        .text_color(theme.muted)
        .text_xs()
        .child(crate::locale::text("默认", "Default"))
}

/// 标题区分未选择和未命名的草稿，长名称不挤掉保存按钮。
///
/// @author kongweiguang
fn profile_heading(
    draft: Option<&ProfileDraft>,
    is_default: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let title = match draft {
        Some(profile) if !profile.name.trim().is_empty() => profile.name.clone().into(),
        Some(_) => crate::locale::text("新建 Profile", "New profile"),
        None => crate::locale::text("选择或新建 Profile", "Select or create a profile"),
    };
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .min_w_0()
                .text_ellipsis()
                .text_size(theme.text_title)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title),
        )
        .when(is_default, |header| {
            header.child(profile_default_tag(theme))
        })
        .text_color(theme.foreground)
}

/// 聚合本地 Profile 表单控件与禁用状态，避免调用处错配字段或只读约束。
///
/// @author kongweiguang
struct ProfileFieldSet<'a> {
    name: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    shell: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    working_directory: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    disabled: bool,
    theme: ThemeTokens,
}

/// 本地 Profile 字段使用 Kit 输入控件；外层观察标记让原生验收通过真实点击填写，
/// 不依赖输入控件的内部路径，也不显示、读取或写入 vault 凭据。
///
/// @author kongweiguang
fn profile_fields(fields: ProfileFieldSet<'_>) -> impl IntoElement {
    let ProfileFieldSet {
        name,
        shell,
        working_directory,
        disabled,
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
        .child(div().text_sm().text_color(theme.field_label).child(label))
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
