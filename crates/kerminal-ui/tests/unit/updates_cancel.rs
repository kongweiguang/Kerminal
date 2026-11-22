//! 更新取消与迟到回执的隔离 GPUI 状态回归。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use gpui_kit::{AppContext, TestAppContext};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, DownloadedUpdate, DownloadedUpdateId,
    UpdateCandidate, UpdateCandidateId, UpdateInstallResult, UpdatePort, UpdateProgress,
    UpdateService,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{UpdatePanelState, UpdatesPanel, settings::AboutUpdateAction};

#[derive(Default)]
struct DiscardProbe {
    attempts: AtomicUsize,
}

#[async_trait]
impl UpdatePort for DiscardProbe {
    /// 本回归只处理已产生的下载回执，禁止额外检查掩盖意外调用。
    async fn check(&self, _: CancellationToken) -> AppResult<Option<UpdateCandidate>> {
        Err(AppError::unsupported("测试禁止检查"))
    }

    /// 下载结果由测试精确注入以复现回执排队时取消的竞争。
    async fn download(
        &self,
        _: UpdateCandidateId,
        _: CancellationToken,
        _: watch::Sender<UpdateProgress>,
    ) -> AppResult<DownloadedUpdate> {
        Err(AppError::unsupported("测试禁止下载"))
    }

    /// 本测试不得执行安装，错误可暴露自动安装意外发生。
    async fn install(&self, _: DownloadedUpdateId) -> UpdateInstallResult {
        UpdateInstallResult::NotStarted(AppError::unsupported("测试禁止安装"))
    }

    /// 首次清理故障、第二次成功，验证句柄保留及用户可重试。
    async fn discard(&self, _: DownloadedUpdateId) -> AppResult<()> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(AppError::infrastructure("模拟清理失败"))
        } else {
            Ok(())
        }
    }
}

/// 已发送取消后收到成功下载时只清理句柄；失败可见且再次取消可完成收尾。
#[gpui_kit::test]
fn cancelled_download_success_is_discarded_and_cleanup_can_retry(app: &mut TestAppContext) {
    app.update(crate::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("isolated runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let probe = Arc::new(DiscardProbe::default());
    let service = UpdateService::new(probe.clone());
    let mounted = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted_panel = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|_| UpdatesPanel::new(Some(service), executor));
        *mounted_panel.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = mounted.borrow_mut().take().expect("updates panel mounted");
    let candidate = UpdateCandidate {
        id: UpdateCandidateId::new(),
        version: "0.4.6".into(),
        notes: None,
        published_at: None,
    };
    let downloaded = DownloadedUpdate {
        id: DownloadedUpdateId::new(),
        version: candidate.version.clone(),
    };
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.state = UpdatePanelState::Checking;
            panel.active_check_generation = Some(1);
            panel.finish_check(Ok(Some(candidate.clone())), false, 1, cx);
            assert_eq!(panel.settings_projection().detail, "v0.4.6");
            panel.status_is_error = true;
            assert!(panel.settings_projection().failed);
            panel.state = UpdatePanelState::Checking;
            panel.active_cancel = Some(CancellationToken::new());
            assert!(panel.settings_projection().can_cancel);
            panel.settings_action(AboutUpdateAction::Cancel, cx);
            assert!(panel.settings_projection().cancelling);
            panel.state = UpdatePanelState::Downloading(candidate.clone());
            panel.settings_auto_install = true;
            panel.cancel_requested = true;
            panel.finish_download(candidate, Ok(downloaded.clone()), false, cx);
            assert!(matches!(panel.state, UpdatePanelState::Discarding(_)));
            assert!(panel.settings_projection().cancelling);
        })
    });
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            assert!(matches!(panel.state, UpdatePanelState::ReadyToInstall(_)));
            let projection = panel.settings_projection();
            assert!(projection.failed && projection.can_cancel && !projection.cancelling);
            panel.settings_action(AboutUpdateAction::Cancel, cx);
            assert!(matches!(panel.state, UpdatePanelState::Discarding(_)));
        })
    });
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(matches!(panel.state, UpdatePanelState::Idle));
        assert!(panel.settings_projection().can_check);
    });
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 2);
}
