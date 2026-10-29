//! Command-line and output isolation checks for the hidden settings runner.
//!
//! @author kongweiguang

use std::{ffi::OsString, fs};

use super::{
    create_requested_output_directory, create_unique_output_directory, ensure_hidden_state,
    parse_dimension, parse_options,
};

/// Keeps argument construction local so paths with spaces exercise the same token boundaries as `args_os`.
fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

/// Preserves a single output path token while accepting dimensions inside the supported viewport range.
#[test]
fn parser_accepts_spaces_and_bounded_dimensions() {
    let options = parse_options(arguments(&[
        "--output-dir",
        "C:\\diagnostics\\settings parity run",
        "--width",
        "1440",
        "--height",
        "900",
    ]))
    .expect("valid hidden-window options should parse");

    assert_eq!(
        options.output_directory.as_deref(),
        Some(std::path::Path::new("C:\\diagnostics\\settings parity run"))
    );
    assert_eq!(options.width, Some(1440));
    assert_eq!(options.height, Some(900));
}

/// Rejects duplicate, unknown, and truncated arguments before they can create output or a native window.
#[test]
fn parser_rejects_repeated_unknown_and_missing_values() {
    assert!(parse_options(arguments(&["--width", "1440", "--width", "1600"])).is_err());
    assert!(parse_options(arguments(&["--output-dir", "one", "--output-dir", "two"])).is_err());
    assert!(parse_options(arguments(&["--unknown"])).is_err());
    assert!(parse_options(arguments(&["--height"])).is_err());
}

/// Rejects dimensions outside supported logical bounds rather than opening an unusable hidden viewport.
#[test]
fn dimension_parser_enforces_bounds_and_integer_input() {
    assert_eq!(
        parse_dimension(OsString::from("800"), "--width", 800, 3840)
            .expect("minimum width is allowed"),
        800
    );
    assert_eq!(
        parse_dimension(OsString::from("2160"), "--height", 600, 2160)
            .expect("maximum height is allowed"),
        2160
    );
    assert!(parse_dimension(OsString::from("799"), "--width", 800, 3840).is_err());
    assert!(parse_dimension(OsString::from("900.5"), "--height", 600, 2160).is_err());
}

/// Creates only a new leaf and leaves a prior artifact directory byte-for-byte untouched on collision.
#[test]
fn requested_output_directory_never_replaces_existing_evidence() {
    let parent = tempfile::tempdir().expect("temporary parent should be available");
    let output = parent.path().join("settings parity");
    let created =
        create_requested_output_directory(&output).expect("new output leaf should be created");
    assert_eq!(created, output);

    let marker = output.join("report.json");
    fs::write(&marker, b"existing evidence").expect("marker should be written");
    assert!(create_requested_output_directory(&output).is_err());
    assert_eq!(
        fs::read(marker).expect("existing marker should remain"),
        b"existing evidence"
    );
}

/// Allocates distinct run roots so repeated captures never overwrite reports or screenshots.
#[test]
fn unique_output_roots_are_distinct() {
    let parent = tempfile::tempdir().expect("temporary parent should be available");
    let base = parent.path().join("settings-parity");
    let first =
        create_unique_output_directory(base.clone()).expect("first run root should be created");
    let second = create_unique_output_directory(base).expect("second run root should be created");

    assert_ne!(first, second);
    assert!(first.is_dir());
    assert!(second.is_dir());
}

/// Fails closed whenever a native platform exposes or activates the supposedly isolated window.
#[test]
fn hidden_window_gate_rejects_visible_or_active_state() {
    let unproven_isolation =
        crate::right_rail_acceptance::isolation::DesktopIsolationEvidence::default();
    assert!(ensure_hidden_state(&unproven_isolation, false, false).is_ok());
    assert!(ensure_hidden_state(&unproven_isolation, true, false).is_err());
    assert!(ensure_hidden_state(&unproven_isolation, false, true).is_err());
}
