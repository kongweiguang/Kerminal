//! 终端工具栏的纯投影、交互状态与类型化意图。
//!
//! @author kongweiguang

pub mod broadcast;
pub mod command_blocks;
pub mod keywords;
pub mod links;
pub mod logging;
pub mod search;
pub mod selection;
mod view;

pub use broadcast::{BroadcastEvent, BroadcastTargetPicker, BroadcastTargetProjection};
pub(crate) use command_blocks::LatestSubmittedCommandBlock;
pub use command_blocks::{
    CommandBlockAction, CommandBlockContent, CommandBlockContentUnavailable, CommandBlockEvent,
    CommandBlockProjection, CommandBlockTimeline, TextHighlightRange, TextSegment,
    command_block_timeline_visible, normalized_text_segments,
};
pub use keywords::{KeywordCellMatch, KeywordCellStyle, KeywordOverlayProjector};
pub use links::{SafeTerminalLink, TerminalLinkError, validate_terminal_link};
pub use logging::{TerminalLogControls, TerminalLogEvent};
pub use search::{SearchOverlayPlacement, TerminalSearchEvent, TerminalSearchToolbar};
pub use selection::{SelectionControls, TerminalSelectionEvent, selection_from_viewport_drag};
pub use view::{TerminalToolsEvent, TerminalToolsView};
