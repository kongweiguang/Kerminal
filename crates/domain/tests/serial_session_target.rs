//! 串口目标的冻结默认线制回归测试。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::sessions::{
    SerialDataBits, SerialFlowControl, SerialParity, SerialSessionTarget, SerialStopBits,
};

/// 固定旧连接配置未填写线制时的兼容默认值，避免新枚举改变既有串口行为。
///
/// @author kongweiguang
#[test]
fn target_constructor_uses_frozen_9600_8n1_without_flow_control() {
    let target = SerialSessionTarget::new(PathBuf::from("COM1"));

    assert_eq!(target.baud_rate, 9600);
    assert_eq!(target.data_bits, SerialDataBits::Eight);
    assert_eq!(target.stop_bits, SerialStopBits::One);
    assert_eq!(target.parity, SerialParity::None);
    assert_eq!(target.flow_control, SerialFlowControl::None);
}
