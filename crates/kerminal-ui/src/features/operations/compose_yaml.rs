//! Compose YAML 页内只读预览的请求状态与回执门禁。
//!
//! @author kongweiguang

use gpui_kit::{AppContext as _, Context, Entity, Window, component::input::EditorState};
use kerminal_domain::{
    files::{FileEndpoint, FilePreview},
    operations::ComposeProjectTarget,
};

use super::{
    ContainerPanelContext, OperationsEvent, OperationsView,
    container_panel::ContainerPanelActionTicket, render::container_panel::ContainerHostPage,
};

pub(crate) const COMPOSE_YAML_PREVIEW_MAX_BYTES: usize = 256 * 1024;

/// 只读代码预览将二进制和非法 UTF-8 分开处理，避免把非文本内容交给编辑器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComposeYamlContentIssue {
    InvalidUtf8,
}

/// 表示提交给本机或远端文件服务的完整 Compose 预览身份。
#[derive(Clone, Debug)]
struct PendingComposeYamlPreview {
    endpoint: FileEndpoint,
    project: ComposeProjectTarget,
    request_id: u64,
    ticket: ContainerPanelActionTicket,
}

/// Compose YAML 页保留路径选择、当前回执和只读编辑器；服务与权限仍由 Root 管理。
pub(super) struct ComposeYamlPreviewState {
    next_request_id: u64,
    scope: Option<ComposeProjectTarget>,
    selected_path: Option<String>,
    pending: Option<PendingComposeYamlPreview>,
    preview: Option<FilePreview>,
    error: Option<String>,
    editor: Option<Entity<EditorState>>,
}

impl Default for ComposeYamlPreviewState {
    /// 空状态不依赖窗口或文件服务，编辑器仅在首个有效文本回执到达后创建。
    fn default() -> Self {
        Self {
            next_request_id: 0,
            scope: None,
            selected_path: None,
            pending: None,
            preview: None,
            error: None,
            editor: None,
        }
    }
}

impl ComposeYamlPreviewState {
    /// 宿主或 runtime 重置时同时清除待处理回执和上个目标的 YAML 内容。
    ///
    /// request id 保持单调递增，防止视图再次绑定后误接受旧回执。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate(&mut self) {
        self.scope = None;
        self.selected_path = None;
        self.pending = None;
        self.preview = None;
        self.error = None;
    }

    /// 项目身份或配置路径变化时回到主线相同的首个 YAML 路径并丢弃旧结果。
    ///
    /// @author kongweiguang
    pub(super) fn select_project(&mut self, project: &ComposeProjectTarget) -> bool {
        if self.scope.as_ref() == Some(project) {
            return false;
        }
        self.invalidate();
        self.scope = Some(project.clone());
        self.selected_path = project.config_files.first().cloned();
        true
    }

    /// 只接受当前 Compose 项目的配置路径，切换后清除旧目标正文但保留同一编辑器实体。
    ///
    /// @author kongweiguang
    fn select_path(&mut self, path: &str) -> bool {
        let Some(project) = self.scope.as_ref() else {
            return false;
        };
        if !project
            .config_files
            .iter()
            .any(|candidate| candidate == path)
        {
            return false;
        }
        if self.selected_path.as_deref() == Some(path) {
            return true;
        }
        self.selected_path = Some(path.to_owned());
        self.pending = None;
        self.preview = None;
        self.error = None;
        true
    }

    /// 为一个路径选择分配新 request id；溢出时拒绝发请求，不复用已用过的身份。
    ///
    /// @author kongweiguang
    fn begin_request(
        &mut self,
        project: &ComposeProjectTarget,
        path: &str,
        endpoint: FileEndpoint,
        ticket: ContainerPanelActionTicket,
    ) -> Option<u64> {
        if self.scope.as_ref() != Some(project) || !self.select_path(path) {
            return None;
        }
        let Some(request_id) = self.next_request_id.checked_add(1) else {
            self.pending = None;
            self.preview = None;
            self.error = Some(crate::locale::text(
                "Compose YAML 预览请求已达到上限，请重新打开项目后重试。",
                "Compose YAML preview request IDs are exhausted. Reopen the project and try again.",
            ).to_string());
            return None;
        };
        self.next_request_id = request_id;
        self.pending = Some(PendingComposeYamlPreview {
            endpoint,
            project: project.clone(),
            request_id,
            ticket,
        });
        self.preview = None;
        self.error = None;
        Some(request_id)
    }

    /// 返回当前回执是否仍对应活动项目、精确 endpoint 和 request id。
    ///
    /// @author kongweiguang
    fn response_matches(&self, request_id: u64, endpoint: &FileEndpoint) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.request_id == request_id
                && &pending.endpoint == endpoint
                && self.scope.as_ref() == Some(&pending.project)
        })
    }

    /// 仅将当前请求票据交给 OperationsView 做 session/runtime generation 的二次核对。
    ///
    /// @author kongweiguang
    fn ticket_for_response(
        &self,
        request_id: u64,
        endpoint: &FileEndpoint,
    ) -> Option<ContainerPanelActionTicket> {
        self.response_matches(request_id, endpoint)
            .then(|| self.pending.as_ref().map(|pending| pending.ticket.clone()))
            .flatten()
    }

    /// 接受唯一当前回执并保留完整 FilePreview 元数据；错误、二进制和非法编码不显示为 YAML。
    ///
    /// @author kongweiguang
    fn accept_response(
        &mut self,
        request_id: u64,
        endpoint: &FileEndpoint,
        result: Result<FilePreview, String>,
        window: &mut Window,
        cx: &mut Context<OperationsView>,
    ) -> bool {
        if !self.response_matches(request_id, endpoint) {
            return false;
        }
        self.pending = None;
        match result {
            Err(error) => {
                self.preview = None;
                self.error = Some(error);
            }
            Ok(preview) if preview.endpoint != *endpoint => {
                self.preview = None;
                self.error = Some(
                    crate::locale::text(
                        "文件服务返回了不同的 Compose YAML 路径，请重试。",
                        "The file service returned a different Compose YAML path. Try again.",
                    )
                    .to_string(),
                );
            }
            Ok(preview) => {
                match compose_yaml_text(&preview) {
                    Ok(Some(content)) => {
                        self.update_editor(content, window, cx);
                        self.error = None;
                    }
                    Ok(None) => {
                        self.error = Some(
                            crate::locale::text(
                                "检测到二进制内容，无法作为 Compose YAML 预览。",
                                "Binary content cannot be shown as a Compose YAML preview.",
                            )
                            .to_string(),
                        );
                    }
                    Err(ComposeYamlContentIssue::InvalidUtf8) => {
                        self.error = Some(
                            crate::locale::text(
                                "文件内容不是有效的 UTF-8，无法作为 Compose YAML 预览。",
                                "The file is not valid UTF-8 and cannot be shown as Compose YAML.",
                            )
                            .to_string(),
                        );
                    }
                }
                self.preview = Some(preview);
            }
        }
        true
    }

    /// 复用一个带 YAML 高亮和行号的只读编辑器，正文更新时不重建焦点与滚动宿主。
    ///
    /// @author kongweiguang
    fn update_editor(
        &mut self,
        content: String,
        window: &mut Window,
        cx: &mut Context<OperationsView>,
    ) {
        if let Some(editor) = self.editor.as_ref() {
            editor.update(cx, |state, cx| state.set_value(content, window, cx));
            return;
        }
        let editor = cx.new(|cx| EditorState::new(window, cx).language("yaml"));
        editor.update(cx, |state, cx| state.set_value(content, window, cx));
        self.editor = Some(editor);
    }

    /// 路径解析失败时保留选中项和可恢复错误，不发出含糊的文件请求。
    ///
    /// @author kongweiguang
    fn fail_selected_path(&mut self, path: &str, message: String) {
        if self.select_path(path) {
            self.pending = None;
            self.preview = None;
            self.error = Some(message);
        }
    }

    /// 当前 YAML 路径只在活动项目的配置列表中存在时可用于刷新、复制或打开。
    ///
    /// @author kongweiguang
    pub(super) fn selected_path(&self) -> Option<&str> {
        self.selected_path.as_deref()
    }

    /// 预览过程中禁用刷新，避免同一份面板重复排队无意义读取。
    ///
    /// @author kongweiguang
    pub(super) fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// 成功和二进制回执都保留文件元数据；失败读取则没有可展示的 preview。
    ///
    /// @author kongweiguang
    pub(super) fn preview(&self) -> Option<&FilePreview> {
        self.preview.as_ref()
    }

    /// 返回当前 YAML 路径对应的可恢复错误。
    ///
    /// @author kongweiguang
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 暴露只读代码编辑器实体；renderer 负责应用 ReadOnly 组件状态。
    ///
    /// @author kongweiguang
    pub(super) fn editor(&self) -> Option<&Entity<EditorState>> {
        self.editor.as_ref()
    }
}

/// 只将有效 UTF-8 文本交给代码编辑器，binary 标记优先于任何字节解码结果。
///
/// @author kongweiguang
fn compose_yaml_text(preview: &FilePreview) -> Result<Option<String>, ComposeYamlContentIssue> {
    if preview.binary {
        return Ok(None);
    }
    std::str::from_utf8(&preview.bytes)
        .map(str::to_owned)
        .map(Some)
        .map_err(|_| ComposeYamlContentIssue::InvalidUtf8)
}

/// 按主线 YAML 页的 footer 语义汇总读取大小、权限、编码、行尾、只读和修改时间。
///
/// @author kongweiguang
pub(super) fn compose_yaml_metadata_items(preview: &FilePreview) -> Vec<String> {
    let mut items = Vec::new();
    let size = preview
        .total_size
        .or_else(|| preview.revision.as_ref().map(|revision| revision.size));
    if let Some(size) = size {
        let size = format_byte_count(size);
        items.push(if preview.truncated {
            format!("{}/{}", format_byte_count(preview.bytes.len() as u64), size)
        } else {
            size
        });
    } else if preview.truncated {
        items.push(format!(
            "{}+",
            format_byte_count(preview.bytes.len() as u64)
        ));
    } else {
        items.push(format_byte_count(preview.bytes.len() as u64));
    }
    if let Some(permissions) = preview
        .revision
        .as_ref()
        .and_then(|value| value.permissions)
    {
        items.push(format!("{:04o}", permissions & 0o7777));
    }
    items.push(if preview.binary {
        "BINARY".to_owned()
    } else {
        "UTF-8".to_owned()
    });
    if !preview.binary
        && let Some(line_ending) = line_ending(&preview.bytes)
    {
        items.push(line_ending.to_owned());
    }
    items.push("RO".to_owned());
    if let Some(modified) = preview
        .revision
        .as_ref()
        .and_then(|revision| revision.modified.as_ref())
    {
        items.push(modified.clone());
    }
    items
}

/// 使用主线相同的二进制单位表示大小，便于截断结果与总大小直接比较。
///
/// @author kongweiguang
fn format_byte_count(value: u64) -> String {
    if value < 1024 {
        return format!("{value} B");
    }
    let units = ["KB", "MB", "GB"];
    let mut size = value as f64 / 1024.0;
    let mut unit_index = 0;
    while size >= 1024.0 && unit_index < units.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    format!(
        "{} {}",
        if size >= 10.0 {
            format!("{size:.0}")
        } else {
            format!("{size:.1}")
        },
        units[unit_index]
    )
}

/// 只在文件内容实际包含换行时报告 LF、CRLF 或混合行尾。
///
/// @author kongweiguang
fn line_ending(bytes: &[u8]) -> Option<&'static str> {
    let mut crlf = false;
    let mut lf = false;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if index > 0 && bytes[index - 1] == b'\r' {
                crlf = true;
            } else {
                lf = true;
            }
        }
    }
    match (crlf, lf) {
        (true, true) => Some("MIXED"),
        (true, false) => Some("CRLF"),
        (false, true) => Some("LF"),
        (false, false) => None,
    }
}

impl OperationsView {
    /// 右栏 YAML 选择与 main 一样把冻结项目路径交给中央只读工作区，显式刷新仍可读取页内预览。
    ///
    /// @author kongweiguang
    pub(super) fn request_compose_yaml_workspace(
        &mut self,
        project: &ComposeProjectTarget,
        path: &str,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, None)
            || self.host_target != project.host_id
            || self.runtime != project.runtime
            || !matches!(self.container_host.page(), ContainerHostPage::ComposeProject { target } if target == project)
        {
            return;
        }
        self.compose_yaml.select_project(project);
        if !self.compose_yaml.select_path(path) {
            return;
        }
        self.request_compose_config(
            project.host_id.clone(),
            path.to_owned(),
            Some(project.working_dir.clone()),
            ticket,
            cx,
        );
    }

    /// 预览请求固定项目、路径、session ticket 和 endpoint；Root 仍是唯一文件服务调用方。
    ///
    /// @author kongweiguang
    pub(super) fn request_compose_yaml_preview(
        &mut self,
        project: &ComposeProjectTarget,
        path: &str,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        if !self.container_action_ticket_is_current(ticket, None)
            || !matches!(
                self.container_panel_context.as_ref(),
                None | Some(ContainerPanelContext::Host { .. })
                    | Some(ContainerPanelContext::WorkspaceHost { .. })
            )
            || self.host_target != project.host_id
            || self.runtime != project.runtime
            || !matches!(
                self.container_host.page(),
                ContainerHostPage::ComposeProject { target } if target == project
            )
        {
            return None;
        }
        self.compose_yaml.select_project(project);
        if !project
            .config_files
            .iter()
            .any(|candidate| candidate == path)
        {
            return None;
        }
        if let Some(request_id) = self.begin_compose_yaml_preview(project, path, ticket, cx) {
            return Some(request_id);
        }
        None
    }

    /// 使用 main 同源的相对路径解析器，拒绝未能绑定到绝对主机文件的配置路径。
    ///
    /// @author kongweiguang
    fn begin_compose_yaml_preview(
        &mut self,
        project: &ComposeProjectTarget,
        path: &str,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let Some((endpoint, _)) = super::actions::compose_config_endpoint(
            project.host_id.clone(),
            path,
            Some(&project.working_dir),
        ) else {
            self.compose_yaml.fail_selected_path(
                path,
                crate::locale::text(
                    "Compose 配置路径不是可定位的主机文件。",
                    "The Compose config path is not a resolvable host file.",
                )
                .to_string(),
            );
            cx.notify();
            return None;
        };
        let request_id =
            self.compose_yaml
                .begin_request(project, path, endpoint.clone(), ticket.clone())?;
        cx.emit(OperationsEvent::ComposeYamlPreviewRequested {
            context: ticket.context.clone(),
            request_id,
            endpoint,
        });
        cx.notify();
        Some(request_id)
    }

    /// 回执同时验证当前 view ticket、请求代次、文件 endpoint 和返回 preview endpoint。
    ///
    /// @author kongweiguang
    pub(crate) fn report_compose_yaml_preview(
        &mut self,
        request_id: u64,
        endpoint: FileEndpoint,
        result: Result<FilePreview, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ticket) = self.compose_yaml.ticket_for_response(request_id, &endpoint) else {
            return false;
        };
        if !self.container_read_ticket_is_current(&ticket) {
            return false;
        }
        let accepted = self
            .compose_yaml
            .accept_response(request_id, &endpoint, result, window, cx);
        if accepted {
            cx.notify();
        }
        accepted
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_compose_yaml.rs"]
mod tests;
