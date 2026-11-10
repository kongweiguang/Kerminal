//! 原生验收窗口复用生产连接表单，仅提供隔离验收所需的面板装配与只读状态。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{Context, Entity, Focusable as _, Render, Window, div, prelude::*};
use kerminal_application::{
    ProfileService,
    connections::{
        RdpLaunchService, SerialPortService,
        management::{HostAuthenticationMode, HostManagementService},
        probe::ConnectionProbeService,
    },
    executor::ApplicationTaskExecutor,
};
use kerminal_domain::{
    connections::RemoteProtocol,
    preferences::{ConfigurationMode, ConfigurationRecoveryState},
};

use super::{
    editor_text::{ListSeparator, format_list, parse_list},
    form::{HostEditorSection, PrivateKeyInputMode},
    local_hosts::LocalHostEditorSection,
    panel::{ConnectionsEditorKind, ConnectionsPanel},
};

/// 持有生产面板实体，测试可切换真实协议视图而不复制表单绘制逻辑。
pub struct HostEditorAcceptanceState {
    panel: Entity<ConnectionsPanel>,
}

/// 展开场景只暴露状态和几何证据，不导出输入值、秘密或生产服务句柄。
pub struct DenseFixtureState {
    pub proxy_enabled: bool,
    pub jump_count: usize,
    pub second_jump_private_key: bool,
    pub rdp_windowed: bool,
    pub prepared_values_match: bool,
    pub scroll_offset_y: f32,
    pub second_jump_lower_focused: bool,
    pub second_jump_lower_visible: bool,
}

impl HostEditorAcceptanceState {
    /// 依赖通过组合根提供，probe 和串口端口由桌面验收替换为可控 fixture。
    ///
    /// @author kongweiguang
    pub fn new(
        hosts: HostManagementService,
        rdp: Option<Arc<RdpLaunchService>>,
        executor: ApplicationTaskExecutor,
        profiles: ProfileService,
        probe: Arc<ConnectionProbeService>,
        serial_ports: Arc<SerialPortService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let recovery = ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        };
        let panel = cx.new(|cx| {
            ConnectionsPanel::new(hosts, rdp, executor, recovery, window, cx)
                .with_profiles(profiles, cx)
                .with_serial_port_service(serial_ports)
                .with_connection_probe_service(probe)
        });
        panel.update(cx, |panel, cx| panel.begin_create_host(window, cx));
        Self { panel }
    }

    /// 与生产协议按钮共用激活入口，Local 切回时必须恢复远端草稿而非修改空表单。
    ///
    /// @author kongweiguang
    pub fn select_protocol(
        &mut self,
        protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel.update(cx, |panel, cx| {
            panel.activate_remote_protocol(protocol, window, cx)
        });
    }

    /// 保留跨协议草稿与默认目标意图，使验收覆盖与用户按钮一致的 Local 往返路径。
    ///
    /// @author kongweiguang
    pub fn select_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.activate_local_host_editor(window, cx));
    }

    /// 分类名称由生产枚举派生，防止截图矩阵遗漏新入口或请求当前协议不支持的页。
    /// @author kongweiguang
    pub fn sections(&self, cx: &gpui_kit::App) -> Vec<&'static str> {
        let panel = self.panel.read(cx);
        if panel.active_editor_kind == Some(ConnectionsEditorKind::Local) {
            return vec!["general", "startup"];
        }
        panel.editor.as_ref().map_or_else(Vec::new, |editor| {
            HostEditorSection::for_protocol(editor.protocol)
                .iter()
                .map(|section| section_name(*section, editor.protocol))
                .collect()
        })
    }

    /// 紧凑窗口的分类入口折叠为菜单时仍复用生产选择用例，报告会标注方法驱动。
    /// @author kongweiguang
    pub fn select_section(
        &mut self,
        section: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            if panel.active_editor_kind == Some(ConnectionsEditorKind::Local) {
                let section = match section {
                    "general" => LocalHostEditorSection::Properties,
                    "startup" => LocalHostEditorSection::Terminal,
                    _ => return false,
                };
                panel.select_local_host_section(section, window, cx);
            } else {
                let Some(editor) = panel.editor.as_ref() else {
                    return false;
                };
                let Some(section) = HostEditorSection::for_protocol(editor.protocol)
                    .iter()
                    .copied()
                    .find(|value| section_name(*value, editor.protocol) == section)
                else {
                    return false;
                };
                panel.select_editor_section(section, window, cx);
            }
            true
        })
    }

    /// 只读实际分类，真实点击后据此确认事件已进入生产状态，避免把截图存在当成点击成功。
    /// @author kongweiguang
    pub fn current_section(&self, cx: &gpui_kit::App) -> Option<&'static str> {
        let panel = self.panel.read(cx);
        if panel.active_editor_kind == Some(ConnectionsEditorKind::Local) {
            return panel
                .local_hosts
                .editor
                .as_ref()
                .map(|editor| match editor.section {
                    LocalHostEditorSection::Properties => "general",
                    LocalHostEditorSection::Terminal => "startup",
                });
        }
        panel
            .editor
            .as_ref()
            .map(|editor| section_name(editor.section, editor.protocol))
    }

    /// 读取活动生产编辑器身份，真实协议点击必须改变它，隐藏缓存草稿不算当前视图。
    /// @author kongweiguang
    pub fn current_protocol(&self, cx: &gpui_kit::App) -> Option<Option<RemoteProtocol>> {
        let panel = self.panel.read(cx);
        if panel.active_editor_kind == Some(ConnectionsEditorKind::Local) {
            panel.local_hosts.editor.as_ref().map(|_| None)
        } else {
            panel.editor.as_ref().map(|editor| Some(editor.protocol))
        }
    }

    /// 公开合成值覆盖分隔符和多行列表；本地环境经真实 Vault 保存，不使用用户秘密材料。
    /// @author kongweiguang
    pub fn prepare_save_fixture(
        &mut self,
        protocol: Option<RemoteProtocol>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            panel.cancel_editor(window, cx);
            if panel.pending_editor_discard {
                panel.confirm_discard_editor(window, cx);
            }
            if let Some(protocol) = protocol {
                panel.activate_remote_protocol(protocol, window, cx);
                let Some(editor) = panel.editor.as_mut() else {
                    return false;
                };
                let host = if protocol == RemoteProtocol::Serial {
                    "COM-ACCEPTANCE-01"
                } else {
                    "fixture.invalid"
                };
                let port = match protocol {
                    RemoteProtocol::Rdp => "3389",
                    RemoteProtocol::Telnet => "23",
                    _ => "22",
                };
                for (input, value) in [
                    (&editor.inputs.name, name),
                    (&editor.inputs.host, host),
                    (&editor.inputs.port, port),
                    (&editor.inputs.username, "acceptance"),
                    (&editor.inputs.serial_baud, "115200"),
                ] {
                    input.update(cx, |input, cx| {
                        input.set_value(value.to_owned(), window, cx)
                    });
                }
                editor.inputs.tags.update(cx, |input, cx| {
                    input.set_value(
                        format_list(&fixture_tags(), ListSeparator::Commas),
                        window,
                        cx,
                    )
                });
                if matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
                    panel.set_authentication_mode(HostAuthenticationMode::Agent, window, cx);
                }
            } else {
                panel.activate_local_host_editor(window, cx);
                let Some(editor) = panel.local_hosts.editor.as_mut() else {
                    return false;
                };
                editor
                    .name
                    .update(cx, |input, cx| input.set_value(name.to_owned(), window, cx));
                editor
                    .shell
                    .update(cx, |input, cx| input.set_value(fixture_shell(), window, cx));
                editor.args.update(cx, |input, cx| {
                    input.set_value(
                        format_list(&fixture_arguments(), ListSeparator::Lines),
                        window,
                        cx,
                    )
                });
                editor.env.update(cx, |input, cx| {
                    input.set_value(fixture_environment(), window, cx)
                });
            }
            cx.notify();
            true
        })
    }

    /// 仅改名称再保存，确保未触碰的列表不会在读回表单后被重新拆分或丢失。
    /// @author kongweiguang
    pub fn rename_saved_fixture(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            let input = if panel.active_editor_kind == Some(ConnectionsEditorKind::Local) {
                panel
                    .local_hosts
                    .editor
                    .as_ref()
                    .map(|editor| editor.name.clone())
            } else {
                panel
                    .editor
                    .as_ref()
                    .map(|editor| editor.inputs.name.clone())
            };
            let Some(input) = input else { return false };
            input.update(cx, |input, cx| {
                input.replace_all(name.to_owned(), window, cx)
            });
            cx.notify();
            true
        })
    }

    /// 保存完成还须等待目录从配置仓储重新读取；表单关闭或内存回执不能单独证明持久化。
    /// @author kongweiguang
    pub fn saved_fixture_id(
        &self,
        protocol: Option<RemoteProtocol>,
        name: &str,
        cx: &gpui_kit::App,
    ) -> Option<String> {
        let panel = self.panel.read(cx);
        if panel.saving
            || panel.loading
            || panel.local_hosts.saving
            || panel.local_hosts.loading
            || panel.editor.is_some()
            || panel.local_hosts.editor.is_some()
        {
            return None;
        }
        if let Some(protocol) = protocol {
            panel
                .rows
                .iter()
                .find(|host| host.name == name && host.protocol == protocol)
                .map(|host| host.id.to_string())
        } else {
            panel
                .local_hosts
                .rows
                .iter()
                .find(|host| host.profile.name == name)
                .map(|host| host.profile.id.clone())
        }
    }

    /// 重开使用生产编辑流程：远端取刷新后的配置行，Local 还会按 Profile ID 再执行异步读回。
    /// @author kongweiguang
    pub fn reopen_saved_fixture(
        &mut self,
        protocol: Option<RemoteProtocol>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            if let Some(protocol) = protocol {
                let Some(id) = panel
                    .rows
                    .iter()
                    .find(|host| host.name == name && host.protocol == protocol)
                    .map(|host| host.id.clone())
                else {
                    return false;
                };
                panel.edit_host(&id, window, cx);
            } else {
                let Some(id) = panel
                    .local_hosts
                    .rows
                    .iter()
                    .find(|host| host.profile.name == name)
                    .map(|host| host.profile.id.clone())
                else {
                    return false;
                };
                panel.begin_edit_local_host(id, window, cx);
            }
            true
        })
    }

    /// 同时核对仓储读回和再次提交的列表投影；报告只输出匹配与否，不暴露配置内容。
    /// @author kongweiguang
    pub fn saved_fixture_matches(
        &self,
        protocol: Option<RemoteProtocol>,
        name: &str,
        cx: &gpui_kit::App,
    ) -> bool {
        let panel = self.panel.read(cx);
        if let Some(protocol) = protocol {
            let Some(editor) = panel.editor.as_ref() else {
                return false;
            };
            let host = if protocol == RemoteProtocol::Serial {
                "COM-ACCEPTANCE-01"
            } else {
                "fixture.invalid"
            };
            editor.original.is_some()
                && editor
                    .original
                    .as_ref()
                    .is_some_and(|host| host.tags == fixture_tags())
                && parse_list(
                    editor.inputs.tags.read(cx).value().as_ref(),
                    ListSeparator::Commas,
                )
                .is_ok_and(|tags| tags == fixture_tags())
                && editor.protocol == protocol
                && editor.inputs.name.read(cx).value().as_ref() == name
                && editor.inputs.host.read(cx).value().as_ref() == host
                && (protocol == RemoteProtocol::Serial
                    || editor.inputs.port.read(cx).value().as_ref()
                        == match protocol {
                            RemoteProtocol::Rdp => "3389",
                            RemoteProtocol::Telnet => "23",
                            _ => "22",
                        })
                && (matches!(protocol, RemoteProtocol::Serial | RemoteProtocol::Telnet)
                    || editor.inputs.username.read(cx).value().as_ref() == "acceptance")
                && (!matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
                    || editor.authentication_mode == HostAuthenticationMode::Agent)
                && (protocol != RemoteProtocol::Serial
                    || editor.inputs.serial_baud.read(cx).value().as_ref() == "115200")
        } else {
            panel.local_hosts.editor.as_ref().is_some_and(|editor| {
                editor.revision.is_some()
                    && editor.name.read(cx).value().as_ref() == name
                    && editor.shell.read(cx).value().as_ref() == fixture_shell()
                    && parse_list(editor.args.read(cx).value().as_ref(), ListSeparator::Lines)
                        .is_ok_and(|args| args == fixture_arguments())
                    && editor.env.read(cx).value().as_ref() == fixture_environment()
            })
        }
    }

    /// 只读探测状态不暴露凭据、主机草稿或仓储句柄。
    ///
    /// @author kongweiguang
    pub fn probe_state(
        &self,
        cx: &gpui_kit::App,
    ) -> (
        bool,
        Option<kerminal_application::connections::probe::ConnectionProbeOutcome>,
    ) {
        let panel = self.panel.read(cx);
        (panel.probe_running, panel.probe_outcome.clone())
    }

    /// 取消只能产生中性状态；只回报错误是否存在，避免将诊断正文或输入写入报告。
    /// @author kongweiguang
    pub fn probe_feedback(&self, cx: &gpui_kit::App) -> (bool, bool) {
        let panel = self.panel.read(cx);
        (panel.probe_cancelled, panel.probe_error.is_some())
    }

    /// 只核对公开合成长错误是否完整保留，不把生产错误正文暴露到验收报告。
    /// @author kongweiguang
    pub fn probe_error_content(&self, expected: &str, cx: &gpui_kit::App) -> (bool, usize) {
        let panel = self.panel.read(cx);
        panel.probe_error.as_ref().map_or((false, 0), |error| {
            (error.contains(expected), error.chars().count())
        })
    }

    /// 给隔离验收填入无敏感性的有效 SSH 草稿，使真实测试按钮路径进入受控探测端口。
    ///
    /// @author kongweiguang
    pub fn set_probe_target(
        &mut self,
        host: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let host = host.to_owned();
        self.panel.update(cx, |panel, cx| {
            let Some(editor) = panel.editor.as_mut() else {
                return false;
            };
            editor.inputs.name.update(cx, |input, cx| {
                input.set_value("Acceptance fixture".to_owned(), window, cx)
            });
            editor
                .inputs
                .host
                .update(cx, |input, cx| input.set_value(host, window, cx));
            editor
                .inputs
                .port
                .update(cx, |input, cx| input.set_value("22".to_owned(), window, cx));
            editor.inputs.username.update(cx, |input, cx| {
                input.set_value("acceptance".to_owned(), window, cx)
            });
            panel.set_authentication_mode(
                kerminal_application::connections::management::HostAuthenticationMode::Agent,
                window,
                cx,
            );
            true
        })
    }

    /// 必须发送真实 Change 事件；set_value 仅作初始化，不会触发生产订阅中的在途请求失效。
    ///
    /// @author kongweiguang
    pub fn change_probe_host(
        &mut self,
        host: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let host = host.to_owned();
        self.panel.update(cx, |panel, cx| {
            let Some(editor) = panel.editor.as_ref() else {
                return false;
            };
            editor
                .inputs
                .host
                .update(cx, |input, cx| input.replace_all(host, window, cx));
            true
        })
    }

    /// 只装配可控错误端口，错误状态仍须由真实测试按钮、应用用例和生产回调产生。
    /// @author kongweiguang
    pub fn replace_probe_service(
        &mut self,
        probe: Arc<ConnectionProbeService>,
        cx: &mut Context<Self>,
    ) {
        self.panel.update(cx, |panel, cx| {
            panel.reset_connection_probe();
            panel.probe_service = Some(probe);
            cx.notify();
        });
    }

    /// 复用生产认证与来源选择入口装配截图，不伪装为原生键盘输入或 Radio 点击。
    /// @author kongweiguang
    pub fn prepare_private_key_source(
        &mut self,
        paste: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            panel.set_authentication_mode(HostAuthenticationMode::PrivateKey, window, cx);
            let Some(editor) = panel.editor.as_mut() else {
                return false;
            };
            let mode = if paste {
                PrivateKeyInputMode::Paste
            } else {
                PrivateKeyInputMode::File
            };
            editor.select_private_key_input_mode(mode, window, cx);
            let selected = editor.private_key_input_mode == mode;
            panel.reset_connection_probe();
            cx.notify();
            selected
        })
    }

    /// 只填公开合成字段；代理开关、跳板新增和认证、RDP 模式均由验收的真实鼠标事件改变。
    /// @author kongweiguang
    pub fn prepare_dense_fields(
        &mut self,
        scene: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            let Some(editor) = panel.editor.as_mut() else {
                return false;
            };
            match scene {
                "proxy" if editor.protocol == RemoteProtocol::Ssh && editor.proxy_enabled => {
                    for (input, value) in [
                        (&editor.inputs.proxy_host, "proxy-acceptance.invalid"),
                        (&editor.inputs.proxy_port, "1080"),
                        (&editor.inputs.proxy_username, "acceptance-proxy"),
                    ] {
                        input.update(cx, |input, cx| input.set_value(value, window, cx));
                    }
                }
                "jump-hosts"
                    if editor.protocol == RemoteProtocol::Ssh && editor.jumps.len() == 2 =>
                {
                    for (index, jump) in editor.jumps.iter().enumerate() {
                        let name = format!("第 {} 跳 · 验收样本", index + 1);
                        let host = format!("jump-{}-acceptance.invalid", index + 1);
                        for (input, value) in [
                            (&jump.name, name.as_str()),
                            (&jump.host, host.as_str()),
                            (&jump.port, "22"),
                            (&jump.username, "acceptance"),
                        ] {
                            input.update(cx, |input, cx| {
                                input.set_value(value.to_owned(), window, cx)
                            });
                        }
                    }
                    editor.jumps[1].key_path.update(cx, |input, cx| {
                        input.set_value("C:\\acceptance\\jump-2.pem", window, cx)
                    });
                }
                "rdp-windowed"
                    if editor.protocol == RemoteProtocol::Rdp && !editor.rdp_fullscreen =>
                {
                    for (input, value) in [
                        (&editor.inputs.rdp_width, "1280"),
                        (&editor.inputs.rdp_height, "720"),
                    ] {
                        input.update(cx, |input, cx| input.set_value(value, window, cx));
                    }
                }
                _ => return false,
            }
            cx.notify();
            true
        })
    }

    /// 根据生产跳板实体身份生成已绘制按钮 ID，不假定实体号或复制认证切换逻辑。
    /// @author kongweiguang
    pub fn second_jump_private_key_selector(&self, cx: &gpui_kit::App) -> Option<String> {
        let editor = self.panel.read(cx).editor.as_ref()?;
        let jump = editor.jumps.get(1)?;
        Some(format!(
            "connections-jump-{:?}-auth-PrivateKey",
            jump.host.entity_id()
        ))
    }

    /// 隐藏窗口不依赖原生 focus-in；显式使用生产焦点与滚动句柄定位，报告不得声称验证自动滚动。
    /// @author kongweiguang
    pub fn position_dense_jumps(
        &mut self,
        position: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.panel.update(cx, |panel, cx| {
            let Some(editor) = panel.editor.as_ref() else {
                return false;
            };
            let Some(jump) = editor.jumps.get(1) else {
                return false;
            };
            let focus = match position {
                "top" | "second-auth" => panel.editor_fields_focus.clone(),
                "lower" if jump.authentication_mode == HostAuthenticationMode::PrivateKey => {
                    jump.passphrase.read(cx).focus_handle(cx)
                }
                _ => return false,
            };
            window.focus(&focus, cx);
            if position == "top" {
                editor.scroll_handle.set_offset(Default::default());
            } else {
                editor.scroll_handle.scroll_to_bottom();
            }
            cx.notify();
            true
        })
    }

    /// 读取生产状态与真实输入 bounds；底部截图必须能证明第二跳可见且拥有焦点。
    /// @author kongweiguang
    pub fn dense_fixture_state(
        &self,
        window: &Window,
        cx: &gpui_kit::App,
    ) -> Option<DenseFixtureState> {
        let editor = self.panel.read(cx).editor.as_ref()?;
        let second_jump = editor.jumps.get(1);
        let second_jump_private_key = second_jump
            .is_some_and(|jump| jump.authentication_mode == HostAuthenticationMode::PrivateKey);
        let second_jump_lower_focused = second_jump
            .is_some_and(|jump| jump.passphrase.read(cx).focus_handle(cx).is_focused(window));
        let second_jump_lower_visible = second_jump.is_some_and(|jump| {
            let bounds = jump.passphrase.read(cx).input_bounds();
            let viewport = editor.scroll_handle.bounds();
            bounds.size.width > gpui_kit::px(0.)
                && bounds.size.height > gpui_kit::px(0.)
                && bounds.left() >= viewport.left()
                && bounds.right() <= viewport.right()
                && bounds.top() >= viewport.top()
                && bounds.bottom() <= viewport.bottom()
        });
        let prepared_values_match = match editor.section {
            HostEditorSection::Proxy => {
                editor.inputs.proxy_host.read(cx).value().as_ref() == "proxy-acceptance.invalid"
                    && editor.inputs.proxy_port.read(cx).value().as_ref() == "1080"
                    && editor.inputs.proxy_username.read(cx).value().as_ref() == "acceptance-proxy"
            }
            HostEditorSection::JumpHosts => {
                editor.jumps.len() == 2
                    && editor.jumps.iter().enumerate().all(|(index, jump)| {
                        jump.name.read(cx).value().as_ref()
                            == format!("第 {} 跳 · 验收样本", index + 1)
                            && jump.host.read(cx).value().as_ref()
                                == format!("jump-{}-acceptance.invalid", index + 1)
                            && jump.port.read(cx).value().as_ref() == "22"
                            && jump.username.read(cx).value().as_ref() == "acceptance"
                    })
                    && second_jump.is_some_and(|jump| {
                        jump.key_path.read(cx).value().as_ref() == "C:\\acceptance\\jump-2.pem"
                    })
            }
            HostEditorSection::Display => {
                editor.inputs.rdp_width.read(cx).value().as_ref() == "1280"
                    && editor.inputs.rdp_height.read(cx).value().as_ref() == "720"
            }
            _ => false,
        };
        Some(DenseFixtureState {
            proxy_enabled: editor.proxy_enabled,
            jump_count: editor.jumps.len(),
            second_jump_private_key,
            rdp_windowed: !editor.rdp_fullscreen,
            prepared_values_match,
            scroll_offset_y: editor.scroll_handle.offset().y.as_f32(),
            second_jump_lower_focused,
            second_jump_lower_visible,
        })
    }

    /// 通过生产面板入口启动测试，覆盖相同的校验、取消令牌与结果状态机。
    ///
    /// @author kongweiguang
    pub fn start_probe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.test_connection(window, cx));
    }

    /// 验收只调用面板的取消入口，保证旧请求失效规则与用户点击一致。
    ///
    /// @author kongweiguang
    pub fn cancel_probe(&mut self, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.cancel_connection_probe(cx));
    }
}

/// 报告使用稳定英文键；串口沿用 Terminal 枚举，但必须与 SSH 终端分类区分。
/// @author kongweiguang
fn section_name(section: HostEditorSection, protocol: RemoteProtocol) -> &'static str {
    match section {
        HostEditorSection::Properties | HostEditorSection::Authentication => "general",
        HostEditorSection::Proxy => "proxy",
        HostEditorSection::JumpHosts => "jump-hosts",
        HostEditorSection::Terminal if protocol == RemoteProtocol::Serial => "serial",
        HostEditorSection::Terminal => "terminal",
        HostEditorSection::Network => "network",
        HostEditorSection::Transfer => "files",
        HostEditorSection::Display => "display",
    }
}

/// 这里只保存平台上已知的 Shell 路径，不执行程序，也不依赖用户默认 Profile。
/// @author kongweiguang
fn fixture_shell() -> &'static str {
    if cfg!(target_os = "windows") {
        "C:\\Windows\\System32\\cmd.exe"
    } else {
        "/bin/sh"
    }
}

/// 使用空参数、多行脚本及 Windows 字面路径检验无损往返；验收不执行该合成命令。
/// @author kongweiguang
fn fixture_arguments() -> Vec<String> {
    [
        "",
        "-Command",
        "Write-Output 1\nWrite-Output 2",
        r"C:\temp\new",
        "\"literal quote\"",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// 标签数组同时覆盖分隔符、字面引号及合法空项，避免仅测普通标签漏掉投影错误。
/// @author kongweiguang
fn fixture_tags() -> Vec<String> {
    ["ops,prod", "\"quoted\"", "中文", ""]
        .map(str::to_owned)
        .to_vec()
}

/// 环境样本的值中含换行与等号；若逐行拆分将多出变量，真实 Vault 读回可发现变化。
/// @author kongweiguang
fn fixture_environment() -> &'static str {
    "\"ACCEPTANCE_MULTILINE=first\nEXTRA=second\"\nACCEPTANCE_PATH=C:\\temp\\new"
}

impl Render for HostEditorAcceptanceState {
    /// 验收窗口嵌入生产持有的面板实体，避免截图与用户实际编辑路径分叉。
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}
