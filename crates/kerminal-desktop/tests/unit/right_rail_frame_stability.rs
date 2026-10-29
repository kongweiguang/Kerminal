//! Pixel-phase tracking tests for the isolated native right-rail screenshot runner.
//!
//! @author kongweiguang

use super::{ExpectedFrameRestoration, StableFrameCandidates};

/// A/A/B/B converges only when the second pixel phase itself recurs.
///
/// @author kongweiguang
#[test]
fn repeated_phases_converge_for_aabb_sequence() {
    let mut candidates = StableFrameCandidates::default();

    assert!(!candidates.observe("A"));
    assert!(!candidates.observe("A"));
    assert!(!candidates.observe("B"));
    assert!(candidates.observe("B"));
    assert!(candidates.contains("A"));
    assert!(candidates.contains("B"));
}

/// A/B alternation is stable only after both known caret phases recur.
///
/// @author kongweiguang
#[test]
fn alternating_phases_converge_after_recurrence() {
    let mut candidates = StableFrameCandidates::default();

    assert!(!candidates.observe("A"));
    assert!(!candidates.observe("B"));
    assert!(!candidates.observe("A"));
    assert!(candidates.observe("B"));
}

/// A third distinct pixel signature clears old phases and restarts stability from that frame.
///
/// @author kongweiguang
#[test]
fn third_phase_resets_candidates_and_sample_count() {
    let mut candidates = StableFrameCandidates::default();

    assert!(!candidates.observe("A"));
    assert!(!candidates.observe("B"));
    assert!(!candidates.observe("A"));
    assert!(!candidates.observe("C"));

    assert!(!candidates.is_stable());
    assert!(!candidates.contains("A"));
    assert!(!candidates.contains("B"));
    assert!(candidates.contains("C"));
}

/// Alternating through three different signatures never keeps an older phase as a stable candidate.
///
/// @author kongweiguang
#[test]
fn three_phase_animation_never_converges() {
    let mut candidates = StableFrameCandidates::default();

    for signature in ["A", "B", "C", "A", "B", "C"] {
        assert!(!candidates.observe(signature));
    }

    assert!(!candidates.is_stable());
    assert_eq!(candidates.signatures.len(), 2);
    assert!(candidates.contains("B"));
    assert!(candidates.contains("C"));
    assert!(!candidates.contains("A"));
}

/// The image written to disk must match one of the candidates accepted before its capture.
///
/// @author kongweiguang
#[test]
fn saved_frame_must_match_a_stable_candidate() {
    let mut candidates = StableFrameCandidates::default();

    assert!(!candidates.observe("A"));
    assert!(!candidates.observe("B"));
    assert!(!candidates.observe("A"));
    assert!(candidates.observe("B"));

    assert!(candidates.accepts_saved_capture("A"));
    assert!(candidates.accepts_saved_capture("B"));
    assert!(!candidates.accepts_saved_capture("C"));
}

/// Restoring the full-window baseline accepts repeated samples from either proven caret phase.
///
/// @author kongweiguang
#[test]
fn expected_frame_restoration_accepts_alternating_baseline_phases() {
    let expected = vec!["A".to_owned(), "B".to_owned()];
    let mut restoration = ExpectedFrameRestoration::new(&expected);

    assert!(!restoration.observe("A"));
    assert!(restoration.observe("B"));
}

/// An unrecorded full-frame phase breaks the restoration streak and never becomes an accepted baseline.
///
/// @author kongweiguang
#[test]
fn unexpected_frame_breaks_restoration_streak() {
    let expected = vec!["A".to_owned(), "B".to_owned()];
    let mut restoration = ExpectedFrameRestoration::new(&expected);

    assert!(!restoration.observe("A"));
    assert!(!restoration.observe("C"));
    assert!(!restoration.observe("B"));
    assert!(restoration.observe("A"));
}
