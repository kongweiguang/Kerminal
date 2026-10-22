//! 运维表单只收集有明确边界的输入，结构化命令参数不经 shell 拼接。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Entity, Subscription, Window,
    component::input::{InputEvent, InputState, TextareaState},
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
    pub(super) forward_bind_host: Entity<InputState>,
    pub(super) forward_bind_port: Entity<InputState>,
    pub(super) forward_destination_host: Entity<InputState>,
    pub(super) forward_destination_port: Entity<InputState>,
    pub(super) forward_form_open: bool,
}

impl OperationsInputs {
    /// 只替换既有控件的 placeholder；Entity 与订阅不变，草稿和焦点继续由原输入状态持有。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_locale(&self, window: &mut Window, cx: &mut Context<OperationsView>) {
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
            state.set_placeholder(crate::locale::text("转发名称", "Forward name"), window, cx);
        });
        self.forward_bind_host.update(cx, |state, cx| {
            state.set_placeholder(crate::locale::text("监听地址", "Bind address"), window, cx);
        });
        self.forward_bind_port.update(cx, |state, cx| {
            state.set_placeholder(crate::locale::text("监听端口", "Bind port"), window, cx);
        });
        self.forward_destination_host.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text(
                    "目标地址（动态代理留空）",
                    "Destination address (leave blank for dynamic proxy)",
                ),
                window,
                cx,
            );
        });
        self.forward_destination_port.update(cx, |state, cx| {
            state.set_placeholder(
                crate::locale::text(
                    "目标端口（动态代理留空）",
                    "Destination port (leave blank for dynamic proxy)",
                ),
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
                "搜索名称、镜像或 Compose 项目",
                "Search name, image, or Compose project",
            ))
        });
        let search_subscription = cx.subscribe(&container_search, |view, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                view.container_search = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let mut inputs = OperationsInputs {
            container_search,
            _subscriptions: vec![search_subscription],
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
                InputState::new(window, cx)
                    .placeholder(crate::locale::text("转发名称", "Forward name"))
            }),
            forward_bind_host: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::locale::text("监听地址", "Bind address"))
            }),
            forward_bind_port: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::locale::text("监听端口", "Bind port"))
            }),
            forward_destination_host: cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::locale::text(
                    "目标地址（动态代理留空）",
                    "Destination address (leave blank for dynamic proxy)",
                ))
            }),
            forward_destination_port: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::locale::text("目标端口", "Destination port"))
            }),
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
    pub(super) fn prepare_forward_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            for input in [
                &inputs.forward_name,
                &inputs.forward_bind_host,
                &inputs.forward_bind_port,
                &inputs.forward_destination_host,
                &inputs.forward_destination_port,
            ] {
                input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
            }
            // 安全 loopback 是常见转发起点，端口仍要求明确输入，避免默认暴露到所有网卡。
            for input in [&inputs.forward_bind_host, &inputs.forward_destination_host] {
                input.update(cx, |state, cx| {
                    state.set_value("127.0.0.1".to_owned(), window, cx)
                });
            }
            if let Some(inputs) = self.inputs.as_mut() {
                inputs.forward_form_open =
                    next_forward_form_open(inputs.forward_form_open, ForwardFormTransition::Add);
            }
        }
        self.forward_kind = ForwardKind::Local;
        self.forward_edit_expected = None;
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
        if self.busy
            || self.forward_save_pending.is_some()
            || !forward.persistent
            || forward.forward.state != kerminal_domain::operations::ForwardState::Stopped
        {
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
    }

    /// 读取单行输入时复制为拥有值，使异步任务不借用 GPUI Entity 或 Context。
    pub(super) fn input_value(&self, input: &Entity<InputState>, cx: &mut Context<Self>) -> String {
        input.update(cx, |state, _| state.value().to_string())
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
