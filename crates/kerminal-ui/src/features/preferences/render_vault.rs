//! Vault readiness、密钥备份、导入和配对恢复页面。
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
use kerminal_domain::preferences::ConfigurationMode;

use crate::theme::ThemeTokens;

use super::panel::{PreferencesPanel, availability_label, vault_action_policy};

impl PreferencesPanel {
    /// Vault 文本只呈现 typed readiness 与操作回执，密钥正文全程由应用 use case 管理。
    pub(super) fn render_vault(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let read_locked = self.vault_busy || self.vault_loading;
        let availability = self.vault_availability.clone();
        let policy = vault_action_policy(
            availability.as_ref(),
            self.recovery.mode == ConfigurationMode::ReadOnlyRecovery,
        );
        let service_missing = self.vault_keys.is_none();
        let mut body = div()
            .id("preferences-vault")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(theme.background)
            .child(vault_heading(theme));

        if service_missing {
            body = body.child(vault_note(
                crate::locale::text(
                    "Vault 密钥服务尚未由应用装配；当前没有可执行的密钥操作。",
                    "The Vault key service is unavailable; key operations cannot run.",
                ),
                theme.error,
            ));
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            body = body.child(vault_note(
                crate::locale::text(
                    "配置处于只读恢复状态，所有密钥写操作均已锁定。",
                    "Configuration is in read-only recovery mode. Vault writes are locked.",
                ),
                theme.error,
            ));
        }
        if let Some(error) = self.vault_error.as_deref() {
            body = body.child(vault_note(error, theme.error));
        }
        if self.vault_outcome_unknown {
            body = body.child(vault_note(
                crate::locale::text("上次密钥操作结果未知。请检查当前状态；后续写入需要重新验证并确认。", "The previous key operation has an unknown result. Check current readiness, then rerun validation and confirm before another write."),
                theme.error,
            ));
        }
        if let Some(status) = self.vault_status.as_deref() {
            body = body.child(vault_note(status, theme.accent));
        }

        let status_value = availability
            .as_ref()
            .map(availability_label)
            .unwrap_or_else(|| {
                if self.vault_loading {
                    crate::locale::text("正在检查 Vault 状态…", "Checking Vault status…")
                        .to_string()
                } else {
                    crate::locale::text("尚未读取 Vault 状态。", "Vault status has not been read.")
                        .to_string()
                }
            });
        let actions = div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("vault-refresh")
                    .compact()
                    .label(if self.vault_loading {
                        crate::locale::text("检查中…", "Checking…")
                    } else {
                        crate::locale::text("检查状态", "Check status")
                    })
                    .disabled(read_locked || service_missing)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.refresh_vault_availability(cx);
                    })),
            )
            .when(policy.ensure, |actions| {
                actions.child(
                    Button::new("vault-ensure")
                        .compact()
                        .label(crate::locale::text(
                            "确保密钥可用",
                            "Ensure key is available",
                        ))
                        .disabled(read_locked || service_missing || !policy.ensure)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.ensure_vault_key(cx);
                        })),
                )
            })
            .when(policy.create, |actions| {
                actions.child(
                    Button::new("vault-create")
                        .compact()
                        .primary()
                        .label(crate::locale::text("创建新密钥", "Create key"))
                        .disabled(read_locked || service_missing || !policy.create)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.create_vault_key(cx);
                        })),
                )
            })
            .when(policy.import, |actions| {
                actions.child(
                    Button::new("vault-import-select")
                        .compact()
                        .label(crate::locale::text(
                            "选择密钥并验证",
                            "Choose and validate key",
                        ))
                        .disabled(read_locked || service_missing || !policy.import)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.choose_vault_import(cx);
                        })),
                )
            })
            .when(policy.export, |actions| {
                actions.child(
                    Button::new("vault-export")
                        .compact()
                        .label(crate::locale::text("导出密钥备份", "Export key backup"))
                        .disabled(read_locked || service_missing || !policy.export)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.choose_vault_export(cx);
                        })),
                )
            })
            .when(policy.restore, |actions| {
                actions.child(
                    Button::new("vault-restore-select")
                        .compact()
                        .label(crate::locale::text(
                            "选择 key 与密文文件",
                            "Choose key and encrypted vault files",
                        ))
                        .disabled(read_locked || service_missing || !policy.restore)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.choose_vault_restore_pair(cx);
                        })),
                )
            })
            .when(policy.rotate, |actions| {
                actions.child(
                    Button::new("vault-rotate-preview")
                        .compact()
                        .label(crate::locale::text("预检密钥轮换", "Preview key rotation"))
                        .disabled(read_locked || service_missing || !policy.rotate)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.preview_vault_rotation(cx);
                        })),
                )
            });

        body = body.child(
            vault_card(crate::locale::text("Vault 状态", "Vault status"), theme)
                .child(status_value)
                .child(actions)
                .child(crate::locale::text("文件选择、密钥验证、原子保存和备份由 application 用例完成。", "File selection, key validation, atomic writes, and backups run through application use cases.")),
        );

        if let Some(path) = self.vault_import_candidate.as_ref() {
            body = body.child(
                vault_card(crate::locale::text("密钥已验证", "Key validated"), theme)
                    .child(crate::locale::text(
                        format!(
                            "所选文件：{}。应用已完成 dry-run，尚未写入当前配置。",
                            path_name(path)
                        ),
                        format!(
                            "Selected file: {}. Validation completed; nothing has been written.",
                            path_name(path)
                        ),
                    ))
                    .child(
                        Button::new("vault-import-confirm")
                            .compact()
                            .primary()
                            .label(crate::locale::text("确认导入密钥", "Confirm key import"))
                            .disabled(read_locked || service_missing || !policy.import)
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.confirm_vault_import(cx);
                            })),
                    ),
            );
        }
        if let Some((key_path, vault_path)) = self.vault_restore_candidate.as_ref() {
            body = body.child(
                vault_card(crate::locale::text("恢复配对已验证", "Vault pair validated"), theme)
                    .child(crate::locale::text(
                        format!("Key：{}；密文：{}。已通过完整性预检，尚未写入。", path_name(key_path), path_name(vault_path)),
                        format!("Key: {}; encrypted vault: {}. Integrity check passed; nothing has been written.", path_name(key_path), path_name(vault_path)),
                    ))
                    .child(
                        Button::new("vault-restore-confirm")
                            .compact()
                            .primary()
                            .label(crate::locale::text("确认恢复配对", "Confirm pair restore"))
                            .disabled(read_locked || service_missing || !policy.restore)
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.confirm_vault_restore(cx);
                            })),
                    ),
            );
        }
        if self.vault_rotation_verified {
            body = body.child(
                vault_card(crate::locale::text("轮换预检已完成", "Rotation preview complete"), theme)
                    .child(crate::locale::text("确认后会重新加密所有凭据，并由应用保留可恢复备份。", "Confirming will re-encrypt all credentials. The application keeps a recoverable backup."))
                    .child(
                        Button::new("vault-rotate-confirm")
                            .compact()
                            .primary()
                            .label(crate::locale::text("确认轮换密钥", "Confirm key rotation"))
                            .disabled(read_locked || service_missing || !policy.rotate)
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.confirm_vault_rotation(cx);
                            })),
                    ),
            );
        }
        body
    }
}

/// 设置密钥页明确导入、导出和轮换会影响凭据，避免将危险操作藏在通用设置表单里。
fn vault_heading(theme: ThemeTokens) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(crate::locale::text("凭据密钥", "Credential keys")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(crate::locale::text("密钥备份与密文数据必须成对保管；页面不会回显密钥正文。", "Keep key backups together with encrypted data. Key contents are never shown here.")),
        )
}

/// 以主题令牌构造恢复和执行结果卡片，确保浅深主题下均有足够对比度。
fn vault_card(title: impl Into<gpui_kit::SharedString>, theme: ThemeTokens) -> gpui_kit::Div {
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

/// 错误和状态只显示 application 已经脱敏的文案，绝不显示 key 文本。
fn vault_note(text: impl Into<gpui_kit::SharedString>, color: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .w_full()
        .p_3()
        .text_size(px(12.))
        .text_color(color)
        .child(text.into())
}

/// 文件选择结果只展示末级名称，不在界面状态栏泄露用户目录结构。
fn path_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::locale::text("已选择文件", "Selected file").to_string())
}
