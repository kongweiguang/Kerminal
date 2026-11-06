//! 文件栏和传输工作台共享最近任务投影及按需展开的恢复材料。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement,
    StatefulInteractiveElement as _,
    component::{
        Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_domain::files::{
    TransferFailureKind, TransferKind, TransferPhase, TransferStatus, TransferSummary,
};
use kerminal_domain::identity::TransferId;

use crate::{locale, theme::ThemeTokens};

use super::{format_size, icon_button, transfer_status};
use crate::features::files::FilesPane;
use crate::features::files::transfer_queue::is_clearable_history;

/// 标题本身控制完整列表显隐，按钮保留键盘语义，清理动作与折叠点击各自独立。
/// 折叠仍提示隐藏的活动及恢复事项；终态清理先释放资源，未知提交继续保留。
/// 列表复用实际字节和提交状态计算百分比；上传下载标明方向，暂存只报告准备就绪，不冒充最终复制完成。
/// 窄栏允许元数据换行以保留完整单位，临时拖出任务不暴露已回收路径的定位或重试。
/// 系统目标尚未释放副本时，即使暂存成功也保留待处理提示并拒绝显示可清理历史。
/// 日常列表仅显示传输与待处理恢复项，不提供归档材料浏览；底层恢复证据继续独立保存。
/// 可恢复的取消显示为暂停并保留原任务；普通失败使用创建后继的重试。
/// 失败卡片只提供有资格的单一重试或续传，未知提交不能触发重试。
/// 失败与取消只显示业务结果和具体原因；内部清理由动作承接，终态不继续展示过期速率。
/// 队列最多使用面板约三分之一的高度，短窗口优先保留文件浏览区；全部任务仍可滚动查看。
/// 空队列只保留标题，续传与重试使用图标及悬停说明，避免重复文案挤占任务内容。
/// 空列表读取时也不增加占位正文；续传受理期间禁用原按钮并在原位显示等待状态。
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
    let has_clearable_recovery =
        orphan_recovery_records(&pane.transfer_recovery_rows, &pane.transfer_rows)
            .iter()
            .any(|record| record.can_dismiss_without_live_task());
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
    // 同一传输以稳定 TransferId 关联实时摘要与恢复记录，只渲染一张卡片。
    let recovery_by_id = recovery_rows
        .iter()
        .map(|record| (record.id.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let clearing_history = pane
        .queue
        .is_clearing_for_origin(pane.transfer_origin.as_ref());
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
    } else if projection.clearable_history_count > 0 || has_clearable_recovery {
        header = header.child(
            Button::new("files-transfer-clear-completed")
                .compact()
                .ghost()
                .debug_selector(|| "rr-files-transfer-clear".into())
                .icon(gpui_kit::assets::IconName::Trash)
                .tooltip(locale::text("清理已结束的传输", "Clear finished transfers"))
                .accessibility_label(locale::text("清理已结束的传输", "Clear finished transfers"))
                .disabled(pane.queue.has_pending_retry() || pane.queue.is_clearing_history())
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
        let locate_id = id.clone();
        let recovery = recovery_by_id.get(&id).copied();
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
        // 恢复记录已确认提交未知时以它为准，避免同一张卡片既写“失败”又写“结论未知”。
        let commit_unknown = summary.failure_kind == Some(TransferFailureKind::CommitUnknown)
            || recovery.is_some_and(|recovery| recovery.commit_unknown);
        let result_status = if commit_unknown {
            locale::text(
                "提交结果未知，请手动核对目标",
                "Commit result unknown; inspect the destination manually",
            )
            .to_string()
        } else if is_paused(summary) {
            locale::text("已暂停", "Paused").to_string()
        } else if drag_staging && summary.status == TransferStatus::Succeeded {
            locale::text("已就绪", "Ready").to_string()
        } else {
            transfer_status(summary.status).to_string()
        };
        let result_status = format!("{direction} · {result_status}");
        let status = match summary.phase {
            Some(phase) => format!(
                "{result_status} · {}",
                transfer_phase_label(phase, summary.bytes_transferred)
            ),
            None => result_status,
        };
        let status = if summary.cleanup_pending
            && !matches!(
                summary.status,
                TransferStatus::Failed | TransferStatus::Cancelled
            ) {
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
            TransferStatus::Cancelled if is_paused(summary) => IconName::Pause,
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
                TransferStatus::Cancelled if is_paused(summary) => theme.muted,
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
        if !drag_staging
            && !matches!(
                summary.status,
                TransferStatus::Failed | TransferStatus::Cancelled
            )
        {
            let locate_label = locale::text("定位目标目录", "Show destination folder");
            let locate_tooltip = format!("{locate_label}\n{destination_location_tooltip}");
            let locate_button = crate::ui_button::icon_button(
                format!("files-locate-destination-{}", summary.id),
                gpui_kit::assets::IconName::Locate,
                locate_label.clone(),
            )
            .with_size(gpui_kit::component::Size::Medium)
            .w(px(28.))
            .h(px(26.))
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                if pane.transfer_rows.iter().any(|row| row.id == locate_id) {
                    pane.locate_transfer_destination(destination_for_location.clone(), cx)
                }
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
        let speed = (!is_terminal(summary.status) && summary.speed_bytes_per_second > 0)
            .then(|| format!("{}/s", format_size(Some(summary.speed_bytes_per_second))));
        let resume_label =
            can_resume_transfer(summary).then(|| locale::text("可续传", "Resumable"));

        if (summary.retryable || can_resume_transfer(summary))
            && summary.retry_successor.is_none()
            && matches!(
                summary.status,
                TransferStatus::Failed | TransferStatus::Cancelled
            )
            && !drag_staging
            && !commit_unknown
        {
            let resume = can_resume_transfer(summary);
            let retrying = pane.queue.is_retrying_transfer(&summary.id);
            let retry_label = if resume {
                locale::text("继续传输", "Resume transfer")
            } else {
                locale::text("重试传输", "Retry transfer")
            };
            title = title.child(
                Button::new(format!("files-retry-{}", summary.id))
                    .compact()
                    .ghost()
                    .flex_shrink_0()
                    .icon(if resume {
                        IconName::Play
                    } else {
                        IconName::RefreshCw
                    })
                    .tooltip(retry_label.clone())
                    .accessibility_label(retry_label)
                    .loading(retrying)
                    .disabled(retrying || pane.queue.is_clearing_history())
                    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                        pane.retry_transfer(retry_id.clone(), resume, cx)
                    })),
            );
        }
        if !is_terminal(summary.status) {
            let pause = !drag_staging && can_pause_transfer(summary);
            title = title.child(
                icon_button(
                    format!("files-cancel-{}", summary.id),
                    if pause {
                        IconName::Pause
                    } else {
                        IconName::Close
                    },
                    if pause {
                        locale::text("暂停传输", "Pause transfer")
                    } else {
                        locale::text("取消传输", "Cancel transfer")
                    },
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
        if summary.cleanup_pending
            && !matches!(
                summary.status,
                TransferStatus::Failed | TransferStatus::Cancelled
            )
            && recovery.is_none_or(|recovery| !recovery.commit_unknown)
        {
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

    // 恢复记录只补没有实时摘要的孤儿任务；同一传输已在上面合并为一张卡片。
    for recovery in orphan_recovery_records(&recovery_rows, &pane.transfer_rows) {
        let id = recovery.id.clone();
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
        list = list.child(
            div()
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
                .child(
                    div()
                        .text_color(theme.warning)
                        .child(if recovery.commit_unknown {
                            locale::text(
                                "提交结果未知，请手动核对目标。",
                                "Commit result unknown; inspect the destination manually.",
                            )
                        } else {
                            locale::text(
                                "上次传输的结果不可用。",
                                "The previous transfer result is unavailable.",
                            )
                        }),
                ),
        );
    }

    div()
        .id("files-transfer-queue")
        .debug_selector(|| "files-transfer-queue".into())
        .w_full()
        .max_h(relative(0.35))
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

/// 同一 TransferId 只保留一张卡片：恢复记录仅在缺少实时摘要时才单独成行，
/// 否则它的动作与结论已并入摘要卡片，重复行会让同一任务出现两套口径。
///
/// @author kongweiguang
fn orphan_recovery_records<'a>(
    recovery: &'a [kerminal_application::transfers::TransferRecoveryRecord],
    summaries: &[TransferSummary],
) -> Vec<&'a kerminal_application::transfers::TransferRecoveryRecord> {
    let summary_ids = summaries
        .iter()
        .map(|summary| &summary.id)
        .collect::<BTreeSet<_>>();
    recovery
        .iter()
        .filter(|record| !summary_ids.contains(&record.id))
        .collect()
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

/// 未知提交即使 status 为 Failed 也属于待核对项，不能当作可清理的普通历史。
///
/// @author kongweiguang
fn needs_attention_or_is_active(summary: &TransferSummary) -> bool {
    !is_terminal(summary.status)
        || summary.cleanup_pending
        || is_unknown_commit(summary)
        || is_paused(summary)
}

/// 只有可信检查点支撑的取消才保留为暂停，普通取消不承诺后续续传。
///
/// @author kongweiguang
fn is_paused(summary: &TransferSummary) -> bool {
    summary.status == TransferStatus::Cancelled && summary.resumable
}

/// 可信断点只能由尚未产生后继的原任务继续，避免旧卡片显示应用层必然拒绝的入口。
///
/// @author kongweiguang
fn can_resume_transfer(summary: &TransferSummary) -> bool {
    summary.resumable
        && summary.retry_successor.is_none()
        && matches!(
            summary.status,
            TransferStatus::Cancelled | TransferStatus::Failed
        )
}

/// 只对已有确认字节且尚未提交的活动单文件提供暂停入口。
///
/// @author kongweiguang
fn can_pause_transfer(summary: &TransferSummary) -> bool {
    summary.request.kind == TransferKind::File
        && summary.status == TransferStatus::Transferring
        && summary.phase != Some(TransferPhase::Committing)
        && summary.bytes_transferred > 0
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

/// 有确认前缀时明确区分续传断点核对与首次源文件校验。
///
/// @author kongweiguang
fn transfer_phase_label(phase: TransferPhase, confirmed_bytes: u64) -> String {
    locale::text(
        match phase {
            TransferPhase::Preparing => "准备传输",
            TransferPhase::Transferring => "传输数据",
            TransferPhase::Verifying if confirmed_bytes > 0 => "核对续传断点",
            TransferPhase::Verifying => "校验源文件",
            TransferPhase::Committing => "提交目标文件",
        },
        match phase {
            TransferPhase::Preparing => "Preparing transfer",
            TransferPhase::Transferring => "Transferring data",
            TransferPhase::Verifying if confirmed_bytes > 0 => "Checking resume checkpoint",
            TransferPhase::Verifying => "Verifying source file",
            TransferPhase::Committing => "Committing destination",
        },
    )
    .to_string()
}

#[cfg(test)]
#[path = "../../../../tests/unit/files_transfer_history_projection.rs"]
mod tests;
