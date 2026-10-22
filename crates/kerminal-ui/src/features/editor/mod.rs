//! 基于 GPUI Kit 编辑组件的本机与远端文本编辑工作区。
//!
//! @author kongweiguang

mod confirmation;
mod context_menu;
mod file_io;
mod language;
mod quit_gate;
mod render;
mod search;
mod search_model;
mod workspace;

pub use quit_gate::{
    EditorQuitGate, EditorSaveFailureAction, EditorSaveSuccessAction, editor_save_failure_action,
    editor_save_success_action, evaluate_editor_quit_gate,
};
pub(crate) use workspace::{EditorTabProjection, EditorWorkspaceProjection};
pub use workspace::{FileEditorWorkspace, FileEditorWorkspaceEvent};
