//! 宿主恢复与目录实体装配；窗口只负责视图，I/O 仍走共享执行器。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use super::*;
use kerminal_application::{connections::management::ManagedHost, workspace::WorkspaceTargetRef};
use kerminal_domain::connections::RemoteProtocol;

/// 未确认协议的界面文案。
///
/// `AppError` 的公开上下文只接受静态字符串，因此错误对象保留固定中文作为日志与兜底；
/// 真正展示给用户的失败/状态文案在这里按当前语言选择中英文。
fn unconfirmed_protocol_message() -> SharedString {
    crate::locale::text(
        "主机目录尚未同步，无法确认协议；已保留材料，请刷新后重试",
        "The host directory has not synced yet, so the protocol could not be confirmed. The data was kept; refresh and try again.",
    )
}

/// 容器能力被禁用时的双语展示文案。
///
/// 门禁拒绝复用既有 `ContainersDisabled` 到 `AppError` 的映射，而 `AppError` 的公开
/// 上下文只接受静态字符串，因此错误对象仍保留固定中文服务日志与兜底；展示层必须在
/// 渲染时按当前语言重新选择，否则英文界面会出现中文状态。这里只覆盖容器能力一项，
/// 协议禁用等既有错误文案保持不变。
///
/// @author kongweiguang
pub(super) fn container_capability_disabled_message(
    language: crate::locale::UiLanguage,
) -> &'static str {
    match language {
        crate::locale::UiLanguage::ZhCn => "工作区容器能力已禁用，原标签和状态仍保留",
        crate::locale::UiLanguage::EnUs => {
            "Container capability for the workspace is disabled; the original tab and state were kept."
        }
    }
}

/// 把门禁拒绝转换成当前语言的展示文案。
///
/// 容器能力禁用和协议未确认都需要替换静态中文：`AppError` 只接受静态字符串，若展示
/// 层直接复用会在英文界面出现中文状态。这两项各自复用本文件已有的双语文案函数，
/// 与恢复路径的提示保持一致；协议禁用等已稳定提示仍沿用 `AppError` 的公开上下文。
///
/// @author kongweiguang
pub(super) fn file_workspace_gate_display_message(
    gate: FileWorkspaceTargetGate,
    error: &AppError,
) -> SharedString {
    match gate {
        FileWorkspaceTargetGate::ContainersDisabled => {
            container_capability_disabled_message(crate::locale::current_language()).into()
        }
        // 与 `finish_unconfirmed_host_workspace` 共用同一文案，避免同一失败原因在
        // 不同收尾路径上出现中英文漂移。
        FileWorkspaceTargetGate::ProtocolUnconfirmed => unconfirmed_protocol_message(),
        _ => error.public_message().into(),
    }
}

impl RootView {
    /// 内部缓冲先登记真实身份，后台恢复不能把其活动文档投影成新的顶层导航。
    ///
    /// 文件工作区经 SFTP 读取远程目录；SSH 端点还复用 SSH 连接单元。导航快照能确认
    /// 协议时同步完成冻结判定；快照缺失且 SSH 被禁用时，不能把未知协议直接当成失败，
    /// 也不能当成纯 SFTP 放行，而是转交主机配置读取做权威确认。确认期间保留 Pending
    /// 与原材料，不创建任何远端实体。
    ///
    /// @author kongweiguang
    pub(in crate::root) fn ensure_host_file_workspace(
        &mut self,
        id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace_files.tabs.get(id) else {
            return;
        };
        let WorkspaceSessionTab::FileWorkspace { target, .. } = runtime.tab.clone() else {
            return;
        };
        match self.file_workspace_gate(&target, cx) {
            FileWorkspaceTargetGate::Allowed => {}
            FileWorkspaceTargetGate::ProtocolUnconfirmed => {
                self.start_host_protocol_confirmation(
                    id,
                    target,
                    ProtocolConfirmationPurpose::Restore,
                    window,
                    cx,
                );
                return;
            }
            gate => {
                if let Some((failure, error)) = gate.rejection() {
                    self.finish_workspace_file_restore(
                        id,
                        Err(failure),
                        file_workspace_gate_display_message(gate, &error),
                        cx,
                    );
                }
                return;
            }
        }
        self.restore_host_file_workspace(id, None, window, cx);
    }

    /// 恢复判定通过后装配文档并收尾；权威确认回执也续接到同一入口。
    ///
    /// `confirmed` 只代表本次流程刚取得的权威协议结论，不写入任何长期状态；它随
    /// 激活与目录装配传递，避免同一流程在导航快照仍缺失时立即重复读取主机配置。
    /// 任何失败仍保留原 descriptor，避免把未完成的远端读取伪装成成功。
    ///
    /// @author kongweiguang
    fn restore_host_file_workspace(
        &mut self,
        id: &TabId,
        confirmed: Option<HostProtocolLookup>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace_files.tabs.get(id) else {
            return;
        };
        let WorkspaceSessionTab::FileWorkspace {
            target,
            state,
            machine_id,
            ..
        } = runtime.tab.clone()
        else {
            return;
        };
        let Some(anchor) = self.workspace_files.host_workspace_anchor(id) else {
            self.finish_workspace_file_restore(
                id,
                Err(WorkspaceRestoreFailure::ResourceFailure),
                "文件工作区目标无法恢复；原数据已保留。".into(),
                cx,
            );
            return;
        };
        let Some(editor) = self.file_editor_workspace.clone() else {
            self.finish_workspace_file_restore(
                id,
                Err(WorkspaceRestoreFailure::ResourceFailure),
                "文件编辑服务尚未配置。".into(),
                cx,
            );
            return;
        };
        let lease = runtime.external_target_lease.clone();
        let transient = runtime.transient;
        let active = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| controller.snapshot().active_tab_id.as_ref() == Some(id));
        for item in &state.items {
            let FileWorkspaceItem::Document {
                id: item_id,
                title,
                path,
                root_path,
                access,
                source,
            } = item
            else {
                continue;
            };
            let endpoint = match self
                .workspace_files
                .host_document_endpoint(id, item_id)
                .ok_or_else(|| AppError::invalid("文档来源或路径与文件工作区目标不匹配"))
            {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    self.finish_workspace_file_restore(
                        id,
                        Err(WorkspaceRestoreFailure::ResourceFailure),
                        error.public_message().into(),
                        cx,
                    );
                    return;
                }
            };
            let readonly = self
                .workspace_files
                .workspace_file_requires_readonly(&endpoint)
                || *access == WorkspaceFileAccess::Readonly;
            let result = editor.update(cx, |workspace, cx| {
                workspace.open_workspace_file(
                    endpoint.clone(),
                    title.clone(),
                    readonly,
                    active && state.active_item_id.as_ref() == Some(item_id),
                    cx,
                )
            });
            if let Err(error) = result {
                self.finish_workspace_file_restore(
                    id,
                    Err(WorkspaceRestoreFailure::ResourceFailure),
                    error.public_message().into(),
                    cx,
                );
                return;
            }
            self.workspace_files.documents.insert(
                (id.clone(), item_id.clone()),
                WorkspaceFileTabRuntime {
                    tab: WorkspaceSessionTab::WorkspaceFile {
                        id: item_id.clone(),
                        tab_group_id: None,
                        title: title.clone(),
                        machine_id: machine_id.clone(),
                        target: target.clone(),
                        path: path.clone(),
                        root_path: root_path.clone(),
                        access: *access,
                        source: *source,
                    },
                    status: WorkspaceFileRestoreState::Ready,
                    endpoint: Some(endpoint),
                    files_pane: None,
                    files_subscriptions: None,
                    external_target_lease: lease.clone(),
                    transient,
                    pending_transfer_endpoints: None,
                },
            );
        }
        self.finish_workspace_file_restore(id, Ok(Some(anchor)), SharedString::default(), cx);
        if active {
            self.activate_host_file_workspace_with_protocol(id, confirmed, window, cx);
        }
    }

    /// 快照无法确认协议时，用主机配置读取做一次权威判定；UI 线程不等待磁盘读取。
    ///
    /// 只做在途去重而不缓存结论：同一 Root/Tab/目标/用途的并发激活复用当前请求，任何
    /// 回执、关闭或目标变化后都移除条目；后续再次需要目录时重新确认，避免把旧协议
    /// 结论带过配置变更。读取失败不能降级成“非持久主机”，否则配置异常会被伪装成
    /// 可放行目标并触发远端 I/O。
    ///
    /// @author kongweiguang
    fn start_host_protocol_confirmation(
        &mut self,
        id: &TabId,
        target: WorkspaceTargetRef,
        purpose: ProtocolConfirmationPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let WorkspaceTargetRef::Ssh { host_id } = &target else {
            // 纯判定只有 SSH 目标会进入未确认分支；其它形状按既有失败语义收尾。
            self.finish_unconfirmed_host_workspace(id, &purpose, cx);
            return;
        };
        let Some(hosts) = self.services.hosts.clone() else {
            // 未装配主机管理时不能伪造协议结论；保留材料并沿用既有未确认文案。
            self.finish_unconfirmed_host_workspace(id, &purpose, cx);
            return;
        };
        // 在途表以 Root 实体 ID 区分窗口；同一持久 TabId 在另一个窗口恢复时不能互相阻塞。
        let owner = cx.entity_id().as_u64();
        let Some(token) = begin_protocol_confirmation(owner, id, &target, &purpose) else {
            return;
        };
        let receipt = ProtocolConfirmationReceipt {
            owner,
            token,
            purpose,
        };
        let host_id = host_id.clone();
        let id = id.clone();
        let executor = self.services.executor.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |_| async move { hosts.load_host(&host_id).await })
                .await;
            let applied = this.update_in(cx, |root, window, cx| {
                root.apply_host_protocol_confirmation(&id, &target, &receipt, result, window, cx);
            });
            if applied.is_err() {
                // 窗口或 Root 实体已释放时 UI 回调不会执行；在途项必须由发起方清除，
                // 否则注册表会留下永远不会被消费的条目。
                take_protocol_confirmation(receipt.owner, &id, receipt.token, &target);
            }
        })
        .detach();
    }

    /// 无法发起权威读取时的收尾：恢复失败保留材料；目录装配只提示并保持 Ready。
    ///
    /// @author kongweiguang
    fn finish_unconfirmed_host_workspace(
        &mut self,
        id: &TabId,
        purpose: &ProtocolConfirmationPurpose,
        cx: &mut Context<Self>,
    ) {
        let Some((failure, _)) = FileWorkspaceTargetGate::ProtocolUnconfirmed.rejection() else {
            return;
        };
        match purpose {
            ProtocolConfirmationPurpose::Restore => {
                self.finish_workspace_file_restore(
                    id,
                    Err(failure),
                    unconfirmed_protocol_message(),
                    cx,
                );
            }
            ProtocolConfirmationPurpose::Directory { .. } => {
                // Ready 是已落地的恢复结果；这里只缺目录实体，不能把它改成失败页。
                self.status = Some(unconfirmed_protocol_message());
                cx.notify();
            }
        }
    }

    /// 回执先在途消费，再复核在途 token、目标与 Tab 状态，最后按用途应用。
    ///
    /// 关闭、替换、目标变化或退出只清理自己的在途条目，不创建 pane；读取失败保留
    /// 可恢复材料并给出失败，显式重试会重新发起确认而不是复用失败结论。目录用途
    /// 的回执只在用户仍停留在发起确认的目录时生效，避免旧回执切走中心页。
    ///
    /// @author kongweiguang
    fn apply_host_protocol_confirmation(
        &mut self,
        id: &TabId,
        target: &WorkspaceTargetRef,
        receipt: &ProtocolConfirmationReceipt,
        result: AppResult<Option<ManagedHost>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 回执必须回到发起确认的同一个 Root；否则不能消费其它窗口的在途项。
        if receipt.owner != cx.entity_id().as_u64() {
            return;
        }
        // 无论结果如何先消费自己的在途项；token/目标不匹配说明已有更新请求，直接丢弃。
        if !take_protocol_confirmation(receipt.owner, id, receipt.token, target) {
            return;
        }
        let lookup = match &result {
            Ok(Some(host)) => HostProtocolLookup::Persisted(host.protocol),
            Ok(None) => HostProtocolLookup::Unpersisted,
            Err(_) => HostProtocolLookup::Failed,
        };
        // 宿主已进入退出流程时不再启动新的恢复动作，避免借用正在释放的实体。
        if self.host_shutdown_requested {
            return;
        }
        let tab_still_current = self.workspace_files.tabs.get(id).is_some_and(|runtime| {
            matches!(
                &runtime.tab,
                WorkspaceSessionTab::FileWorkspace { target: current, .. } if current == target
            )
        });
        if !tab_still_current {
            return;
        }
        match &receipt.purpose {
            ProtocolConfirmationPurpose::Restore => {
                if self.workspace_files.restore_state(id)
                    != Some(&WorkspaceFileRestoreState::Pending)
                {
                    return;
                }
                if lookup == HostProtocolLookup::Failed {
                    self.finish_workspace_file_restore(
                        id,
                        Err(WorkspaceRestoreFailure::ResourceFailure),
                        crate::locale::text(
                            "主机配置读取失败，文件工作区暂未恢复；原标签和材料已保留，可稍后重试。",
                            "Reading the host configuration failed, so the file workspace was not restored. The tab and its data were kept; try again later.",
                        ),
                        cx,
                    );
                    return;
                }
                let Some(gate) = self.file_workspace_confirmed_gate(target, lookup) else {
                    return;
                };
                if let Some((failure, error)) = gate.rejection() {
                    // 协议禁用是当前配置结论；不缓存为可复用放行，重新启用后显式重试
                    // 会重新确认。
                    self.finish_workspace_file_restore(
                        id,
                        Err(failure),
                        file_workspace_gate_display_message(gate, &error),
                        cx,
                    );
                    return;
                }
                self.restore_host_file_workspace(id, Some(lookup), window, cx);
            }
            ProtocolConfirmationPurpose::Directory { item } => {
                // 目录回执只对仍处于前台的文件工作区生效。用户已切到终端或其它标签时，
                // 强行激活会清掉共享编辑器的当前文档、为非活动 Tab 建 pane 并抢占中心页；
                // 这里直接丢弃，用户再次点击目录会重新确认。
                let tab_is_active = self
                    .workspace_controller
                    .as_ref()
                    .is_some_and(|controller| {
                        controller.snapshot().active_tab_id.as_ref() == Some(id)
                    });
                if !tab_is_active {
                    return;
                }
                if self.workspace_files.restore_state(id) != Some(&WorkspaceFileRestoreState::Ready)
                {
                    return;
                }
                let active_item = self.workspace_files.tabs.get(id).and_then(|runtime| {
                    let WorkspaceSessionTab::FileWorkspace { state, .. } = &runtime.tab else {
                        return None;
                    };
                    state.active_item_id.clone()
                });
                if active_item.as_ref() != Some(item) {
                    return;
                }
                if lookup == HostProtocolLookup::Failed {
                    self.status = Some(crate::locale::text(
                        "主机配置读取失败，目录暂未打开；原标签和材料已保留，可稍后重试。",
                        "Reading the host configuration failed, so the directory was not opened. The tab and its data were kept; try again later.",
                    ));
                    cx.notify();
                    return;
                }
                let Some(gate) = self.file_workspace_confirmed_gate(target, lookup) else {
                    return;
                };
                if let Some((_, error)) = gate.rejection() {
                    // Ready 已成立，只是当前配置不允许该目录；保留恢复结果供启用后重试。
                    self.status = Some(file_workspace_gate_display_message(gate, &error));
                    cx.notify();
                    return;
                }
                self.activate_host_file_workspace_with_protocol(id, Some(lookup), window, cx);
            }
        }
    }

    /// 目录实体按内部稳定 ID 缓存，切文件或重复激活不会再次扫描目录。
    ///
    /// `confirmed` 是本次流程刚取得的权威协议结论；同一流程的恢复回执把它带下来，
    /// 使同步建 pane 路径不必在导航快照仍缺失时重新读取配置。
    ///
    /// @author kongweiguang
    fn ensure_host_directory(
        &mut self,
        owner: &TabId,
        item: &TabId,
        confirmed: Option<HostProtocolLookup>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        let key = (owner.clone(), item.clone());
        if self.workspace_files.directories.contains_key(&key) {
            return Ok(());
        }
        // 只在闭包内借用 runtime，协议门禁与 Entity 构造需要按当前目标重新读取 Root 状态。
        let (target, path, lease, origins) = {
            let runtime = self
                .workspace_files
                .tabs
                .get(owner)
                .ok_or_else(|| AppError::not_found("文件工作区已关闭"))?;
            let WorkspaceSessionTab::FileWorkspace { target, state, .. } = &runtime.tab else {
                return Err(AppError::conflict("文件工作区类型已变化"));
            };
            let FileWorkspaceItem::Directory { path, .. } = state
                .items
                .iter()
                .find(|entry| entry.id() == item)
                .ok_or_else(|| AppError::not_found("目录标签已关闭"))?
            else {
                return Err(AppError::invalid("当前标签不是目录"));
            };
            (
                target.clone(),
                path.clone(),
                runtime.external_target_lease.clone(),
                workspace_origins(owner, state),
            )
        };
        // 门禁必须早于 FilesPane 创建：面板一构造就会订阅并读取远端目录，失败后再撤销
        // 也无法收回 I/O。同步路径不等待导航补同步，而是按现有快照、主机配置确认与冻结
        // 启用视图拒绝，保证恢复路径得出的权威结论能贯穿后续目录装配。
        if let Some((_, error)) = self
            .file_workspace_gate_with_protocol(&target, confirmed, cx)
            .rejection()
        {
            return Err(error);
        }
        let endpoint = endpoint_at_path(
            &self
                .workspace_files
                .host_workspace_anchor(owner)
                .ok_or_else(|| AppError::conflict("目录目标已失效"))?,
            &path,
        )?;
        let files = self
            .services
            .files
            .clone()
            .ok_or_else(|| AppError::conflict("文件服务尚未配置"))?;
        let transfers = self
            .services
            .transfers
            .clone()
            .ok_or_else(|| AppError::conflict("传输服务尚未配置"))?;
        let executor = self.services.executor.clone();
        let lookup = crate::features::files::FileTargetNameLookup::new(
            self.services.hosts.clone(),
            self.services.operations.clone(),
        );
        let pane = cx.new(|_| FilesPane::new(files, transfers, executor, lookup, endpoint.clone()));
        let density = super::super::super::preferences::current_interface_density(self);
        pane.update(cx, |pane, cx| {
            pane.set_interface_density(density, cx);
            pane.configure_file_workspace(endpoint, lease, origins, cx)
        })?;
        let subscriptions = Self::subscribe_files_pane_events(&pane, window, cx);
        self.workspace_files
            .directories
            .insert(key, (pane, subscriptions));
        Ok(())
    }

    /// 宿主的目录和文档共用一个中心 surface，但只激活当前内容而不重建另一类状态。
    ///
    /// 外部调用只有导航快照这一条协议来源，因此先不携带权威结论；目录装配若仍遇到
    /// 快照缺失，会在下面重新发起确认，而不是复用上次恢复留下的结论。
    ///
    /// @author kongweiguang
    pub(in crate::root) fn activate_host_file_workspace(
        &mut self,
        id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_host_file_workspace_with_protocol(id, None, window, cx);
    }

    /// 带本次流程权威结论的激活；结论只活到本次目录装配结束，不进入长期状态。
    ///
    /// @author kongweiguang
    fn activate_host_file_workspace_with_protocol(
        &mut self,
        id: &TabId,
        confirmed: Option<HostProtocolLookup>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 导航投影、恢复回执和故障页重试都会在恢复结果之后继续调用激活；失败或待定时
        // 必须在这里闭环，否则会创建目录 pane、发起远端读取并把中心页切成编辑器，
        // 覆盖已经呈现的故障文案。Ready 判定之外不改变任何中心状态。
        if !matches!(
            self.workspace_files.restore_state(id),
            Some(WorkspaceFileRestoreState::Ready)
        ) {
            return;
        }
        let Some(WorkspaceFileTabRuntime {
            tab: WorkspaceSessionTab::FileWorkspace { target, state, .. },
            ..
        }) = self.workspace_files.tabs.get(id)
        else {
            return;
        };
        let target = target.clone();
        let Some(item) = state
            .active_item_id
            .as_ref()
            .and_then(|active| state.items.iter().find(|item| item.id() == active))
            .or_else(|| {
                state
                    .items
                    .iter()
                    .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            })
            .cloned()
        else {
            return;
        };
        if let Some(editor) = self.file_editor_workspace.clone() {
            editor.update(cx, |editor, cx| editor.set_embedded_navigation(true, cx));
        }
        if let FileWorkspaceItem::Directory { id: item_id, .. } = item {
            // 已缓存的目录 pane 不会再发起远端读取，无需为了同一个目标重复确认；只有
            // 首次装配目录且快照缺失时才需要一次权威结论。
            let directory_cached = self
                .workspace_files
                .directories
                .contains_key(&(id.clone(), item_id.clone()));
            if !directory_cached
                && confirmed.is_none()
                && self.file_workspace_gate(&target, cx)
                    == FileWorkspaceTargetGate::ProtocolUnconfirmed
            {
                self.start_host_protocol_confirmation(
                    id,
                    target,
                    ProtocolConfirmationPurpose::Directory { item: item_id },
                    window,
                    cx,
                );
                self.center_panel = CenterPanel::Editor;
                cx.notify();
                return;
            }
            if let Some(editor) = self.file_editor_workspace.clone() {
                editor.update(cx, |editor, cx| editor.deactivate_document(cx));
            }
            // `ensure_host_directory` 内部的门禁保留作防御；这里在装配前用本次流程的
            // 权威结论提前收尾，保证容器能力被禁用时仍先清空共享编辑器当前文档、按
            // 当前语言给出状态，并且不创建任何目录 pane 或发起远端读取。
            if !directory_cached {
                let gate = self.file_workspace_gate_with_protocol(&target, confirmed, cx);
                if let Some((_, error)) = gate.rejection() {
                    self.status = Some(file_workspace_gate_display_message(gate, &error));
                    self.center_panel = CenterPanel::Editor;
                    cx.notify();
                    return;
                }
            }
            if let Err(error) = self.ensure_host_directory(id, &item_id, confirmed, window, cx) {
                self.status = Some(error.public_message().into());
            }
        } else if let Some(endpoint) = self.workspace_files.host_active_document(id)
            && let Some(editor) = self.file_editor_workspace.clone()
        {
            editor.update(cx, |editor, cx| {
                editor.activate_open_file(endpoint, cx);
            });
        }
        self.center_panel = CenterPanel::Editor;
        cx.notify();
    }
}

/// 文件工作区目标在创建目录实体前的冻结判定。
///
/// 目录装配是同步路径，不能等待主机导航补同步；快照缺失时若 SSH 单元也被禁用，
/// 纯判定先返回 `ProtocolUnconfirmed`，由恢复路径用主机配置读取做权威确认，确认
/// 结果再回填给后续目录装配。容器目录经 SSH 执行并归属 containers 单元，两者缺一
/// 都不得创建远端 pane。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileWorkspaceTargetGate {
    Allowed,
    ProtocolDisabled(RemoteProtocol),
    ContainersDisabled,
    ProtocolUnconfirmed,
}

/// 快照缺失时 `load_host` 给出的权威协议结论。
///
/// `Unpersisted` 只表示该身份不是持久主机，涵盖 external/临时 lease 目标；它不能与
/// `Failed` 合并，否则配置读取异常会被伪装成可放行的纯 SFTP 目标并发起远端 I/O。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostProtocolLookup {
    Persisted(RemoteProtocol),
    Unpersisted,
    Failed,
}

/// 文件工作区目标的纯门禁判定；参数只来自导航快照与冻结启用视图，便于离线覆盖。
///
/// 快照明确为 SSH 主机时再要求 SSH 单元；快照给出其它协议（如纯 SFTP 主机）时只按
/// SFTP 判定，避免把允许恢复的主机误判成 SSH 目标。容器目标始终带 host_id，因此
/// 必须同时具备 SSH 能力与 containers 单元。
///
/// @author kongweiguang
pub(super) fn file_workspace_target_gate(
    target: &WorkspaceTargetRef,
    host_protocol: Option<RemoteProtocol>,
    protocol_enabled: impl Fn(RemoteProtocol) -> bool,
    containers_enabled: bool,
) -> FileWorkspaceTargetGate {
    match target {
        WorkspaceTargetRef::Local { .. } => FileWorkspaceTargetGate::Allowed,
        WorkspaceTargetRef::Ssh { .. } => {
            if !protocol_enabled(RemoteProtocol::Sftp) {
                return FileWorkspaceTargetGate::ProtocolDisabled(RemoteProtocol::Sftp);
            }
            match host_protocol {
                Some(RemoteProtocol::Ssh) if !protocol_enabled(RemoteProtocol::Ssh) => {
                    FileWorkspaceTargetGate::ProtocolDisabled(RemoteProtocol::Ssh)
                }
                None if !protocol_enabled(RemoteProtocol::Ssh) => {
                    FileWorkspaceTargetGate::ProtocolUnconfirmed
                }
                _ => FileWorkspaceTargetGate::Allowed,
            }
        }
        WorkspaceTargetRef::DockerContainer { .. } => {
            if !protocol_enabled(RemoteProtocol::Ssh) {
                FileWorkspaceTargetGate::ProtocolDisabled(RemoteProtocol::Ssh)
            } else if !containers_enabled {
                FileWorkspaceTargetGate::ContainersDisabled
            } else {
                FileWorkspaceTargetGate::Allowed
            }
        }
        // Telnet/Serial/RDP/Unavailable 不是文件工作区目录目标；沿用原有放行，
        // 不在这条同步路径推断额外的目标类别。
        _ => FileWorkspaceTargetGate::Allowed,
    }
}

/// 用权威主机读取结论重判 SSH 文件工作区目标；读取失败返回 `None`，永不放行。
///
/// 持久 SSH 主机仍要求 SSH 单元，禁用时沿用既有 `ProtocolDisabled(Ssh)`；纯 SFTP
/// 主机和不在配置中的 external/临时目标只要求 SFTP，不伪造 SSH 依赖。容器与其它
/// 目标不由这条确认路径改变语义，仍委托原纯判定处理。
///
/// @author kongweiguang
pub(super) fn file_workspace_confirmed_target_gate(
    target: &WorkspaceTargetRef,
    lookup: HostProtocolLookup,
    protocol_enabled: impl Fn(RemoteProtocol) -> bool,
    containers_enabled: bool,
) -> Option<FileWorkspaceTargetGate> {
    let protocol = match lookup {
        HostProtocolLookup::Failed => return None,
        HostProtocolLookup::Unpersisted => None,
        HostProtocolLookup::Persisted(protocol) => Some(protocol),
    };
    match target {
        WorkspaceTargetRef::Ssh { .. } => {
            if !protocol_enabled(RemoteProtocol::Sftp) {
                return Some(FileWorkspaceTargetGate::ProtocolDisabled(
                    RemoteProtocol::Sftp,
                ));
            }
            if protocol == Some(RemoteProtocol::Ssh) && !protocol_enabled(RemoteProtocol::Ssh) {
                return Some(FileWorkspaceTargetGate::ProtocolDisabled(
                    RemoteProtocol::Ssh,
                ));
            }
            Some(FileWorkspaceTargetGate::Allowed)
        }
        _ => Some(file_workspace_target_gate(
            target,
            protocol,
            protocol_enabled,
            containers_enabled,
        )),
    }
}

/// 导航快照缺失时的权威确认只在途去重，不保留长期协议结论。
///
/// 恢复、目录装配与后续激活每次需要目录时都可能重新确认；结论只随当前回执使用，
/// 避免 Tab 关闭后永久占用进程级状态。Root 字段与 workspace_files 不在本次写入
/// 范围，因此放在模块级注册表；键带上 Root 的实体 ID，避免多窗口恢复同一持久
/// TabId 时互相阻塞或误消费，所有结果路径都会移除自己的条目。
type RootProtocolConfirmationKey = (u64, TabId);
static PROTOCOL_CONFIRMATIONS: OnceLock<
    Mutex<BTreeMap<RootProtocolConfirmationKey, InFlightProtocolConfirmation>>,
> = OnceLock::new();
static NEXT_PROTOCOL_CONFIRMATION: AtomicU64 = AtomicU64::new(1);

/// 一次在途确认的用途；失败时恢复与目录装配的收尾语义不同。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProtocolConfirmationPurpose {
    /// 首次恢复：成功后写入 Ready 并继续文档装配。
    Restore,
    /// Ready 后的目录装配：成功后只创建目录实体，不改变恢复状态。
    Directory { item: TabId },
}

/// 在途确认只保留 token、目标与用途；token 防止旧回执覆盖新请求。
struct InFlightProtocolConfirmation {
    token: u64,
    target: WorkspaceTargetRef,
    purpose: ProtocolConfirmationPurpose,
}

/// 回执应用所需的在途标识。
///
/// 把 Root 实体 ID、token 与用途合并成一个请求回执，避免应用入口参数过多挤在一起
/// 影响可读性；它只随一次异步任务流转，不进入任何长期状态。owner 用于校验回执仍
/// 回到发起确认的同一个 Root，而不是共享同一持久 TabId 的其它窗口。
struct ProtocolConfirmationReceipt {
    owner: u64,
    token: u64,
    purpose: ProtocolConfirmationPurpose,
}

/// 注册表只在 UI 线程的短锁区间读写；锁中毒时恢复内部状态而不是让恢复流程 panic。
fn protocol_confirmations() -> std::sync::MutexGuard<
    'static,
    BTreeMap<RootProtocolConfirmationKey, InFlightProtocolConfirmation>,
> {
    PROTOCOL_CONFIRMATIONS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 登记在途确认；同一 Root、Tab、目标与用途的重复激活复用现有请求而不重复读取。
fn begin_protocol_confirmation(
    owner: u64,
    id: &TabId,
    target: &WorkspaceTargetRef,
    purpose: &ProtocolConfirmationPurpose,
) -> Option<u64> {
    let mut entries = protocol_confirmations();
    let key = (owner, id.clone());
    if matches!(
        entries.get(&key),
        Some(InFlightProtocolConfirmation {
            target: pending,
            purpose: pending_purpose,
            ..
        }) if pending == target && pending_purpose == purpose
    ) {
        return None;
    }
    let token = NEXT_PROTOCOL_CONFIRMATION.fetch_add(1, Ordering::Relaxed);
    entries.insert(
        key,
        InFlightProtocolConfirmation {
            token,
            target: target.clone(),
            purpose: purpose.clone(),
        },
    );
    Some(token)
}

/// 消费自己的在途条目；token/目标不匹配说明已有更新请求，不能移除他人的条目。
fn take_protocol_confirmation(
    owner: u64,
    id: &TabId,
    token: u64,
    target: &WorkspaceTargetRef,
) -> bool {
    let mut entries = protocol_confirmations();
    let key = (owner, id.clone());
    let accepts = matches!(
        entries.get(&key),
        Some(InFlightProtocolConfirmation {
            token: current,
            target: pending,
            ..
        }) if *current == token && pending == target
    );
    if accepts {
        entries.remove(&key);
    }
    accepts
}

impl FileWorkspaceTargetGate {
    /// 把纯判定映射到既有恢复失败类别；`None` 表示放行。
    ///
    /// 协议禁用复用统一文案；容器与快照缺失使用静态说明，避免把启停或导航未同步
    /// 误报成配置删除，并明确保留原标签和材料供刷新后重试。
    ///
    /// @author kongweiguang
    pub(super) fn rejection(self) -> Option<(WorkspaceRestoreFailure, AppError)> {
        match self {
            Self::Allowed => None,
            Self::ProtocolDisabled(protocol) => {
                Some(crate::root::workspace_sessions::workspace_protocol_disabled(protocol))
            }
            Self::ContainersDisabled => {
                // 直接复用展示层文案，避免中文字面量出现两份后与英文分支漂移；
                // `AppError` 的兜底文案仍固定为中文。
                Some(crate::root::workspace_sessions::workspace_unit_disabled(
                    container_capability_disabled_message(crate::locale::UiLanguage::ZhCn),
                ))
            }
            Self::ProtocolUnconfirmed => Some((
                WorkspaceRestoreFailure::UnavailableTarget(
                    WorkspaceTargetUnavailableReason::UnknownKind,
                ),
                AppError::conflict("主机目录尚未同步，无法确认协议；已保留材料，请刷新后重试"),
            )),
        }
    }
}

impl RootView {
    /// 读取当前导航快照后按文件工作区目标做冻结门禁；目录创建与恢复共用同一判定。
    ///
    /// 这条纯快照路径不查询在途或历史确认结论；快照缺失时仍以 `ProtocolUnconfirmed`
    /// 表达“尚未确认”，由调用方按用途发起一次权威读取，避免把旧结论当成当前配置。
    ///
    /// @author kongweiguang
    fn file_workspace_gate(
        &self,
        target: &WorkspaceTargetRef,
        cx: &Context<Self>,
    ) -> FileWorkspaceTargetGate {
        let host_protocol = match target {
            WorkspaceTargetRef::Ssh { host_id } => self
                .host_navigation
                .as_ref()
                .and_then(|navigation| navigation.read(cx).host_snapshot(host_id))
                .map(|host| host.protocol),
            _ => None,
        };
        let protocol_enabled = |protocol| {
            self.services
                .hosts
                .as_ref()
                .is_none_or(|hosts| hosts.protocol_enabled(protocol))
        };
        let containers_enabled = self.services.plugins.providers().is_empty()
            || self.services.plugins.is_enabled("kerminal.containers");
        file_workspace_target_gate(target, host_protocol, protocol_enabled, containers_enabled)
    }

    /// 把本次流程刚取得的权威结论用于同一次激活/目录装配；没有结论时退回快照判定。
    ///
    /// 读取失败没有权威结论，必须保持“未确认”，不能回退到导航快照而放行远端 I/O。
    ///
    /// @author kongweiguang
    pub(super) fn file_workspace_gate_with_protocol(
        &self,
        target: &WorkspaceTargetRef,
        confirmed: Option<HostProtocolLookup>,
        cx: &Context<Self>,
    ) -> FileWorkspaceTargetGate {
        match confirmed {
            Some(HostProtocolLookup::Failed) => FileWorkspaceTargetGate::ProtocolUnconfirmed,
            Some(lookup) => self
                .file_workspace_confirmed_gate(target, lookup)
                .unwrap_or(FileWorkspaceTargetGate::ProtocolUnconfirmed),
            None => self.file_workspace_gate(target, cx),
        }
    }

    /// 用注入的冻结启用视图重判权威协议结论；`None` 表示读取失败，绝不能被当成放行。
    ///
    /// @author kongweiguang
    fn file_workspace_confirmed_gate(
        &self,
        target: &WorkspaceTargetRef,
        lookup: HostProtocolLookup,
    ) -> Option<FileWorkspaceTargetGate> {
        let protocol_enabled = |protocol| {
            self.services
                .hosts
                .as_ref()
                .is_none_or(|hosts| hosts.protocol_enabled(protocol))
        };
        let containers_enabled = self.services.plugins.providers().is_empty()
            || self.services.plugins.is_enabled("kerminal.containers");
        file_workspace_confirmed_target_gate(target, lookup, protocol_enabled, containers_enabled)
    }
}
