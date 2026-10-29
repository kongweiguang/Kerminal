//! 仅供隔离验收调用的生产 Logs 入口。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{AppError, sessions::UiSessionBinding};
use kerminal_domain::{
    commands::{CommandHistoryRecordRequest, CommandHistorySource, CommandHistoryTarget},
    identity::HostId,
    sessions::SessionOwner,
};
use tokio_util::sync::CancellationToken;

use super::{LogsTarget, LogsView};
use crate::features::commands::history_query::{PAGE_SIZE, page_window};

const USER_SEED_COUNT: usize = 9;
const TOOL_SEED_COUNT: usize = 1;

#[derive(Default)]
pub(super) struct LogsAcceptanceState {
    generation: u64,
    seed: Option<SeedRun>,
}

struct SeedRun {
    generation: u64,
    run_id: String,
    prefix: String,
    binding: UiSessionBinding,
    target: LogsTarget,
    expected_count: usize,
    recorded_count: usize,
    phase: SeedPhase,
    error: Option<String>,
    cancellation: Option<CancellationToken>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SeedPhase {
    Pending,
    Complete,
    Failed,
}

pub struct RightRailAcceptanceLogsSnapshot {
    run_id: String,
    prefix: String,
    query: String,
    source: Option<CommandHistorySource>,
    page: usize,
    page_count: usize,
    total_entries: usize,
    seeded_entries: usize,
    entries: Vec<RightRailAcceptanceLogsEntry>,
}

pub struct RightRailAcceptanceLogsEntry {
    id: String,
    command: String,
    source: CommandHistorySource,
}

impl RightRailAcceptanceLogsSnapshot {
    /// 供外部验收驱动器校验读回归属，避免把上一次 run 的快照误认为当前结果。
    ///
    /// @author kongweiguang
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// 供验收驱动器保持搜索、删除和清理范围一致，避免触及无关历史记录。
    ///
    /// @author kongweiguang
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// 供验收驱动器确认输入与已完成查询使用同一条件，避免只验证数据库结果而漏掉界面状态。
    ///
    /// @author kongweiguang
    pub fn query(&self) -> &str {
        &self.query
    }

    /// 供验收驱动器区分来源筛选状态与查询文本，验证来源筛选确实作用于生产列表。
    ///
    /// @author kongweiguang
    pub fn source(&self) -> Option<CommandHistorySource> {
        self.source
    }

    /// 使用生产界面的 1-based 页码，方便验收确认分页操作后的实际选择。
    ///
    /// @author kongweiguang
    pub fn page(&self) -> usize {
        self.page
    }

    /// 按当前搜索与来源筛选计算边界，供验收验证页码和页数随结果变化。
    ///
    /// @author kongweiguang
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    /// 统计所有分页中的筛选结果，不把当前页行数误当成总量。
    ///
    /// @author kongweiguang
    pub fn total_entries(&self) -> usize {
        self.total_entries
    }

    /// 回读历史服务确认数，防止将被策略跳过或部分失败的种子当成写入成功。
    ///
    /// @author kongweiguang
    pub fn seeded_entries(&self) -> usize {
        self.seeded_entries
    }

    /// 只暴露当前页的验收字段，使驱动器能检查实际列表并将删除限制到可见记录。
    ///
    /// @author kongweiguang
    pub fn entries(&self) -> &[RightRailAcceptanceLogsEntry] {
        &self.entries
    }
}

impl RightRailAcceptanceLogsEntry {
    /// 仅用于将删除动作绑定到当前可见记录，避免用列表索引或旧目标 ID 操作。
    ///
    /// @author kongweiguang
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 用于验证前缀搜索和来源分页，限制验收读回只暴露所需命令文本。
    ///
    /// @author kongweiguang
    pub fn command(&self) -> &str {
        &self.command
    }

    /// 用于核对 User/Tool 样本和过滤结果，不要求驱动器读取完整历史实体。
    ///
    /// @author kongweiguang
    pub fn source(&self) -> CommandHistorySource {
        self.source
    }
}

impl LogsAcceptanceState {
    /// 目标变化或隐藏右栏时作废待写入种子，因为写入回执绑定于单一 pane。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate_pending(&mut self, reason: &str) -> bool {
        let Some(seed) = self.seed.as_mut() else {
            return false;
        };
        if seed.phase != SeedPhase::Pending {
            return false;
        }
        if let Some(cancellation) = seed.cancellation.take() {
            cancellation.cancel();
        }
        seed.phase = SeedPhase::Failed;
        seed.error = Some(reason.to_owned());
        true
    }
}

impl LogsView {
    /// 通过 CommandsService 写入唯一 fixture 前缀，并将每条记录绑定到已验证的可见 SSH pane。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_seed_history(
        &mut self,
        run_id: &str,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_run_id(run_id)?;
        let target = self.acceptance_target(binding)?.clone();
        if let Some(seed) = self.acceptance.seed.as_ref() {
            if seed.run_id == run_id {
                if seed.binding != *binding || seed.target != target {
                    return Err("同一验收 runId 已绑定到不同的 Logs target".to_owned());
                }
                return match seed.phase {
                    SeedPhase::Pending | SeedPhase::Complete => Ok(true),
                    SeedPhase::Failed => Err(seed
                        .error
                        .clone()
                        .unwrap_or_else(|| "右栏日志验收种子写入失败".to_owned())),
                };
            }
            if seed.phase == SeedPhase::Pending {
                return Err("另一个右栏日志验收种子仍在写入".to_owned());
            }
        }
        if self.loading || self.clear_target_in_flight.is_some() {
            return Err("命令历史仍有操作进行中，不能开始验收种子写入".to_owned());
        }

        let prefix = format!("rr-ui-{run_id}-");
        let requests = acceptance_seed_requests(&prefix, &target, binding)?;
        let generation = self.acceptance.generation.wrapping_add(1).max(1);
        self.acceptance.generation = generation;
        let cancellation = CancellationToken::new();
        self.acceptance.seed = Some(SeedRun {
            generation,
            run_id: run_id.to_owned(),
            prefix: prefix.clone(),
            binding: binding.clone(),
            target: target.clone(),
            expected_count: requests.len(),
            recorded_count: 0,
            phase: SeedPhase::Pending,
            error: None,
            cancellation: Some(cancellation.clone()),
        });

        self.cancel_read();
        self.generation = self.generation.wrapping_add(1).max(1);
        self.read_key = None;
        self.loaded_read_key = None;
        self.entries.clear();
        self.query_value = prefix.clone();
        self.query
            .update(cx, |input, cx| input.set_value(prefix, window, cx));
        self.source = None;
        self.page = 1;
        self.loading = true;
        self.error = None;
        self.status = None;
        cx.notify();

        let commands = self.commands.clone();
        let executor = self.executor.clone();
        let expected_count = requests.len();
        let binding = binding.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(
                    cancellation.clone(),
                    move |operation_cancellation| async move {
                        let mut recorded_count = 0;
                        for request in requests {
                            if operation_cancellation.is_cancelled() {
                                return Err(AppError::cancelled("右栏日志验收种子写入已取消"));
                            }
                            let result = commands.record_history(request).await?;
                            if !result.recorded {
                                return Err(AppError::invalid("右栏日志验收种子记录未写入"));
                            }
                            recorded_count += 1;
                        }
                        Ok(recorded_count)
                    },
                )
                .await;

            let _ = this.update(cx, |view, cx| {
                view.finish_acceptance_seed(
                    generation,
                    &target,
                    &binding,
                    expected_count,
                    result,
                    cx,
                );
            });
        })
        .detach();
        Ok(true)
    }

    /// 分别报告种子写入和查询完成，空结果仍可与未完成区分，并保留部分写入错误。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_seed_ready(
        &self,
        run_id: &str,
        binding: &UiSessionBinding,
    ) -> Result<bool, String> {
        let seed = self.acceptance_seed(run_id, binding)?;
        match seed.phase {
            SeedPhase::Pending => Ok(false),
            SeedPhase::Failed => Err(seed
                .error
                .clone()
                .unwrap_or_else(|| "右栏日志验收种子写入失败".to_owned())),
            SeedPhase::Complete => {
                if seed.recorded_count != seed.expected_count {
                    return Err("日志验收种子回执数量与预期不一致".to_owned());
                }
                if let Some(error) = self.error.as_ref() {
                    return Err(format!("日志历史查询失败：{error}"));
                }
                Ok(self.right_rail_acceptance_history_ready(binding))
            }
        }
    }

    /// 只返回本次 run 前缀下的记录，避免验收代码读取无关命令历史。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snapshot(
        &self,
        run_id: &str,
        binding: &UiSessionBinding,
    ) -> Result<RightRailAcceptanceLogsSnapshot, String> {
        let seed = self.acceptance_seed(run_id, binding)?;
        if seed.phase != SeedPhase::Complete {
            return Err("右栏日志验收种子尚未完成".to_owned());
        }
        if !self.right_rail_acceptance_history_ready(binding) {
            return Err("日志历史尚未完成当前查询".to_owned());
        }
        if self
            .entries
            .iter()
            .any(|entry| !entry.command.starts_with(&seed.prefix))
        {
            return Err("日志结果包含本次验收前缀之外的历史记录".to_owned());
        }

        let (page, start, end) = page_window(self.page, self.entries.len());
        let entries = self.entries[start..end]
            .iter()
            .map(|entry| RightRailAcceptanceLogsEntry {
                id: entry.id.clone(),
                command: entry.command.clone(),
                source: entry.source,
            })
            .collect();
        Ok(RightRailAcceptanceLogsSnapshot {
            run_id: seed.run_id.clone(),
            prefix: seed.prefix.clone(),
            query: self.query_value.clone(),
            source: self.source,
            page,
            page_count: self.entries.len().div_ceil(PAGE_SIZE).max(1),
            total_entries: self.entries.len(),
            seeded_entries: seed.recorded_count,
            entries,
        })
    }

    /// 通过保留的 Input 状态和生产历史重载路径修改搜索条件。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_set_query(
        &mut self,
        run_id: &str,
        binding: &UiSessionBinding,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let prefix = self.acceptance_mutation_prefix(run_id, binding)?;
        if !query.starts_with(&prefix) {
            return Err("验收搜索必须保留本次唯一历史前缀".to_owned());
        }
        self.ensure_acceptance_idle()?;
        if self.query_value == query {
            return Ok(());
        }
        self.query_value = query.clone();
        self.query
            .update(cx, |input, cx| input.set_value(query, window, cx));
        self.page = 1;
        self.reload_history(cx);
        cx.notify();
        Ok(())
    }

    /// 通过 Logs 的常规分页复位和可取消重载路径修改来源筛选。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_set_source(
        &mut self,
        run_id: &str,
        binding: &UiSessionBinding,
        source: Option<CommandHistorySource>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.acceptance_mutation_prefix(run_id, binding)?;
        self.ensure_acceptance_idle()?;
        self.set_source(source, cx);
        Ok(())
    }

    /// 先验证目标页存在，再通过生产分页边界逻辑切换页面。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_set_page(
        &mut self,
        run_id: &str,
        binding: &UiSessionBinding,
        page: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.acceptance_mutation_prefix(run_id, binding)?;
        self.ensure_acceptance_idle()?;
        let page_count = self.entries.len().div_ceil(PAGE_SIZE).max(1);
        if !(1..=page_count).contains(&page) {
            return Err("右栏日志验收请求的页码超出当前结果范围".to_owned());
        }
        self.set_page(page, cx);
        Ok(())
    }

    /// 只通过生产删除路径删除当前可见且属于本次 run 前缀的记录。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_delete_entry(
        &mut self,
        run_id: &str,
        binding: &UiSessionBinding,
        entry_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let prefix = self.acceptance_mutation_prefix(run_id, binding)?;
        self.ensure_acceptance_idle()?;
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == entry_id && entry.command.starts_with(&prefix))
            .ok_or_else(|| "验收只能删除当前前缀下可见的日志记录".to_owned())?;
        let entry_id = entry.id.clone();
        self.delete_entry(&entry_id, cx);
        Ok(())
    }

    /// 仅在唯一 run 前缀是唯一筛选条件时调用生产 pane 清空动作。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_clear_history(
        &mut self,
        run_id: &str,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let prefix = self.acceptance_mutation_prefix(run_id, binding)?;
        self.ensure_acceptance_idle()?;
        if self.query_value != prefix || self.source.is_some() {
            return Err("清空验收历史前必须恢复唯一前缀搜索并移除来源筛选".to_owned());
        }
        self.clear_history(cx);
        Ok(())
    }

    /// 只接受当前 generation、pane 和活动状态的异步回执，避免旧写入覆盖新目标。
    ///
    /// @author kongweiguang
    fn finish_acceptance_seed(
        &mut self,
        generation: u64,
        target: &LogsTarget,
        binding: &UiSessionBinding,
        expected_count: usize,
        result: kerminal_application::AppResult<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(seed) = self.acceptance.seed.as_mut() else {
            return;
        };
        if seed.generation != generation || seed.phase != SeedPhase::Pending {
            return;
        }
        seed.cancellation = None;
        if self.target.as_ref() != Some(target)
            || target.binding() != binding
            || seed.binding != *binding
            || !self.active
        {
            seed.phase = SeedPhase::Failed;
            seed.error = Some("验收种子写入期间 Logs 目标或活动 pane 已变化".to_owned());
            return;
        }

        match result {
            Ok(recorded_count) if recorded_count == expected_count => {
                seed.recorded_count = recorded_count;
                seed.phase = SeedPhase::Complete;
                self.loading = false;
                self.error = None;
                self.reload_history(cx);
            }
            Ok(recorded_count) => {
                let error = format!("验收种子仅写入 {recorded_count}/{expected_count} 条历史记录");
                seed.recorded_count = recorded_count;
                seed.phase = SeedPhase::Failed;
                seed.error = Some(error.clone());
                self.loading = false;
                self.error = Some(error);
            }
            Err(error) => {
                let error = error.public_message().to_owned();
                seed.phase = SeedPhase::Failed;
                seed.error = Some(error.clone());
                self.loading = false;
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    /// 只允许写入当前活动且 scope 与可见 SSH pane 一致的 Logs target。
    ///
    /// @author kongweiguang
    fn acceptance_target(&self, binding: &UiSessionBinding) -> Result<&LogsTarget, String> {
        if !self.active {
            return Err("Logs 工具当前未激活".to_owned());
        }
        let target = self
            .target
            .as_ref()
            .ok_or_else(|| "Logs 当前没有绑定 UI pane".to_owned())?;
        if target.binding() != binding {
            return Err("Logs binding 与 fixture pane 不一致".to_owned());
        }
        if target.target != CommandHistoryTarget::Ssh
            || target.scope.target != Some(CommandHistoryTarget::Ssh)
            || target.scope.remote_host_id.is_none()
        {
            return Err("日志验收只允许写入绑定到可信 SSH host 的 UI pane".to_owned());
        }
        let Some(pane_id) = target.scope.pane_id.as_ref() else {
            return Err("Logs pane scope 不完整".to_owned());
        };
        if !matches!(
            &binding.owner,
            SessionOwner::Ui { pane_id: owner_pane, .. } if owner_pane == pane_id
        ) {
            return Err("Logs pane scope 与 fixture binding owner 不一致".to_owned());
        }
        Ok(target)
    }

    /// 将后续读写绑定到同一 run、binding 和 target，拒绝过期或串 pane 的验收请求。
    ///
    /// @author kongweiguang
    fn acceptance_seed(
        &self,
        run_id: &str,
        binding: &UiSessionBinding,
    ) -> Result<&SeedRun, String> {
        let seed = self
            .acceptance
            .seed
            .as_ref()
            .ok_or_else(|| "右栏日志验收种子尚未启动".to_owned())?;
        if seed.run_id != run_id {
            return Err("右栏日志验收 runId 与当前种子不一致".to_owned());
        }
        if seed.binding != *binding || seed.target.binding() != binding {
            return Err("右栏日志验收种子属于不同 fixture binding".to_owned());
        }
        if self.target.as_ref() != Some(&seed.target) {
            return Err("右栏日志验收种子的 pane target 已发生变化".to_owned());
        }
        if !self.active {
            return Err("Logs 工具在验收过程中已停用".to_owned());
        }
        Ok(seed)
    }

    /// 返回本次唯一命令前缀，确保所有验收筛选和写操作留在隔离记录范围内。
    ///
    /// @author kongweiguang
    fn acceptance_mutation_prefix(
        &self,
        run_id: &str,
        binding: &UiSessionBinding,
    ) -> Result<String, String> {
        let seed = self.acceptance_seed(run_id, binding)?;
        if seed.phase != SeedPhase::Complete {
            return Err(seed
                .error
                .clone()
                .unwrap_or_else(|| "右栏日志验收种子尚未完成".to_owned()));
        }
        if !self.query_value.starts_with(&seed.prefix) {
            return Err("当前搜索已离开本次验收历史范围".to_owned());
        }
        Ok(seed.prefix.clone())
    }

    /// 串行化验收动作，防止未完成查询、删除或清理与下一步状态检查交错。
    ///
    /// @author kongweiguang
    fn ensure_acceptance_idle(&self) -> Result<(), String> {
        if self.loading || self.clear_target_in_flight.is_some() {
            return Err("日志历史仍有操作进行中".to_owned());
        }
        if let Some(error) = self.error.as_ref() {
            return Err(format!("日志历史当前不可操作：{error}"));
        }
        Ok(())
    }
}

/// 使用生产 Logs 查询和清理采用的同一 pane scope 构造验收历史记录。
///
/// @author kongweiguang
fn acceptance_seed_requests(
    prefix: &str,
    target: &LogsTarget,
    binding: &UiSessionBinding,
) -> Result<Vec<CommandHistoryRecordRequest>, String> {
    let (tab_id, pane_id) = match &binding.owner {
        SessionOwner::Ui { tab_id, pane_id } => (tab_id.clone(), pane_id.clone()),
        SessionOwner::Agent { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => {
            return Err("日志验收只允许来自 UI pane 的 binding".to_owned());
        }
    };
    let history_target = target
        .scope
        .target
        .ok_or_else(|| "Logs 历史 target scope 缺失".to_owned())?;
    let scope_pane_id = target
        .scope
        .pane_id
        .as_ref()
        .ok_or_else(|| "Logs 历史 pane scope 缺失".to_owned())?;
    if scope_pane_id != &pane_id || target.target != history_target {
        return Err("Logs target 与验收 pane scope 不一致".to_owned());
    }
    let remote_host_id: Option<HostId> = target.scope.remote_host_id.clone();
    let session_id = binding.session_id.clone();
    let pane_id = Some(pane_id);
    let tab_id = Some(tab_id);
    let mut requests = Vec::with_capacity(USER_SEED_COUNT + TOOL_SEED_COUNT);
    for index in 1..=USER_SEED_COUNT {
        requests.push(seed_request(
            format!("{prefix}user-{index:02}"),
            CommandHistorySource::User,
            history_target,
            session_id.clone(),
            pane_id.clone(),
            tab_id.clone(),
            remote_host_id.clone(),
        ));
    }
    for index in 1..=TOOL_SEED_COUNT {
        requests.push(seed_request(
            format!("{prefix}tool-{index:02}"),
            CommandHistorySource::Tool,
            history_target,
            session_id.clone(),
            pane_id.clone(),
            tab_id.clone(),
            remote_host_id.clone(),
        ));
    }
    if requests
        .iter()
        .any(|request| !request.command.starts_with(prefix))
    {
        return Err("右栏日志验收种子前缀构造不一致".to_owned());
    }
    Ok(requests)
}

/// 构造一条非敏感命令历史记录，不在远端会话中执行命令。
///
/// @author kongweiguang
fn seed_request(
    command: String,
    source: CommandHistorySource,
    target: CommandHistoryTarget,
    session_id: kerminal_domain::identity::SessionId,
    pane_id: Option<kerminal_domain::identity::PaneId>,
    tab_id: Option<kerminal_domain::identity::TabId>,
    remote_host_id: Option<HostId>,
) -> CommandHistoryRecordRequest {
    CommandHistoryRecordRequest {
        command,
        source,
        target,
        record: Some(true),
        session_id: Some(session_id),
        pane_id,
        tab_id,
        profile_id: None,
        remote_host_id,
        cwd: None,
        shell: None,
    }
}

/// 限制 run ID 字符集，使前缀稳定且可用于历史查询。
///
/// @author kongweiguang
fn validate_run_id(run_id: &str) -> Result<(), String> {
    if run_id.is_empty()
        || run_id.len() > 64
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("右栏日志验收 runId 只能包含 1 至 64 个 ASCII 字母、数字或连字符".to_owned());
    }
    Ok(())
}
