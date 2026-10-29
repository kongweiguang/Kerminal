//! 紧凑文件栏的名称、权限和删除弹框；操作数据仍由 FilesPane 与 FileService 持有。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, FocusHandle, IntoElement, StatefulInteractiveElement as _,
    Window,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        input::Input,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind};

use crate::{locale, theme::ThemeTokens};

use super::{FilesPane, NameAction};

impl FilesPane {
    /// 文件弹框沿用 main SFTP `max-w-xl` 的 576 逻辑像素上限，由 Root 外框负责表面样式。
    ///
    /// @author kongweiguang
    pub(crate) fn global_dialog_max_width(&self) -> f32 {
        if self.pending_transfer_conflict_count().is_some() {
            672.0
        } else {
            576.0
        }
    }

    /// 只有紧凑栏的新建、重命名和权限操作提升到 Root 弹框；删除继续共用其危险确认。
    ///
    /// @author kongweiguang
    pub(crate) fn pending_global_dialog(&self) -> bool {
        self.pending_transfer_conflict_count().is_some()
            || self.delete_confirmation.is_some()
            || (self.compact_layout
                && (self.name_action.is_some() || self.permission_dialog_target.is_some()))
    }

    /// main 默认先聚焦标题栏关闭按钮；显式持有同一句柄，避免依赖 Button 内部嵌套元素键。
    ///
    /// @author kongweiguang
    pub(crate) fn global_dialog_focus_handle(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        self.global_dialog_close_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// 在同一根遮罩中渲染 FilesPane 所有全局弹框，避免删除与表单使用两套视觉和焦点行为。
    ///
    /// @author kongweiguang
    pub(crate) fn render_global_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.pending_global_dialog() {
            return None;
        }
        let close_focus = self.global_dialog_focus_handle(cx);
        let theme = ThemeTokens::current(window, cx).for_tool_panel();
        if let Some(conflict_count) = self.pending_transfer_conflict_count() {
            return Some(render_transfer_conflict_dialog(
                conflict_count,
                theme,
                &close_focus,
                cx,
            ));
        }
        if let Some(entries) = self.delete_confirmation.clone() {
            return Some(render_delete_dialog(entries, theme, &close_focus, cx));
        }
        if let Some(action) = self.name_action {
            return Some(render_name_dialog(self, action, theme, &close_focus, cx));
        }
        self.permission_dialog_target
            .clone()
            .map(|(side, endpoint)| {
                render_permission_dialog(self, side, &endpoint, theme, &close_focus, cx)
            })
    }

    /// 关闭弹框只撤销尚未提交的表单或删除确认；已提交的文件请求继续由应用层完成。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_global_dialog(&mut self, cx: &mut Context<Self>) {
        if self.pending_transfer_conflict_count().is_some() {
            self.cancel_transfer_conflict(cx);
            return;
        }
        if self.name_action_busy || self.permissions.busy {
            return;
        }
        if self.delete_confirmation.is_some() {
            self.cancel_delete_confirmation(cx);
            return;
        }
        if self.name_action.take().is_some() {
            self.name_action_side = None;
            self.name_action_target = None;
            self.status = None;
            cx.notify();
            return;
        }
        if self.permission_dialog_target.take().is_some() {
            self.status = None;
            cx.notify();
        }
    }

    /// 确认入口复用原文件操作校验；权限表单在选择目标变化后拒绝提交旧草稿。
    ///
    /// @author kongweiguang
    pub(crate) fn confirm_global_dialog(&mut self, cx: &mut Context<Self>) {
        if self.name_action_busy || self.permissions.busy {
            return;
        }
        if self.delete_confirmation.is_some() {
            self.confirm_delete_confirmation(cx);
        } else if self.name_action.is_some() {
            self.commit_name_action(cx);
        } else if let Some((side, endpoint)) = self.permission_dialog_target.clone() {
            let selected = self
                .panels
                .get(side)
                .and_then(|panel| panel.selected.as_ref());
            if selected.is_some_and(|entry| entry.endpoint == endpoint) {
                self.active_side = side;
                self.apply_permissions(cx);
            } else {
                self.permission_dialog_target = None;
                self.status = Some(
                    locale::text(
                        "所选文件已变化，请重新打开权限编辑。",
                        "The selected item changed. Reopen its permissions to continue.",
                    )
                    .to_string(),
                );
                cx.notify();
            }
        }
    }

    /// 权限操作从文件行右键菜单进入，并冻结行 endpoint 以匹配主分支的单项 chmod 对话框。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_permissions(
        &mut self,
        side: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .panels
            .get(side)
            .and_then(|panel| panel.selected.clone())
        else {
            self.status = Some(
                locale::text("请先选择文件或目录。", "Select a file or folder first.").to_string(),
            );
            cx.notify();
            return;
        };
        if !self.permissions.can_show_editor(Some(&entry)) {
            self.status = Some(self.permissions.explanation(Some(&entry)));
            cx.notify();
            return;
        }
        self.permissions.ensure_input(window, cx);
        self.permissions.sync_selection(Some(&entry), window, cx);
        self.active_side = side;
        self.name_action = None;
        self.name_action_side = None;
        self.name_action_target = None;
        self.name_action_busy = false;
        self.delete_confirmation = None;
        self.permission_dialog_target = self.compact_layout.then_some((side, entry.endpoint));
        self.status = None;
        cx.notify();
    }
}

/// 按 main 的三张策略卡片即时提交已冻结批次，不增加额外确认步骤。
///
/// @author kongweiguang
fn render_transfer_conflict_dialog(
    conflict_count: usize,
    theme: ThemeTokens,
    close_focus: &FocusHandle,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let options = [
        (
            "files-transfer-conflict-overwrite",
            ConflictPolicy::Overwrite,
            locale::text("覆盖目标", "Overwrite target"),
            locale::text("替换目标文件。", "Replace the destination file."),
        ),
        (
            "files-transfer-conflict-rename",
            ConflictPolicy::Rename,
            locale::text("自动重命名", "Rename automatically"),
            locale::text("生成新文件名。", "Generate a new file name."),
        ),
        (
            "files-transfer-conflict-skip",
            ConflictPolicy::Skip,
            locale::text("跳过冲突", "Skip conflicts"),
            locale::text("跳过已存在项目。", "Skip existing items."),
        ),
    ];
    let cancel_pane = cx.entity().clone();
    dialog_surface(theme)
        .id("files-transfer-conflict-dialog")
        .child(dialog_header(
            locale::text("处理传输冲突", "Resolve transfer conflicts"),
            locale::text(
                "请选择冲突处理方式。",
                "Choose how to handle the conflicts.",
            ),
            dialog_close_button(false, close_focus, cx),
            theme,
        ))
        .child(
            dialog_body()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.warning.opacity(0.24))
                        .bg(theme.warning.opacity(0.12))
                        .px_3()
                        .py_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(theme.text_sm)
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme.warning)
                                .child(gpui_kit::assets::IconName::CircleAlert)
                                .child(locale::text(
                                    format!("检测到 {conflict_count} 个目标冲突"),
                                    format!("Detected {conflict_count} target conflicts"),
                                )),
                        )
                        .child(
                            div()
                                .pl_6()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(locale::text(
                                    "策略会应用到本次队列的所有冲突。",
                                    "The selected policy applies to every conflict in this batch.",
                                )),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(options.into_iter().map(|(id, policy, label, description)| {
                            let action_pane = cx.entity().clone();
                            Button::new(id)
                                .small()
                                .outline()
                                .rounded(theme.radius_sm)
                                .w_full()
                                .h(px(68.))
                                .justify_start()
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .items_start()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(theme.text_sm)
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .text_color(theme.foreground)
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .text_size(theme.text_xs)
                                                .text_color(theme.muted)
                                                .child(description),
                                        ),
                                )
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                    action_pane.update(cx, |pane, cx| {
                                        pane.resolve_transfer_conflict(policy, cx);
                                    });
                                }))
                        })),
                ),
        )
        .child(
            div()
                .w_full()
                .flex_shrink_0()
                .flex()
                .justify_end()
                .border_t_1()
                .border_color(theme.border)
                .px_5()
                .py_4()
                .child(
                    dialog_button_label(
                        Button::new("files-transfer-conflict-cancel")
                            .compact()
                            .ghost(),
                        locale::text("取消", "Cancel"),
                    )
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cancel_pane.update(cx, |pane, cx| {
                            pane.cancel_transfer_conflict(cx);
                        });
                    })),
                ),
        )
        .into_any_element()
}

/// 新建与重命名共用受限名称输入，但标题和已冻结目标明确区分两种动作。
///
/// @author kongweiguang
fn render_name_dialog(
    pane: &mut FilesPane,
    action: NameAction,
    theme: ThemeTokens,
    close_focus: &FocusHandle,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let target = pane.name_action_target.as_ref();
    let busy = pane.name_action_busy;
    let is_rename = matches!(action, NameAction::Rename);
    let rename_directory = target
        .and_then(super::parent_endpoint)
        .map(|target| endpoint_path_label(&target))
        .unwrap_or_default();
    let original_name = target.and_then(super::endpoint_name).unwrap_or_default();
    let (title, detail, field_label, confirm_label) = match action {
        NameAction::CreateDirectory => (
            locale::text("新建目录", "New folder"),
            target.map(endpoint_path_label).unwrap_or_default(),
            locale::text("新目录路径", "New directory path"),
            locale::text("创建", "Create"),
        ),
        NameAction::Rename => (
            locale::text("重命名", "Rename"),
            target.map(endpoint_path_label).unwrap_or_default(),
            locale::text("新名称", "New name"),
            locale::text("重命名", "Rename"),
        ),
    };
    let input = pane
        .name_input
        .as_ref()
        .map(|input| dialog_text_input(input, theme, cx.theme().font_family.clone()));
    let has_input = input.is_some();
    let error = (!busy).then(|| pane.status.clone()).flatten();
    let cancel_pane = cx.entity().clone();
    let confirm_pane = cx.entity().clone();
    dialog_surface(theme)
        .id("files-global-name-dialog")
        .child(dialog_header(
            title,
            detail,
            dialog_close_button(busy, close_focus, cx),
            theme,
        ))
        .child(
            dialog_body()
                .when(is_rename, |body| {
                    body.child(dialog_readonly_field(
                        "files-global-dialog-parent",
                        locale::text("所在目录", "Parent directory"),
                        rename_directory,
                        theme,
                    ))
                    .child(dialog_readonly_field(
                        "files-global-dialog-original-name",
                        locale::text("原名称", "Current name"),
                        original_name,
                        theme,
                    ))
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(dialog_field_label(field_label, theme))
                        .when_some(input, |field, input| field.child(input)),
                )
                .children(error.map(|error| dialog_error(error, theme))),
        )
        .child(dialog_footer(
            dialog_button_label(
                Button::new("files-global-dialog-cancel").compact().ghost(),
                locale::text("取消", "Cancel"),
            )
            .disabled(busy)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cancel_pane.update(cx, |pane, cx| pane.cancel_global_dialog(cx));
            })),
            dialog_button_label(
                Button::new("files-global-dialog-confirm")
                    .compact()
                    .primary()
                    .icon(gpui_kit::assets::IconName::Check),
                if busy {
                    locale::text("处理中…", "Working…")
                } else {
                    confirm_label
                },
            )
            .disabled(busy || !has_input)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                confirm_pane.update(cx, |pane, cx| pane.confirm_global_dialog(cx));
            })),
            theme,
        ))
        .into_any_element()
}

/// POSIX mode 弹框展示路径和真实权限，不把 Windows ACL 或缺失 metadata 伪装成八进制值。
///
/// @author kongweiguang
fn render_permission_dialog(
    pane: &mut FilesPane,
    side: usize,
    endpoint: &FileEndpoint,
    theme: ThemeTokens,
    close_focus: &FocusHandle,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let selected = pane
        .panels
        .get(side)
        .and_then(|panel| panel.selected.as_ref());
    let mode = selected
        .filter(|entry| entry.endpoint == *endpoint)
        .and_then(|entry| entry.permissions)
        .map(super::permissions::format_octal_mode)
        .unwrap_or_default();
    let busy = pane.permissions.busy;
    let input = pane
        .permissions
        .input
        .as_ref()
        .map(|input| dialog_text_input(input, theme, cx.theme().font_family.clone()));
    let has_input = input.is_some();
    let error = (!busy).then(|| pane.status.clone()).flatten();
    let cancel_pane = cx.entity().clone();
    let confirm_pane = cx.entity().clone();
    dialog_surface(theme)
        .id("files-global-permissions-dialog")
        .child(dialog_header(
            locale::text("修改权限", "Change permissions"),
            endpoint_path_label(endpoint),
            dialog_close_button(busy, close_focus, cx),
            theme,
        ))
        .child(
            dialog_body()
                .child(dialog_readonly_field(
                    "files-global-dialog-path",
                    locale::text("文件路径", "File path"),
                    endpoint_path_label(endpoint),
                    theme,
                ))
                .child(dialog_readonly_field(
                    "files-global-dialog-mode",
                    locale::text("当前权限", "Current mode"),
                    if mode.is_empty() {
                        locale::text("不可用", "Unavailable").to_string()
                    } else {
                        mode
                    },
                    theme,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(dialog_field_label(
                            locale::text("权限模式", "Permission mode"),
                            theme,
                        ))
                        .when_some(input, |field, input| field.child(input)),
                )
                .children(error.map(|error| dialog_error(error, theme))),
        )
        .child(dialog_footer(
            dialog_button_label(
                Button::new("files-global-dialog-cancel").compact().ghost(),
                locale::text("取消", "Cancel"),
            )
            .disabled(busy)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cancel_pane.update(cx, |pane, cx| pane.cancel_global_dialog(cx));
            })),
            dialog_button_label(
                Button::new("files-global-dialog-confirm")
                    .compact()
                    .primary()
                    .icon(gpui_kit::assets::IconName::Check),
                if busy {
                    locale::text("处理中…", "Working…")
                } else {
                    locale::text("保存权限", "Save permissions")
                },
            )
            .disabled(busy || !has_input)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                confirm_pane.update(cx, |pane, cx| pane.confirm_global_dialog(cx));
            })),
            theme,
        ))
        .into_any_element()
}

/// 删除弹框冻结目标类型，批量预览最多显示八项并说明目录递归删除范围。
///
/// @author kongweiguang
fn render_delete_dialog(
    entries: Vec<FileEntry>,
    theme: ThemeTokens,
    close_focus: &FocusHandle,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let total = entries.len();
    let directory_count = entries
        .iter()
        .filter(|entry| entry.kind == FileEntryKind::Directory)
        .count();
    let summary = if total == 1 {
        let entry = &entries[0];
        let kind = if entry.kind == FileEntryKind::Directory {
            locale::text("目录", "folder")
        } else {
            locale::text("文件", "file")
        };
        format!(
            "{} {}：{}",
            locale::text("将删除", "Delete"),
            kind,
            endpoint_path_label(&entry.endpoint)
        )
    } else {
        locale::text(
            format!("将删除 {total} 项，其中 {directory_count} 个目录。"),
            format!("Delete {total} items, including {directory_count} folders."),
        )
        .to_string()
    };
    let title = if total > 1 {
        locale::text(format!("删除 {total} 项"), format!("Delete {total} items"))
    } else {
        locale::text("删除", "Delete")
    };
    let description = delete_dialog_description(&entries);
    let labels = entries
        .iter()
        .take(8)
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    let hidden_count = total.saturating_sub(labels.len());
    let cancel_pane = cx.entity().clone();
    let confirm_pane = cx.entity().clone();
    dialog_surface(theme)
        .id("files-global-delete-dialog")
        .child(dialog_header(
            title,
            description,
            dialog_close_button(false, close_focus, cx),
            theme,
        ))
        .child(
            dialog_body()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(summary),
                )
                .when(total > 1, |body| {
                    body.child(
                        div()
                            .id("files-global-delete-targets")
                            .max_h(px(160.))
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(labels.into_iter().map(|label| {
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(theme.secondary)
                                    .text_size(theme.text_xs)
                                    .child(label)
                            }))
                            .when(hidden_count > 0, |list| {
                                list.child(
                                    div()
                                        .px_3()
                                        .py_1()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(locale::text(
                                            format!("还有 {hidden_count} 项未显示。"),
                                            format!("{} more items are not shown.", hidden_count),
                                        )),
                                )
                            }),
                    )
                })
                .when(directory_count > 0, |body| {
                    body.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(locale::text(
                        "目录会递归删除，包含其中所有文件和子目录。",
                        "Folders are deleted recursively, including all files and subfolders.",
                    )),
                    )
                }),
        )
        .child(dialog_footer(
            dialog_button_label(
                Button::new("files-global-dialog-cancel").compact().ghost(),
                locale::text("取消", "Cancel"),
            )
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cancel_pane.update(cx, |pane, cx| pane.cancel_global_dialog(cx));
            })),
            dialog_button_label(
                Button::new("files-global-dialog-confirm")
                    .compact()
                    .danger()
                    .icon(gpui_kit::assets::IconName::Check),
                if total > 1 {
                    locale::text(
                        format!("确认删除 {total} 项"),
                        format!("Confirm delete {total} items"),
                    )
                } else {
                    locale::text("确认删除", "Confirm delete")
                },
            )
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                confirm_pane.update(cx, |pane, cx| pane.confirm_global_dialog(cx));
            })),
            theme,
        ))
        .into_any_element()
}

/// 同一表面和间距用于所有全局动作，让 Root 遮罩只负责窗口级交互边界。
///
/// @author kongweiguang
fn dialog_surface(theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .text_color(theme.foreground)
}

/// 标题、路径和关闭动作复用 main ModalShell 的 header 结构；Root 持有表面与遮罩。
///
/// @author kongweiguang
fn dialog_header(
    title: impl IntoElement,
    description: impl IntoElement,
    close: Button,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_start()
        .justify_between()
        .gap_4()
        .border_b_1()
        .border_color(theme.border)
        .px_5()
        .py_4()
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(17.))
                        .line_height(px(24.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .mt_1()
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .text_color(theme.muted)
                        .child(description),
                ),
        )
        .child(close)
}

/// 关闭按钮复用已有取消路径；表单提交占用输入时禁用，避免出现可见但无效的关闭动作。
///
/// @author kongweiguang
fn dialog_close_button(
    busy: bool,
    close_focus: &FocusHandle,
    cx: &mut Context<FilesPane>,
) -> Button {
    let label = locale::text("关闭弹框", "Close dialog");
    Button::new("files-global-dialog-close")
        .focus_handle(close_focus)
        .w(px(36.))
        .h(px(36.))
        .flex_shrink_0()
        .ghost()
        .icon(gpui_kit::assets::IconName::X)
        .accessibility_label(label.clone())
        .tooltip(label)
        .disabled(busy)
        .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
            pane.cancel_global_dialog(cx);
        }))
}

/// Button 会在内容行内应用 compact 字体；弹框动作必须在可见文字节点显式指定 main 字号。
///
/// @author kongweiguang
fn dialog_button_label(button: Button, label: impl Into<gpui_kit::SharedString>) -> Button {
    let label = label.into();
    button.accessibility_label(label.clone()).child(
        div()
            .text_size(px(16.))
            .line_height(px(24.))
            .font_weight(gpui_kit::FontWeight::NORMAL)
            .child(label),
    )
}

/// 正文使用 ModalShell 的内容区内边距，字段分组各自处理标签间距。
///
/// @author kongweiguang
fn dialog_body() -> gpui_kit::Div {
    div()
        .w_full()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap_3()
        .px_5()
        .py_4()
}

/// 保留输入实体与焦点，并使用系统字体及文件工具栏字段令牌，避免继承默认输入色。
///
/// @author kongweiguang
fn dialog_text_input(
    input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
    theme: ThemeTokens,
    system_font_family: gpui_kit::SharedString,
) -> impl IntoElement {
    let input = gpui_kit::Styled::h(Input::new(input).w_full(), px(36.));
    let input = gpui_kit::Styled::px(input, px(12.));
    let input = gpui_kit::Styled::text_size(input, px(16.));
    let input = gpui_kit::Styled::line_height(input, px(24.));
    let input = gpui_kit::Styled::font_weight(input, gpui_kit::FontWeight::NORMAL);
    let input = gpui_kit::Styled::font_family(input, system_font_family);
    let input = gpui_kit::Styled::bg(input, theme.field_surface());
    let input = gpui_kit::Styled::border_color(input, theme.strong_border());
    gpui_kit::Styled::text_color(input, theme.foreground)
}

/// 表单标签始终与真实输入绑定，并给八进制草稿保留等宽字形。
///
/// @author kongweiguang
fn dialog_field_label(label: impl IntoElement, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .line_height(px(16.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme.muted)
        .child(label)
}

/// 只读路径用次级表面表达上下文，不允许确认时把显示路径误当成可编辑目标。
///
/// @author kongweiguang
fn dialog_readonly_field(
    id: &'static str,
    label: impl IntoElement,
    value: String,
    theme: ThemeTokens,
) -> impl IntoElement {
    let tooltip_value = value.clone();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(dialog_field_label(label, theme))
        .child(
            div()
                .id(id)
                .w_full()
                .min_w(px(0.))
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary)
                .text_size(px(12.))
                .line_height(px(16.))
                .font_family("monospace")
                .overflow_hidden()
                .text_ellipsis()
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tooltip_value.clone())
                        .build(window, cx)
                })
                .child(value),
        )
}

/// 错误采用 main 状态提示的语义表面并保留在表单中，用户可直接修正后重试。
///
/// @author kongweiguang
fn dialog_error(error: String, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(theme.error.opacity(0.2))
        .bg(theme.error.opacity(0.1))
        .px_3()
        .py_2()
        .text_size(theme.text_sm)
        .text_color(theme.error)
        .child(error)
}

/// 删除说明按目标端点标明受影响的文件系统，与远程和本机主流程分别对应。
///
/// @author kongweiguang
fn delete_dialog_description(entries: &[FileEntry]) -> String {
    let (zh_scope, en_scope) = entries
        .first()
        .map(|entry| match &entry.endpoint {
            FileEndpoint::Local { .. } => ("本机文件系统", "the local file system"),
            FileEndpoint::Remote { .. } => ("远程文件系统", "the remote file system"),
            FileEndpoint::Container { .. } => ("容器文件系统", "the container file system"),
        })
        .unwrap_or(("文件系统", "the file system"));
    if entries.len() > 1 {
        locale::text(
            format!("此操作会直接修改{zh_scope}：{} 项。", entries.len()),
            format!(
                "This action directly modifies {en_scope}: {} items.",
                entries.len()
            ),
        )
        .to_string()
    } else {
        locale::text(
            format!("此操作会直接修改{zh_scope}。"),
            format!("This action directly modifies {en_scope}."),
        )
        .to_string()
    }
}

/// 按钮顺序固定为取消后确认，危险动作仅改变确认按钮语义色。
///
/// @author kongweiguang
fn dialog_footer(cancel: Button, confirm: Button, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex_shrink_0()
        .flex()
        .justify_end()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .px_5()
        .py_4()
        .child(cancel)
        .child(confirm)
}

/// 文件对话框和 main 一样显示纯路径；执行目标仍由冻结的 endpoint 持有。
///
/// @author kongweiguang
fn endpoint_path_label(endpoint: &FileEndpoint) -> String {
    super::render::browser::endpoint_path(endpoint)
}
