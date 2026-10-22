//! 传输队列视图始终保留活动项与资源清理待办；历史单独折叠。
//!
//! @author kongweiguang

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
    px,
};
use kerminal_domain::files::{TransferFailureKind, TransferPhase, TransferStatus, TransferSummary};

use crate::{locale, theme::ThemeTokens};

use super::{format_size, icon_button, icon_button_without_tooltip, transfer_status};
use crate::features::files::FilesPane;

/// 未归档任务、未知提交和 cleanup_pending 不受历史折叠影响，全部可在受限滚动区访问。
/// 仅在归档读取完成且确认无任务、历史或恢复项时将折叠标题收成空态；空态仍保留历史图标入口，因为其他面板或 MCP 可能稍后写入冷档，当前为空不能证明后续无历史。
/// 错误文案在构建元素前复制为拥有值，避免 UI 元素生命周期超过队列摘要借用；后台读取只更新固定头栏，避免进度通知撑高列表。
/// 紧凑右栏将恢复动作单独换行，宽布局收缩并省略错误文本且用提示保留全文，避免长错误把重试入口挤出视口。
/// 恢复记录的实际目标、可换行动作与证据结论分行，不能用压缩对象名换取恢复操作可达性。
///
/// @author kongweiguang
pub(super) fn render_transfers(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    if pane.transfer_rows.is_empty()
        && pane.transfer_recovery_rows.is_empty()
        && pane.queue.archive_records.is_empty()
        && !pane.queue.archive_loading
        // 空页不证明此后没有冷档；保留历史刷新入口，不为每次进度广播扫描整个目录。
        && !pane.queue.archive_loaded
        && pane.queue.archive_error.is_none()
        && !pane.queue.recovery_list_loading
        && pane.queue.recovery_list_error.is_none()
        && !pane.queue.history_expanded
    {
        return div().h(px(0.)).into_any_element();
    }
    let compact_layout = pane.compact_layout;
    let recovery_rows = pane
        .transfer_recovery_rows
        .iter()
        .filter(|record| {
            !record.archived
                || record.cleanup_pending
                || record.checkpoint.is_some()
                || !record.staging_ownership.is_empty()
                || pane.queue.history_expanded
        })
        .chain(
            pane.queue
                .archive_records
                .iter()
                .filter(|_| pane.queue.history_expanded),
        )
        .cloned()
        .collect::<Vec<_>>();
    let projection = project_transfer_queue(&pane.transfer_rows, pane.queue.history_expanded);
    let rows = projection.visible_rows;
    let has_rows =
        !rows.is_empty() || !recovery_rows.is_empty() || pane.queue.recovery_list_error.is_some();
    let clearing_history = pane.queue.is_clearing_completed_history();
    let empty_collapsed_queue = !pane.queue.history_expanded
        && pane.queue.archive_loaded
        && !pane.queue.archive_loading
        && pane.queue.archive_error.is_none()
        && !pane.queue.recovery_list_loading
        && pane.queue.recovery_list_error.is_none()
        && !clearing_history
        && pane.transfer_rows.is_empty()
        && pane.transfer_recovery_rows.is_empty()
        && pane.queue.archive_records.is_empty()
        && projection.active_or_attention_count == 0
        && projection.clearable_history_count == 0;
    let history_button = Button::new("files-transfer-history")
        .compact()
        .ghost()
        .when(empty_collapsed_queue, |button| {
            button
                .icon(gpui_kit::assets::IconName::ListClock)
                .tooltip(locale::text("传输历史", "Transfer history"))
                .accessibility_label(locale::text("传输历史", "Transfer history"))
        })
        .when(!empty_collapsed_queue, |button| {
            button.label(if pane.queue.history_expanded {
                locale::text("隐藏历史", "Hide history")
            } else {
                locale::text("显示历史", "Show history")
            })
        })
        .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_transfer_history(cx)));

    let mut list = div()
        .id("files-transfer-queue")
        .w_full()
        .max_h(px(136.))
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel);
    let mut header = div()
        .id("files-transfer-queue-header")
        .w_full()
        .h(px(28.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_size(theme.text_xs)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(div().text_color(theme.accent).child(IconName::ArrowDown))
        .child(if empty_collapsed_queue {
            locale::text("传输队列", "Transfer queue")
        } else {
            locale::text("传输", "Transfers")
        })
        .child(history_button)
        .child(if empty_collapsed_queue {
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(theme.muted)
                .text_right()
                .child(locale::text("暂无任务", "No tasks"))
        } else if pane.queue.recovery_list_loading {
            div()
                .flex_1()
                .min_w(px(0.))
                .text_ellipsis()
                .text_color(theme.muted)
                .child(locale::text("读取恢复列表…", "Reading recovery list…"))
        } else if compact_layout {
            div().flex_1()
        } else {
            div().flex_1().child(locale::text(
                format!(
                    "{} 个活动/待处理任务 · {} 项可清理历史",
                    projection.active_or_attention_count, projection.clearable_history_count
                ),
                format!(
                    "{} active or needs attention · {} clearable history entries",
                    projection.active_or_attention_count, projection.clearable_history_count
                ),
            ))
        });
    if clearing_history {
        header = header
            .child(
                div()
                    .px_2()
                    .text_color(theme.subtle)
                    .child(locale::text("清理中…", "Clearing…")),
            )
            .child(
                Button::new("files-transfer-clear-completed-cancel")
                    .compact()
                    .ghost()
                    .label(locale::text("取消", "Cancel"))
                    .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                        pane.cancel_completed_transfer_history(cx)
                    })),
            );
    } else if projection.clearable_history_count > 0 && !pane.host_picker.enabled {
        header = header.child(
            Button::new("files-transfer-clear-completed")
                .compact()
                .ghost()
                .label(locale::text("清理已完成", "Clear completed"))
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                    pane.clear_completed_transfer_history(cx)
                })),
        );
    }
    list = list.child(header);

    if let Some(error) = &pane.queue.recovery_list_error {
        let message = locale::text(
            format!("恢复材料列表读取失败：{error}"),
            format!("Recovery list could not be read: {error}"),
        );
        let retry_button = Button::new("files-transfer-recovery-list-retry")
            .compact()
            .ghost()
            .flex_shrink_0()
            .label(if pane.queue.recovery_list_loading {
                locale::text("读取中…", "Reading…")
            } else {
                locale::text("重试恢复列表", "Retry recovery list")
            })
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
        let result_status = if summary.failure_kind == Some(TransferFailureKind::CommitUnknown) {
            locale::text(
                "提交结果未知，请先核对目标",
                "Commit result unknown; inspect the destination first",
            )
            .to_string()
        } else {
            transfer_status(summary.status).to_string()
        };
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
        let destination = summary
            .actual_destination
            .as_ref()
            .unwrap_or(&summary.request.destination);
        let destination_name = super::endpoint_name(destination)
            .unwrap_or_else(|| locale::text("目标", "Destination").to_string());
        let destination_tooltip = pane.target_names.endpoint_detail(destination);
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
        let ratio = summary.total_bytes.and_then(|total| {
            (total > 0)
                .then(|| (summary.bytes_transferred as f64 / total as f64).clamp(0.0, 1.0) as f32)
        });
        let mut item = div()
            .id(format!("files-transfer-{}", summary.id))
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
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(source_name),
                    )
                    .child(div().flex_shrink_0().text_color(theme.subtle).child("→"))
                    .child(
                        div()
                            .id(format!("files-transfer-destination-{}", summary.id))
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .tooltip(move |window, cx| {
                                Tooltip::new(destination_tooltip.clone()).build(window, cx)
                            })
                            .child(destination_name),
                    ),
            );
        if !compact_layout {
            title = title
                .child(div().text_color(status_tone).child(status.clone()))
                .child(div().text_color(theme.subtle).child(progress.clone()));
        }
        let locate_label = locale::text("定位目标目录", "Show destination folder");
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
                .tooltip(move |window, cx| Tooltip::new(locate_label.clone()).build(window, cx))
                .child(locate_button),
        );
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
        } else if summary.retryable {
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
        if compact_layout {
            item = item.child(
                div()
                    .ml(px(22.))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_color(status_tone)
                            .text_ellipsis()
                            .child(status),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.subtle)
                            .child(progress),
                    )
                    .when_some(speed.clone(), |row, speed| {
                        row.child(div().flex_shrink_0().text_color(theme.subtle).child(speed))
                    })
                    .when_some(resume_label.clone(), |row, resume| {
                        row.child(div().flex_shrink_0().text_color(theme.accent).child(resume))
                    }),
            );
        }
        if !compact_layout {
            if let Some(speed) = speed {
                item = item.child(
                    div()
                        .ml(px(22.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(theme.text_xs)
                        .text_color(theme.subtle)
                        .child(locale::text("速率", "Rate"))
                        .child(speed)
                        .when_some(resume_label, |row, resume| row.child(resume)),
                );
            } else if let Some(resume) = resume_label {
                item = item.child(
                    div()
                        .ml(px(22.))
                        .text_size(theme.text_xs)
                        .text_color(theme.accent)
                        .child(resume),
                );
            }
        }
        if let Some(ratio) = ratio {
            let width = (180.0 * ratio).max(if ratio > 0.0 { 2.0 } else { 0.0 });
            item = item.child(
                div()
                    .ml(px(22.))
                    .w(px(180.))
                    .h(px(3.))
                    .rounded_full()
                    .bg(theme.muted.opacity(0.16))
                    .child(div().w(px(width)).h(px(3.)).rounded_full().bg(
                        if summary.cleanup_pending {
                            theme.warning
                        } else {
                            status_tone
                        },
                    )),
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
        let description = if recovery.archived {
            format!(
                "{} · {evidence_description}",
                locale::text("已人工归档（非成功）", "Manually archived, not successful")
            )
        } else {
            evidence_description.to_string()
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
                .label(locale::text("定位", "Locate"))
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
                    .label(locale::text("人工归档（非成功）", "Archive, not success"))
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
        let mut recovery_item = div().px_3().py_2().border_t_1().border_color(theme.border)
            .flex().flex_col().gap_1().text_size(theme.text_xs)
            .child(div().w_full().min_w(px(0.)).flex().items_center().gap_2()
                .child(div().flex_shrink_0().text_color(theme.warning).child(IconName::CircleAlert))
                .child(div().id(format!("recovery-destination-{id}")).flex_1().min_w(px(0.)).overflow_hidden().text_ellipsis()
                    .tooltip(move |window, cx| Tooltip::new(destination_tooltip.clone()).build(window, cx))
                    .child(label)))
            .child(actions)
            .child(div().text_color(theme.subtle).child(description))
            .when(recovery.cleanup_pending || recovery.checkpoint.is_some() || !recovery.staging_ownership.is_empty(), |element| element.child(div().text_color(theme.warning)
                .child(locale::text("原清理或暂存登记仍保留；登记不代表对象已创建，归档后可继续定位查看。", "Original cleanup or staging registration is retained. Registration does not prove creation; it remains locatable after archiving."))));
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

    if pane.queue.history_expanded {
        if let Some(error) = &pane.queue.archive_error {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_color(theme.warning)
                    .child(error.clone()),
            );
        }
        let mut paging = div()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .child(locale::text(
                "归档证据（非成功）",
                "Archived evidence, not successful",
            ))
            .child(
                Button::new("transfer-archive-first-page")
                    .compact()
                    .ghost()
                    .label(locale::text("刷新首页", "Refresh first page"))
                    .disabled(pane.queue.archive_loading)
                    .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                        pane.load_transfer_archive_page(None, cx)
                    })),
            );
        if let Some(cursor) = pane.queue.archive_next_cursor.clone() {
            paging = paging.child(
                Button::new("transfer-archive-next-page")
                    .compact()
                    .ghost()
                    .label(locale::text("下一页", "Next page"))
                    .disabled(pane.queue.archive_loading)
                    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                        pane.load_transfer_archive_page(Some(cursor.clone()), cx)
                    })),
            );
        }
        if pane.queue.archive_loading {
            paging = paging.child(locale::text("读取中…", "Loading…"));
        }
        list = list.child(paging);
    }

    if !has_rows && pane.queue.history_expanded {
        list.child(
            div()
                .px_3()
                .py_2()
                .text_color(theme.subtle)
                .text_size(theme.text_xs)
                .child(locale::text(
                    "没有可显示的历史记录。",
                    "No history entries to show.",
                )),
        )
        .into_any_element()
    } else {
        list.into_any_element()
    }
}

/// 让未知提交和待清理资源始终位于队列视线内，普通终态才由历史折叠控制。
///
/// @author kongweiguang
struct TransferQueueProjection<'a> {
    visible_rows: Vec<&'a TransferSummary>,
    clearable_history_count: usize,
    active_or_attention_count: usize,
}

/// 按同一组终态规则生成可见行、清理计数和待处理计数，避免未知提交被折叠成普通历史。
///
/// @author kongweiguang
fn project_transfer_queue(
    summaries: &[TransferSummary],
    history_expanded: bool,
) -> TransferQueueProjection<'_> {
    let clearable_history_count = summaries
        .iter()
        .filter(|summary| is_clearable_history(summary))
        .count();
    let active_or_attention_count = summaries
        .iter()
        .filter(|summary| needs_attention_or_is_active(summary))
        .count();
    let mut visible_rows = summaries
        .iter()
        .filter(|summary| {
            !is_terminal(summary.status)
                || summary.cleanup_pending
                || is_unknown_commit(summary)
                || history_expanded
        })
        .collect::<Vec<_>>();
    visible_rows.sort_by(|left, right| {
        let rank = |summary: &&TransferSummary| {
            if summary.cleanup_pending || is_unknown_commit(summary) {
                0
            } else if !is_terminal(summary.status) {
                1
            } else {
                2
            }
        };
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });

    TransferQueueProjection {
        visible_rows,
        clearable_history_count,
        active_or_attention_count,
    }
}

/// 顶栏清理入口与队列行使用相同的终态规则，未知提交和清理待办不会被误计为可删除。
///
/// @author kongweiguang
pub(super) fn clearable_history_count(summaries: &[TransferSummary]) -> usize {
    project_transfer_queue(summaries, false).clearable_history_count
}

/// 仅普通已结束记录计入可清理历史，未知回执与待清理资源必须保留给用户处理。
///
/// @author kongweiguang
fn is_clearable_history(summary: &TransferSummary) -> bool {
    is_terminal(summary.status) && !summary.cleanup_pending && !is_unknown_commit(summary)
}

/// 未知提交即使 status 为 Failed 也属于待核对项，折叠历史时保持可见并计入注意项。
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
