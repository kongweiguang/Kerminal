//! 命令片段、工作流与建议来源的基础设施适配器。
//!
//! @author kongweiguang

mod builtin_snippets;
mod catalog;
mod snippet_import;
mod snippet_transfer_file;
mod snippets;
mod specs;
mod workflows;

pub use catalog::FileCommandCatalogRepository;
pub use snippet_transfer_file::SystemSnippetTransferFileAdapter;
pub use specs::StaticSpecSuggestionProvider;
pub mod suggestion_sources;
pub use suggestion_sources::{RemoteCommandSuggestionProvider, RemoteSuggestionPolicy};
