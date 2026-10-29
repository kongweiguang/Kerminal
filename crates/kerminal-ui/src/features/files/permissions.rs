//! 文件权限输入状态与 POSIX mode 展示；具体读取和 chmod 始终由 FileService 执行。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Entity, Subscription, Window,
    component::input::{InputEvent, InputState},
};
use kerminal_domain::files::{FileEndpoint, FileEntry, FileEntryKind};

pub(super) mod mode;

/// 表格行补充 POSIX 文件类型前缀，并复用权限编辑器同一套特殊位显示规则。
///
/// @author kongweiguang
pub(super) fn format_posix_mode(kind: FileEntryKind, mode: u32) -> String {
    let kind_prefix = match kind {
        FileEntryKind::Directory => 'd',
        FileEntryKind::File => '-',
        FileEntryKind::Symlink => 'l',
        FileEntryKind::Other => '?',
    };
    format!("{kind_prefix}{}", mode::format_symbolic_mode(mode))
}

/// 与权限对话框共享八进制格式化，但不把内部 mode 模块暴露给文件面板外的调用方。
///
/// @author kongweiguang
pub(super) fn format_octal_mode(mode: u32) -> String {
    mode::format_octal_mode(mode)
}

use crate::locale;

/// 输入框只缓存用户正在编辑的 mode；真实权限与可用能力来自当前 FileEntry 和服务响应。
pub(super) struct PermissionEditor {
    pub(super) input: Option<Entity<InputState>>,
    _subscription: Option<Subscription>,
    selected_endpoint: Option<FileEndpoint>,
    unsupported_endpoints: Vec<FileEndpoint>,
    pub(super) busy: bool,
}

impl Default for PermissionEditor {
    /// 首次没有选择文件时不预填权限值，避免把空 metadata 伪装成 0644。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            input: None,
            _subscription: None,
            selected_endpoint: None,
            unsupported_endpoints: Vec::new(),
            busy: false,
        }
    }
}

impl PermissionEditor {
    /// 首次获得窗口后再创建 Kit 输入组件，使 FilesPane 构造不依赖 UI runtime。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<super::FilesPane>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(locale::text(
                "八进制权限，例如 0755",
                "Octal mode, e.g. 0755",
            ))
        });
        let subscription = cx.subscribe(&input, |pane, input, event, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter {
                secondary: false,
                shift: false,
            } if pane.permission_dialog_target.is_some()
                && pane.delete_confirmation.is_none()
                && !input.read(cx).is_composing() =>
            {
                pane.confirm_global_dialog(cx);
            }
            InputEvent::PressEnter { .. } | InputEvent::Focus | InputEvent::Blur => {}
        });
        self.input = Some(input);
        self._subscription = Some(subscription);
    }

    /// 仅在所选 endpoint 改变时同步输入，避免每次 GPUI 重绘都覆盖用户草稿。
    ///
    /// @author kongweiguang
    pub(super) fn sync_selection(
        &mut self,
        selected: Option<&FileEntry>,
        window: &mut Window,
        cx: &mut Context<super::FilesPane>,
    ) {
        let endpoint = selected.map(|entry| entry.endpoint.clone());
        if endpoint == self.selected_endpoint {
            return;
        }
        self.selected_endpoint = endpoint.clone();
        if let Some(input) = self.input.as_ref() {
            let value = selected
                .filter(|entry| supports_posix_mode_endpoint(&entry.endpoint))
                .and_then(|entry| entry.permissions)
                .map(mode::format_octal_mode)
                .unwrap_or_default();
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
    }

    /// 返回当前输入草稿；调用方必须通过 `parse_octal_mode` 再做完整范围验证。
    ///
    /// @author kongweiguang
    pub(super) fn value(&self, cx: &mut Context<super::FilesPane>) -> Option<String> {
        self.input
            .as_ref()
            .map(|input| input.update(cx, |state, _| state.value().to_string()))
    }

    /// 仅允许有真实 mode 元数据的普通文件或目录调用 chmod。
    ///
    /// @author kongweiguang
    pub(super) fn can_edit(&self, selected: Option<&FileEntry>) -> bool {
        self.can_show_editor(selected) && !self.busy
    }

    /// 没有 POSIX 语义、权限 metadata 或 provider 能力时不展示可编辑的 mode 输入。
    ///
    /// @author kongweiguang
    pub(super) fn can_show_editor(&self, selected: Option<&FileEntry>) -> bool {
        selected.is_some_and(|entry| {
            matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory)
                && supports_posix_mode_endpoint(&entry.endpoint)
                && entry.permissions.is_some()
                && !self.is_unsupported(&entry.endpoint)
        })
    }

    /// 页面明确说明为何当前权限不能编辑，而不是静默显示一个默认权限。
    ///
    /// @author kongweiguang
    pub(super) fn explanation(&self, selected: Option<&FileEntry>) -> String {
        let Some(entry) = selected else {
            return locale::text(
                "选择文件或目录以查看权限。",
                "Select a file or folder to inspect permissions.",
            )
            .to_string();
        };
        if !matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory) {
            return locale::text(
                "符号链接和特殊项不在权限编辑范围内。",
                "Symlinks and special entries are not editable here.",
            )
            .to_string();
        }
        if !supports_posix_mode_endpoint(&entry.endpoint) {
            return locale::text(
                "Windows 本机权限使用 ACL；此处不读取或修改 ACL。",
                "Windows local permissions use ACLs; this view does not read or change them.",
            )
            .to_string();
        }
        if self.is_unsupported(&entry.endpoint) {
            return locale::text(
                "此位置不支持 chmod；刷新目录后可重新探测。",
                "chmod is unsupported here; refresh the directory to probe again.",
            )
            .to_string();
        }
        let Some(mode) = entry.permissions else {
            return locale::text(
                "来源未提供 POSIX 权限元数据，无法安全编辑。",
                "This source did not provide POSIX mode metadata; editing is unavailable.",
            )
            .to_string();
        };
        let mode = mode & 0o7777;
        let special = special_bits_description(mode);
        locale::text(
            format!(
                "{} · {}{}",
                mode::format_octal_mode(mode),
                mode::format_symbolic_mode(mode),
                special
            ),
            format!(
                "{} · {}{}",
                mode::format_octal_mode(mode),
                mode::format_symbolic_mode(mode),
                special
            ),
        )
        .to_string()
    }

    /// 将 provider 明确报告的 Unsupported 限制为当前对象，刷新后允许再次探测。
    ///
    /// @author kongweiguang
    pub(super) fn mark_unsupported(&mut self, endpoint: FileEndpoint) {
        if !self.unsupported_endpoints.contains(&endpoint) {
            self.unsupported_endpoints.push(endpoint);
        }
    }

    /// 显式刷新当前目录时清除该目录能力缓存，不影响其它主机或路径的探测结果。
    ///
    /// @author kongweiguang
    pub(super) fn clear_unsupported_in_directory(&mut self, directory: &FileEndpoint) {
        self.unsupported_endpoints
            .retain(|endpoint| super::parent_endpoint(endpoint).as_ref() != Some(directory));
    }

    /// 重置选择快照，下一次绘制从新读到的 FileEntry 恢复真实 mode。
    ///
    /// @author kongweiguang
    pub(super) fn reset_selection(&mut self) {
        self.selected_endpoint = None;
    }

    /// 只对完整 endpoint 做能力缓存查询，避免把一次对象级失败传播到整个主机。
    ///
    /// @author kongweiguang
    fn is_unsupported(&self, endpoint: &FileEndpoint) -> bool {
        self.unsupported_endpoints.contains(endpoint)
    }
}

/// 根据 endpoint 的文件系统语义拒绝把 Windows 本机 ACL 映射值冒充 POSIX mode。
///
/// @author kongweiguang
pub(super) fn supports_posix_mode_endpoint(endpoint: &FileEndpoint) -> bool {
    match endpoint {
        FileEndpoint::Local { .. } => cfg!(target_family = "unix"),
        FileEndpoint::Remote { .. } | FileEndpoint::Container { .. } => true,
    }
}

/// 仅把 Windows 本机普通文件/目录的 ACL 能力说明压成紧凑行，其他具体权限错误仍完整展示。
///
/// @author kongweiguang
pub(super) fn is_windows_local_acl_limitation(entry: &FileEntry) -> bool {
    matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory)
        && matches!(&entry.endpoint, FileEndpoint::Local { .. })
        && !cfg!(target_family = "unix")
}

impl super::FilesPane {
    /// 校验完整 octal 草稿并经 FileService 提交；metadata 缺失时拒绝猜测权限值。
    ///
    /// @author kongweiguang
    pub(super) fn apply_permissions(&mut self, cx: &mut Context<Self>) {
        let side = self.active_side;
        let Some(entry) = self.panels[side].selected.clone() else {
            self.status = Some(self.permissions.explanation(None));
            cx.notify();
            return;
        };
        if !self.permissions.can_edit(Some(&entry)) {
            self.status = Some(self.permissions.explanation(Some(&entry)));
            cx.notify();
            return;
        }
        let Some(input) = self.permissions.value(cx) else {
            self.status = Some(
                locale::text(
                    "权限输入框尚未就绪。",
                    "The permission editor is not ready.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(mode) = mode::parse_octal_mode(&input) else {
            self.status = Some(
                locale::text(
                    "请输入八进制权限，例如 755、0755 或 0o4755。",
                    "Enter an octal mode, such as 755, 0755, or 0o4755.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        let endpoint = entry.endpoint.clone();
        let command_endpoint = endpoint.clone();
        let selected_name = entry.name.clone();
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
        let files = self.files.clone();
        let executor = self.executor.clone();
        self.permissions.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let _external_target_scope = external_target_lease;
                    files.chmod(cancel, command_endpoint, mode).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.permissions.busy = false;
                    match result {
                        Ok(()) => {
                            if pane
                                .permission_dialog_target
                                .as_ref()
                                .is_some_and(|(_, target)| target == &endpoint)
                            {
                                pane.permission_dialog_target = None;
                            }
                            pane.status = Some(
                                locale::text("文件权限已更新。", "Permissions updated.")
                                    .to_string(),
                            );
                            pane.permissions.reset_selection();
                            pane.panels[side].pending_selection = Some(selected_name);
                            pane.load_panel(side, cx);
                        }
                        Err(error) => {
                            if matches!(
                                error.public_code(),
                                kerminal_application::PublicErrorCode::Unsupported
                                    | kerminal_application::PublicErrorCode::CapabilityNotSupported
                            ) {
                                pane.permissions.mark_unsupported(endpoint.clone());
                            }
                            pane.status = Some(super::localized_error(&error));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }
}

/// 将 POSIX 特殊权限位翻译成简短说明，避免用户只能猜测 mode 的前三位含义。
///
/// @author kongweiguang
fn special_bits_description(mode: u32) -> String {
    let mut bits: Vec<String> = Vec::new();
    if mode & 0o4000 != 0 {
        bits.push(locale::text("setuid", "setuid").to_string());
    }
    if mode & 0o2000 != 0 {
        bits.push(locale::text("setgid", "setgid").to_string());
    }
    if mode & 0o1000 != 0 {
        bits.push(locale::text("sticky", "sticky").to_string());
    }
    if bits.is_empty() {
        String::new()
    } else {
        format!(" · {}", bits.join("/"))
    }
}
