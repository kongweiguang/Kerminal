//! 为 reader 测试构造确定性的 Bash 探测状态。
//!
//! @author kongweiguang

use super::*;

impl BashProbeGate {
    /// 直接设置探测状态，让 reader 测试不依赖真实握手或 writer。
    pub(in crate::sessions) fn bash_for_test(pending: bool) -> Self {
        Self::bash_for_test_with_deadline(pending, false)
    }

    /// 保留已过期的探测记录，模拟超时先于清理发生。
    pub(in crate::sessions) fn bash_with_expired_probe_for_test() -> Self {
        Self::bash_for_test_with_deadline(true, true)
    }

    /// 截止时间直接取当前时刻表示已过期，避免依赖真实计时器。
    fn bash_for_test_with_deadline(pending: bool, expired: bool) -> Self {
        let mut editing = EditingRuntime::new(1, Some("test-nonce".to_owned()));
        editing.status = EditingStatus::Installed {
            shell: EditingShell::Bash,
            version: "5.1.0".to_owned(),
        };
        if pending {
            let (reply, _receiver) = oneshot::channel();
            let now = tokio::time::Instant::now();
            editing.pending = Some(PendingProbe {
                id: 1,
                binding: UiSessionBinding {
                    session_id: kerminal_domain::identity::SessionId::new(),
                    owner: SessionOwner::Mcp,
                    session_generation: 1,
                },
                context: TerminalCommandContextVersion {
                    session_generation: 1,
                    prompt_epoch: 0,
                    input_generation: 0,
                },
                deadline: if expired { now } else { now + PROBE_TIMEOUT },
                reply: Some(reply),
            });
        }
        Self::new(Arc::new(StdMutex::new(editing)), 1)
    }
}
