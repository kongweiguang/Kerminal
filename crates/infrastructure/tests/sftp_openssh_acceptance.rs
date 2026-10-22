//! 使用任务私有 OpenSSH internal-sftp 验证真实文件与传输链路。
//!
//! 环境契约和清理边界见 support/sftp_acceptance/。
//! 综合场景按 sftp_openssh_file_transfer_editor_and_resume 精确筛选；本机源链接权限另由
//! sftp_openssh_local_source_symlink_policy 显式验收，避免平台权限缺失遮蔽远端协议场景。
//!
//! @author kongweiguang

#[path = "support/sftp_acceptance/mod.rs"]
mod sftp_acceptance;
mod support;

use std::io;

use sftp_acceptance::{
    AcceptanceResult, OpenSshFixtureConfig, SftpAcceptanceFixture,
    exercise_local_source_symlink_policy, exercise_sftp, prepare_target_workspace, run_stage,
};

/// 执行真实 OpenSSH 文件/传输场景，避免场景失败时遮蔽 cleanup 失败证据。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private OpenSSH internal-sftp fixture; see support/sftp_acceptance"]
async fn sftp_openssh_file_transfer_editor_and_resume() -> AcceptanceResult {
    let config = run_stage("fixture.validate_environment", async {
        OpenSshFixtureConfig::from_environment()
    })
    .await?;
    let fixture = run_stage("fixture.construct_services", async {
        SftpAcceptanceFixture::new(&config)
    })
    .await?;
    let exercise = run_stage("scenario.all", exercise_sftp(&fixture)).await;
    let cleanup = run_stage("fixture.cleanup", fixture.cleanup()).await;
    match (exercise, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(exercise_error), Ok(())) => Err(exercise_error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(exercise_error), Err(cleanup_error)) => Err(io::Error::other(format!(
            "SFTP scenario failed ({exercise_error}); fixture cleanup also failed ({cleanup_error})"
        ))
        .into()),
    }
}

/// 单独验证本机源链接策略，避免本机权限缺失阻断主综合用例中的 mtime 与远端链接证据。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private OpenSSH fixture and local file-symlink privileges; permission failures are intentionally visible"]
async fn sftp_openssh_local_source_symlink_policy() -> AcceptanceResult {
    let config = run_stage("fixture.validate_environment", async {
        OpenSshFixtureConfig::from_environment()
    })
    .await?;
    let fixture = run_stage("fixture.construct_services", async {
        SftpAcceptanceFixture::new(&config)
    })
    .await?;
    let preparation = run_stage(
        "workspace.create_target",
        prepare_target_workspace(&fixture),
    )
    .await;
    let exercise = match preparation {
        Ok(()) => {
            run_stage(
                "scenario.local_source_symlink_policy",
                exercise_local_source_symlink_policy(&fixture),
            )
            .await
        }
        Err(error) => Err(error),
    };
    let cleanup = run_stage("fixture.cleanup", fixture.cleanup()).await;
    match (exercise, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(exercise_error), Ok(())) => Err(exercise_error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(exercise_error), Err(cleanup_error)) => Err(io::Error::other(format!(
            "local source symlink scenario failed ({exercise_error}); fixture cleanup also failed ({cleanup_error})"
        ))
        .into()),
    }
}
