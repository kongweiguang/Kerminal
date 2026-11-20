//! ReplaceFileW 1175 窄重试的故障注入回归：只重试一次，且任何复核失败都保留首个未知结果。
//!
//! @author kongweiguang

use std::{cell::Cell, fs, io, path::PathBuf};

use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_REMOVE_REPLACED;

use super::{AtomicCommitError, CommitMode, commit_staged_file_with_replace_retry_using};

/// 每个测试使用独立临时目录，避免触碰真实的用户数据根。
struct Fixture {
    _root: tempfile::TempDir,
    staging: PathBuf,
    destination: PathBuf,
    pending: Vec<u8>,
}

impl Fixture {
    /// 写入可提交的 staging 和待替换目标；测试只通过注入的 commit 控制结果，不依赖真实占用。
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create isolated directory");
        let staging = root.path().join("session.json.tmp");
        let destination = root.path().join("session.json");
        let pending = b"{\"version\":3,\"candidate\":true}".to_vec();
        fs::write(&staging, &pending).expect("write staging");
        fs::write(&destination, b"{\"version\":3,\"candidate\":false}").expect("write target");
        Self {
            _root: root,
            staging,
            destination,
            pending,
        }
    }
}

/// 构造绑定版本常量的精确 1175 未知结果，防止测试与生产错误码漂移。
fn replace_busy() -> AtomicCommitError {
    AtomicCommitError::CommitUnknown(io::Error::from_raw_os_error(
        ERROR_UNABLE_TO_REMOVE_REPLACED as i32,
    ))
}

/// 断言返回的仍是首个 CommitUnknown 1175，而不是被降级成确定未提交或新的成功。
fn assert_busy_unknown(result: Result<(), AtomicCommitError>) {
    let Err(AtomicCommitError::CommitUnknown(error)) = result else {
        panic!("expected the original commit-unknown result");
    };
    assert_eq!(
        error.raw_os_error(),
        Some(ERROR_UNABLE_TO_REMOVE_REPLACED as i32)
    );
}

/// 1175 后 staging 与 CAS 复核都通过时只允许一次二次提交，并返回第二次的确定结果。
#[test]
fn replace_1175_is_retried_once_after_revalidation() {
    let fixture = Fixture::new();
    let attempts = Cell::new(0);
    let waits = Cell::new(0);
    let revalidated = Cell::new(0);

    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::ReplaceExisting,
        &fixture.pending,
        || {
            revalidated.set(revalidated.get() + 1);
            true
        },
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(replace_busy())
            } else {
                Ok(())
            }
        },
        || waits.set(waits.get() + 1),
    );

    assert!(result.is_ok());
    assert_eq!(attempts.get(), 2, "1175 后只允许一次重试");
    assert_eq!(waits.get(), 1);
    assert_eq!(revalidated.get(), 1);
}

/// 第二次提交仍是 1175 时必须停止，不能把一次窄重试扩大成循环重试。
#[test]
fn repeated_1175_is_not_retried_again() {
    let fixture = Fixture::new();
    let attempts = Cell::new(0);
    let waits = Cell::new(0);

    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::ReplaceExisting,
        &fixture.pending,
        || true,
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(replace_busy())
        },
        || waits.set(waits.get() + 1),
    );

    assert_busy_unknown(result);
    assert_eq!(attempts.get(), 2, "最多只允许一次重试");
    assert_eq!(waits.get(), 1);
}

/// CAS 复核拒绝（目标 revision 变化或已取消）时保留首个未知结果，绝不覆盖真实冲突。
#[test]
fn rejected_revalidation_preserves_first_unknown() {
    let fixture = Fixture::new();
    let attempts = Cell::new(0);
    let revalidated = Cell::new(0);

    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::ReplaceExisting,
        &fixture.pending,
        || {
            revalidated.set(revalidated.get() + 1);
            false
        },
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(replace_busy())
        },
        || {},
    );

    assert_busy_unknown(result);
    assert_eq!(attempts.get(), 1, "复核拒绝后不能再提交");
    assert_eq!(revalidated.get(), 1);
}

/// 二次提交为 NotCommitted 时仍保留首个未知，避免把一次占用误判成确定未提交。
#[test]
fn second_not_committed_preserves_first_unknown() {
    let fixture = Fixture::new();
    let attempts = Cell::new(0);

    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::ReplaceExisting,
        &fixture.pending,
        || true,
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(replace_busy())
            } else {
                Err(AtomicCommitError::NotCommitted(io::Error::other(
                    "second attempt failed before committing",
                )))
            }
        },
        || {},
    );

    assert_busy_unknown(result);
    assert_eq!(attempts.get(), 2);
}

/// 非 1175 的错误和其他提交模式都不进入重试，原错误分类必须原样返回。
#[test]
fn other_errors_and_modes_are_not_retried() {
    let fixture = Fixture::new();
    let attempts = Cell::new(0);
    let waits = Cell::new(0);
    let revalidated = Cell::new(0);

    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::ReplaceExisting,
        &fixture.pending,
        || {
            revalidated.set(revalidated.get() + 1);
            true
        },
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(AtomicCommitError::CommitUnknown(
                io::Error::from_raw_os_error(5),
            ))
        },
        || waits.set(waits.get() + 1),
    );

    let Err(AtomicCommitError::CommitUnknown(error)) = result else {
        panic!("expected the original access-denied unknown");
    };
    assert_eq!(error.raw_os_error(), Some(5));
    assert_eq!(attempts.get(), 1);
    assert_eq!(waits.get(), 0);
    assert_eq!(revalidated.get(), 0);

    let attempts = Cell::new(0);
    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::CreateNew,
        &fixture.pending,
        || panic!("create-new must not revalidate for a replace retry"),
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(replace_busy())
        },
        || panic!("create-new must not wait for a replace retry"),
    );

    assert_busy_unknown(result);
    assert_eq!(attempts.get(), 1);
}

/// 等待期间 staging 内容被改动时无法确认二次提交输入，必须停止并保留首个未知结果。
#[test]
fn changed_staging_preserves_first_unknown() {
    let fixture = Fixture::new();
    let attempts = Cell::new(0);
    let revalidated = Cell::new(0);
    let staging = fixture.staging.clone();

    let result = commit_staged_file_with_replace_retry_using(
        &fixture.staging,
        &fixture.destination,
        CommitMode::ReplaceExisting,
        &fixture.pending,
        || {
            revalidated.set(revalidated.get() + 1);
            true
        },
        |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(replace_busy())
        },
        || {
            fs::write(&staging, b"tampered and much longer than expected")
                .expect("mutate staging during wait")
        },
    );

    assert_busy_unknown(result);
    assert_eq!(attempts.get(), 1, "staging 不匹配时不能再提交");
    assert_eq!(revalidated.get(), 0, "staging 复核失败时不能调用 CAS 复核");
}
