//! 当前格式只接受契约 3；契约 1、2 只能通过显式迁移，未知代不猜测。
//!
//! @author kongweiguang

use super::*;

/// 精确列举支持边界，防止用小于当前版本的宽泛判断放行未知旧契约。
#[test]
fn only_known_previous_contracts_are_migratable() {
    assert_eq!(
        classify_marker_bytes(&current_marker_bytes()).expect("当前 marker 可分类"),
        MarkerRead::Current
    );
    assert_eq!(
        classify_marker_bytes(&previous_marker_bytes()).expect("旧 marker 可分类"),
        MarkerRead::PreviousSupported(2)
    );
    assert_eq!(
        classify_marker_bytes(&earliest_marker_bytes()).expect("最早已发布 marker 可分类"),
        MarkerRead::PreviousSupported(1)
    );
    for (format, contract, expected) in [
        (
            4,
            0,
            DataMigrationBlocker::UnsupportedMigrationContract {
                migration_contract: 0,
            },
        ),
        (
            4,
            4,
            DataMigrationBlocker::FutureMigrationContract {
                migration_contract: 4,
            },
        ),
        (
            3,
            1,
            DataMigrationBlocker::UnsupportedDataFormat { data_format: 3 },
        ),
        (
            5,
            1,
            DataMigrationBlocker::FutureDataFormat { data_format: 5 },
        ),
    ] {
        let bytes = format!("data_format = {format}\nmigration_contract = {contract}\n");
        assert_eq!(
            classify_marker_bytes(bytes.as_bytes()).expect("已知字段可分类"),
            MarkerRead::Blocked(expected)
        );
    }
}

/// 旧 marker 可以保留注释和换行供 CAS 备份，但未来字段不因版本号已知而被忽略。
#[test]
fn previous_marker_preserves_semantic_scope_without_accepting_unknown_fields() {
    let source = b"# previous candidate\r\nmigration_contract = 1\r\ndata_format = 4\r\n";
    assert_eq!(
        classify_marker_bytes(source).expect("旧 marker 原文可分类"),
        MarkerRead::PreviousSupported(1)
    );
    assert_eq!(
        classify_marker_bytes(b"data_format = 4\nmigration_contract = 1\nfuture = true\n")
            .expect("未来字段可分类"),
        MarkerRead::Blocked(DataMigrationBlocker::MarkerCorrupt),
    );
}
