//! 设置显示单位转换的边界测试。
//!
//! @author kongweiguang

use super::{parse_scaled_u32, parse_scaled_update};

/// 小数秒和 MiB 按存储单位四舍五入，支持 SFTP 的 32 KiB 步进输入。
#[test]
fn scaled_settings_inputs_round_to_milliseconds_and_bytes() {
    assert_eq!(parse_scaled_u32("1.25", 1_000.), Ok(1_250));
    assert_eq!(parse_scaled_u32("0.03125", 1_048_576.), Ok(32_768));
    assert_eq!(parse_scaled_u32("0.1", 1_048_576.), Ok(104_858));
}

/// 不接受非有限数值、负数或缩放后超出配置字段范围的输入。
#[test]
fn scaled_settings_inputs_reject_invalid_or_overflowing_values() {
    assert!(parse_scaled_u32("NaN", 1_000.).is_err());
    assert!(parse_scaled_u32("inf", 1_000.).is_err());
    assert!(parse_scaled_u32("-0.5", 1_000.).is_err());
    assert!(parse_scaled_u32("5000", 1_048_576.).is_err());
}

/// 字段边界在显示单位转换后校验，避免配置层静默钳制用户输入。
#[test]
fn scaled_settings_inputs_enforce_notification_and_packet_ranges() {
    let mut parsed = None;
    assert!(
        parse_scaled_update("0.5", "minimum", 1_000., 1_000, 120_000, |value| {
            parsed = Some(value)
        })
        .1
        .is_some()
    );
    assert!(
        parse_scaled_update("0.5", "packet", 1_048_576., 32_768, 262_144, |value| {
            parsed = Some(value)
        })
        .1
        .is_some()
    );
    assert!(
        parse_scaled_update("0.25", "packet", 1_048_576., 32_768, 262_144, |value| {
            parsed = Some(value)
        })
        .1
        .is_none()
    );
    assert_eq!(parsed, Some(262_144));
}

/// 输入格式错误不替换 typed 值；恢复成当前值后解析成功，调用方可正常清错并安排保存。
#[test]
fn restoring_an_invalid_input_to_its_existing_value_is_accepted() {
    let mut stored = 10_000;
    let (_, error) = parse_scaled_update("invalid", "minimum", 1_000., 1_000, 120_000, |value| {
        stored = value;
    });
    assert!(error.is_some());
    assert_eq!(stored, 10_000);

    let (_, error) = parse_scaled_update("10", "minimum", 1_000., 1_000, 120_000, |value| {
        stored = value;
    });
    assert!(error.is_none());
    assert_eq!(stored, 10_000);
}
