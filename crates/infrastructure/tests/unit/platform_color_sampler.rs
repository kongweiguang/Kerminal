//! Regression coverage for the pure color-sampler input state machine.
//!
//! @author kongweiguang

use std::time::Duration;

use super::{
    MouseCaptureInput, MouseCapturePhase, bounded_sampling_timeout, transition_mouse_capture_phase,
};

/// Keeps the eyedropper trigger's release available to its original UI control.
#[test]
fn trigger_release_passes_through_and_arms_sampling() {
    assert_eq!(
        transition_mouse_capture_phase(
            MouseCapturePhase::AwaitingTriggerRelease,
            MouseCaptureInput::LeftButtonUp,
        ),
        (MouseCapturePhase::Armed, false)
    );
}

/// Requires both edges of the first selection click to be swallowed before returning a color.
#[test]
fn selection_down_and_up_are_swallowed_once() {
    let (phase, down_swallowed) =
        transition_mouse_capture_phase(MouseCapturePhase::Armed, MouseCaptureInput::LeftButtonDown);
    assert_eq!(phase, MouseCapturePhase::AwaitingSelectionRelease);
    assert!(down_swallowed);

    let (phase, up_swallowed) =
        transition_mouse_capture_phase(phase, MouseCaptureInput::LeftButtonUp);
    assert_eq!(phase, MouseCapturePhase::Selected);
    assert!(up_swallowed);
}

/// Ensures keyboard, movement, wheel, and every non-left-button input remain untouched in each phase.
#[test]
fn other_input_is_never_swallowed() {
    for phase in [
        MouseCapturePhase::Inactive,
        MouseCapturePhase::AwaitingTriggerRelease,
        MouseCapturePhase::Armed,
        MouseCapturePhase::AwaitingSelectionRelease,
        MouseCapturePhase::Selected,
    ] {
        assert_eq!(
            transition_mouse_capture_phase(phase, MouseCaptureInput::Other),
            (phase, false)
        );
    }
}

/// Prevents later clicks from leaking into another app after the one requested sample completes.
#[test]
fn later_clicks_pass_through_after_selection() {
    assert_eq!(
        transition_mouse_capture_phase(
            MouseCapturePhase::Selected,
            MouseCaptureInput::LeftButtonDown,
        ),
        (MouseCapturePhase::Selected, false)
    );
    assert_eq!(
        transition_mouse_capture_phase(
            MouseCapturePhase::Selected,
            MouseCaptureInput::LeftButtonUp,
        ),
        (MouseCapturePhase::Selected, false)
    );
}

/// Keeps caller timeouts intact below the cap and bounds oversized or infinite requests.
#[test]
fn timeout_is_capped_without_extending_short_requests() {
    assert_eq!(
        bounded_sampling_timeout(Duration::from_secs(45)),
        Duration::from_secs(30)
    );
    assert_eq!(
        bounded_sampling_timeout(Duration::from_millis(250)),
        Duration::from_millis(250)
    );
    assert_eq!(bounded_sampling_timeout(Duration::ZERO), Duration::ZERO);
}
