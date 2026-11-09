//! 通过真实连接面板与应用 probe service 验证异步回执和私钥草稿隔离。
//!
//! @author kongweiguang

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult,
    connections::probe::{
        ConnectionProbeBackend, ConnectionProbeOutcome, ConnectionProbeService,
        ConnectionProbeTarget, ProbeCredentialMaterial, ProbeVerificationLevel,
    },
};
use kerminal_domain::connections::SshAuthentication;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;

const PASTED_PRIVATE_KEY: &str =
    "-----BEGIN PRIVATE KEY-----\nfixture-probe-material\n-----END PRIVATE KEY-----";

/// 只暴露本次探测是否收到临时材料，避免测试观察通道再次保存私钥正文。
struct ProbeStarted {
    id: usize,
    endpoint_host: String,
    cancellation: CancellationToken,
    private_key_matches: bool,
    private_key_reference_is_ephemeral: bool,
    authentication_reference_matches: bool,
    credential_count: usize,
    credential_debug_redacts_material: bool,
}

/// 探测后端故意忽略取消并等待测试放行，以复现最慢回执覆盖当前表单的竞态。
struct GatedProbeBackend {
    started: mpsc::UnboundedSender<ProbeStarted>,
    finished: mpsc::UnboundedSender<usize>,
    release: Arc<Semaphore>,
    next_id: AtomicUsize,
}

impl ConnectionProbeBackend for GatedProbeBackend {
    /// 只在受控回执被放行后完成，且只发送脱敏观察值；取消后的成功结果是旧回执门禁的回归输入。
    fn probe<'port, 'async_trait>(
        &'port self,
        target: ConnectionProbeTarget,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = AppResult<ConnectionProbeOutcome>> + Send + 'async_trait>>
    where
        'port: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let authentication_reference = match target.profile.authentication.as_ref() {
                Some(SshAuthentication::PrivateKey { secret, .. }) => secret.as_ref(),
                _ => None,
            };
            let matching_credential = target.transient_credentials.iter().find(|credential| {
                authentication_reference.is_some_and(|reference| reference == &credential.reference)
            });
            let private_key_matches = matching_credential.is_some_and(|credential| {
                matches!(
                    &credential.material,
                    ProbeCredentialMaterial::PrivateKey(value)
                        if value.as_slice() == PASTED_PRIVATE_KEY.as_bytes()
                )
            });
            let credential_debug_redacts_material = matching_credential
                .is_some_and(|credential| !format!("{credential:?}").contains(PASTED_PRIVATE_KEY));
            let observation = ProbeStarted {
                id,
                endpoint_host: target.profile.endpoint.host.clone(),
                cancellation,
                private_key_matches,
                private_key_reference_is_ephemeral: authentication_reference
                    .is_some_and(|reference| reference.as_str().starts_with("probe-")),
                authentication_reference_matches: matching_credential.is_some(),
                credential_count: target.transient_credentials.len(),
                credential_debug_redacts_material,
            };
            self.started
                .send(observation)
                .map_err(|_| AppError::infrastructure("测试探测观察者已关闭"))?;

            let permit = self
                .release
                .acquire()
                .await
                .map_err(|_| AppError::infrastructure("测试探测回执门禁已关闭"))?;
            permit.forget();
            let _ = self.finished.send(id);
            Ok(ConnectionProbeOutcome::Verified(
                ProbeVerificationLevel::SshAuthenticated,
            ))
        })
    }
}

/// 每个请求都经过真实 `ConnectionProbeService`，信号量允许单独释放迟到回执。
fn gated_probe_service() -> (
    Arc<ConnectionProbeService>,
    mpsc::UnboundedReceiver<ProbeStarted>,
    mpsc::UnboundedReceiver<usize>,
    Arc<Semaphore>,
) {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (finished_tx, finished_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let backend = Arc::new(GatedProbeBackend {
        started: started_tx,
        finished: finished_tx,
        release: release.clone(),
        next_id: AtomicUsize::new(0),
    });
    (
        Arc::new(ConnectionProbeService::new(backend)),
        started_rx,
        finished_rx,
        release,
    )
}

/// 将真实 service 接到现有只读面板夹具，仓储一旦收到写入即让回归失败。
fn install_probe_service(
    panel: &Entity<ConnectionsPanel>,
    visual: &mut VisualTestContext,
    service: Arc<ConnectionProbeService>,
) {
    update_panel(panel, visual, move |panel, _, _| {
        panel.probe_service = Some(service);
    });
}

/// 通过生产输入实体建立一个可探测的 SSH 草稿，不打开 socket 或触碰用户目录。
fn configure_ssh_editor(
    panel: &Entity<ConnectionsPanel>,
    visual: &mut VisualTestContext,
    create_new: bool,
    name: &str,
    host: &str,
    authentication_mode: HostAuthenticationMode,
    private_key: Option<&str>,
) {
    let name = name.to_owned();
    let host = host.to_owned();
    let private_key = private_key.map(str::to_owned);
    update_panel(panel, visual, move |panel, window, cx| {
        if create_new {
            panel.begin_create_host(window, cx);
        }
        panel.set_authentication_mode(authentication_mode, window, cx);
        if private_key.is_some() {
            panel
                .editor
                .as_mut()
                .unwrap()
                .select_private_key_input_mode(
                    super::super::super::form::PrivateKeyInputMode::Paste,
                    window,
                    cx,
                );
        }
        let editor = panel.editor.as_ref().expect("SSH 编辑器已打开");
        editor
            .inputs
            .name
            .update(cx, |input, cx| input.replace_all(name, window, cx));
        editor
            .inputs
            .host
            .update(cx, |input, cx| input.replace_all(host, window, cx));
        editor.inputs.username.update(cx, |input, cx| {
            input.replace_all("fixture-user", window, cx)
        });
        if let Some(private_key) = private_key {
            editor.inputs.private_key.update(cx, |state, cx| {
                state.set_value(private_key, window, cx);
            });
        }
    });
}

/// 用原按钮身份命中测试或取消，避免绕过控件的禁用、忙碌与点击分发规则。
/// @author kongweiguang
fn click_probe_action(visual: &mut VisualTestContext) {
    let bounds = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|item| {
                item.path()
                    .iter()
                    .any(|id| id == &gpui_kit::ElementId::from("connections-editor-probe"))
            })
            .expect("生产测试按钮必须始终使用稳定标识")
            .bounds()
    });
    visual.simulate_click(bounds.center(), Default::default());
    draw(visual);
}

/// 点击生产测试按钮启动后端，并等待确切请求到达受控边界。
/// @author kongweiguang
fn start_probe(
    runtime: &Runtime,
    _panel: &Entity<ConnectionsPanel>,
    visual: &mut VisualTestContext,
    started: &mut mpsc::UnboundedReceiver<ProbeStarted>,
) -> ProbeStarted {
    click_probe_action(visual);
    pump_runtime(runtime, visual);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), started.recv())
            .await
            .expect("连接测试应到达注入的后端")
            .expect("测试观察通道保持打开")
    })
}

/// 测试没有平台帧循环，显式交付错误聚焦和 ScrollAnchor 的后继帧，避免只验证到旧焦点。
/// @author kongweiguang
fn settle_validation_focus(visual: &mut VisualTestContext) {
    draw(visual);
    for _ in 0..2 {
        visual.update(|window, cx| window.simulate_next_frame(cx));
        draw(visual);
    }
}

/// 明确选择空文件来源不能偷用旧 Vault 私钥；同一校验必须挡住真实测试/保存按钮，切回粘贴仍可保留旧值。
/// @author kongweiguang
#[gpui_kit::test]
fn empty_file_source_blocks_probe_and_save_without_clearing_saved_key(app: &mut TestAppContext) {
    use super::super::super::form::{HostEditorField, PrivateKeyInputMode};
    use gpui_kit::Focusable as _;
    use kerminal_domain::connections::CredentialRef;

    for protocol in [RemoteProtocol::Ssh, RemoteProtocol::Sftp] {
        let mut host = existing_host();
        host.protocol = protocol;
        host.authentication_mode = HostAuthenticationMode::PrivateKey;
        let saved = SshAuthentication::PrivateKey {
            path: None,
            secret: Some(CredentialRef::new("fixture-saved-private-key").unwrap()),
            passphrase: None,
        };
        host.authentication = Some(saved.clone());
        let id = host.id.clone();
        let (runtime, panel, visual) = fixture(app, vec![host], Vec::new());
        let (service, mut started, mut finished, release) = gated_probe_service();
        install_probe_service(&panel, visual, service);
        update_panel(&panel, visual, |panel, window, cx| {
            panel.edit_host(&id, window, cx);
            panel
                .editor
                .as_mut()
                .unwrap()
                .select_private_key_input_mode(PrivateKeyInputMode::File, window, cx);
            let focus = panel
                .editor
                .as_ref()
                .unwrap()
                .inputs
                .username
                .read(cx)
                .focus_handle(cx);
            window.focus(&focus, cx);
        });
        click_probe_action(visual);
        let save = visual.debug_bounds("connections-editor-save").unwrap();
        visual.simulate_click(save.center(), Default::default());
        draw(visual);
        pump_runtime(&runtime, visual);
        settle_validation_focus(visual);
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        update_panel(&panel, visual, |panel, window, cx| {
            assert!(!panel.saving && !panel.probe_running);
            let editor = panel.editor.as_mut().unwrap();
            assert_eq!(
                editor.validation_issue.as_ref().unwrap().field,
                HostEditorField::PrivateKey
            );
            assert!(
                editor.private_key_focus_handle(cx).is_focused(window),
                "私钥来源 {:?}：应聚焦 {:?}，实际 {:?}",
                editor.private_key_input_mode,
                editor.private_key_focus_handle(cx),
                window.focused(cx)
            );
            assert!(editor.credential_changes(cx).is_empty());
            assert_eq!(
                editor.original.as_ref().unwrap().authentication,
                Some(saved.clone())
            );
            editor.select_private_key_input_mode(PrivateKeyInputMode::Paste, window, cx);
            assert_eq!(
                editor.to_draft(&[], cx).unwrap().authentication,
                Some(saved.clone())
            );
        });
        let probe = start_probe(&runtime, &panel, visual, &mut started);
        assert_eq!(
            probe.credential_count, 0,
            "继续使用旧引用，不创建临时替换材料"
        );
        assert!(!probe.private_key_reference_is_ephemeral);
        release_probe(&runtime, visual, &release, &mut finished, probe.id);
    }
}

/// 以可访问按钮名称驱动清除/撤销，避免测试直接置位而遗漏实际回调的凭据与探测清理。
/// @author kongweiguang
fn click_saved_private_key_action(visual: &mut VisualTestContext, clearing: bool) {
    let label = if clearing {
        crate::locale::text("保留原私钥", "Keep existing Private key")
    } else {
        crate::locale::text("清除已保存私钥", "Clear saved Private key")
    };
    let bounds = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|item| item.label() == Some(label.as_ref()))
            .expect("私钥应提供清除及撤销按钮")
            .bounds()
    });
    visual.simulate_click(bounds.center(), Default::default());
    draw(visual);
}

/// 历史双来源在清除或复制移除 Vault 后必须由用户补正文或改选文件，隐藏路径不能绕过当前来源。
/// @author kongweiguang
#[gpui_kit::test]
fn pasted_key_does_not_fall_back_to_hidden_file_after_clear_or_duplicate(app: &mut TestAppContext) {
    use super::super::super::form::{HostEditorField, PrivateKeyInputMode};
    use kerminal_domain::connections::CredentialRef;

    for (protocol, duplicate) in [
        (RemoteProtocol::Ssh, false),
        (RemoteProtocol::Sftp, false),
        (RemoteProtocol::Ssh, true),
        (RemoteProtocol::Sftp, true),
    ] {
        let mut host = existing_host();
        host.protocol = protocol;
        host.authentication_mode = HostAuthenticationMode::PrivateKey;
        let saved = SshAuthentication::PrivateKey {
            path: Some("historical-private-key".into()),
            secret: Some(CredentialRef::new("fixture-saved-private-key").unwrap()),
            passphrase: None,
        };
        host.authentication = Some(saved.clone());
        let id = host.id.clone();
        let (runtime, panel, visual) = fixture(app, vec![host], Vec::new());
        visual.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(1200.)));
        let (service, mut started, mut finished, release) = gated_probe_service();
        install_probe_service(&panel, visual, service);
        update_panel(&panel, visual, |panel, window, cx| {
            if duplicate {
                panel.duplicate_saved_host(&id, window, cx);
            } else {
                panel.edit_host(&id, window, cx);
            }
            assert_eq!(
                panel.editor.as_ref().unwrap().private_key_input_mode,
                PrivateKeyInputMode::Paste
            );
        });
        if !duplicate {
            click_saved_private_key_action(visual, false);
        }
        click_probe_action(visual);
        let save = visual.debug_bounds("connections-editor-save").unwrap();
        visual.simulate_click(save.center(), Default::default());
        draw(visual);
        pump_runtime(&runtime, visual);
        settle_validation_focus(visual);
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        update_panel(&panel, visual, |panel, window, cx| {
            assert!(!panel.saving && !panel.probe_running);
            let editor = panel.editor.as_ref().unwrap();
            assert_eq!(editor.clear_private_key, !duplicate);
            assert_eq!(
                editor.validation_issue.as_ref().unwrap().field,
                HostEditorField::PrivateKey
            );
            assert!(
                editor.private_key_focus_handle(cx).is_focused(window),
                "私钥来源 {:?}：应聚焦 {:?}，实际 {:?}",
                editor.private_key_input_mode,
                editor.private_key_focus_handle(cx),
                window.focused(cx)
            );
            assert_eq!(
                editor
                    .original
                    .as_ref()
                    .map(|host| host.authentication.clone()),
                (!duplicate).then(|| Some(saved.clone()))
            );
        });
        if !duplicate {
            click_saved_private_key_action(visual, true);
        }
        update_panel(&panel, visual, |panel, window, cx| {
            let editor = panel.editor.as_mut().unwrap();
            if duplicate {
                editor.select_private_key_input_mode(PrivateKeyInputMode::File, window, cx);
            }
            assert!(!editor.clear_private_key);
            assert!(editor.credential_changes(cx).is_empty());
            assert_eq!(
                editor.to_draft(&[], cx).unwrap().authentication,
                if duplicate {
                    Some(SshAuthentication::PrivateKey {
                        path: Some("historical-private-key".into()),
                        secret: None,
                        passphrase: None,
                    })
                } else {
                    Some(saved.clone())
                }
            );
        });
        let probe = start_probe(&runtime, &panel, visual, &mut started);
        assert_eq!(probe.credential_count, 0);
        assert!(!probe.private_key_reference_is_ephemeral);
        release_probe(&runtime, visual, &release, &mut finished, probe.id);
    }
}

/// 测试期间只改变局部反馈，保留正文几何与输入实体；编辑会取消旧请求且迟到成功不能复活。
/// @author kongweiguang
#[gpui_kit::test]
fn pending_probe_keeps_form_stable_and_input_edits_cancel_old_result(app: &mut TestAppContext) {
    let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    let (service, mut started, mut finished, release) = gated_probe_service();
    install_probe_service(&panel, visual, service);
    configure_ssh_editor(
        &panel,
        visual,
        true,
        "stable",
        "stable.example.test",
        HostAuthenticationMode::Agent,
        None,
    );
    update_panel(&panel, visual, |panel, window, cx| {
        panel.select_editor_section(HostEditorSection::Network, window, cx);
    });
    let before = visual.update(|_, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        (
            editor.inputs.keepalive.entity_id(),
            editor.scroll_handle.offset(),
        )
    });
    let save_bounds = visual
        .debug_bounds("connections-editor-save")
        .expect("保存按钮可见");
    let viewport_bounds = visual
        .debug_bounds("connections-editor-viewport")
        .expect("正文视口可见");
    let probe = start_probe(&runtime, &panel, visual, &mut started);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.editor.as_ref().unwrap();
        assert!(panel.probe_running);
        assert!(
            !panel.host_editor_fields_disabled(),
            "测试不应使整张表单变灰"
        );
        assert_eq!(editor.section, HostEditorSection::Network);
        assert_eq!(editor.inputs.keepalive.entity_id(), before.0);
        assert_eq!(editor.scroll_handle.offset(), before.1);
    });
    assert_eq!(
        visual.debug_bounds("connections-editor-save"),
        Some(save_bounds)
    );
    assert_eq!(
        visual.debug_bounds("connections-editor-viewport"),
        Some(viewport_bounds)
    );
    update_panel(&panel, visual, |panel, window, cx| {
        panel
            .editor
            .as_ref()
            .unwrap()
            .inputs
            .keepalive
            .update(cx, |input, cx| input.replace_all("45", window, cx));
    });
    assert!(probe.cancellation.is_cancelled());
    release_probe(&runtime, visual, &release, &mut finished, probe.id);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.editor.as_ref().unwrap();
        assert_eq!(editor.inputs.keepalive.read(app).value().as_str(), "45");
        assert!(!panel.probe_running);
        assert!(panel.probe_outcome.is_none());
        assert!(panel.probe_error.is_none());
    });
    assert_eq!(
        visual.debug_bounds("connections-editor-save"),
        Some(save_bounds)
    );
    assert_eq!(
        visual.debug_bounds("connections-editor-viewport"),
        Some(viewport_bounds)
    );
}

/// 后加跳板也必须使测试失效，重排则保持输入实体与凭据所有权一起移动。
/// @author kongweiguang
#[gpui_kit::test]
fn added_jump_input_invalidates_probe_and_reorder_keeps_identity(app: &mut TestAppContext) {
    let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    let (service, mut started, mut finished, release) = gated_probe_service();
    install_probe_service(&panel, visual, service);
    configure_ssh_editor(
        &panel,
        visual,
        true,
        "jump draft",
        "target.example.test",
        HostAuthenticationMode::Agent,
        None,
    );
    update_panel(&panel, visual, |panel, window, cx| {
        for index in 0..2 {
            panel.add_jump(window, cx);
            let jump = &panel.editor.as_ref().unwrap().jumps[index];
            jump.host.update(cx, |input, cx| {
                input.replace_all(format!("jump-{index}.example.test"), window, cx)
            });
            jump.username.update(cx, |input, cx| {
                input.replace_all("fixture-user", window, cx)
            });
        }
    });
    let first = visual.update(|_, app| {
        panel.read(app).editor.as_ref().unwrap().jumps[0]
            .host
            .entity_id()
    });
    let probe = start_probe(&runtime, &panel, visual, &mut started);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.editor.as_ref().unwrap().jumps[0]
            .host
            .update(cx, |input, cx| {
                input.replace_all("edited.example.test", window, cx)
            });
    });
    assert!(
        probe.cancellation.is_cancelled(),
        "新增跳板的输入变化也必须取消旧测试"
    );
    release_probe(&runtime, visual, &release, &mut finished, probe.id);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.move_jump(0, false, window, cx)
    });
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.editor.as_ref().unwrap();
        assert_eq!(editor.jumps[1].host.entity_id(), first);
        assert_eq!(
            editor.jumps[1].host.read(app).value().as_str(),
            "edited.example.test"
        );
        assert!(panel.probe_outcome.is_none());
    });
}

/// 等待被明确放行的 adapter 结束，再泵 GPUI 队列以验证实际面板回调。
fn release_probe(
    runtime: &Runtime,
    visual: &mut VisualTestContext,
    release: &Semaphore,
    finished: &mut mpsc::UnboundedReceiver<usize>,
    expected_id: usize,
) {
    release.add_permits(1);
    let finished_id = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), finished.recv())
            .await
            .expect("受控 probe 应按测试放行完成")
            .expect("测试完成通道保持打开")
    });
    assert_eq!(finished_id, expected_id);
    pump_runtime(runtime, visual);
}

/// 取消后关闭并重开草稿时，旧成功回执不得改变新编辑器或其在途探测状态。
#[gpui_kit::test]
fn canceled_closed_probe_receipt_cannot_mutate_reopened_editor(app: &mut TestAppContext) {
    let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    let (service, mut started, mut finished, release) = gated_probe_service();
    install_probe_service(&panel, visual, service);
    configure_ssh_editor(
        &panel,
        visual,
        true,
        "cancel draft",
        "cancel.example.test",
        HostAuthenticationMode::Agent,
        None,
    );

    let cancelled = start_probe(&runtime, &panel, visual, &mut started);
    click_probe_action(visual);
    assert!(cancelled.cancellation.is_cancelled());
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(!panel.probe_running);
        assert!(panel.probe_cancelled);
        assert!(panel.probe_error.is_none());
        assert!(panel.probe_outcome.is_none());
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx);
    });
    visual.update(|_, app| assert!(panel.read(app).pending_editor_discard));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.confirm_discard_editor(window, cx);
    });
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(panel.editor.is_none(), "确认关闭应销毁原 SSH 草稿");
        assert!(!panel.editor_open());
    });

    configure_ssh_editor(
        &panel,
        visual,
        true,
        "new draft",
        "new.example.test",
        HostAuthenticationMode::Agent,
        None,
    );
    let current = start_probe(&runtime, &panel, visual, &mut started);
    assert!(cancelled.cancellation.is_cancelled());
    assert_ne!(cancelled.id, current.id);

    release_probe(&runtime, visual, &release, &mut finished, cancelled.id);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.editor.as_ref().expect("新 SSH 草稿仍打开");
        assert_eq!(
            editor.inputs.host.read(app).value().as_str(),
            "new.example.test"
        );
        assert!(panel.probe_running, "旧成功回执不能结束新 probe");
        assert!(panel.probe_error.is_none());
        assert!(panel.probe_outcome.is_none());
    });

    release_probe(&runtime, visual, &release, &mut finished, current.id);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(!panel.probe_running);
        assert_eq!(
            panel.probe_outcome,
            Some(ConnectionProbeOutcome::Verified(
                ProbeVerificationLevel::SshAuthenticated,
            ))
        );
    });
}

/// 多行 Textarea 的真实输入经生产表单和 application probe service 变成一次性私钥材料，不进入主机写入路径。
#[gpui_kit::test]
fn pasted_private_key_textarea_reaches_ephemeral_probe_without_host_write(
    app: &mut TestAppContext,
) {
    let (runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    let (service, mut started, mut finished, release) = gated_probe_service();
    install_probe_service(&panel, visual, service);
    configure_ssh_editor(
        &panel,
        visual,
        true,
        "pasted key draft",
        "key.example.test",
        HostAuthenticationMode::PrivateKey,
        Some(PASTED_PRIVATE_KEY),
    );
    let private_key_input = visual.update(|_, app| {
        panel
            .read(app)
            .editor
            .as_ref()
            .expect("SSH 编辑器已打开")
            .inputs
            .private_key
            .clone()
    });

    let probe = start_probe(&runtime, &panel, visual, &mut started);
    assert_eq!(probe.endpoint_host, "key.example.test");
    assert!(
        probe.private_key_matches,
        "textarea 私钥正文应进入 probe material"
    );
    assert!(probe.private_key_reference_is_ephemeral);
    assert!(probe.authentication_reference_matches);
    assert_eq!(probe.credential_count, 1);
    assert!(probe.credential_debug_redacts_material);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(!panel.saving, "测试连接不得启动主机保存事务");
        assert!(panel.pending_host_save_recovery.is_none());
        assert!(!panel.save_outcome_unknown);
    });

    release_probe(&runtime, visual, &release, &mut finished, probe.id);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(
            panel.probe_outcome,
            Some(ConnectionProbeOutcome::Verified(
                ProbeVerificationLevel::SshAuthenticated,
            ))
        );
        assert!(!panel.saving);
    });

    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.confirm_discard_editor(window, cx);
    });
    visual.update(|_, app| {
        assert!(private_key_input.read(app).value().is_empty());
        let panel = panel.read(app);
        assert!(panel.editor.is_none());
        assert!(panel.pending_host_save_recovery.is_none());
    });
}
