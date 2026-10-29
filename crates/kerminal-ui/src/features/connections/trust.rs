//! SSH 主机指纹首次信任与变化确认界面。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::connections::HostKeyStatus;

use crate::theme::ThemeTokens;

use super::panel::{ConnectionsPanel, PendingTrust, host_key_trust_allowed};
use kerminal_domain::preferences::ConfigurationMode;

/// 展示完整指纹并沿用应用的决策遮罩；变化密钥或只读恢复状态不提供继续连接的信任动作。
/// @author kongweiguang
pub(super) fn render_trust_confirmation(
    pending: Option<&PendingTrust>,
    configuration_mode: ConfigurationMode,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<impl IntoElement> {
    let pending = pending?;
    let challenge = &pending.challenge;
    let changed = matches!(challenge.status, HostKeyStatus::Changed { .. });
    let trust_allowed = host_key_trust_allowed(&challenge.status, configuration_mode);
    let cancel_requested = pending
        .operation_cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.is_cancelled());
    let previous = match &challenge.status {
        HostKeyStatus::Changed {
            previous_fingerprint_sha256,
        } => Some(previous_fingerprint_sha256.as_str()),
        HostKeyStatus::Known | HostKeyStatus::Unknown => None,
    };
    let rotation_allowed = matches!(
        &challenge.status,
        HostKeyStatus::Changed {
            previous_fingerprint_sha256
        } if previous_fingerprint_sha256 != &challenge.fingerprint_sha256
    ) && configuration_mode != ConfigurationMode::ReadOnlyRecovery;
    let headline = if changed {
        crate::locale::text("远端主机指纹已变化", "Remote host key changed")
    } else {
        crate::locale::text("首次连接到此主机", "First connection to this host")
    };
    let button_label = if pending.operation_busy {
        crate::locale::text("正在验证…", "Verifying…")
    } else if pending.second_confirmation {
        crate::locale::text("信任指纹并继续", "Trust key and continue")
    } else {
        crate::locale::text("我已核对指纹，继续", "I verified the key; continue")
    };
    let blocked_reason = if changed && previous == Some(challenge.fingerprint_sha256.as_str()) {
        Some(crate::locale::text(
            "此密钥已标记为撤销，不能通过轮换继续。请核实服务端身份并检查 known_hosts 撤销记录。",
            "This key is revoked and cannot be accepted through rotation. Verify the server identity and review the known_hosts revocation entry.",
        ))
    } else if changed && configuration_mode == ConfigurationMode::ReadOnlyRecovery {
        Some(crate::locale::text(
            "此指纹与已保存值不同，连接已阻止。配置处于只读恢复模式；先恢复写入能力，再核实密钥轮换。",
            "This key differs from the saved value, so the connection is blocked. Configuration is read-only during recovery; restore writes and verify the key rotation first.",
        ))
    } else if changed {
        Some(crate::locale::text(
            "连接已阻止。请通过独立可信渠道核对主机、端口、旧指纹和新指纹；确认后可明确替换此端点的旧记录。",
            "The connection is blocked. Verify the host, port, old fingerprint, and new fingerprint through an independent trusted channel; then explicitly replace the old record for this endpoint.",
        ))
    } else if configuration_mode == ConfigurationMode::ReadOnlyRecovery {
        Some(crate::locale::text(
            "配置处于只读恢复模式，无法写入新的 known_hosts 记录，因此不会继续连接。请先恢复配置后重新连接。",
            "Configuration is in read-only recovery mode, so a new known_hosts entry cannot be written and the connection will not continue. Recover configuration before reconnecting.",
        ))
    } else {
        None
    };
    let trust_button = trust_allowed.then(|| {
        Button::new("connections-hostkey-confirm")
            .compact()
            .primary()
            .disabled(pending.operation_busy)
            .label(button_label)
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.confirm_host_key(cx)))
    });
    let rotation_button = rotation_allowed.then(|| {
        Button::new("connections-hostkey-rotate")
            .compact()
            .primary()
            .disabled(pending.operation_busy)
            .label(if pending.operation_busy {
                crate::locale::text("正在核对…", "Verifying…")
            } else if pending.second_confirmation {
                crate::locale::text("再次确认并替换", "Confirm replacement")
            } else {
                crate::locale::text("已核实，替换并重试", "Verified; replace and retry")
            })
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                panel.confirm_host_key_rotation(cx);
            }))
    });
    Some(
        div()
            .id("connections-hostkey-confirmation")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.dialog_scrim)
            .child(
                div()
                    .w_full()
                    .max_w(px(510.))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(if changed { theme.error } else { theme.accent })
                    .rounded_md()
                    .text_color(theme.foreground)
                    .child(div().text_size(px(18.)).font_weight(gpui_kit::FontWeight::BOLD).child(headline))
                    .child(div().text_size(px(12.)).text_color(theme.muted).child(format!(
                        "{} · {} · {} · {} · {}",
                        pending
                            .display_host
                            .as_deref()
                            .unwrap_or(challenge.host_id.as_str()),
                        endpoint_label(&challenge.endpoint),
                        peer_label(&challenge.peer),
                        challenge.algorithm,
                        challenge.host_id.as_str(),
                    )))
                    .children(pending.operation_busy.then(|| {
                        div()
                            .w_full()
                            .p_3()
                            .rounded_sm()
                            .bg(theme.sidebar)
                            .text_size(px(12.))
                            .text_color(theme.warning)
                            .child(if cancel_requested {
                                crate::locale::text(
                                    "已请求取消；操作可能已经提交。正在等待真实回执并回读原端点。",
                                    "Cancellation requested. The write may already have committed; waiting for its receipt and reading back the original endpoint.",
                                )
                            } else {
                                crate::locale::text(
                                    "正在重新握手和写入主机信任。取消后仍会等待真实回执并回读原端点。",
                                    "Rechecking and writing host trust. A cancellation request still waits for the real receipt and reads back the original endpoint.",
                                )
                            })
                    }))
                    .children(pending.external_classification.as_ref().map(|classification| {
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(super::external::external_classification_text(classification))
                    }))
                    .child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded_sm()
                            .bg(theme.sidebar)
                            .text_size(px(13.))
                            .child(crate::locale::text(
                                format!("当前指纹：{}", challenge.fingerprint_sha256),
                                format!("Current fingerprint: {}", challenge.fingerprint_sha256),
                            )),
                    )
                    .children(previous.map(|fingerprint| {
                        div()
                            .w_full()
                            .p_3()
                            .rounded_sm()
                            .bg(theme.sidebar)
                            .text_size(px(12.))
                            .text_color(theme.error)
                            .child(crate::locale::text(
                                format!("已保存指纹：{fingerprint}"),
                                format!("Saved fingerprint: {fingerprint}"),
                            ))
                    }))
                    .child(div().text_size(px(12.)).text_color(theme.muted).child(
                        if changed {
                            crate::locale::text(
                                "变化可能表示服务器重装、密钥轮换或连接被拦截。请通过独立可信渠道核对，不要只凭本提示确认。",
                                "A changed key may indicate a server reinstall, key rotation, or interception. Verify it through an independent trusted channel before continuing.",
                            )
                        } else {
                            crate::locale::text(
                                "确认前应用不会保存此指纹，也不会继续建立终端或文件连接。请通过独立可信渠道核对。",
                                "The app will not save this key or continue to a terminal or file connection until you confirm. Verify it through an independent trusted channel.",
                            )
                        },
                    ))
                    .children(blocked_reason.map(|message| {
                        div()
                            .w_full()
                            .p_3()
                            .rounded_sm()
                            .bg(theme.sidebar)
                            .text_size(px(12.))
                            .text_color(theme.error)
                            .child(message)
                    }))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("connections-hostkey-cancel")
                                    .compact()
                                    .disabled(cancel_requested)
                                    .label(if cancel_requested {
                                        crate::locale::text("等待结果…", "Waiting for result…")
                                    } else if pending.operation_busy {
                                        crate::locale::text("请求取消", "Request cancel")
                                    } else {
                                        crate::locale::text("取消连接", "Cancel connection")
                                    })
                                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_pending_trust(cx))),
                            )
                            .children(trust_button)
                            .children(rotation_button),
                    ),
            ),
    )
}

/// 明确标记链路中出问题的目标或跳板，避免用户把跳板指纹误认为最终主机。
fn peer_label(peer: &kerminal_application::connections::HostKeyPeer) -> String {
    match peer {
        kerminal_application::connections::HostKeyPeer::Target => {
            crate::locale::text("最终主机", "Target host").to_string()
        }
        kerminal_application::connections::HostKeyPeer::Jump { index } => crate::locale::text(
            format!("第 {} 个跳板", index + 1),
            format!("Jump host {}", index + 1),
        )
        .to_string(),
    }
}

/// 在指纹确认页明确显示实际握手的 host:port，尤其避免跳板目标被显示成最终主机。
/// @author kongweiguang
fn endpoint_label(endpoint: &kerminal_domain::connections::HostEndpoint) -> String {
    if endpoint.host.contains(':') {
        format!("[{}]:{}", endpoint.host, endpoint.port)
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}
