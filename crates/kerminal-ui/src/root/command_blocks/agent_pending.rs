//! Agent command-block pending-send state shared by the Root coordinator and retained Agent panel.
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use kerminal_domain::identity::{AgentSessionId, SessionId, TabId};

use crate::features::agents::{
    AgentPendingSend, AgentPendingSendPreview, AgentSendHistoryMetadata, AgentSendOutcome,
};

use super::{
    RequestTicket,
    agent::{CapturedAgentCommandSource, CapturedAgentCommandTarget},
};

pub(super) const PENDING_SEND_TTL: Duration = Duration::from_secs(60);
pub(super) const AGENT_SEND_HISTORY_LIMIT: usize = 8;

/// One request owns a frozen source revision, optional exact target, short-lived preview and TTL.
pub(super) struct PendingAgentSendEntry {
    pub(super) request: AgentPendingSend,
    pub(super) source: CapturedAgentCommandSource,
    pub(super) source_tab_id: Option<TabId>,
    pub(super) ticket: RequestTicket,
    pub(super) target_selection: Option<PendingAgentTargetSelection>,
    pub(super) target: Option<CapturedAgentCommandTarget>,
    pub(super) preview: Option<AgentPendingSendPreview>,
    pub(super) deadline: Instant,
    pub(super) target_generation: u64,
    pub(super) unknown_warning_visible: bool,
    pub(super) submission_in_flight: bool,
}

/// Target choice includes the Agent watch generation so a reused terminal ID cannot satisfy a stale wait.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingAgentTargetSelection {
    pub(super) agent_session_id: AgentSessionId,
    pub(super) terminal_session_id: SessionId,
    pub(super) watch_generation: u64,
}

/// Send history callback data contains only safe metadata; body text remains solely in the preview.
#[derive(Clone)]
pub(super) struct AgentSendHistoryContext {
    pub(super) agent_session_id: AgentSessionId,
    pub(super) byte_length: usize,
    pub(super) created_at: String,
    pub(super) request_id: String,
    pub(super) source_label: String,
    pub(super) tab_id: Option<TabId>,
}

/// The pending request is global like main's request coordinator, while metadata stays scoped per Tab.
#[derive(Default)]
pub(super) struct AgentPendingSendState {
    next_request_id: u64,
    pub(super) active: Option<PendingAgentSendEntry>,
    history_by_tab: HashMap<Option<TabId>, Vec<AgentSendHistoryMetadata>>,
}

impl AgentPendingSendState {
    /// Generate an opaque in-process identity; only equality is meaningful to panel callbacks.
    pub(super) fn next_request_id(&mut self) -> String {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        format!("agent-command-block-{}", self.next_request_id)
    }

    /// Take only the matching active request so a stale panel event cannot consume a replacement.
    pub(super) fn take(&mut self, request_id: &str) -> Option<PendingAgentSendEntry> {
        if self
            .active
            .as_ref()
            .is_some_and(|entry| entry.request.request_id == request_id)
        {
            self.active.take()
        } else {
            None
        }
    }

    /// Build the panel projection for one tab without exposing body text in its history metadata.
    pub(super) fn panel_state(
        &self,
        tab_id: Option<&TabId>,
    ) -> (
        Option<AgentPendingSend>,
        Option<AgentPendingSendPreview>,
        Vec<AgentSendHistoryMetadata>,
    ) {
        let active = self
            .active
            .as_ref()
            .filter(|entry| entry.source_tab_id.as_ref() == tab_id);
        (
            active.map(|entry| entry.request.clone()),
            active.and_then(|entry| entry.preview.clone()),
            self.history_by_tab
                .get(&tab_id.cloned())
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// Replace an existing request's outcome or prepend a new row, preserving the eight-row UI bound.
    pub(super) fn record_outcome(
        &mut self,
        context: AgentSendHistoryContext,
        outcome: AgentSendOutcome,
    ) {
        let history = self.history_by_tab.entry(context.tab_id).or_default();
        let metadata = AgentSendHistoryMetadata {
            agent_session_id: context.agent_session_id,
            byte_length: context.byte_length,
            created_at: context.created_at,
            outcome,
            request_id: context.request_id,
            source_label: context.source_label,
        };
        if let Some(existing) = history
            .iter_mut()
            .find(|item| item.request_id == metadata.request_id)
        {
            *existing = metadata;
        } else {
            history.insert(0, metadata);
        }
        history.truncate(AGENT_SEND_HISTORY_LIMIT);
    }

    /// Create history only after an Agent target exists; no synthetic session IDs are used for pre-target cancellation.
    pub(super) fn entry_outcome(
        &mut self,
        entry: &PendingAgentSendEntry,
        outcome: AgentSendOutcome,
    ) {
        let Some(target) = entry.target_selection.as_ref() else {
            return;
        };
        self.record_outcome(
            AgentSendHistoryContext {
                agent_session_id: target.agent_session_id.clone(),
                byte_length: entry
                    .preview
                    .as_ref()
                    .map_or(0, |preview| preview.byte_length),
                created_at: chrono::Utc::now().to_rfc3339(),
                request_id: entry.request.request_id.clone(),
                source_label: entry.request.source_label.clone(),
                tab_id: entry.source_tab_id.clone(),
            },
            outcome,
        );
    }
}
