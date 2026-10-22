//! 容器文本保存 flags 到领域 CAS/显式写入策略的外置契约测试。
//!
//! @author kongweiguang

use kerminal_domain::files::{FileRevision, WriteTextStrategy};

use super::{RevisionArgs, write_strategy};

/// 固定非空 revision 样本，检查所有 flags 组合都不能降级为无 CAS 覆盖。
#[test]
fn expected_revision_always_remains_compare_revision() {
    let expected = FileRevision {
        size: 42,
        modified: Some("1792200000".to_owned()),
        permissions: Some(0o640),
        content_digest: Some("0123456789abcdef".to_owned()),
    };

    for create in [false, true] {
        for overwrite in [false, true] {
            let result = write_strategy(Some(revision_args()), create, overwrite);
            match result {
                Ok(WriteTextStrategy::CompareRevision(actual)) => assert_eq!(actual, expected),
                other => panic!(
                    "expectedRevision must remain CAS for create={create}, overwrite={overwrite}; got {other:?}"
                ),
            }
        }
    }
}

/// 冻结的 MCP 参数允许两个显式 flags 同时为真表示 upsert；策略保留这项兼容意图。
#[test]
fn create_and_overwrite_together_mean_explicit_upsert() {
    assert_eq!(
        write_strategy(None, true, true).expect("explicit upsert is valid"),
        WriteTextStrategy::ExplicitOverwrite
    );
}

/// 无 revision 时只接受明确的新建或覆盖意图，避免悄悄猜测默认写入策略。
#[test]
fn no_revision_requires_create_or_overwrite_intent() {
    assert_eq!(
        write_strategy(None, true, false).expect("create-only request is valid"),
        WriteTextStrategy::CreateNew
    );
    assert_eq!(
        write_strategy(None, false, true).expect("overwrite-only request is valid"),
        WriteTextStrategy::ExplicitOverwrite
    );
    assert!(write_strategy(None, false, false).is_err());
}

/// 返回与实际 MCP expectedRevision 一致的完整旧字段供策略 mapper 转换。
fn revision_args() -> RevisionArgs {
    RevisionArgs {
        size: 42,
        modified: Some("1792200000".to_owned()),
        permissions: Some("0640".to_owned()),
        permissions_mode: Some(0o640),
        content_sha256: Some("0123456789abcdef".to_owned()),
    }
}
