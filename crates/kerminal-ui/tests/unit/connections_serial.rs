//! 串口设备选择器区分服务不可用、枚举失败与真实空列表。
//!
//! @author kongweiguang

use super::{SerialPortListState, serial_port_list_state};
use crate::features::connections::panel::SerialPortScanIssue;

/// 未注入平台服务时给出不可用提示，不把此状态显示成空设备目录。
/// @author kongweiguang
#[test]
fn missing_service_is_not_reported_as_an_empty_directory() {
    assert_eq!(
        serial_port_list_state(false, false, false, None, 0),
        SerialPortListState::ServiceUnavailable
    );
}

/// 明确区分加载、取消、驱动失败、未扫描和空扫描反馈。
/// @author kongweiguang
#[test]
fn scan_feedback_preserves_each_completion_state() {
    assert_eq!(
        serial_port_list_state(true, true, false, None, 0),
        SerialPortListState::Loading
    );
    assert_eq!(
        serial_port_list_state(true, false, true, Some(SerialPortScanIssue::Cancelled), 0),
        SerialPortListState::Cancelled
    );
    assert_eq!(
        serial_port_list_state(
            true,
            false,
            true,
            Some(SerialPortScanIssue::EnumerationFailed),
            0
        ),
        SerialPortListState::Failed
    );
    assert_eq!(
        serial_port_list_state(true, false, false, None, 0),
        SerialPortListState::NotEnumerated
    );
    assert_eq!(
        serial_port_list_state(true, false, true, None, 0),
        SerialPortListState::Empty
    );
    assert_eq!(
        serial_port_list_state(true, false, true, None, 1),
        SerialPortListState::Available
    );
}
