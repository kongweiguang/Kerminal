//! 运维表单只收集有明确边界的输入，结构化命令参数不经 shell 拼接。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Entity, SharedString, Subscription, Window,
    component::{
        input::{InputEvent, InputState, TextareaState},
        searchable_list::SearchableListItem,
        select::{SelectEvent, SelectState},
    },
};
use kerminal_application::operations::{ManagedPortForwardSnapshot, PortForwardDefinition};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, PortForwardConfig, ProxyApplyScope, ProxyProtocol,
        RemoteAccessScope,
    },
};
use std::net::IpAddr;

use super::OperationsView;

/// 监听范围保存语义选项，输入文本只承载自定义地址，避免下拉标签与请求地址漂移。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ForwardBindScope {
    Loopback,
    AllInterfaces,
    Custom,
}

impl ForwardBindScope {
    /// 已知回环和未指定地址归入固定选项；其它内容作为用户显式指定的地址保留。
    ///
    /// @author kongweiguang
    pub(super) fn from_host(host: &str) -> Self {
        let host = host.trim();
        if host.eq_ignore_ascii_case("localhost") {
            return Self::Loopback;
        }
        match host.parse::<IpAddr>() {
            Ok(address) if address.is_loopback() => Self::Loopback,
            Ok(address) if address.is_unspecified() => Self::AllInterfaces,
            _ => Self::Custom,
        }
    }

    /// 固定范围映射为规范地址；空的自定义草稿沿用 main 的 loopback 安全默认值。
    ///
    /// @author kongweiguang
    pub(super) fn resolve_host(self, custom_host: &str) -> String {
        match self {
            Self::Loopback => "127.0.0.1".to_owned(),
            Self::AllInterfaces => "0.0.0.0".to_owned(),
            Self::Custom => {
                let host = custom_host.trim();
                if host.is_empty() {
                    "127.0.0.1".to_owned()
                } else {
                    host.to_owned()
                }
            }
        }
    }
}

/// 保留 main 中互相独立的本机、主机和 SOCKS 端点草稿，切换场景时不覆盖用户输入。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq, Eq)]
struct ForwardFormDrafts {
    local_bind_host: String,
    local_custom_bind_host: String,
    local_bind_scope: ForwardBindScope,
    local_listen_port: String,
    remote_bind_host: String,
    remote_custom_bind_host: String,
    remote_bind_scope: ForwardBindScope,
    remote_listen_port: String,
    host_target_host: String,
    host_target_port: String,
    local_target_host: String,
    local_target_port: String,
    local_socks_port: String,
}

impl ForwardFormDrafts {
    /// 端口和目标初值与 main 的表单一致；远端 SOCKS 与远端服务共用主机监听状态。
    ///
    /// @author kongweiguang
    fn main_defaults() -> Self {
        Self {
            local_bind_host: "127.0.0.1".to_owned(),
            local_custom_bind_host: "127.0.0.1".to_owned(),
            local_bind_scope: ForwardBindScope::Loopback,
            local_listen_port: "15432".to_owned(),
            remote_bind_host: "127.0.0.1".to_owned(),
            remote_custom_bind_host: "127.0.0.1".to_owned(),
            remote_bind_scope: ForwardBindScope::Loopback,
            remote_listen_port: "18080".to_owned(),
            host_target_host: "127.0.0.1".to_owned(),
            host_target_port: "5432".to_owned(),
            local_target_host: "127.0.0.1".to_owned(),
            local_target_port: "3000".to_owned(),
            local_socks_port: "1080".to_owned(),
        }
    }
}

/// 下拉项携带稳定语义值，显示标签只负责中英文展示和搜索。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct ForwardBindScopeOption {
    scope: ForwardBindScope,
    label: SharedString,
}

impl SearchableListItem for ForwardBindScopeOption {
    type Value = ForwardBindScope;

    /// 搜索和辅助技术使用完整范围说明，避免单独的地址数字难以辨认。
    ///
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        self.label.clone()
    }

    /// 选择器把范围枚举交给表单，不从本地化文字反解析状态。
    ///
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.scope
    }
}

/// 监听范围选项与 main 的表单一致，固定选项使用安全且可预测的绑定地址。
///
/// @author kongweiguang
fn forward_bind_scope_options() -> Vec<ForwardBindScopeOption> {
    [
        (
            ForwardBindScope::Loopback,
            crate::locale::text("仅本机 (127.0.0.1)", "Local only (127.0.0.1)"),
        ),
        (
            ForwardBindScope::AllInterfaces,
            crate::locale::text("全部接口 (0.0.0.0)", "All interfaces (0.0.0.0)"),
        ),
        (
            ForwardBindScope::Custom,
            crate::locale::text("自定义地址", "Custom address"),
        ),
    ]
    .into_iter()
    .map(|(scope, label)| ForwardBindScopeOption { scope, label })
    .collect()
}

/// 输入控件在首次绘制后持久化，避免重绘时丢失正在编辑的端口或 session 名称。
pub(super) struct OperationsInputs {
    pub(super) container_search: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    pub(super) tmux_name: Entity<InputState>,
    pub(super) tmux_working_directory: Entity<InputState>,
    pub(super) tmux_rename: Entity<InputState>,
    pub(super) exec_program: Entity<InputState>,
    pub(super) exec_arguments: Entity<TextareaState>,
    pub(super) forward_name: Entity<InputState>,
    pub(super) forward_bind_scope: Entity<SelectState<Vec<ForwardBindScopeOption>>>,
    pub(super) forward_bind_host: Entity<InputState>,
    pub(super) forward_bind_port: Entity<InputState>,
    pub(super) forward_destination_host: Entity<InputState>,
    pub(super) forward_destination_port: Entity<InputState>,
    forward_drafts: ForwardFormDrafts,
    pub(super) forward_form_open: bool,
}

impl OperationsInputs {
    /// 只替换既有控件的 placeholder；Entity 与订阅不变，草稿和焦点继续由原输入状态持有。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_locale(&self, window: &mut Window, cx: &mut Context<OperationsView>) {
        let bind_scope = self
            .forward_bind_scope
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or(ForwardBindScope::Loopback);
        self.forward_bind_scope.update(cx, |select, cx| {
            select.set_items(forward_bind_scope_options(), window, cx);
            select.set_selected_value(&bind_scope, window, cx);
        });
        self.container_search.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text(
                    "搜索名称、镜像或 Compose 项目",
                    "Search name, image, or Compose project",
                ),
                window,
                cx,
            );
        });
        self.tmux_name.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text("tmux session 名称", "tmux session name"),
                window,
                cx,
            );
        });
        self.tmux_working_directory.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text("工作目录（可选）", "Working directory (optional)"),
                window,
                cx,
            );
        });
        self.tmux_rename.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text("新的 session 名称", "New session name"),
                window,
                cx,
            );
        });
        self.exec_program.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text("容器内程序，例如 sh", "Program inside container, e.g. sh"),
                window,
                cx,
            );
        });
        self.exec_arguments.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text(
                    "每行一个参数；参数保持独立，不经过 shell 拆分",
                    "One argument per line; arguments stay separate and are not shell-split",
                ),
                window,
                cx,
            );
        });
        self.forward_name.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text("例如 PostgreSQL 隧道", "e.g. PostgreSQL tunnel"),
                window,
                cx,
            );
        });
    }
}

/// 表单字段错误以稳定短信息返回，避免把未校验输入交给远程用例。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OperationsFormError {
    EmptyProgram,
    InvalidProgram,
    InvalidArgument,
    MissingSshHost,
    InvalidForwardName,
    InvalidEndpoint,
    MissingDestination,
}

impl OperationsFormError {
    /// 错误提示说明修复方向，不暴露基础设施错误或输入中的控制字符。
    pub(super) fn message(self) -> String {
        let (zh, en) = match self {
            Self::EmptyProgram => (
                "请输入容器内可执行程序。",
                "Enter a program inside the container.",
            ),
            Self::InvalidProgram => (
                "程序名不能包含空白或控制字符。",
                "The program cannot contain whitespace or control characters.",
            ),
            Self::InvalidArgument => (
                "参数不能包含控制字符。",
                "Arguments cannot contain control characters.",
            ),
            Self::MissingSshHost => (
                "端口转发需要先选择明确的 SSH 主机。",
                "Select an explicit SSH host before creating a forward.",
            ),
            Self::InvalidForwardName => (
                "请填写不含控制字符的转发名称。",
                "Enter a forward name without control characters.",
            ),
            Self::InvalidEndpoint => (
                "监听地址和端口无效，端口需在 1–65535 之间。",
                "The endpoint is invalid; port must be between 1 and 65535.",
            ),
            Self::MissingDestination => (
                "此类型需要填写目标地址和端口。",
                "This forward type requires a destination endpoint.",
            ),
        };
        crate::locale::text(zh, en).to_string()
    }
}

/// 端口转发表单的可见性事件；输入 Entity 保留在 Inputs 中，不随页面切换重建。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ForwardFormTransition {
    Add,
    Cancel,
    CreateSucceeded,
    CreateFailed,
}

/// 只切换列表/表单可见状态，失败保留草稿，成功或取消回列表。
///
/// @author kongweiguang
pub(super) fn next_forward_form_open(
    currently_open: bool,
    transition: ForwardFormTransition,
) -> bool {
    match transition {
        ForwardFormTransition::Add => true,
        ForwardFormTransition::Cancel | ForwardFormTransition::CreateSucceeded => false,
        ForwardFormTransition::CreateFailed => currently_open,
    }
}

impl OperationsView {
    /// 输入和 Enter 订阅只创建一次并由视图持有；提交只路由当前弹层，继续复用原目标门禁。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inputs.is_some() {
            return;
        }
        let container_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索应用、服务、镜像、端口或 ID",
                "Search projects, services, images, ports, or IDs",
            ))
        });
        let search_subscription = cx.subscribe(&container_search, |view, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                view.container_search = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let forward_bind_host = cx.new(|cx| InputState::new(window, cx));
        let bind_host_change_subscription =
            cx.subscribe(&forward_bind_host, |view, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let custom_scope = view.inputs.as_ref().is_some_and(|inputs| {
                        inputs
                            .forward_bind_scope
                            .read(cx)
                            .selected_value()
                            .is_some_and(|scope| *scope == ForwardBindScope::Custom)
                    });
                    if custom_scope {
                        let host = input.read(cx).value().to_string();
                        if let Some(inputs) = view.inputs.as_mut() {
                            match view.forward_kind {
                                ForwardKind::Local | ForwardKind::Dynamic => {
                                    inputs.forward_drafts.local_custom_bind_host = host;
                                }
                                ForwardKind::Remote | ForwardKind::RemoteDynamic => {
                                    inputs.forward_drafts.remote_custom_bind_host = host;
                                }
                            }
                        }
                    }
                    cx.notify();
                }
            });
        let forward_bind_scope = cx.new(|cx| {
            SelectState::new(
                forward_bind_scope_options(),
                Some(gpui_kit::component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let bind_scope_subscription = cx.subscribe_in(
            &forward_bind_scope,
            window,
            |view, _, event: &SelectEvent<Vec<ForwardBindScopeOption>>, window, cx| {
                let SelectEvent::Confirm(Some(scope)) = event else {
                    return;
                };
                let Some(inputs) = view.inputs.as_ref() else {
                    return;
                };
                let current_host = inputs
                    .forward_bind_host
                    .update(cx, |state, _| state.value().to_string());
                let next_host = if *scope == ForwardBindScope::Custom {
                    match view.forward_kind {
                        ForwardKind::Local | ForwardKind::Dynamic => {
                            inputs.forward_drafts.local_custom_bind_host.clone()
                        }
                        ForwardKind::Remote | ForwardKind::RemoteDynamic => {
                            inputs.forward_drafts.remote_custom_bind_host.clone()
                        }
                    }
                } else {
                    scope.resolve_host(&current_host)
                };
                inputs
                    .forward_bind_host
                    .update(cx, |state, cx| state.set_value(next_host, window, cx));
                cx.notify();
            },
        );
        let mut inputs = OperationsInputs {
            container_search,
            _subscriptions: vec![
                search_subscription,
                bind_scope_subscription,
                bind_host_change_subscription,
            ],
            tmux_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::locale::text(
                    "tmux session 名称",
                    "tmux session name",
                ))
            }),
            tmux_working_directory: cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::locale::text(
                    "工作目录（可选）",
                    "Working directory (optional)",
                ))
            }),
            tmux_rename: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::locale::text("新的 session 名称", "New session name"))
            }),
            exec_program: cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::locale::text(
                    "容器内程序，例如 sh",
                    "Program inside container, e.g. sh",
                ))
            }),
            exec_arguments: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(crate::locale::text(
                        "每行一个参数；参数保持独立，不经过 shell 拆分",
                        "One argument per line; arguments stay separate and are not shell-split",
                    ))
                    .auto_grow(2, 5)
            }),
            forward_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::locale::text(
                    "例如 PostgreSQL 隧道",
                    "e.g. PostgreSQL tunnel",
                ))
            }),
            forward_bind_scope,
            forward_bind_host,
            forward_bind_port: cx.new(|cx| InputState::new(window, cx)),
            forward_destination_host: cx.new(|cx| InputState::new(window, cx)),
            forward_destination_port: cx.new(|cx| InputState::new(window, cx)),
            forward_drafts: ForwardFormDrafts::main_defaults(),
            forward_form_open: false,
        };
        for input in [
            inputs.tmux_name.clone(),
            inputs.tmux_working_directory.clone(),
            inputs.tmux_rename.clone(),
            inputs.forward_name.clone(),
            inputs.forward_bind_host.clone(),
            inputs.forward_bind_port.clone(),
            inputs.forward_destination_host.clone(),
            inputs.forward_destination_port.clone(),
        ] {
            inputs
                ._subscriptions
                .push(cx.subscribe(&input, |view, _, event, cx| {
                    if !matches!(
                        event,
                        InputEvent::PressEnter {
                            secondary: false,
                            shift: false
                        }
                    ) {
                        return;
                    }
                    if view.tmux_dialog_context.is_some() {
                        view.confirm_root_confirmation(cx);
                    } else if view
                        .inputs
                        .as_ref()
                        .is_some_and(|inputs| inputs.forward_form_open)
                    {
                        view.save_forward(true, cx);
                    }
                }));
        }
        self.inputs = Some(inputs);
    }

    /// 修改 session 名称前显式初始化输入实体并恢复当前目标名。
    pub(super) fn prepare_tmux_rename(
        &mut self,
        session_id: String,
        session_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_inputs(window, cx);
        if let Some(inputs) = &self.inputs {
            inputs.tmux_rename.update(cx, |state, cx| {
                state.set_value(session_name, window, cx);
            });
        }
        self.tmux_rename_target = Some(session_id);
        cx.notify();
    }

    /// 新建表单清空旧草稿并冻结打开时的 SSH HostId，目标栏随后切换也不改变定义归属。
    ///
    /// @author kongweiguang
    pub(crate) fn prepare_forward_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.forward_save_pending.is_some() {
            return;
        }
        let Some(add_host_expected) = super::resolve_forward_add_host(
            self.tool_rail_target.as_ref(),
            self.host_target.as_ref(),
        ) else {
            return;
        };
        self.ensure_inputs(window, cx);
        if let Some(inputs) = self.inputs.as_ref() {
            inputs
                .forward_name
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
            if let Some(inputs) = self.inputs.as_mut() {
                inputs.forward_drafts = ForwardFormDrafts::main_defaults();
                inputs.forward_form_open =
                    next_forward_form_open(inputs.forward_form_open, ForwardFormTransition::Add);
            }
        }
        self.forward_kind = ForwardKind::Local;
        self.restore_forward_form_draft(ForwardKind::Local, window, cx);
        self.forward_edit_expected = None;
        self.forward_edit_transient_id = None;
        self.forward_add_host_expected = Some(add_host_expected);
        self.forward_save_pending = None;
        self.compose_pending_down = None;
        self.compose_pending_context = None;
        self.confirmed_compose_down = None;
        self.forward_pending_delete = None;
        self.status = None;
        cx.emit(super::OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 编辑只允许原主机和原 ID，保留字段取值后再显示表单，防止编辑变成隐式重绑。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_forward_edit(
        &mut self,
        forward: ManagedPortForwardSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.forward_save_pending.is_some() {
            return;
        }
        self.ensure_inputs(window, cx);
        if let Some(inputs) = self.inputs.as_ref() {
            let config = &forward.forward.config;
            inputs.forward_name.update(cx, |state, cx| {
                state.set_value(config.name.clone(), window, cx)
            });
            inputs.forward_bind_host.update(cx, |state, cx| {
                state.set_value(config.bind.host.clone(), window, cx)
            });
            inputs.forward_bind_scope.update(cx, |select, cx| {
                select.set_selected_value(
                    &ForwardBindScope::from_host(&config.bind.host),
                    window,
                    cx,
                );
            });
            inputs.forward_bind_port.update(cx, |state, cx| {
                state.set_value(config.bind.port.get().to_string(), window, cx)
            });
            let (destination_host, destination_port) = config.destination.as_ref().map_or_else(
                || (String::new(), String::new()),
                |destination| (destination.host.clone(), destination.port.get().to_string()),
            );
            inputs.forward_destination_host.update(cx, |state, cx| {
                state.set_value(destination_host, window, cx)
            });
            inputs.forward_destination_port.update(cx, |state, cx| {
                state.set_value(destination_port, window, cx)
            });
            if let Some(inputs) = self.inputs.as_mut() {
                inputs.forward_form_open = true;
            }
            self.forward_kind = config.kind;
            self.forward_edit_transient_id =
                (!forward.persistent).then(|| forward.forward.id.clone());
            self.forward_edit_expected = Some(PortForwardDefinition {
                id: forward.forward.id.clone(),
                config: config.clone(),
            });
            self.forward_add_host_expected = None;
            self.forward_save_pending = None;
            self.compose_pending_down = None;
            self.compose_pending_context = None;
            self.confirmed_compose_down = None;
            self.forward_pending_delete = None;
            self.status = None;
            cx.emit(super::OperationsEvent::RootConfirmationChanged);
            cx.notify();
        }
        self.capture_forward_form_draft(forward.forward.config.kind, cx);
    }

    /// 监听范围下拉是请求语义的唯一来源；空名称仅在提交时映射为 main 的主机名和 SSH flag。
    ///
    /// @author kongweiguang
    pub(super) fn input_value(&self, input: &Entity<InputState>, cx: &mut Context<Self>) -> String {
        let value = input.update(cx, |state, _| state.value().to_string());
        let Some(inputs) = self.inputs.as_ref().filter(|inputs| {
            inputs.forward_bind_host.entity_id() == input.entity_id()
                || inputs.forward_name.entity_id() == input.entity_id()
        }) else {
            return value;
        };
        if inputs.forward_name.entity_id() == input.entity_id() {
            if value.trim().is_empty() {
                let Some(host_id) = self.forward_form_host_id() else {
                    return value;
                };
                let host_name = self
                    .host_display_names
                    .get(&host_id)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| host_id.as_str().to_owned());
                return default_forward_name(&host_name, self.forward_kind);
            }
            return value;
        }
        let scope = inputs
            .forward_bind_scope
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or_else(|| ForwardBindScope::from_host(&value));
        scope.resolve_host(&value)
    }

    /// 切换转发类型前保存当前端点，目标类型恢复自己的 main 默认值或尚未提交的草稿。
    ///
    /// @author kongweiguang
    pub(super) fn switch_forward_kind(
        &mut self,
        next_kind: ForwardKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.forward_kind == next_kind {
            return;
        }
        self.capture_forward_form_draft(self.forward_kind, cx);
        self.forward_kind = next_kind;
        self.restore_forward_form_draft(next_kind, window, cx);
        cx.notify();
    }

    /// 只在每种转发共用同一组端点时同步草稿，远端 -R/SOCKS 共用主机监听值。
    ///
    /// @author kongweiguang
    fn capture_forward_form_draft(&mut self, kind: ForwardKind, cx: &mut Context<Self>) {
        let Some(inputs) = self.inputs.as_ref() else {
            return;
        };
        let bind_host = inputs
            .forward_bind_host
            .update(cx, |state, _| state.value().to_string());
        let bind_port = inputs
            .forward_bind_port
            .update(cx, |state, _| state.value().to_string());
        let bind_scope = inputs
            .forward_bind_scope
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or(ForwardBindScope::from_host(&bind_host));
        let destination_host = inputs
            .forward_destination_host
            .update(cx, |state, _| state.value().to_string());
        let destination_port = inputs
            .forward_destination_port
            .update(cx, |state, _| state.value().to_string());
        let Some(inputs) = self.inputs.as_mut() else {
            return;
        };
        let drafts = &mut inputs.forward_drafts;

        match kind {
            ForwardKind::Local => {
                drafts.local_bind_host = bind_host;
                if bind_scope == ForwardBindScope::Custom {
                    drafts.local_custom_bind_host = drafts.local_bind_host.clone();
                }
                drafts.local_bind_scope = bind_scope;
                drafts.local_listen_port = bind_port;
                drafts.host_target_host = destination_host;
                drafts.host_target_port = destination_port;
            }
            ForwardKind::Remote => {
                drafts.remote_bind_host = bind_host;
                if bind_scope == ForwardBindScope::Custom {
                    drafts.remote_custom_bind_host = drafts.remote_bind_host.clone();
                }
                drafts.remote_bind_scope = bind_scope;
                drafts.remote_listen_port = bind_port;
                drafts.local_target_host = destination_host;
                drafts.local_target_port = destination_port;
            }
            ForwardKind::Dynamic => {
                drafts.local_bind_host = bind_host;
                if bind_scope == ForwardBindScope::Custom {
                    drafts.local_custom_bind_host = drafts.local_bind_host.clone();
                }
                drafts.local_bind_scope = bind_scope;
                drafts.local_socks_port = bind_port;
            }
            ForwardKind::RemoteDynamic => {
                drafts.remote_bind_host = bind_host;
                if bind_scope == ForwardBindScope::Custom {
                    drafts.remote_custom_bind_host = drafts.remote_bind_host.clone();
                }
                drafts.remote_bind_scope = bind_scope;
                drafts.remote_listen_port = bind_port;
            }
        }
    }

    /// 恢复当前类型对应的独立草稿；共享的远端 listener 保持 -R 与远端 SOCKS 一致。
    ///
    /// @author kongweiguang
    fn restore_forward_form_draft(
        &mut self,
        kind: ForwardKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(inputs) = self.inputs.as_ref() else {
            return;
        };
        let drafts = inputs.forward_drafts.clone();
        let (bind_host, bind_scope, bind_port, destination_host, destination_port) = match kind {
            ForwardKind::Local => (
                drafts.local_bind_host,
                drafts.local_bind_scope,
                drafts.local_listen_port,
                drafts.host_target_host,
                drafts.host_target_port,
            ),
            ForwardKind::Remote => (
                drafts.remote_bind_host,
                drafts.remote_bind_scope,
                drafts.remote_listen_port,
                drafts.local_target_host,
                drafts.local_target_port,
            ),
            ForwardKind::Dynamic => (
                drafts.local_bind_host,
                drafts.local_bind_scope,
                drafts.local_socks_port,
                String::new(),
                String::new(),
            ),
            ForwardKind::RemoteDynamic => (
                drafts.remote_bind_host,
                drafts.remote_bind_scope,
                drafts.remote_listen_port,
                String::new(),
                String::new(),
            ),
        };
        inputs
            .forward_bind_host
            .update(cx, |state, cx| state.set_value(bind_host, window, cx));
        inputs
            .forward_bind_port
            .update(cx, |state, cx| state.set_value(bind_port, window, cx));
        inputs.forward_bind_scope.update(cx, |select, cx| {
            select.set_selected_value(&bind_scope, window, cx)
        });
        inputs.forward_destination_host.update(cx, |state, cx| {
            state.set_value(destination_host, window, cx)
        });
        inputs.forward_destination_port.update(cx, |state, cx| {
            state.set_value(destination_port, window, cx)
        });
    }

    /// 小型表单输入转为拥有字符串后按行拆分，避开 Ropey beta 的迭代差异且不解析 shell。
    pub(super) fn argument_values(&self, cx: &mut Context<Self>) -> Vec<String> {
        self.inputs
            .as_ref()
            .map(|inputs| {
                inputs.exec_arguments.update(cx, |state, _| {
                    let text = state.text().to_string();
                    text.lines()
                        .filter(|line| !line.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
            })
            .unwrap_or_default()
    }
}

/// 空名称沿用 main 的后端命名约定，远端动态 SOCKS 使用 OpenSSH 的 `-R` 标识。
///
/// @author kongweiguang
pub(super) fn default_forward_name(host_name: &str, kind: ForwardKind) -> String {
    let flag = match kind {
        ForwardKind::Local => "-L",
        ForwardKind::Remote | ForwardKind::RemoteDynamic => "-R",
        ForwardKind::Dynamic => "-D",
    };
    format!("{} {flag}", host_name.trim())
}

/// 校验容器程序与 argv 的边界，避免把整行用户文本误当成 shell command。
pub(super) fn container_command(
    program: &str,
    arguments: Vec<String>,
) -> Result<Vec<String>, OperationsFormError> {
    let program = program.trim();
    if program.is_empty() {
        return Err(OperationsFormError::EmptyProgram);
    }
    if program.chars().any(char::is_whitespace) || program.chars().any(char::is_control) {
        return Err(OperationsFormError::InvalidProgram);
    }
    if arguments
        .iter()
        .any(|argument| argument.chars().any(char::is_control))
    {
        return Err(OperationsFormError::InvalidArgument);
    }
    let mut command = Vec::with_capacity(arguments.len() + 1);
    command.push(program.to_owned());
    command.extend(arguments);
    Ok(command)
}

/// 四类端口转发共享一份经过验证的配置；远程监听只默认要求 loopback 范围。
pub(super) fn forward_config(
    host_id: Option<HostId>,
    name: &str,
    kind: ForwardKind,
    bind_host: &str,
    bind_port: &str,
    destination_host: &str,
    destination_port: &str,
) -> Result<PortForwardConfig, OperationsFormError> {
    let host_id = host_id.ok_or(OperationsFormError::MissingSshHost)?;
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(OperationsFormError::InvalidForwardName);
    }
    let bind = endpoint(bind_host, bind_port)?;
    let dynamic = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
    let destination = if dynamic {
        None
    } else if destination_host.trim().is_empty() && destination_port.trim().is_empty() {
        return Err(OperationsFormError::MissingDestination);
    } else {
        Some(endpoint(destination_host, destination_port)?)
    };
    let remote_access_scope = remote_access_scope(kind, bind_host);
    Ok(PortForwardConfig {
        host_id,
        name: name.to_owned(),
        kind,
        bind,
        destination,
        proxy_protocol: dynamic.then_some(ProxyProtocol::Socks5),
        remote_access_scope,
        proxy_apply_scope: ProxyApplyScope::None,
    })
}

/// 远程监听范围由用户输入的 bind 地址归一化，避免 Loopback 元数据与外部监听地址矛盾。
///
/// @author kongweiguang
fn remote_access_scope(kind: ForwardKind, bind_host: &str) -> Option<RemoteAccessScope> {
    if !matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic) {
        return None;
    }
    if bind_host.trim().eq_ignore_ascii_case("localhost") {
        return Some(RemoteAccessScope::Loopback);
    }
    let Ok(address) = bind_host.trim().parse::<IpAddr>() else {
        return Some(RemoteAccessScope::Custom);
    };
    Some(match address {
        IpAddr::V4(address) if address.is_loopback() => RemoteAccessScope::Loopback,
        IpAddr::V6(address) if address.is_loopback() => RemoteAccessScope::Loopback,
        IpAddr::V4(address) if address.is_unspecified() => RemoteAccessScope::AllInterfaces,
        IpAddr::V6(address) if address.is_unspecified() => RemoteAccessScope::AllInterfaces,
        IpAddr::V4(address) if address.is_private() => RemoteAccessScope::PrivateNetwork,
        IpAddr::V6(address) if address.segments()[0] & 0xfe00 == 0xfc00 => {
            RemoteAccessScope::PrivateNetwork
        }
        _ => RemoteAccessScope::Custom,
    })
}

/// 端点只接受显式主机和非零 16 位端口，不猜测默认监听范围。
fn endpoint(host: &str, port: &str) -> Result<ForwardEndpoint, OperationsFormError> {
    let port = port
        .trim()
        .parse::<u16>()
        .map_err(|_| OperationsFormError::InvalidEndpoint)?;
    ForwardEndpoint::new(host, port).map_err(|_| OperationsFormError::InvalidEndpoint)
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_forms.rs"]
mod tests;
