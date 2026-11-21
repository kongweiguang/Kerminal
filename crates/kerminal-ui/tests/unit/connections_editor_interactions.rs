//! 协议返回、嵌套确认焦点及字段门禁的生产表单回归。
//!
//! @author kongweiguang

use gpui_kit::Focusable as _;

use super::super::super::local_hosts::LocalHostEditorSection;
use super::*;

/// 用真实控件边界检查端口对齐与窄窗宽度，确保字段错误也不会挤压表单。
/// @author kongweiguang
#[gpui_kit::test]
fn endpoint_port_aligns_with_general_fields_in_wide_and_compact_editors(app: &mut TestAppContext) {
    use crate::features::connections::form::{HostEditorField, HostEditorValidationIssue};
    use gpui_kit::{ElementId, px, size};

    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    for width in [1360., 800.] {
        visual.simulate_resize(size(px(width), px(800.)));
        for protocol in [
            RemoteProtocol::Ssh,
            RemoteProtocol::Sftp,
            RemoteProtocol::Rdp,
            RemoteProtocol::Telnet,
        ] {
            update_panel(&panel, visual, |panel, window, cx| {
                panel.create_host_with_protocol(protocol, window, cx);
            });
            let bounds = |visual: &mut VisualTestContext, identity: &'static str| {
                visual.update(|window, _| {
                    gpui_kit::base::test_support::find(window, &[], &ElementId::from(identity))
                        .expect("常规页必须渲染真实输入框")
                        .bounds()
                })
            };
            let name = bounds(visual, "connections-editor-name-input");
            let port = bounds(visual, "connections-editor-port-input");
            let group = visual
                .debug_bounds("connections-editor-create-group")
                .expect("分组行必须保留新建按钮");
            assert_eq!(port.right(), name.right(), "{protocol:?} at {width}");
            assert_eq!(group.right(), name.right(), "{protocol:?} at {width}");
            assert_eq!(group.size.width, name.size.height);
            assert_eq!(group.size.height, name.size.height);
            if width == 800. {
                let page = visual.debug_bounds("connections-editor-viewport").unwrap();
                assert!(page.right() - name.right() <= px(72.), "{name:?} {page:?}");
            }

            update_panel(&panel, visual, |panel, _, cx| {
                panel.editor.as_mut().unwrap().validation_issue = Some(HostEditorValidationIssue {
                    field: HostEditorField::Port,
                    section: HostEditorSection::Properties,
                    message: "请输入有效的端口。".to_owned(),
                });
                cx.notify();
            });
            assert_eq!(bounds(visual, "connections-editor-port-input"), port);
        }
    }
}

/// 本地两页应和远端共用标题位置，切换分类时首行起点和固定底栏保持稳定。
/// @author kongweiguang
#[gpui_kit::test]
fn local_and_remote_share_section_heading_and_footer_geometry(app: &mut TestAppContext) {
    let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1360.), gpui_kit::px(900.)));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
    });
    let heading = "connections-editor-section-heading";
    let remote_heading = visual.debug_bounds(heading).expect("远端分类标题应可见");
    let remote_save = visual.debug_bounds("connections-editor-save").unwrap();
    let remote_cancel = visual
        .debug_bounds("connections-editor-footer-cancel")
        .unwrap();
    assert_eq!(remote_cancel.size, remote_save.size);

    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    pump_runtime(&runtime, visual);
    let local_save = visual
        .debug_bounds("connections-local-editor-save")
        .unwrap();
    let local_cancel = visual
        .debug_bounds("connections-local-editor-cancel")
        .unwrap();
    assert_eq!(local_save, remote_save);
    assert_eq!(local_cancel.size, remote_cancel.size);
    assert_eq!(visual.debug_bounds(heading), Some(remote_heading));

    let terminal = visual
        .debug_bounds("connections-local-editor-section-Terminal")
        .unwrap();
    visual.simulate_click(terminal.center(), Default::default());
    draw(visual);
    visual.update(|_, app| {
        assert_eq!(
            panel.read(app).local_hosts.editor.as_ref().unwrap().section,
            LocalHostEditorSection::Terminal
        );
    });
    assert_eq!(visual.debug_bounds(heading), Some(remote_heading));
    assert_eq!(
        visual.debug_bounds("connections-local-editor-save"),
        Some(local_save)
    );
}

/// 五种远端协议共用标签投影；既有逗号、引号及空标签在仅改名称后必须保持同一数组。
/// @author kongweiguang
#[gpui_kit::test]
fn rename_preserves_tags_with_commas_in_all_remote_protocols(app: &mut TestAppContext) {
    use kerminal_domain::sessions::SerialSessionTarget;
    for protocol in [
        RemoteProtocol::Ssh,
        RemoteProtocol::Sftp,
        RemoteProtocol::Rdp,
        RemoteProtocol::Telnet,
        RemoteProtocol::Serial,
    ] {
        let mut host = existing_host();
        host.protocol = protocol;
        host.tags = vec![
            "ops,prod".to_owned(),
            "\"quoted\"".to_owned(),
            String::new(),
        ];
        host.authentication_mode = HostAuthenticationMode::Agent;
        host.authentication = None;
        if protocol == RemoteProtocol::Serial {
            host.endpoint.host = "COM1".to_owned();
            host.endpoint.port = 0;
            host.endpoint.username.clear();
            host.serial = Some(SerialSessionTarget::new("COM1".into()));
        }
        let id = host.id.clone();
        let tags = host.tags.clone();
        let (_runtime, panel, visual) = fixture(app, vec![host], Vec::new());
        update_panel(&panel, visual, |panel, window, cx| {
            panel.edit_host(&id, window, cx);
            let editor = panel.editor.as_ref().unwrap();
            editor
                .inputs
                .name
                .update(cx, |input, cx| input.replace_all("renamed", window, cx));
            assert_eq!(editor.to_draft(&panel.groups, cx).unwrap().tags, tags);
            editor.inputs.tags.update(cx, |input, cx| {
                input.replace_all("\"unfinished", window, cx)
            });
            panel.save_editor(window, cx);
            assert!(!panel.saving);
            assert_eq!(
                panel
                    .editor
                    .as_ref()
                    .unwrap()
                    .validation_issue
                    .as_ref()
                    .unwrap()
                    .field,
                super::super::super::form::HostEditorField::Tags
            );
        });
    }
}

/// 展开代理后直接断言真实输入框的可读宽度和纵向分离，防止宽窗与窄窗只剩控件边框。
/// @author kongweiguang
#[gpui_kit::test]
fn enabled_proxy_fields_remain_readable_in_wide_and_narrow_editors(app: &mut TestAppContext) {
    use gpui_kit::{ElementId, px, size};

    for (width, height) in [(1360., 900.), (800., 600.)] {
        for protocol in [RemoteProtocol::Ssh, RemoteProtocol::Sftp] {
            let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
            visual.simulate_resize(size(px(width), px(height)));
            update_panel(&panel, visual, |panel, window, cx| {
                panel.create_host_with_protocol(protocol, window, cx);
                panel.select_editor_section(HostEditorSection::Proxy, window, cx);
                let editor = panel.editor.as_mut().unwrap();
                editor.proxy_enabled = true;
                editor.inputs.proxy_port.update(cx, |input, cx| {
                    input.set_value("1080", window, cx);
                });
                editor.inputs.proxy_username.update(cx, |input, cx| {
                    input.set_value("proxy-user", window, cx);
                });
                cx.notify();
            });
            let (port, username) = visual.update(|window, _| {
                let snapshots = gpui_kit::base::test_support::snapshots(window);
                let bounds = |identity: &'static str| {
                    snapshots
                        .iter()
                        .find(|item| {
                            item.path()
                                .iter()
                                .any(|id| id == &ElementId::from(identity))
                        })
                        .expect("展开代理必须渲染实际端口和用户名输入框")
                        .bounds()
                };
                (
                    bounds("connections-proxy-port-input"),
                    bounds("connections-proxy-username-input"),
                )
            });
            assert!(port.size.width >= px(128.), "端口需完整容纳数值：{port:?}");
            assert!(
                username.size.width >= px(240.),
                "用户名需具有可读宽度：{username:?}"
            );
            assert!(port.origin.y + port.size.height <= username.origin.y);
            let body = visual.debug_bounds("connections-editor-viewport").unwrap();
            assert!(username.origin.x + username.size.width <= body.origin.x + body.size.width);
        }
    }
}

/// 真实来源单选须卸载另一入口、清理互斥材料并把焦点交给可见输入，不能只改变外观。
/// @author kongweiguang
#[gpui_kit::test]
fn private_key_source_radios_keep_one_input_and_transfer_focus(app: &mut TestAppContext) {
    use crate::features::connections::form::PrivateKeyInputMode;

    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.set_authentication_mode(HostAuthenticationMode::PrivateKey, window, cx);
        panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .key_path
            .update(cx, |input, cx| {
                input.replace_all("fixture-key", window, cx);
            });
    });
    for (label, mode) in [
        (
            crate::locale::text("粘贴内容", "Pasted key"),
            PrivateKeyInputMode::Paste,
        ),
        (
            crate::locale::text("文件", "File"),
            PrivateKeyInputMode::File,
        ),
    ] {
        let bounds = visual.update(|window, _| {
            gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .find(|item| item.label() == Some(label.as_ref()))
                .expect("私钥来源应由真实单选控件呈现")
                .bounds()
        });
        visual.simulate_click(bounds.center(), Default::default());
        draw(visual);
        visual.update(|window, app| {
            let editor = panel.read(app).editor.as_ref().unwrap();
            assert_eq!(editor.private_key_input_mode, mode);
            assert!(editor.private_key_focus_handle(app).is_focused(window));
            assert!(editor.inputs.key_path.read(app).value().is_empty());
            assert!(editor.inputs.private_key.read(app).value().is_empty());
        });
        if mode == PrivateKeyInputMode::Paste {
            update_panel(&panel, visual, |panel, window, cx| {
                panel
                    .editor
                    .as_ref()
                    .unwrap()
                    .inputs
                    .private_key
                    .update(cx, |input, cx| {
                        input.set_value("fixture private key body", window, cx);
                    });
                panel.set_authentication_mode(HostAuthenticationMode::Password, window, cx);
                panel.set_authentication_mode(HostAuthenticationMode::PrivateKey, window, cx);
                assert!(
                    panel
                        .editor
                        .as_ref()
                        .unwrap()
                        .private_key_focus_handle(cx)
                        .is_focused(window),
                    "恢复私钥认证时必须聚焦可见的粘贴输入"
                );
            });
        }
    }
}

/// 历史配置可同时保留路径和 Vault 正文，来源展示须与运行时优先使用 Vault 的规则一致且不改写旧值。
/// @author kongweiguang
#[gpui_kit::test]
fn saved_private_key_source_matches_vault_priority(app: &mut TestAppContext) {
    use crate::features::connections::form::PrivateKeyInputMode;
    use kerminal_domain::connections::{CredentialRef, SshAuthentication};

    let mut host = existing_host();
    host.authentication_mode = HostAuthenticationMode::PrivateKey;
    host.authentication = Some(SshAuthentication::PrivateKey {
        path: Some("historical-key".into()),
        secret: Some(CredentialRef::new("saved-private-key").unwrap()),
        passphrase: None,
    });
    let id = host.id.clone();
    let (_runtime, panel, visual) = fixture(app, vec![host], Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.edit_host(&id, window, cx);
        let editor = panel.editor.as_mut().unwrap();
        assert_eq!(editor.private_key_input_mode, PrivateKeyInputMode::Paste);
        assert!(!editor.is_dirty(cx));
        assert!(editor.credential_changes(cx).is_empty());
        editor.select_private_key_input_mode(PrivateKeyInputMode::File, window, cx);
        assert!(editor.private_key_file_source);
        assert!(editor.is_dirty(cx), "明确选择文件才替换 Vault 来源");
    });
}

/// 改选跳板文件必须同时改变探测草稿与 Vault 操作；清空、撤销及重排均保持同一凭据归属。
/// @author kongweiguang
#[gpui_kit::test]
fn jump_key_file_replacement_and_reorder_use_the_current_credential_slot(app: &mut TestAppContext) {
    use kerminal_application::connections::management::{
        HostCredentialChange, HostCredentialSlot, ManagedJumpHost,
    };
    use kerminal_domain::connections::{CredentialRef, SshAuthentication};

    let mut host = existing_host();
    let id = host.id.clone();
    let saved_key = CredentialRef::new("fixture-saved-jump-key").unwrap();
    host.route.jump_hosts = vec![
        ManagedJumpHost {
            name: "key jump".into(),
            endpoint: host.endpoint.clone(),
            authentication: Some(SshAuthentication::PrivateKey {
                path: Some("original-key".into()),
                secret: Some(saved_key.clone()),
                passphrase: None,
            }),
            authentication_mode: HostAuthenticationMode::PrivateKey,
        },
        ManagedJumpHost {
            name: "agent jump".into(),
            endpoint: host.endpoint.clone(),
            authentication: Some(SshAuthentication::Agent),
            authentication_mode: HostAuthenticationMode::Agent,
        },
    ];
    let (_runtime, panel, visual) = fixture(app, vec![host], Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.edit_host(&id, window, cx);
    });

    for path in ["replacement-key", "", "original-key"] {
        update_panel(&panel, visual, |panel, window, cx| {
            let editor = panel.editor.as_mut().unwrap();
            editor.jumps[0].key_path.update(cx, |input, cx| {
                input.replace_all(path, window, cx);
            });
            let draft = editor.to_draft(&[], cx).unwrap();
            let Some(SshAuthentication::PrivateKey { secret, .. }) =
                draft.route.jump_hosts[0].authentication.as_ref()
            else {
                panic!("跳板应保持私钥认证");
            };
            let changes = editor.credential_changes(cx);
            if path == "replacement-key" {
                assert!(secret.is_none(), "新文件路径应供探测直接使用");
                assert!(matches!(
                    changes.as_slice(),
                    [HostCredentialChange::Clear {
                        slot: HostCredentialSlot::JumpPrivateKey(0)
                    }]
                ));
            } else {
                assert_eq!(secret.as_ref(), Some(&saved_key));
                assert!(changes.is_empty(), "取消或撤销不应清除旧私钥");
            }
        });
    }
    update_panel(&panel, visual, |panel, window, cx| {
        panel.editor.as_ref().unwrap().jumps[0]
            .key_path
            .update(cx, |input, cx| {
                input.replace_all("replacement-key", window, cx)
            });
        panel.move_jump(0, false, window, cx);
        let editor = panel.editor.as_mut().unwrap();
        assert!(matches!(
            editor.credential_changes(cx).as_slice(),
            [HostCredentialChange::Clear {
                slot: HostCredentialSlot::JumpPrivateKey(1)
            }]
        ));
        let draft = editor.to_draft(&[], cx).unwrap();
        assert!(matches!(
            draft.route.jump_hosts[1].authentication,
            Some(SshAuthentication::PrivateKey {
                secret: None,
                ref path,
                ..
            }) if path.as_deref() == Some(std::path::Path::new("replacement-key"))
        ));
    });
}

/// 主机编辑器标题栏只保留关闭动作，不再渲染收藏入口。
/// @author kongweiguang
#[gpui_kit::test]
fn host_editor_header_renders_close_without_favorite(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
    });
    let labels = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter_map(|item| item.label().map(str::to_owned))
            .collect::<Vec<_>>()
    });
    let close_label = crate::locale::text("关闭连接编辑器", "Close connection editor");
    assert!(
        labels.iter().any(|label| label == close_label.as_ref()),
        "标题栏必须保留关闭按钮"
    );
    assert!(
        !labels
            .iter()
            .any(|label| label.contains("收藏") || label.contains("favorite")),
        "主机编辑器不应再渲染收藏按钮"
    );
}

/// 长反馈只能占用既有底栏；比较完整正文和按钮边界，防止只验证起点而漏掉正文被压缩。
/// @author kongweiguang
#[gpui_kit::test]
fn host_editor_feedback_preserves_field_and_action_positions(app: &mut TestAppContext) {
    for local in [false, true] {
        let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
        visual.simulate_resize(gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(700.)));
        update_panel(&panel, visual, |panel, window, cx| {
            if local {
                panel.begin_create_local_host(None, window, cx);
            } else {
                panel.begin_create_host(window, cx);
            }
        });
        pump_runtime(&runtime, visual);
        update_panel(&panel, visual, |panel, _, cx| {
            panel.status = None;
            panel.local_hosts.status = None;
            cx.notify();
        });
        let (field, save) = if local {
            (
                "connections-local-editor-viewport",
                "connections-local-editor-save",
            )
        } else {
            ("connections-editor-username-row", "connections-editor-save")
        };
        let field_before = visual.debug_bounds(field).expect("表单字段应可测量");
        let save_before = visual.debug_bounds(save).expect("保存按钮应可测量");
        let viewport = if local {
            "connections-local-editor-viewport"
        } else {
            "connections-editor-viewport"
        };
        let viewport_before = visual.debug_bounds(viewport).expect("正文视口应可测量");
        update_panel(&panel, visual, |panel, _, cx| {
            if local {
                panel.local_hosts.status = Some("读取失败，请重试。".repeat(12));
            } else {
                panel.status = Some("连接测试失败，请重试。".repeat(12));
            }
            cx.notify();
        });
        assert!(visual.debug_bounds("connections-editor-feedback").is_none());
        assert_eq!(
            visual.debug_bounds(field).expect("字段仍应存在"),
            field_before
        );
        assert_eq!(
            visual.debug_bounds(save).expect("保存按钮仍应存在"),
            save_before
        );
        assert_eq!(visual.debug_bounds(viewport), Some(viewport_before));
        update_panel(&panel, visual, |panel, _, cx| {
            if local {
                panel.local_hosts.status = None;
            } else {
                panel.status = None;
            }
            cx.notify();
        });
        assert!(visual.debug_bounds("connections-editor-feedback").is_none());
        assert_eq!(visual.debug_bounds(viewport), Some(viewport_before));
        assert_eq!(visual.debug_bounds(save), Some(save_before));
    }
}

/// 字段错误的边框与详情不得改变行高或正文范围，避免定位之外的额外布局跳动。
/// @author kongweiguang
#[gpui_kit::test]
fn field_error_keeps_row_and_viewport_geometry(app: &mut TestAppContext) {
    use crate::features::connections::form::{HostEditorField, HostEditorValidationIssue};

    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(700.)));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
    });
    let row = visual
        .debug_bounds("connections-editor-username-row")
        .unwrap();
    let viewport = visual.debug_bounds("connections-editor-viewport").unwrap();
    let save = visual.debug_bounds("connections-editor-save").unwrap();
    update_panel(&panel, visual, |panel, _, cx| {
        panel.editor.as_mut().unwrap().validation_issue = Some(HostEditorValidationIssue {
            field: HostEditorField::Username,
            section: HostEditorSection::Properties,
            message: "请输入有效用户名。".repeat(40),
        });
        cx.notify();
    });
    assert_eq!(
        visual.debug_bounds("connections-editor-username-row"),
        Some(row)
    );
    assert_eq!(
        visual.debug_bounds("connections-editor-viewport"),
        Some(viewport)
    );
    assert_eq!(visual.debug_bounds("connections-editor-save"), Some(save));
}

/// 修正名称或标签只撤销相应校验反馈，已经完成且连接参数未变的测试仍然有效。
/// @author kongweiguang
#[gpui_kit::test]
fn metadata_edits_clear_their_error_without_invalidating_probe(app: &mut TestAppContext) {
    use crate::features::connections::form::{HostEditorField, HostEditorValidationIssue};
    use kerminal_application::connections::probe::{
        ConnectionProbeOutcome, ProbeVerificationLevel,
    };

    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx)
    });
    for field in [HostEditorField::Name, HostEditorField::Tags] {
        update_panel(&panel, visual, |panel, window, cx| {
            let editor = panel.editor.as_mut().unwrap();
            editor.validation_issue = Some(HostEditorValidationIssue {
                field,
                section: HostEditorSection::Properties,
                message: "元数据无效".into(),
            });
            panel.status = Some("元数据无效".into());
            panel.probe_outcome = Some(ConnectionProbeOutcome::Verified(
                ProbeVerificationLevel::SshAuthenticated,
            ));
            let input = match field {
                HostEditorField::Name => editor.inputs.name.clone(),
                _ => editor.inputs.tags.clone(),
            };
            input.update(cx, |input, cx| input.replace_all("已修正", window, cx));
        });
        visual.update(|_, app| {
            let panel = panel.read(app);
            assert!(panel.editor.as_ref().unwrap().validation_issue.is_none());
            assert!(panel.status.is_none());
            assert!(panel.probe_outcome.is_some());
        });
    }
}

/// 迟到回执不能填隐藏表单；返回后的默认值须保护完整手动命令，不能混用其它 Shell 或 argv。
/// @author kongweiguang
#[gpui_kit::test]
fn interrupted_local_defaults_resume_without_overwriting_edits(app: &mut TestAppContext) {
    let (profiles, release) = delayed_profiles();
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    for (manual_shell, manual_args) in [("", ""), ("", "--manual"), ("cmd.exe", "")] {
        let command_edited = !manual_shell.is_empty() || !manual_args.is_empty();
        update_panel(&panel, visual, |panel, window, cx| {
            panel.begin_create_local_host(None, window, cx);
        });
        pump_runtime(&runtime, visual);
        update_panel(&panel, visual, |panel, window, cx| {
            panel.activate_remote_protocol(RemoteProtocol::Ssh, window, cx);
        });
        release.add_permits(1);
        pump_runtime(&runtime, visual);
        visual.update(|_, app| {
            let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
            assert!(editor.shell.read(app).value().is_empty());
        });
        update_panel(&panel, visual, |panel, window, cx| {
            panel.activate_local_host_editor(window, cx);
            if !manual_args.is_empty() {
                panel
                    .local_hosts
                    .editor
                    .as_ref()
                    .unwrap()
                    .args
                    .update(cx, |input, cx| input.replace_all(manual_args, window, cx));
            }
            if !manual_shell.is_empty() {
                panel
                    .local_hosts
                    .editor
                    .as_ref()
                    .unwrap()
                    .shell
                    .update(cx, |input, cx| input.replace_all(manual_shell, window, cx));
            }
        });
        release.add_permits(1);
        pump_runtime(&runtime, visual);
        visual.update(|_, app| {
            let panel = panel.read(app);
            let editor = panel.local_hosts.editor.as_ref().unwrap();
            assert_eq!(
                editor.shell.read(app).value().as_str(),
                if command_edited {
                    manual_shell.to_owned()
                } else {
                    default_profile().shell
                }
            );
            assert_eq!(editor.cwd.read(app).value().as_str(), "C:\\fixture");
            assert_eq!(
                editor.args.read(app).value().as_str(),
                if command_edited {
                    manual_args
                } else {
                    "-NoLogo"
                }
            );
            assert_eq!(panel.local_host_is_dirty(app), command_edited);
        });
        if command_edited {
            update_panel(&panel, visual, |panel, window, cx| {
                panel.cancel_editor(window, cx)
            });
            visual.update(|_, app| assert!(panel.read(app).pending_editor_discard));
            update_panel(&panel, visual, |panel, window, cx| {
                panel.confirm_discard_editor(window, cx)
            });
        } else {
            close_clean_editor(&panel, visual);
        }
    }
}

/// 显式选择已保存 Profile 跨协议中断后仍是显式意图；重试不能退化为仅填初始空字段。
/// @author kongweiguang
#[gpui_kit::test]
fn interrupted_profile_selection_retains_request_snapshot(app: &mut TestAppContext) {
    let release_default = Arc::new(Semaphore::new(0));
    let release_selected = Arc::new(Semaphore::new(0));
    let profile = default_profile();
    let profile_id = profile.id.clone();
    let row = Arc::new(Mutex::new(VersionedProfile {
        profile,
        revision: ConfigRevision::from_sha256("stored-revision".into()),
    }));
    let profiles = ProfileService::new(
        Arc::new(DelayedProfileDirectory {
            release_default: release_default.clone(),
            selected: Some((row, release_selected.clone())),
        }),
        Arc::new(NoPlatformShellFallback),
    );
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx)
    });
    release_default.add_permits(1);
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .args
            .update(cx, |input, cx| {
                input.replace_all("--before-request", window, cx)
            });
        panel.use_local_profile(profile_id, cx);
    });
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Ssh, window, cx)
    });
    release_selected.add_permits(1);
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .update(cx, |input, cx| {
                input.replace_all("KEEP=after-request", window, cx)
            });
    });
    release_selected.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.local_hosts.editor.as_ref().unwrap();
        assert_eq!(editor.args.read(app).value().as_str(), "-NoLogo");
        assert_eq!(editor.env.read(app).value().as_str(), "KEEP=after-request");
        assert!(panel.local_host_is_dirty(app));
    });
}

/// 默认项与显式 Profile 使用独立闸门，在窄窗口保留真实读取链路和几何约束以复现迟到投影。
/// @author kongweiguang
fn pending_local_profile_fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<ConnectionsPanel>,
    &mut VisualTestContext,
    Arc<Semaphore>,
) {
    let release_default = Arc::new(Semaphore::new(0));
    let release_selected = Arc::new(Semaphore::new(0));
    let mut selected = default_profile();
    selected.id = "fixture-selected".into();
    selected.name = "Selected Profile".into();
    selected.is_default = false;
    selected.shell = "C:\\selected\\pwsh.exe".into();
    selected.args = vec!["--selected".into()];
    selected.cwd = Some("C:\\selected".into());
    selected.env = BTreeMap::from([("SELECTED".into(), "profile".into())]);
    let profiles = ProfileService::new(
        Arc::new(DelayedProfileDirectory {
            release_default: release_default.clone(),
            selected: Some((
                Arc::new(Mutex::new(VersionedProfile {
                    profile: selected,
                    revision: ConfigRevision::from_sha256("selected-revision".into()),
                })),
                release_selected.clone(),
            )),
        }),
        Arc::new(NoPlatformShellFallback),
    );
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(600.)));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx)
    });
    release_default.add_permits(1);
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, _, cx| {
        panel.use_local_profile("fixture-selected".into(), cx)
    });
    pump_runtime(&runtime, visual);
    (runtime, panel, visual, release_selected)
}

/// 保存按钮和共用保存入口都须等待所选 Profile，不能先提交旧参数再用新参数刷新表单。
/// @author kongweiguang
#[gpui_kit::test]
fn pending_local_profile_blocks_save_until_projection_finishes(app: &mut TestAppContext) {
    let (runtime, panel, visual, release) = pending_local_profile_fixture(app);
    let before = visual
        .debug_bounds("connections-local-editor-viewport")
        .expect("正文应保持可测量");
    let (save_bounds, profile_bounds, clickable) = visual.update(|window, _| {
        let button = gpui_kit::base::test_support::find(
            window,
            &[],
            &gpui_kit::ElementId::from("connections-local-editor-save"),
        )
        .expect("生产保存按钮必须存在");
        let profile = gpui_kit::base::test_support::find(
            window,
            &[],
            &gpui_kit::ElementId::from("connections-local-profile-picker"),
        )
        .expect("Profile 入口仍须可用");
        (button.bounds(), profile.bounds(), button.supports_click())
    });
    assert!(!clickable, "读取所选 Profile 时不应允许提交旧参数");
    visual.simulate_click(profile_bounds.center(), Default::default());
    draw(visual);
    visual.update(|window, _| {
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible()
                    && node.role() == Some(gpui_kit::Role::MenuItem)
                    && node.label() == Some("Selected Profile")),
            "读取期间仍应能打开 Profile 菜单改选"
        );
    });
    visual.simulate_keystrokes("escape");
    draw(visual);
    visual.simulate_click(save_bounds.center(), Default::default());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.save_local_host(window, cx);
        assert!(!panel.local_hosts.saving, "保存入口也必须经过同一门禁");
        assert!(panel.local_hosts.default_target_request.is_some());
    });
    release.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|window, app| {
        let panel = panel.read(app);
        let editor = panel.local_hosts.editor.as_ref().unwrap();
        assert_eq!(
            editor.shell.read(app).value().as_str(),
            "C:\\selected\\pwsh.exe"
        );
        assert_eq!(editor.args.read(app).value().as_str(), "--selected");
        assert!(panel.local_hosts.default_target_request.is_none());
        let button = gpui_kit::base::test_support::find(
            window,
            &[],
            &gpui_kit::ElementId::from("connections-local-editor-save"),
        )
        .unwrap();
        assert!(button.supports_click(), "读取完成后可直接保存");
        assert_eq!(button.bounds(), save_bounds);
        let profile = gpui_kit::base::test_support::find(
            window,
            &[],
            &gpui_kit::ElementId::from("connections-local-profile-picker"),
        )
        .unwrap();
        assert_eq!(profile.bounds(), profile_bounds);
    });
    assert_eq!(
        visual.debug_bounds("connections-local-editor-viewport"),
        Some(before),
        "读取状态不能挤动表单正文"
    );
}

/// 模拟持久化边界冻结后再交付真实 Profile 回执，核对草稿与未知结果提示均不被覆盖。
/// @author kongweiguang
#[gpui_kit::test]
fn late_local_profile_preserves_frozen_save_draft_and_status(app: &mut TestAppContext) {
    for unknown in [false, true] {
        let (runtime, panel, visual, release) = pending_local_profile_fixture(app);
        update_panel(&panel, visual, |panel, _, cx| {
            panel.local_hosts.saving = !unknown;
            panel.local_hosts.save_outcome_unknown = unknown;
            panel.status = Some("保存结果必须保留".into());
            panel.local_hosts.status = panel.status.clone();
            cx.notify();
        });
        release.add_permits(1);
        pump_runtime(&runtime, visual);
        visual.update(|_, app| {
            let panel = panel.read(app);
            let editor = panel.local_hosts.editor.as_ref().unwrap();
            let original = default_profile();
            assert_eq!(editor.shell.read(app).value().as_str(), original.shell);
            assert_eq!(editor.args.read(app).value().as_str(), "-NoLogo");
            assert_eq!(editor.cwd.read(app).value().as_str(), "C:\\fixture");
            assert_eq!(editor.env.read(app).value().as_str(), "FIXTURE=profile");
            assert_eq!(panel.status.as_deref(), Some("保存结果必须保留"));
            assert_eq!(panel.local_hosts.status, panel.status);
            assert_eq!(panel.local_hosts.saving, !unknown);
            assert_eq!(panel.local_hosts.save_outcome_unknown, unknown);
            assert!(panel.local_hosts.default_target_request.is_none());
        });
    }
}

/// 继续编辑应返回用户刚离开的可见字段，不将焦点交给当前分区没有渲染的名称输入。
/// @author kongweiguang
#[gpui_kit::test]
fn keep_editing_restores_visible_remote_and_local_input_focus(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.select_editor_section(HostEditorSection::Network, window, cx);
        panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .update(cx, |input, cx| input.replace_all("45", window, cx));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        let focus = panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        panel.cancel_editor(window, cx);
    });
    visual.update(|window, app| {
        let panel = panel.read(app);
        assert!(panel.pending_editor_discard);
        assert!(panel.discard_focus.contains_focused(window, app));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|window, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        assert_eq!(editor.section, HostEditorSection::Network);
        assert!(
            editor
                .inputs
                .keepalive
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx);
        panel.confirm_discard_editor(window, cx);
        panel.begin_create_local_host(None, window, cx);
        panel.select_local_host_section(LocalHostEditorSection::Terminal, window, cx);
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .update(cx, |input, cx| input.replace_all("KEEP=focus", window, cx));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        let focus = panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        panel.cancel_editor(window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|window, app| {
        let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
        assert_eq!(editor.section, LocalHostEditorSection::Terminal);
        assert!(editor.env.read(app).focus_handle(app).is_focused(window));
    });
}

/// 先前焦点属于已卸载分区时，继续编辑应落在当前可见字段而不复活旧分区焦点。
/// @author kongweiguang
#[gpui_kit::test]
fn keep_editing_falls_back_to_current_section_after_focus_unmount(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        panel.select_editor_section(HostEditorSection::Network, window, cx);
        panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .update(cx, |input, cx| input.replace_all("45", window, cx));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        let focus = panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        panel.cancel_editor(window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_editor_section(HostEditorSection::Terminal, window, cx)
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|window, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        assert_eq!(editor.section, HostEditorSection::Terminal);
        assert!(
            editor
                .inputs
                .term
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
}
