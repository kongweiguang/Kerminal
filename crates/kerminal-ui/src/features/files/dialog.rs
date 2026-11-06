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
    px, relative,
};
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind};

use crate::{locale, theme::ThemeTokens};

use super::{FileTargetNameState, FilesPane, NameAction, TransferConflictKind};

/// 展示值使用可读名称，完整身份留给提示；两者都来自同一个已冻结端点。
struct TransferConflictPreview {
    source: (String, String),
    destination: (String, String),
    kind: TransferConflictKind,
}

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

    /// 只有紧凑栏的新建/重命名提升到 Root 弹框；权限编辑在所有布局统一走显式弹框。
    ///
    /// @author kongweiguang
    pub(crate) fn pending_global_dialog(&self) -> bool {
        self.pending_transfer_conflict_count().is_some()
            || self.delete_confirmation.is_some()
            || self.permission_dialog_target.is_some()
            || (self.compact_layout && self.name_action.is_some())
    }

    /// main 默认先聚焦标题栏关闭按钮；显式持有同一句柄，避免依赖 Button 内部嵌套元素键。
    ///
    /// @author kongweiguang
    pub(crate) fn global_dialog_focus_handle(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        self.global_dialog_close_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// 在同一根遮罩中渲染文件弹框；冲突预览仅取预检确认的请求，不改变冻结计划。
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
        if let Some(pending) = self.pending_transfer_conflict.as_ref() {
            let conflict_count = pending.conflicts.len();
            let previews = pending
                .conflict_items()
                .take(3)
                .map(|(request, kind)| TransferConflictPreview {
                    source: conflict_endpoint_preview(&request.source, &self.target_names),
                    destination: conflict_endpoint_preview(
                        &request.destination,
                        &self.target_names,
                    ),
                    kind,
                })
                .collect();
            return Some(render_transfer_conflict_dialog(
                conflict_count,
                previews,
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
    /// 不替换尚未收尾的名称或权限表单，使旧回执不会清除新操作的忙碌状态。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_permissions(
        &mut self,
        side: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.name_action_busy || self.permissions.busy {
            return;
        }
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
        self.permission_dialog_target = Some((side, entry.endpoint));
        self.status = None;
        cx.notify();
    }
}

/// 冲突策略立即应用于冻结批次；预览限定高度，防止多项冲突挤出小窗口的决策入口。
/// listener 已借用 FilesPane，回调直接使用该借用，不能再次通过 Entity::update 重入。
///
/// @author kongweiguang
fn render_transfer_conflict_dialog(
    conflict_count: usize,
    previews: Vec<TransferConflictPreview>,
    theme: ThemeTokens,
    close_focus: &FocusHandle,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let options = [
        (
            "files-transfer-conflict-overwrite",
            ConflictPolicy::Overwrite,
            locale::text("覆盖目标", "Overwrite target"),
            locale::text("替换目标项目。", "Replace the destination item."),
        ),
        (
            "files-transfer-conflict-rename",
            ConflictPolicy::Rename,
            locale::text("自动重命名", "Rename automatically"),
            locale::text("生成新名称。", "Generate a new name."),
        ),
        (
            "files-transfer-conflict-skip",
            ConflictPolicy::Skip,
            locale::text("跳过冲突", "Skip conflicts"),
            locale::text("跳过冲突项目。", "Skip conflicting items."),
        ),
    ];
    dialog_surface(theme)
        .id("files-transfer-conflict-dialog")
        .child(dialog_header(
            locale::text("处理传输冲突", "Resolve transfer conflicts"),
            locale::text(
                "请选择冲突处理方式。",
                "Choose how to handle the conflicts.",
            ),
            dialog_close_button(false, close_focus, theme, cx),
            theme,
        ))
        .child(
            dialog_body(theme)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
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
                .child(render_transfer_conflict_previews(
                    conflict_count,
                    previews,
                    theme,
                ))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(options.into_iter().map(|(id, policy, label, description)| {
                            Button::new(id)
                                .small()
                                .outline()
                                .when(policy == ConflictPolicy::Overwrite, Button::danger)
                                .rounded(theme.radius_sm)
                                .w_full()
                                .h(theme.metrics.control_height * 1.5)
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
                                                .text_color(
                                                    if policy == ConflictPolicy::Overwrite {
                                                        theme.error
                                                    } else {
                                                        theme.foreground
                                                    },
                                                )
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .text_size(theme.text_xs)
                                                .text_color(theme.muted)
                                                .child(description),
                                        ),
                                )
                                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                                    pane.resolve_transfer_conflict(policy, cx);
                                }))
                        })),
                ),
        )
        .child(
            crate::ui_surface::dialog_actions(theme)
                .w_full()
                .border_t_1()
                .border_color(theme.border)
                .px(theme.metrics.dialog_padding)
                .py(theme.metrics.section_gap)
                .child(
                    dialog_button_label(
                        Button::new("files-transfer-conflict-cancel")
                            .compact()
                            .ghost(),
                        locale::text("取消", "Cancel"),
                    )
                    .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                        pane.cancel_transfer_conflict(cx);
                    })),
                ),
        )
        .into_any_element()
}

/// 文件名和短目标名优先用于识别，完整路径与身份留在提示中，避免长父目录挤走实际对象。
///
/// @author kongweiguang
fn conflict_endpoint_preview(
    endpoint: &FileEndpoint,
    target_names: &FileTargetNameState,
) -> (String, String) {
    (
        format!(
            "{} · {}",
            super::endpoint_name(endpoint).unwrap_or_else(|| endpoint_path_label(endpoint)),
            target_names.display(endpoint).compact
        ),
        target_names.endpoint_detail(endpoint),
    )
}

/// 来源和目标各占一行以保留两端对象；预览随密度缩放并滚动，完整身份在受视口约束的提示中换行。
///
/// @author kongweiguang
fn render_transfer_conflict_previews(
    conflict_count: usize,
    previews: Vec<TransferConflictPreview>,
    theme: ThemeTokens,
) -> impl IntoElement {
    let hidden_count = conflict_count.saturating_sub(previews.len());
    div()
        .id("files-transfer-conflict-previews")
        .w_full()
        .max_h(theme.metrics.control_height * 2.)
        .min_h(px(0.))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_1()
        .children(previews.into_iter().enumerate().map(|(index, preview)| {
            let paths = [
                ("source", locale::text("来源", "From"), preview.source),
                (
                    "destination",
                    locale::text("目标", "To"),
                    preview.destination,
                ),
            ];
            div()
                .w_full()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.secondary)
                .text_size(theme.text_xs)
                .line_height(relative(1.5))
                .children(paths.into_iter().map(|(part, label, (value, full))| {
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(div().flex_shrink_0().text_color(theme.muted).child(label))
                        .child(
                            div()
                                .id(format!("files-transfer-conflict-{index}-{part}"))
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .tooltip(move |window, cx| {
                                    let width = px((window.viewport_size().width.as_f32() - 64.)
                                        .clamp(160., 520.));
                                    let detail = full.clone();
                                    gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
                                        div().w(width).whitespace_normal().child(detail.clone())
                                    })
                                    .max_w(width + px(16.))
                                    .build(window, cx)
                                })
                                .child(value),
                        )
                }))
                .when(preview.kind == TransferConflictKind::Repeated, |item| {
                    item.child(div().text_color(theme.warning).child(locale::text(
                        "本批次中目标重名",
                        "Duplicate destination in this batch",
                    )))
                })
        }))
        .when(hidden_count > 0, |list| {
            list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(locale::text(
                        format!("还有 {hidden_count} 个冲突未显示。"),
                        format!("{hidden_count} more conflicts are not shown."),
                    )),
            )
        })
}

/// 新建与重命名共用受限名称输入，但标题和已冻结目标明确区分两种动作。
/// 按钮沿用 listener 提供的唯一实体借用，保持鼠标提交与输入框 Enter 使用同一操作入口。
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
    dialog_surface(theme)
        .id("files-global-name-dialog")
        .child(dialog_header(
            title,
            detail,
            dialog_close_button(busy, close_focus, theme, cx),
            theme,
        ))
        .child(
            dialog_body(theme)
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
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.cancel_global_dialog(cx);
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
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.confirm_global_dialog(cx);
            })),
            theme,
        ))
        .into_any_element()
}

/// POSIX mode 弹框展示路径和真实权限，不把 Windows ACL 或缺失 metadata 伪装成八进制值。
/// listener 内直接操作当前 pane，避免按钮在处理点击时重入其实体。
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
    dialog_surface(theme)
        .id("files-global-permissions-dialog")
        .child(dialog_header(
            locale::text("修改权限", "Change permissions"),
            endpoint_path_label(endpoint),
            dialog_close_button(busy, close_focus, theme, cx),
            theme,
        ))
        .child(
            dialog_body(theme)
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
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.cancel_global_dialog(cx);
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
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.confirm_global_dialog(cx);
            })),
            theme,
        ))
        .into_any_element()
}

/// 删除弹框冻结目标类型，以短标题说明动作，完整对象路径和递归范围留在正文。
/// 文件名长度不受界面控制，因此不放入标题，避免窄窗口中标点孤行或挤压主要操作。
/// 点击回调复用 listener 的当前借用，不再对同一 FilesPane 调用嵌套 update。
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
    let title = if let [entry] = entries.as_slice() {
        if entry.kind == FileEntryKind::Directory {
            locale::text("删除目录？", "Delete folder?")
        } else {
            locale::text("删除文件？", "Delete file?")
        }
    } else {
        locale::text(
            format!("删除 {total} 项？"),
            format!("Delete {total} items?"),
        )
    };
    let description = delete_dialog_description(&entries);
    let labels = entries
        .iter()
        .take(8)
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    let hidden_count = total.saturating_sub(labels.len());
    dialog_surface(theme)
        .id("files-global-delete-dialog")
        .child(dialog_header(
            title,
            description,
            dialog_close_button(false, close_focus, theme, cx),
            theme,
        ))
        .child(
            dialog_body(theme)
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
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.cancel_global_dialog(cx);
            })),
            dialog_button_label(
                Button::new("files-global-dialog-confirm")
                    .compact()
                    .danger()
                    .icon(gpui_kit::assets::IconName::Trash),
                if total > 1 {
                    locale::text(format!("删除 {total} 项"), format!("Delete {total} items"))
                } else {
                    locale::text("删除", "Delete")
                },
            )
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.confirm_global_dialog(cx);
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

/// 标题、路径和关闭动作复用主题字号及弹层留白；Root 持有表面与遮罩。
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
        .gap(theme.metrics.section_gap)
        .border_b_1()
        .border_color(theme.border)
        .px(theme.metrics.dialog_padding)
        .py(theme.metrics.section_gap)
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(theme.text_heading)
                        .line_height(relative(1.25))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .mt_1()
                        .text_size(theme.text_xs)
                        .line_height(relative(1.5))
                        .text_color(theme.muted)
                        .child(description),
                ),
        )
        .child(close)
}

/// 关闭按钮复用已有取消路径与共享控件槽；表单提交期间禁用，避免出现无效关闭动作。
///
/// @author kongweiguang
fn dialog_close_button(
    busy: bool,
    close_focus: &FocusHandle,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> Button {
    let label = locale::text("关闭弹框", "Close dialog");
    Button::new("files-global-dialog-close")
        .focus_handle(close_focus)
        .size(theme.metrics.control_height)
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

/// 动作文案交给 Kit Button 排版，使字号、图标间距和用户界面缩放保持同一尺度。
///
/// @author kongweiguang
fn dialog_button_label(button: Button, label: impl Into<gpui_kit::SharedString>) -> Button {
    let label = label.into();
    button.accessibility_label(label.clone()).label(label)
}

/// 正文与标题、操作栏共享弹层留白；字段分组各自处理标签间距。
///
/// @author kongweiguang
fn dialog_body(theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap(theme.metrics.panel_padding)
        .px(theme.metrics.dialog_padding)
        .py(theme.metrics.section_gap)
}

/// 保留输入实体与焦点，并使用系统字体和主题字号，避免对话框字段与工作区各用一套文本尺度。
///
/// @author kongweiguang
fn dialog_text_input(
    input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
    theme: ThemeTokens,
    system_font_family: gpui_kit::SharedString,
) -> impl IntoElement {
    let input = gpui_kit::Styled::h(Input::new(input).w_full(), theme.metrics.control_height);
    let input = gpui_kit::Styled::px(input, px(12.));
    let input = gpui_kit::Styled::text_size(input, theme.text_sm);
    let input = gpui_kit::Styled::line_height(input, relative(1.5));
    let input = gpui_kit::Styled::font_weight(input, gpui_kit::FontWeight::NORMAL);
    let input = gpui_kit::Styled::font_family(input, system_font_family);
    let input = gpui_kit::Styled::bg(input, theme.field_surface());
    let input = gpui_kit::Styled::border_color(input, theme.strong_border());
    gpui_kit::Styled::text_color(input, theme.foreground)
}

/// 标签使用主题辅助文本尺度，使长文案和界面放大与字段保持一致。
///
/// @author kongweiguang
fn dialog_field_label(label: impl IntoElement, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_xs)
        .line_height(relative(4. / 3.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme.muted)
        .child(label)
}

/// 只读路径用次级表面表达上下文；等宽文字随界面缩放，执行目标仍由冻结的 endpoint 持有。
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
                .text_size(theme.text_xs)
                .line_height(relative(4. / 3.))
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

/// 按钮顺序固定为取消后确认，窄窗口和放大字体时允许换行并保留两个操作。
///
/// @author kongweiguang
fn dialog_footer(cancel: Button, confirm: Button, theme: ThemeTokens) -> impl IntoElement {
    crate::ui_surface::dialog_actions(theme)
        .w_full()
        .border_t_1()
        .border_color(theme.border)
        .px(theme.metrics.dialog_padding)
        .py(theme.metrics.section_gap)
        .child(cancel)
        .child(confirm)
}

/// 文件对话框和 main 一样显示纯路径；执行目标仍由冻结的 endpoint 持有。
///
/// @author kongweiguang
fn endpoint_path_label(endpoint: &FileEndpoint) -> String {
    super::render::browser::endpoint_path(endpoint)
}
