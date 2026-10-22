//! Pure terminal emulation and protocol encoding; PTY, network I/O and rendering stay in adapters.
//!
//! @author kongweiguang

mod cursor_style;
pub mod engine;
pub mod input;
pub mod output_capture;
pub mod shell_integration;
pub mod snapshot;

pub use engine::{
    TerminalEngine, TerminalEngineError, TerminalHostEvent, TerminalLimits, TerminalOutput,
};
pub use output_capture::{
    TRANSCRIPT_BLOCK_BYTES_LIMIT, TRANSCRIPT_BLOCK_LIMIT, TRANSCRIPT_SESSION_BYTES_LIMIT,
    TerminalOutputEvent, TerminalTranscriptSnapshot, TerminalTranscriptState, TranscriptCaptureId,
    TranscriptIncompleteReason, TranscriptUnavailableReason,
};
pub use shell_integration::ShellIntegrationMarker;
pub use snapshot::{
    PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot, RenderSnapshotRequest,
    TerminalColor, TerminalCursorCell, TerminalModes, TerminalPalette, TerminalRgb,
    TerminalSearchRequest, TerminalSearchResult, TerminalSelection,
};
