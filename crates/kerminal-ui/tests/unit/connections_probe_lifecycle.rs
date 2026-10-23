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

/// 由生产按钮对应的 `test_connection` 入口启动后端，并等待确切请求到达受控边界。
fn start_probe(
    runtime: &Runtime,
    panel: &Entity<ConnectionsPanel>,
    visual: &mut VisualTestContext,
    started: &mut mpsc::UnboundedReceiver<ProbeStarted>,
) -> ProbeStarted {
    update_panel(panel, visual, |panel, window, cx| {
        panel.test_connection(window, cx);
    });
    pump_runtime(runtime, visual);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), started.recv())
            .await
            .expect("连接测试应到达注入的后端")
            .expect("测试观察通道保持打开")
    })
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
    update_panel(&panel, visual, |panel, _, cx| {
        panel.cancel_connection_probe(cx);
    });
    assert!(cancelled.cancellation.is_cancelled());
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(!panel.probe_running);
        assert!(panel.probe_error.is_some());
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
