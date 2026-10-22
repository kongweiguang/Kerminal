//! 配置同步状态与用户显式 Git 操作的原生设置页面。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, StatefulInteractiveElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::configuration_sync::{
    ConfigurationSyncOperationKind, ConfigurationSyncOperationResult, ConfigurationSyncOutcome,
    SyncRepositoryState,
};

use crate::theme::ThemeTokens;

use super::panel::PreferencesPanel;

impl PreferencesPanel {
    /// 以应用状态驱动 Fetch、Pull、本地提交和 Push 的可用性，避免将 commit 显示为已同步。
    pub(super) fn render_configuration_sync(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status = self.sync_status.clone();
        let busy = self.sync_loading || self.sync_busy.is_some();
        let sync_running = self.sync_busy.is_some();
        let cancel_requested = self.sync_cancel_requested();
        let service_missing = self.configuration_sync.is_none();
        let repository_ready = status.as_ref().is_some_and(|status| {
            status.git_available
                && status.writable
                && status.repository == SyncRepositoryState::Ready
        });
        let rebase = status
            .as_ref()
            .is_some_and(|status| status.rebase_in_progress);
        let has_conflicts = status
            .as_ref()
            .is_some_and(|status| !status.conflicts.is_empty());
        let can_fetch = repository_ready && !rebase && !busy;
        let can_pull = repository_ready
            && !rebase
            && !has_conflicts
            && !self.sync_outcome_unknown
            && status.as_ref().is_some_and(|status| {
                status.upstream.is_some() && status.behind.unwrap_or_default() > 0
            })
            && !busy;
        let can_commit = repository_ready
            && !has_conflicts
            && !self.sync_outcome_unknown
            && status
                .as_ref()
                .is_some_and(|status| !status.local_changes.is_empty())
            && !busy;
        let can_push = repository_ready
            && !rebase
            && !has_conflicts
            && !self.sync_outcome_unknown
            && status.as_ref().is_some_and(|status| {
                status.upstream.is_some() && status.ahead.unwrap_or_default() > 0
            })
            && !busy;
        let can_continue = repository_ready && rebase && !busy;
        let can_abort = repository_ready && rebase && !busy;

        let mut body = div()
            .id("preferences-configuration-sync")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(theme.background)
            .child(sync_heading(theme));

        if service_missing {
            body = body.child(sync_note(
                sync_text(
                    "配置同步服务尚未装配；此页面当前只展示不可用状态。",
                    "Configuration sync is unavailable because the service is not configured.",
                ),
                theme.error,
            ));
        }
        if self.sync_outcome_unknown {
            body = body.child(sync_note(
                sync_text(
                    "上次操作结果未知。请 Fetch 并核对远端状态，确认前不会自动重试。",
                    "The previous result is unknown. Fetch and verify the remote state; no retry will happen automatically.",
                ),
                theme.accent,
            ));
        }
        if cancel_requested {
            body = body.child(sync_note(
                crate::locale::text(
                    "已请求取消，正在等待 Git 清理回执。",
                    "Cancellation requested. Waiting for Git cleanup receipt.",
                )
                .to_string(),
                theme.accent,
            ));
        }
        if let Some(error) = self.sync_error.as_deref() {
            body = body.child(sync_note(error, theme.error));
        }

        let operation_buttons = div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("sync-refresh")
                    .compact()
                    .label(if self.sync_loading {
                        crate::locale::text("读取中…", "Loading…")
                    } else {
                        crate::locale::text("刷新状态", "Refresh status")
                    })
                    .disabled(busy || service_missing)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.refresh_configuration_sync(cx);
                    })),
            )
            .child(sync_button(
                "sync-fetch",
                crate::locale::text("Fetch 远端", "Fetch remote"),
                can_fetch,
                ConfigurationSyncOperationKind::Fetch,
                cx,
            ))
            .when(sync_running, |row| {
                row.child(
                    Button::new("sync-cancel")
                        .compact()
                        .label(if cancel_requested {
                            crate::locale::text("等待清理…", "Waiting for cleanup…")
                        } else {
                            crate::locale::text("取消当前同步", "Cancel sync")
                        })
                        .disabled(cancel_requested)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_configuration_sync(cx);
                        })),
                )
            })
            .child(sync_button(
                "sync-pull",
                crate::locale::text("Pull 并变基", "Pull and rebase"),
                can_pull,
                ConfigurationSyncOperationKind::Pull,
                cx,
            ))
            .child(sync_button(
                "sync-commit",
                crate::locale::text("提交本地变更", "Commit local changes"),
                can_commit,
                ConfigurationSyncOperationKind::Commit,
                cx,
            ))
            .child(sync_button(
                "sync-push",
                crate::locale::text("Push 到远端", "Push to remote"),
                can_push,
                ConfigurationSyncOperationKind::Push,
                cx,
            ))
            .when(rebase, |row| {
                row.child(sync_button(
                    "sync-continue-rebase",
                    crate::locale::text("继续变基", "Continue rebase"),
                    can_continue,
                    ConfigurationSyncOperationKind::ContinueRebase,
                    cx,
                ))
                .child(sync_button(
                    "sync-abort-rebase",
                    crate::locale::text("中止变基", "Abort rebase"),
                    can_abort,
                    ConfigurationSyncOperationKind::AbortRebase,
                    cx,
                ))
            });
        body = body.child(
            sync_card(sync_text("同步操作", "Sync actions"), theme).child(operation_buttons),
        );

        if let Some(status) = status {
            body = body.child(sync_state_card(&status, theme));
            if !status.local_changes.is_empty() {
                let rows = status
                    .local_changes
                    .iter()
                    .map(|entry| sync_path_row(entry.path.clone(), path_flags(entry), theme))
                    .collect::<Vec<_>>();
                body = body.child(
                    sync_card(sync_text("可同步的本地变更", "Local changes"), theme)
                        .child(div().flex().flex_col().gap_1().children(rows)),
                );
            }
            if !status.protected_changes.is_empty() {
                let rows = status
                    .protected_changes
                    .iter()
                    .map(|entry| {
                        sync_path_row(entry.path.clone(), sync_text("受保护", "Protected"), theme)
                    })
                    .collect::<Vec<_>>();
                body = body.child(
                    sync_card(sync_text("受保护文件", "Protected files"), theme)
                        .child(crate::locale::text("这些凭据与事务文件不会进入配置提交。", "Credentials and transaction files are excluded from configuration commits."))
                        .child(div().flex().flex_col().gap_1().children(rows)),
                );
            }
            if !status.conflicts.is_empty() {
                let rows = status
                    .conflicts
                    .iter()
                    .map(|path| {
                        sync_path_row(path.clone(), sync_text("需处理冲突", "Conflict"), theme)
                    })
                    .collect::<Vec<_>>();
                body = body.child(
                    sync_card(sync_text("变基冲突", "Rebase conflicts"), theme)
                        .child(crate::locale::text("先在工作区解决下面的冲突，再继续变基或显式中止。", "Resolve these conflicts in the workspace before continuing or aborting the rebase."))
                        .child(div().flex().flex_col().gap_1().children(rows)),
                );
            }
            if !status.missing_ignore_rules.is_empty() {
                let rows = status
                    .missing_ignore_rules
                    .iter()
                    .map(|rule| {
                        sync_path_row(
                            rule.clone(),
                            sync_text("建议忽略规则", "Suggested ignore rule"),
                            theme,
                        )
                    })
                    .collect::<Vec<_>>();
                body = body.child(
                    sync_card(
                        sync_text("缺少的保护规则", "Missing protection rules"),
                        theme,
                    )
                    .child(div().flex().flex_col().gap_1().children(rows)),
                );
            }
        } else if self.sync_loading {
            body = body.child(
                sync_card(sync_text("仓库状态", "Repository status"), theme).child(
                    crate::locale::text(
                        "正在读取本地配置仓库状态…",
                        "Reading local configuration repository status…",
                    ),
                ),
            );
        } else if !service_missing {
            body = body.child(
                sync_card(sync_text("仓库状态", "Repository status"), theme).child(
                    crate::locale::text(
                        "尚未读取仓库状态。",
                        "Repository status has not been read.",
                    ),
                ),
            );
        }

        if let Some(result) = self.sync_result.as_ref() {
            body = body.child(sync_operation_card(result, theme));
        }
        body
    }
}

/// 每个 Git 动作都有独立 ID 和 application operation kind，按钮只发出显式用户意图。
fn sync_button(
    id: &'static str,
    label: impl Into<gpui_kit::SharedString>,
    enabled: bool,
    kind: ConfigurationSyncOperationKind,
    cx: &mut Context<PreferencesPanel>,
) -> Button {
    Button::new(id)
        .compact()
        .label(label)
        .primary()
        .disabled(!enabled)
        .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
            panel.run_configuration_sync(kind, cx);
        }))
}

/// 仓库状态以实际上游、分支和 ahead/behind 显示，避免把未推送提交误标为已同步。
fn sync_state_card(
    status: &kerminal_application::configuration_sync::ConfigurationSyncStatus,
    theme: ThemeTokens,
) -> impl IntoElement {
    let repository = match status.repository {
        SyncRepositoryState::Missing => {
            sync_text("未初始化 Git 仓库", "Git repository is not initialized")
        }
        SyncRepositoryState::Ready => sync_text("仓库已就绪", "Repository is ready"),
        SyncRepositoryState::Detached => sync_text("当前处于 detached HEAD", "HEAD is detached"),
        SyncRepositoryState::NoUpstream => {
            sync_text("当前分支没有配置上游", "Current branch has no upstream")
        }
        SyncRepositoryState::PendingBusy => crate::locale::text(
            "正在完成上次 Git 清理，工作区暂不可操作",
            "Finishing cleanup from the previous Git command; sync is temporarily unavailable",
        )
        .to_string(),
    };
    let branch = status.branch.as_deref().unwrap_or("-");
    let upstream = status.upstream.as_deref().unwrap_or("-");
    let ahead = status
        .ahead
        .map(|count| count.to_string())
        .unwrap_or_else(|| sync_text("未知", "Unknown"));
    let behind = status
        .behind
        .map(|count| count.to_string())
        .unwrap_or_else(|| sync_text("未知", "Unknown"));
    sync_card(sync_text("仓库状态", "Repository status"), theme)
        .child(sync_text_row(
            "Git",
            if status.git_available {
                sync_text("可用", "Available")
            } else {
                sync_text("不可用", "Unavailable")
            },
            theme,
        ))
        .child(sync_text_row(
            sync_text("仓库", "Repository"),
            repository,
            theme,
        ))
        .child(sync_text_row(
            sync_text("本地权限", "Local access"),
            if status.writable {
                sync_text("可写", "Writable")
            } else {
                sync_text("只读", "Read only")
            },
            theme,
        ))
        .child(sync_text_row(sync_text("分支", "Branch"), branch, theme))
        .child(sync_text_row(
            sync_text("上游", "Upstream"),
            upstream,
            theme,
        ))
        .child(sync_text_row(
            sync_text("领先 / 落后", "Ahead / behind"),
            &format!("{ahead} / {behind}"),
            theme,
        ))
        .when(status.rebase_in_progress, |card| {
            card.child(sync_note(
                sync_text("变基正在进行中。", "A rebase is in progress."),
                theme.accent,
            ))
        })
}

/// 显示 application 返回的操作种类、终态、提交摘要和受影响路径。
fn sync_operation_card(
    result: &kerminal_application::configuration_sync::ConfigurationSyncOperationResult,
    theme: ThemeTokens,
) -> impl IntoElement {
    let kind = match result.kind {
        ConfigurationSyncOperationKind::Fetch => sync_text("Fetch 远端引用", "Fetch remote refs"),
        ConfigurationSyncOperationKind::Pull => sync_text("Pull 并变基", "Pull and rebase"),
        ConfigurationSyncOperationKind::Commit => sync_text("本地提交", "Local commit"),
        ConfigurationSyncOperationKind::Push => sync_text("Push 远端", "Push to remote"),
        ConfigurationSyncOperationKind::ContinueRebase => sync_text("继续变基", "Continue rebase"),
        ConfigurationSyncOperationKind::AbortRebase => sync_text("中止变基", "Abort rebase"),
    };
    let outcome = match result.outcome {
        ConfigurationSyncOutcome::Succeeded => sync_text("已完成", "Completed"),
        ConfigurationSyncOutcome::NoChanges => {
            sync_text("没有可处理的变更", "No changes to process")
        }
        ConfigurationSyncOutcome::Conflict => sync_text("遇到冲突", "Conflicts found"),
        ConfigurationSyncOutcome::Cancelled => sync_text("已取消", "Cancelled"),
        ConfigurationSyncOutcome::Blocked => sync_text("被安全规则阻止", "Blocked by safety rules"),
        ConfigurationSyncOutcome::OutcomeUnknown => {
            sync_text("结果未知，需检查状态", "Unknown result; verify status")
        }
    };
    let mut card = sync_card(sync_text("最近一次操作", "Last operation"), theme)
        .child(sync_text_row(sync_text("动作", "Action"), kind, theme))
        .child(sync_text_row(sync_text("结果", "Result"), outcome, theme))
        .child(sync_operation_summary(result));
    if let Some(hash) = result.commit_hash.as_deref() {
        card = card.child(sync_text_row(sync_text("提交", "Commit"), hash, theme));
    }
    if !result.affected_paths.is_empty() {
        let rows = result
            .affected_paths
            .iter()
            .map(|path| sync_path_row(path.clone(), sync_text("受影响", "Affected"), theme))
            .collect::<Vec<_>>();
        card = card.child(div().flex().flex_col().gap_1().children(rows));
    }
    card
}

/// 英文界面不直接展示 application 当前固定为中文的 Git 摘要，改由稳定类型生成安全说明。
fn sync_operation_summary(result: &ConfigurationSyncOperationResult) -> String {
    let en_us = match (result.kind, result.outcome) {
        (ConfigurationSyncOperationKind::Fetch, ConfigurationSyncOutcome::Succeeded) => {
            "Remote references were fetched."
        }
        (ConfigurationSyncOperationKind::Pull, ConfigurationSyncOutcome::Succeeded) => {
            "Remote changes were applied to the local branch."
        }
        (ConfigurationSyncOperationKind::Commit, ConfigurationSyncOutcome::Succeeded) => {
            "A local commit was created."
        }
        (ConfigurationSyncOperationKind::Push, ConfigurationSyncOutcome::Succeeded) => {
            "The remote confirmed the push."
        }
        (ConfigurationSyncOperationKind::ContinueRebase, ConfigurationSyncOutcome::Succeeded) => {
            "The rebase completed."
        }
        (ConfigurationSyncOperationKind::AbortRebase, ConfigurationSyncOutcome::Succeeded) => {
            "The branch state from before the rebase was restored."
        }
        (ConfigurationSyncOperationKind::Push, ConfigurationSyncOutcome::NoChanges) => {
            "There are no local commits waiting to be pushed."
        }
        (_, ConfigurationSyncOutcome::NoChanges) => "There were no changes to process.",
        (_, ConfigurationSyncOutcome::Conflict) => {
            "Conflicts were found. Resolve the listed paths before continuing."
        }
        (_, ConfigurationSyncOutcome::Cancelled) => {
            "The operation was cancelled. Review repository status before retrying."
        }
        (_, ConfigurationSyncOutcome::Blocked) => {
            "Repository or safety checks blocked the operation. Review the status and affected paths."
        }
        (_, ConfigurationSyncOutcome::OutcomeUnknown) => {
            "The result is unknown. Refresh repository status before retrying."
        }
    };
    crate::locale::text(result.summary.clone(), en_us).to_string()
}

/// 为状态路径汇总其 stage 状态，UI 不读取工作树文件内容。
fn path_flags(path: &kerminal_application::configuration_sync::SyncPathStatus) -> String {
    let mut flags = Vec::new();
    if path.staged {
        flags.push(sync_text("已暂存", "Staged"));
    }
    if path.unstaged {
        flags.push(sync_text("已修改", "Modified"));
    }
    if path.untracked {
        flags.push(sync_text("未跟踪", "Untracked"));
    }
    if path.conflicted {
        flags.push(sync_text("冲突", "Conflict"));
    }
    if flags.is_empty() {
        sync_text("状态未知", "Unknown status")
    } else {
        flags.join(" · ")
    }
}

/// 同步页面标题说明 Git 动作分别执行，不会在保存配置时暗中联网。
fn sync_heading(theme: ThemeTokens) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(crate::locale::text("配置同步", "Configuration sync")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(crate::locale::text("Fetch、Pull、本地提交和 Push 都由你分别触发。", "Fetch, Pull, local commit, and Push are separate actions that you trigger explicitly.")),
        )
}

/// 以统一边框和主题面板色组织同步状态块，浅色与深色都消费共享令牌。
fn sync_card(title: impl Into<String>, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .bg(theme.panel)
        .border_1()
        .border_color(theme.muted)
        .rounded_md()
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// 状态说明用主题警告或错误色显示，其余正文继承全局前景色。
fn sync_note(text: impl Into<String>, color: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .w_full()
        .p_3()
        .text_size(px(12.))
        .text_color(color)
        .child(text.into())
}

/// 标签和值使用固定栏宽，以便状态面板在窄窗口中仍易扫描。
fn sync_text_row(
    label: impl Into<String>,
    value: impl Into<String>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .gap_3()
        .child(
            div()
                .w(px(100.))
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(label.into()),
        )
        .child(div().flex_1().text_size(px(12.)).child(value.into()))
}

/// 同步路径行持有路径与状态文本，避免 GPUI 元素捕获短生命周期借用。
///
/// @author kongweiguang
fn sync_path_row(path: String, status: String, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .gap_3()
        .child(div().flex_1().min_w(px(0.)).text_size(px(12.)).child(path))
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(status),
        )
}

/// 同步视图文案必须同时适配 Kit 当前 locale，业务字段本身始终保持原值。
fn sync_text(zh_cn: &str, en_us: &str) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}
