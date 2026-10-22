//! 命令目录、历史、建议和批量执行的领域规则。
//!
//! @author kongweiguang

pub mod broadcast;
pub mod history;
pub mod snippet;
pub mod suggestion;
pub mod workflow;

pub use broadcast::{BroadcastAnalysis, BroadcastRisk, BroadcastTarget};
pub use history::{
    CommandHistoryEntry, CommandHistoryListRequest, CommandHistoryRecordRequest,
    CommandHistoryRecordResult, CommandHistorySource, CommandHistoryTarget,
    CommandHistoryValidationError, NormalizedCommandHistory, validate_history_context,
};
pub use snippet::{
    SnippetApplicabilityAssessment, SnippetApplicabilityReason, SnippetApplicabilityStatus,
    SnippetCatalogItem, SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetContextBinding,
    SnippetContextBindingKind, SnippetDefaultAction, SnippetDefinition, SnippetPreference,
    SnippetPreferenceKey, SnippetRisk, SnippetScope, SnippetUsageAction, SnippetUsageClearResult,
    SnippetUsageReceipt, SnippetUsageRecordRequest, SnippetValidationError, SnippetVariable,
    SnippetVariableKind, SnippetVariableRenderStrategy, assess_snippet_applicability,
};
pub use suggestion::{
    CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionFeedbackAction,
    CommandSuggestionFeedbackRequest, CommandSuggestionReplacementRange, CommandSuggestionRequest,
    CommandSuggestionSensitivity, SuggestionPresentation, SuggestionProviderKind,
    SuggestionQueryMode,
};
pub use workflow::{
    CommandWorkflow, CommandWorkflowStep, WorkflowListRequest, WorkflowRun, WorkflowRunState,
    WorkflowScope, WorkflowStepInput, WorkflowStepResult, WorkflowStepState,
    WorkflowTransitionError, WorkflowUpdateRequest, WorkflowValidationError, validate_workflow,
};
