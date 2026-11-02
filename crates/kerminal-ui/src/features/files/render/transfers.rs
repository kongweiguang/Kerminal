//! 文件栏和传输工作台共享最近任务投影及按需展开的恢复材料。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use gpui_kit::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement,
    StatefulInteractiveElement as _,
    component::{
        Disableable as _, IconName,
        button::{Button, ButtonVariants as _},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_domain::files::{TransferFailureKind, TransferPhase, TransferStatus, TransferSummary};
use kerminal_domain::identity::TransferId;

use crate::{locale, theme::ThemeTokens};

use super::{format_size, icon_button, icon_button_without_tooltip, transfer_status};
use crate::features::files::FilesPane;

/// 标题本身控制完整列表显隐，按钮保留键盘语义，清理动作与折叠点击各自独立。
/// 折叠仍提示隐藏的活动及恢复事项，未知提交和待清理资源不会进入清理范围。
/// 列表复用实际字节和提交状态计算百分比；上传下载标明方向，暂存只报告准备就绪，不冒充最终复制完成。
/// 窄栏允许元数据换行以保留完整单位，临时拖出任务不暴露已回收路径的定位或重试。
/// 系统目标尚未释放副本时，即使暂存成功也保留待处理提示并拒绝显示可清理历史。
/// 日常列表仅显示传输与待处理恢复项，不提供归档材料浏览；底层恢复证据继续独立保存。
/// 清理与清理取消均用图标，避免忙碌状态撑挤标题。
/// 恢复列表重试保留按钮标签，只转动原位图标，错误收尾前不清空现有材料。
///
/// @author kongweiguang
pub(super) fn render_transfers(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let compact_layout = pane.compact_layout;
    let projection =
        project_transfer_queue(&pane.transfer_rows, pane.queue.history_expanded, |id| {
            pane.transfers.is_drag_staging_pending_delivery(id)
        });
    let hidden_attention =
        hidden_attention_count(&pane.transfer_rows, &pane.transfer_recovery_rows, |id| {
            pane.transfers.is_drag_staging_pending_delivery(id)
        });
    let recovery_rows = if pane.queue.history_expanded {
        pane.transfer_recovery_rows
            .iter()
            .filter(|record| recovery_needs_attention(record))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let rows = projection.visible_rows;
    let has_rows =
        !rows.is_empty() || !recovery_rows.is_empty() || pane.queue.recovery_list_error.is_some();
    let clearing_history = pane.queue.is_clearing_completed_history();
    let empty_queue = !pane.queue.recovery_list_loading
        && pane.queue.recovery_list_error.is_none()
        && !clearing_history
        && pane.transfer_rows.is_empty()
        && !pane
            .transfer_recovery_rows
            .iter()
            .any(recovery_needs_attention)
        && projection.active_or_attention_count == 0
        && projection.clearable_history_count == 0;
    let mut list = div()
        .id("files-transfer-list")
        .debug_selector(|| "rr-files-transfer-list".into())
        .w_full()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_y_scroll();
    let title = if !pane.transfer_rows.is_empty() {
        let count = pane.transfer_rows.len();
        locale::text(format!("传输 ({count})"), format!("Transfers ({count})"))
    } else {
        locale::text("传输", "Transfers")
    };
    let mut history_content = div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_2()
        .text_size(theme.text_xs)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(div().flex_shrink_0().text_color(theme.accent).child(
            if pane.queue.history_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            },
        ))
        .child(title);
    history_content = if empty_queue {
        history_content.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(theme.muted)
                .text_right()
                .child(locale::text("暂无任务", "No tasks")),
        )
    } else if !pane.queue.history_expanded && pane.queue.recovery_list_error.is_some() {
        history_content.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .text_color(theme.warning)
                .text_right()
                .child(locale::text("恢复列表读取失败", "Recovery list failed")),
        )
    } else if !pane.queue.history_expanded && hidden_attention > 0 {
        history_content.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .text_color(theme.warning)
                .text_right()
                .child(locale::text(
                    format!("{hidden_attention} 项进行中或待处理"),
                    format!("{hidden_attention} active or needs attention"),
                )),
        )
    } else {
        history_content.child(div().flex_1())
    };
    let history_button = Button::new("files-transfer-history")
        .compact()
        .ghost()
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .debug_selector(|| "rr-files-transfer-history".into())
        .toggled(pane.queue.history_expanded)
        .accessibility_label(if pane.queue.history_expanded {
            locale::text("收起传输列表", "Collapse transfers")
        } else {
            locale::text("展开传输列表", "Expand transfers")
        })
        .child(history_content)
        .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_transfer_history(cx)));
    let mut header = div()
        .id("files-transfer-queue-header")
        .w_full()
        .h(px(28.))
        .pr_1()
        .flex()
        .items_center()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(history_button);
    if clearing_history {
        header = header.child(
            Button::new("files-transfer-clear-completed-cancel")
                .compact()
                .ghost()
                .icon(IconName::Close)
                .tooltip(locale::text(
                    "正在清理，点击取消",
                    "Clearing; click to cancel",
                ))
                .accessibility_label(locale::text("取消清理", "Cancel cleanup"))
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                    pane.cancel_completed_transfer_history(cx)
                })),
        );
    } else if projection.clearable_history_count > 0 {
        header = header.child(
            Button::new("files-transfer-clear-completed")
                .compact()
                .ghost()
                .debug_selector(|| "rr-files-transfer-clear".into())
                .icon(gpui_kit::assets::IconName::Trash)
                .tooltip(locale::text("清理已结束的传输", "Clear finished transfers"))
                .accessibility_label(locale::text("清理已结束的传输", "Clear finished transfers"))
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                    pane.clear_completed_transfer_history(cx)
                })),
        );
    }
    if let Some(error) = &pane.queue.recovery_list_error {
        let message = locale::text(
            format!("恢复材料列表读取失败：{error}"),
            format!("Recovery list could not be read: {error}"),
        );
        let retry_button = Button::new("files-transfer-recovery-list-retry")
            .compact()
            .ghost()
            .flex_shrink_0()
            .label(locale::text("重试恢复列表", "Retry recovery list"))
            .icon(IconName::RefreshCw)
            .loading(pane.queue.recovery_list_loading)
            .loading_icon(IconName::RefreshCw)
            .disabled(pane.queue.recovery_list_loading)
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.reload_transfers(cx)));
        let error_row = if compact_layout {
            div()
                .id("files-transfer-recovery-list-error")
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .items_start()
                .gap_2()
                .child(div().w_full().text_color(theme.warning).child(message))
                .child(retry_button)
        } else {
            let tooltip_message = message.clone();
            div()
                .id("files-transfer-recovery-list-error")
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .id("files-transfer-recovery-list-error-message")
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(theme.warning)
                        .tooltip(move |window, cx| {
                            Tooltip::new(tooltip_message.clone()).build(window, cx)
                        })
                        .child(message),
                )
                .child(retry_button)
        };
        list = list.child(error_row);
    }

    for summary in rows {
        let id = summary.id.clone();
        let retry_id = id.clone();
        let cleanup_id = id.clone();
        let cancel_cleanup_id = id.clone();
        let cleanup_busy = pane.queue.cleanup_retry_id() == Some(&id);
        let any_cleanup_busy = pane.queue.is_retrying_cleanup();
        let drag_staging = pane.transfers.is_drag_staging(&id);
        let direction = if drag_staging {
            locale::text("拖出准备", "Prepare drag")
        } else {
            match (&summary.request.source, &summary.request.destination) {
                (kerminal_domain::files::FileEndpoint::Local { .. }, destination)
                    if !matches!(
                        destination,
                        kerminal_domain::files::FileEndpoint::Local { .. }
                    ) =>
                {
                    locale::text("上传", "Upload")
                }
                (source, kerminal_domain::files::FileEndpoint::Local { .. })
                    if !matches!(source, kerminal_domain::files::FileEndpoint::Local { .. }) =>
                {
                    locale::text("下载", "Download")
                }
                _ => locale::text("复制", "Copy"),
            }
        };
        let result_status = if summary.failure_kind == Some(TransferFailureKind::CommitUnknown) {
            locale::text(
                "提交结果未知，请先核对目标",
                "Commit result unknown; inspect the destination first",
            )
            .to_string()
        } else if drag_staging && summary.status == TransferStatus::Succeeded {
            locale::text("已就绪", "Ready").to_string()
        } else {
            transfer_status(summary.status).to_string()
        };
        let result_status = format!("{direction} · {result_status}");
        let status = match summary.phase {
            Some(phase) => format!("{result_status} · {}", transfer_phase_label(phase)),
            None => result_status,
        };
        let status = if summary.cleanup_pending {
            format!(
                "{} · {}",
                status,
                locale::text("资源清理待处理", "resource cleanup pending")
            )
        } else {
            status
        };
        let safe_error = summary.safe_error.clone().unwrap_or_default();
        let source_name = super::endpoint_name(&summary.request.source)
            .unwrap_or_else(|| locale::text("文件", "File").to_string());
        let source_detail = pane.target_names.endpoint_detail(&summary.request.source);
        let destination = summary
            .actual_destination
            .as_ref()
            .unwrap_or(&summary.request.destination);
        let destination_name = super::endpoint_name(destination)
            .unwrap_or_else(|| locale::text("目标", "Destination").to_string());
        let show_destination_name = destination_name != source_name;
        let destination_tooltip = pane.target_names.endpoint_detail(destination);
        let source_tooltip = if drag_staging {
            source_detail
        } else {
            format!("{source_detail} → {destination_tooltip}")
        };
        let destination_location_tooltip = destination_tooltip.clone();
        let destination_for_location = destination.clone();
        let progress = summary
            .total_bytes
            .map(|total| {
                format!(
                    "{} / {}",
                    format_size(Some(summary.bytes_transferred)),
                    format_size(Some(total))
                )
            })
            .unwrap_or_else(|| format_size(Some(summary.bytes_transferred)));
        let percent = transfer_percent(summary);
        let transfer_selector = format!("rr-files-transfer-{}", summary.id);
        let mut item = div()
            .id(format!("files-transfer-{}", summary.id))
            .debug_selector(move || transfer_selector.clone())
            .w_full()
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .border_t_1()
            .border_color(theme.border);
        let status_icon = match summary.status {
            TransferStatus::Succeeded | TransferStatus::Unchanged | TransferStatus::Skipped => {
                IconName::CircleCheck
            }
            TransferStatus::Failed | TransferStatus::Cancelled => IconName::CircleAlert,
            _ => IconName::LoaderCircle,
        };
        let status_tone = if summary.cleanup_pending {
            theme.warning
        } else {
            match summary.status {
                TransferStatus::Succeeded | TransferStatus::Unchanged | TransferStatus::Skipped => {
                    theme.success
                }
                TransferStatus::Failed | TransferStatus::Cancelled => theme.error,
                _ => theme.accent,
            }
        };
        let mut title = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .text_size(theme.text_xs)
            .child(div().text_color(status_tone).child(status_icon))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .id(format!("files-transfer-source-{}", summary.id))
                            .min_w(px(0.))
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .tooltip(move |window, cx| {
                                Tooltip::new(source_tooltip.clone()).build(window, cx)
                            })
                            .child(source_name),
                    )
                    .when(show_destination_name, |names| {
                        names
                            .child(div().flex_shrink_0().text_color(theme.subtle).child("→"))
                            .child(
                                div()
                                    .id(format!("files-transfer-destination-{}", summary.id))
                                    .min_w(px(0.))
                                    .flex_1()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(destination_tooltip.clone()).build(window, cx)
                                    })
                                    .child(destination_name),
                            )
                    }),
            );
        if !drag_staging {
            let locate_label = locale::text("定位目标目录", "Show destination folder");
            let locate_tooltip = format!("{locate_label}\n{destination_location_tooltip}");
            let locate_button = icon_button_without_tooltip(
                format!("files-locate-destination-{}", summary.id),
                IconName::FolderOpen,
                locate_label.clone(),
                false,
            )
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.locate_transfer_destination(destination_for_location.clone(), cx)
            }));
            title = title.child(
                div()
                    .id(format!("files-locate-tooltip-{}", summary.id))
                    .w(px(28.))
                    .h(px(26.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .tooltip(move |window, cx| {
                        Tooltip::new(locate_tooltip.clone()).build(window, cx)
                    })
                    .child(locate_button),
            );
        }
        let speed = (summary.speed_bytes_per_second > 0)
            .then(|| format!("{}/s", format_size(Some(summary.speed_bytes_per_second))));
        let resume_label = summary
            .resumable
            .then(|| locale::text("可续传", "Resumable"));

        if summary.cleanup_pending {
            title = title.child(
                icon_button(
                    format!("files-retry-cleanup-{}", summary.id),
                    IconName::RefreshCw,
                    if cleanup_busy {
                        locale::text("清理中…", "Cleaning…")
                    } else {
                        locale::text("重试清理", "Retry cleanup")
                    },
                    any_cleanup_busy,
                )
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.retry_cleanup(cleanup_id.clone(), cx)
                })),
            );
            if cleanup_busy {
                title = title.child(
                    icon_button(
                        format!("files-cancel-cleanup-{}", summary.id),
                        IconName::Close,
                        locale::text("取消清理重试", "Cancel cleanup retry"),
                        false,
                    )
                    .on_click(cx.listener(
                        move |pane, _: &ClickEvent, _, cx| {
                            pane.cancel_cleanup_retry(&cancel_cleanup_id, cx)
                        },
                    )),
                );
            }
        } else if summary.retryable && !drag_staging {
            title = title.child(
                icon_button(
                    format!("files-retry-{}", summary.id),
                    IconName::RefreshCw,
                    locale::text("创建重试任务", "Queue a retry"),
                    false,
                )
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.retry_transfer(retry_id.clone(), cx)
                })),
            );
        }
        if !is_terminal(summary.status) {
            title = title.child(
                icon_button(
                    format!("files-cancel-{}", summary.id),
                    IconName::Close,
                    locale::text("取消传输", "Cancel transfer"),
                    false,
                )
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.cancel_transfer(id.clone(), cx)
                })),
            );
        }
        item = item.child(title);
        let speed_selector = format!("rr-files-transfer-speed-{}", summary.id);
        let status_tooltip = status.clone();
        item = item.child(
            div()
                .ml(px(22.))
                .min_w(px(0.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .text_size(theme.text_xs)
                .child(
                    div()
                        .id(format!("files-transfer-status-{}", summary.id))
                        .flex_1()
                        .min_w_16()
                        .overflow_hidden()
                        .text_color(status_tone)
                        .text_ellipsis()
                        .tooltip(move |window, cx| {
                            Tooltip::new(status_tooltip.clone()).build(window, cx)
                        })
                        .child(status),
                )
                .child(
                    div()
                        .debug_selector({
                            let selector = format!("rr-files-transfer-percent-{}", summary.id);
                            move || selector.clone()
                        })
                        .flex_shrink_0()
                        .text_color(theme.foreground)
                        .child(
                            percent
                                .map(|value| format!("{value}%"))
                                .unwrap_or_else(|| "—".into()),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.subtle)
                        .child(progress),
                )
                .when_some(speed, |row, speed| {
                    row.child(
                        div()
                            .id(speed_selector.clone())
                            .debug_selector(move || speed_selector)
                            .flex_shrink_0()
                            .text_color(theme.subtle)
                            .child(speed),
                    )
                })
                .when_some(resume_label, |row, resume| {
                    row.child(div().flex_shrink_0().text_color(theme.accent).child(resume))
                }),
        );
        if let Some(percent) = percent {
            item = item.child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .child(div().w(px(22.)).flex_shrink_0())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(3.))
                            .rounded_full()
                            .bg(theme.muted.opacity(0.16))
                            .child(
                                div()
                                    .w(relative(f32::from(percent) / 100.0))
                                    .h(px(3.))
                                    .rounded_full()
                                    .bg(status_tone),
                            ),
                    ),
            );
        }
        if summary.partial_commit {
            item = item.child(
                div()
                    .ml(px(22.))
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(format!(
                        "{} {} · {}",
                        locale::text("已确认目标项", "Confirmed target items"),
                        summary.commit_summary.committed_entries(),
                        format_size(Some(summary.commit_summary.committed_bytes()))
                    )),
            );
        }
        if !safe_error.is_empty() {
            item = item.child(
                div()
                    .ml(px(22.))
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(safe_error),
            );
        }
        if summary.cleanup_pending {
            item = item.child(
                div()
                    .ml(px(22.))
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(locale::text(
                        "数据状态已确定，资源仍待清理。",
                        "Data result is final; resource cleanup is pending.",
                    )),
            );
        }
        list = list.child(item);
    }

    for recovery in recovery_rows {
        use kerminal_application::transfers::TransferRecoveryInspection;
        let id = recovery.id.clone();
        let inspect_id = id.clone();
        let archive_id = id.clone();
        let destination = recovery
            .actual_destination
            .clone()
            .unwrap_or(recovery.request.destination.clone());
        let label = super::endpoint_name(&destination)
            .unwrap_or_else(|| locale::text("原目标", "Original destination").to_string());
        let label = if recovery.drag_staging {
            format!("{} · {label}", locale::text("拖出准备", "Drag preparation"))
        } else {
            label
        };
        let destination_tooltip = pane.target_names.endpoint_detail(&destination);
        let evidence_description = match recovery.inspection {
            TransferRecoveryInspection::DestinationMatches => locale::text(
                "核对证据：目标内容一致",
                "Inspection evidence: destination matched",
            ),
            TransferRecoveryInspection::DestinationMissing => locale::text(
                "核对证据：原目标不存在",
                "Inspection evidence: destination missing",
            ),
            TransferRecoveryInspection::DestinationDiffers => locale::text(
                "核对证据：目标内容不同",
                "Inspection evidence: destination differed",
            ),
            TransferRecoveryInspection::Insufficient => locale::text(
                "现有证据不足，提交结论未知",
                "Insufficient evidence; commit remains unknown",
            ),
        };
        let description = evidence_description.to_string();
        let description = if recovery.drag_staging {
            format!(
                "{} · {description}",
                locale::text(
                    "仅核对准备副本，不能据此确认系统目标复制完成",
                    "Prepared copy only; this does not confirm the system target copy"
                )
            )
        } else {
            description
        };
        let busy = pane.queue.recovery_busy();
        let own_busy = pane.queue.recovery_id() == Some(&id);
        let mut actions = recovery_actions_row();
        if !recovery.archived {
            actions = actions.child(
                Button::new(format!("recovery-inspect-{id}"))
                    .compact()
                    .ghost()
                    .label(if own_busy {
                        locale::text("核对中…", "Inspecting…")
                    } else if recovery.drag_staging {
                        locale::text("核对准备副本", "Inspect prepared copy")
                    } else {
                        locale::text("核对恢复", "Inspect recovery")
                    })
                    .disabled(busy)
                    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                        pane.inspect_transfer_recovery(inspect_id.clone(), cx)
                    })),
            );
        }
        actions = actions.child(
            Button::new(format!("recovery-locate-{id}"))
                .compact()
                .ghost()
                .label(if recovery.drag_staging {
                    locale::text("查看准备目录", "Show prepared folder")
                } else {
                    locale::text("定位", "Locate")
                })
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.locate_transfer_destination(destination.clone(), cx)
                })),
        );
        if own_busy {
            actions =
                actions.child(
                    Button::new(format!("recovery-cancel-{id}"))
                        .compact()
                        .ghost()
                        .label(locale::text("取消", "Cancel"))
                        .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                            pane.cancel_transfer_recovery(cx)
                        })),
                );
        } else if !recovery.archived {
            actions = actions.child(
                Button::new(format!("recovery-archive-{id}"))
                    .compact()
                    .ghost()
                    .icon(gpui_kit::assets::IconName::Trash)
                    .tooltip(locale::text(
                        "从列表移除，保留恢复材料",
                        "Remove from list; retain recovery data",
                    ))
                    .accessibility_label(locale::text(
                        "从列表移除恢复项",
                        "Remove recovery item from list",
                    ))
                    .disabled(busy)
                    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                        pane.archive_transfer_recovery(archive_id.clone(), cx)
                    })),
            );
        }
        let expanded = pane.queue.is_recovery_expanded(&id);
        if !recovery.staging_ownership.is_empty() {
            let expand_id = id.clone();
            actions = actions.child(
                Button::new(format!("recovery-staging-{id}"))
                    .compact()
                    .ghost()
                    .label(locale::text(
                        format!("暂存 ({})", recovery.staging_ownership.len()),
                        format!("Staging ({})", recovery.staging_ownership.len()),
                    ))
                    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                        pane.toggle_recovery_staging(expand_id.clone(), cx)
                    })),
            );
        }
        let mut recovery_item = div()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap_1()
            .text_size(theme.text_xs)
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.warning)
                            .child(IconName::CircleAlert),
                    )
                    .child(
                        div()
                            .id(format!("recovery-destination-{id}"))
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .tooltip(move |window, cx| {
                                Tooltip::new(destination_tooltip.clone()).build(window, cx)
                            })
                            .child(label),
                    ),
            )
            .child(actions)
            .child(div().text_color(theme.subtle).child(description))
            .when(
                recovery.cleanup_pending
                    || recovery.checkpoint.is_some()
                    || !recovery.staging_ownership.is_empty(),
                |element| {
                    element.child(div().text_color(theme.warning).child(locale::text(
                        "恢复材料已保留，可定位核对。",
                        "Recovery data is retained for inspection.",
                    )))
                },
            );
        if expanded {
            for (index, ownership) in recovery.staging_ownership.iter().enumerate() {
                let staging = ownership.staging_destination.clone();
                let detail = pane.target_names.endpoint_detail(&staging);
                recovery_item = recovery_item.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(theme.subtle)
                                .child(detail),
                        )
                        .child(
                            Button::new(format!("recovery-stage-{id}-{index}"))
                                .compact()
                                .ghost()
                                .label(locale::text("定位暂存", "Locate staging"))
                                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                                    pane.locate_transfer_destination(staging.clone(), cx)
                                })),
                        ),
                );
            }
        }
        list = list.child(recovery_item);
    }

    if !has_rows && pane.queue.history_expanded {
        list = list.child(
            div()
                .px_3()
                .py_2()
                .text_color(theme.subtle)
                .text_size(theme.text_xs)
                .child(locale::text(
                    "没有可显示的历史记录。",
                    "No history entries to show.",
                )),
        );
    }
    div()
        .id("files-transfer-queue")
        .w_full()
        .max_h(px(300.))
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(header)
        .when(pane.queue.history_expanded && !empty_queue, |queue| {
            queue.child(list)
        })
        .into_any_element()
}

/// 收敛数量与状态统计，展示顺序沿用队列的权威入队序号。
///
/// @author kongweiguang
struct TransferQueueProjection<'a> {
    visible_rows: Vec<&'a TransferSummary>,
    clearable_history_count: usize,
    active_or_attention_count: usize,
}

/// 折叠隐藏列表但保留各类状态统计与清理门禁，展开按原始队列顺序展示。
/// 拖出暂存的成功只代表本机准备完成，系统目标仍持有时不能当作普通已结束历史。
///
/// @author kongweiguang
fn project_transfer_queue(
    summaries: &[TransferSummary],
    history_expanded: bool,
    pending_delivery: impl Fn(&TransferId) -> bool,
) -> TransferQueueProjection<'_> {
    let clearable_history_count = summaries
        .iter()
        .filter(|summary| is_clearable_history(summary) && !pending_delivery(&summary.id))
        .count();
    let active_or_attention_count = summaries
        .iter()
        .filter(|summary| needs_attention_or_is_active(summary) || pending_delivery(&summary.id))
        .count();
    let visible_rows = if history_expanded {
        summaries.iter().collect()
    } else {
        Vec::new()
    };

    TransferQueueProjection {
        visible_rows,
        clearable_history_count,
        active_or_attention_count,
    }
}

/// 只为折叠隐藏的活动、系统交接或恢复项计数；同一任务的活摘要与恢复记录不能重复计数。
///
/// @author kongweiguang
fn hidden_attention_count(
    summaries: &[TransferSummary],
    recovery: &[kerminal_application::transfers::TransferRecoveryRecord],
    pending_delivery: impl Fn(&TransferId) -> bool,
) -> usize {
    summaries
        .iter()
        .filter(|summary| needs_attention_or_is_active(summary) || pending_delivery(&summary.id))
        .map(|summary| &summary.id)
        .chain(
            recovery
                .iter()
                .filter(|record| recovery_needs_attention(record))
                .map(|record| &record.id),
        )
        .collect::<BTreeSet<_>>()
        .len()
}

/// 归档浏览已移除，但旧记录仍有待清理材料时必须继续可见，避免隐藏无法收尾的恢复项。
///
/// @author kongweiguang
fn recovery_needs_attention(
    record: &kerminal_application::transfers::TransferRecoveryRecord,
) -> bool {
    !record.archived
        || record.cleanup_pending
        || record.checkpoint.is_some()
        || !record.staging_ownership.is_empty()
}

/// 仅普通已结束记录计入可清理历史，未知回执与待清理资源必须保留给用户处理。
///
/// @author kongweiguang
fn is_clearable_history(summary: &TransferSummary) -> bool {
    is_terminal(summary.status) && !summary.cleanup_pending && !is_unknown_commit(summary)
}

/// 未知提交即使 status 为 Failed 也属于待核对项，不能当作可清理的普通历史。
///
/// @author kongweiguang
fn needs_attention_or_is_active(summary: &TransferSummary) -> bool {
    !is_terminal(summary.status) || summary.cleanup_pending || is_unknown_commit(summary)
}

/// 通过领域失败类别识别提交结果未知，避免从错误文案推断是否可以删除记录。
///
/// @author kongweiguang
fn is_unknown_commit(summary: &TransferSummary) -> bool {
    summary.failure_kind == Some(TransferFailureKind::CommitUnknown)
}

/// 将成功以外的终态和活跃阶段统一判定，清理待办由 cleanup_pending 单独覆盖显示。
///
/// @author kongweiguang
fn is_terminal(status: TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled
    )
}

/// 只有成功回执代表目标提交完成；未知总量保持未知，未成功的任务即使字节到齐也不显示 100%。
/// 使用宽整数避免大文件字节数计算时溢出，并与进度条共用同一数值。
///
/// @author kongweiguang
fn transfer_percent(summary: &TransferSummary) -> Option<u8> {
    if summary.status == TransferStatus::Succeeded {
        return Some(100);
    }
    let total = summary.total_bytes.filter(|total| *total > 0)?;
    let percent = (u128::from(summary.bytes_transferred) * 100 / u128::from(total)).min(99);
    Some(percent as u8)
}

/// 用运行阶段解释传输字节尚未增长的实际工作，不把预读校验伪装成传输进度。
///
/// @author kongweiguang
fn transfer_phase_label(phase: TransferPhase) -> String {
    locale::text(
        match phase {
            TransferPhase::Preparing => "准备传输",
            TransferPhase::Transferring => "传输数据",
            TransferPhase::Verifying => "校验源文件",
            TransferPhase::Committing => "提交目标文件",
        },
        match phase {
            TransferPhase::Preparing => "Preparing transfer",
            TransferPhase::Transferring => "Transferring data",
            TransferPhase::Verifying => "Verifying source file",
            TransferPhase::Committing => "Committing destination",
        },
    )
    .to_string()
}

/// 恢复动作独占可换行的内容宽度，不与目标标题争抢空间，也不改各按钮的 owner 或忙碌门禁。
/// 长文案和暂存入口自然增加行高，由原队列滚动区承接，避免窄栏以裁切恢复操作换取紧凑。
///
/// @author kongweiguang
fn recovery_actions_row() -> gpui_kit::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_1()
}

#[cfg(test)]
#[path = "../../../../tests/unit/files_transfer_history_projection.rs"]
mod tests;
