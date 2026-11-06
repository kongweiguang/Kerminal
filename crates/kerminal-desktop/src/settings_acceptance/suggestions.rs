//! 在私有桌面验证真实 Shell、生产建议视图与键盘、鼠标接受及焦点闭环。
//!
//! @author kongweiguang

use kerminal_application::{
    connections::{
        credentials::{SecretValue, VaultKeyAvailability},
        management::{HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft},
    },
    sessions::{ShellEditingShell, ShellEditingSnapshot},
};
use kerminal_domain::{
    commands::{
        CommandHistoryListRequest, CommandHistoryRecordRequest, CommandHistorySource,
        CommandHistoryTarget,
    },
    connections::{HostEndpoint, RemoteProtocol, SshAuthentication},
    identity::HostId,
};
use kerminal_ui::SuggestionsAcceptanceState;

use super::*;

const PREFIX: &str = "zqkmr";
const COMPLETE: &str = "zqkmrsuggestionfixture";
const SUFFIX: &str = "suggestionfixture";
const SSH_FIXTURE_HOST_ID: &str = "suggestions-ssh-fixture";
const SSH_FIXTURE_ENV: &str = "KERMINAL_SUGGESTIONS_ACCEPTANCE_SSH";

/// 验收端点与两份凭据路径按名称关联，避免相同 PathBuf 类型在装配时交换含义。
struct SshFixtureInputs {
    endpoint: HostEndpoint,
    private_key: PathBuf,
    known_hosts: PathBuf,
}

/// 显式 fixture 输入只能指向仓库 target 下的专用目录，端点限定 IPv4 loopback。
/// 不接受真实主机、用户 SSH 配置或验收目录外的密钥文件。
/// PowerShell/.NET 可把关闭开关保留为空环境值；空值与未设置都保持本地验收。
///
/// @author kongweiguang
fn ssh_fixture_inputs() -> Result<Option<SshFixtureInputs>, Box<dyn Error>> {
    match std::env::var(SSH_FIXTURE_ENV) {
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Ok(value) if value.is_empty() => return Ok(None),
        Ok(value) if value == "1" => {}
        _ => return Err(io::Error::other("invalid SSH suggestion acceptance mode").into()),
    }
    let target: std::net::SocketAddr = std::env::var("KERM_ACCEPT_SSH_TARGET")?.parse()?;
    if target.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) || target.port() == 0 {
        return Err(io::Error::other("SSH acceptance target must be IPv4 loopback").into());
    }
    let username = std::env::var("KERM_ACCEPT_SSH_USERNAME")?;
    if username != "root" {
        return Err(io::Error::other("SSH acceptance requires the private fixture account").into());
    }
    let allowed = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/diagnostics/command-suggestions-ssh-fixtures")
        .canonicalize()?;
    let root = PathBuf::from(
        std::env::var_os("KERMINAL_SUGGESTIONS_SSH_FIXTURE_ROOT")
            .ok_or_else(|| io::Error::other("SSH fixture root is missing"))?,
    )
    .canonicalize()?;
    if root == allowed || !root.starts_with(&allowed) {
        return Err(io::Error::other("SSH fixture root is outside its isolated directory").into());
    }
    let validate = |variable: &str, limit: u64| -> Result<PathBuf, Box<dyn Error>> {
        let path = PathBuf::from(
            std::env::var_os(variable)
                .ok_or_else(|| io::Error::other("SSH fixture input is missing"))?,
        );
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() == 0
            || metadata.len() > limit
        {
            return Err(io::Error::other("SSH fixture input is not a bounded regular file").into());
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&root) {
            return Err(io::Error::other("SSH fixture input is outside its private root").into());
        }
        Ok(canonical)
    };
    let key = validate("KERM_ACCEPT_SSH_PRIVATE_KEY_FILE", 64 * 1024)?;
    let known_hosts = validate("KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE", 1024 * 1024)?;
    let known = fs::read_to_string(&known_hosts)?;
    let mut lines = known.lines().filter(|line| !line.trim().is_empty());
    let expected = format!("[127.0.0.1]:{} ssh-ed25519 ", target.port());
    if !lines.next().is_some_and(|line| line.starts_with(&expected)) || lines.next().is_some() {
        return Err(
            io::Error::other("SSH fixture host-key pin does not match its endpoint").into(),
        );
    }
    Ok(Some(SshFixtureInputs {
        endpoint: HostEndpoint {
            host: "127.0.0.1".to_owned(),
            port: target.port(),
            username,
        },
        private_key: key,
        known_hosts,
    }))
}

/// 使用应用的 vault 与主机用例登记一次性密钥，并固定本次 fixture 的 host key。
/// 任何失败均停止验收，不尝试覆盖已有配置或回退到真实主机。
///
/// @author kongweiguang
async fn prepare_ssh_fixture(
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
) -> Result<Option<HostId>, Box<dyn Error>> {
    let Some(SshFixtureInputs {
        endpoint,
        private_key,
        known_hosts,
    }) = ssh_fixture_inputs()?
    else {
        return Ok(None);
    };
    let destination = context.output_directory.join("config").join("known_hosts");
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut fs::File::open(known_hosts)?, &mut output)?;
    output.sync_all()?;
    drop(output);
    let key_text = fs::read_to_string(private_key)?;
    let secret = SecretValue::new(key_text);
    let host_id = HostId::parse(SSH_FIXTURE_HOST_ID)?;
    let host_service = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("SSH fixture requires host management"))?;
    let vault = services
        .vault_keys
        .clone()
        .ok_or_else(|| io::Error::other("SSH fixture requires an isolated vault"))?;
    let draft = HostDraft {
        id: Some(host_id.clone()),
        group_id: None,
        name: "Suggestion SSH acceptance".to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint,
        authentication: Some(SshAuthentication::PrivateKey {
            path: None,
            secret: None,
            passphrase: None,
        }),
        authentication_mode: HostAuthenticationMode::PrivateKey,
        rdp_password: None,
        route: Default::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 20_000,
        keepalive_interval_ms: None,
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
    };
    services
        .executor
        .run(move |cancellation| async move {
            match vault.ensure().await? {
                VaultKeyAvailability::Ready { .. } | VaultKeyAvailability::Created { .. } => {}
                _ => {
                    return Err(kerminal_application::AppError::infrastructure(
                        "isolated fixture vault unavailable",
                    ));
                }
            }
            host_service
                .save_host_with_credentials(
                    draft,
                    None,
                    vec![HostCredentialChange::Replace {
                        slot: HostCredentialSlot::TargetPrivateKey,
                        secret,
                    }],
                    cancellation,
                )
                .await?;
            Ok(())
        })
        .await?;
    Ok(Some(host_id))
}

/// 仅在独立配置根写入历史样本；策略跳过不是有效的准备结果。
///
/// @author kongweiguang
async fn seed_history(
    services: &kerminal_ui::AppServices,
    host_id: Option<&HostId>,
) -> Result<(), Box<dyn Error>> {
    let commands = services
        .commands
        .clone()
        .ok_or_else(|| io::Error::other("suggestion acceptance requires command history"))?;
    let remote_host_id = host_id.cloned();
    let target = if remote_host_id.is_some() {
        CommandHistoryTarget::Ssh
    } else {
        CommandHistoryTarget::Local
    };
    let result = services
        .executor
        .run(move |_| async move {
            commands
                .record_history(CommandHistoryRecordRequest {
                    command: COMPLETE.to_owned(),
                    source: CommandHistorySource::User,
                    target,
                    record: Some(true),
                    session_id: None,
                    pane_id: None,
                    tab_id: None,
                    profile_id: None,
                    remote_host_id,
                    cwd: None,
                    shell: None,
                })
                .await
        })
        .await?;
    if !result.recorded
        || result
            .entry
            .as_ref()
            .is_none_or(|entry| entry.command != COMPLETE)
    {
        return Err(
            io::Error::other("isolated suggestion history fixture was not recorded").into(),
        );
    }
    Ok(())
}

/// 先绘制真实帧再读取当前 pane，避免观察到上一次布局或失效的输入绑定。
///
/// @author kongweiguang
fn state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<Option<SuggestionsAcceptanceState>, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        window.refresh();
        draw_acceptance_frame(window, app);
        root.update(app, |root, cx| {
            root.suggestions_acceptance_state(window, cx)
        })
    })?)
}

/// 只等待真实生产状态，超时诊断不输出终端输入或用户路径。
///
/// @author kongweiguang
async fn wait_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    condition: impl Fn(&SuggestionsAcceptanceState) -> bool,
) -> Result<SuggestionsAcceptanceState, Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let current = state(cx, root, window)?;
        if current.as_ref().is_some_and(&condition) {
            return Ok(current.expect("condition requires a state"));
        }
        if Instant::now() >= deadline {
            let missing = window.update(cx, |_, window, app| {
                root.read(app).suggestions_acceptance_diagnostic(window)
            })?;
            let detail = current.as_ref().map(|state| {
                (
                    state.focused,
                    state.input_idle,
                    state.context.is_some(),
                    state.menu_state,
                )
            });
            return Err(io::Error::other(format!(
                "suggestion acceptance timed out at {stage}; state={detail:?}; readiness={missing}"
            ))
            .into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

/// 通过真实 GPUI 文本提交逐字输入；返回首次派发时间，排除此前准备帧对输入延迟的干扰。
///
/// @author kongweiguang
fn type_text(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    text: &str,
) -> Result<Instant, Box<dyn Error>> {
    let started = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let started = Instant::now();
        for character in text.chars() {
            let key = character.to_string();
            window.dispatch_keystroke(
                Keystroke {
                    modifiers: gpui_pre::Modifiers::default(),
                    key: key.clone(),
                    key_char: Some(key),
                },
                app,
            );
        }
        Ok::<_, io::Error>(started)
    })??;
    Ok(started)
}

/// 一次窗口事件批次中退格，防止自动探测插入逐键操作之间而污染验收时序。
///
/// @author kongweiguang
fn erase_suffix(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<(), Box<dyn Error>> {
    let key = Keystroke::parse("backspace")
        .map_err(|error| io::Error::other(format!("invalid backspace key: {error}")))?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        for _ in 0..SUFFIX.chars().count() {
            window.dispatch_keystroke(key.clone(), app);
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 菜单进入完成态证明该代生产快照已返回，避免验收读回与 UI 的探测在途竞争。
///
/// @author kongweiguang
async fn open_menu(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<SuggestionsAcceptanceState, Box<dyn Error>> {
    press_key(cx, window, "alt-/")?;
    let state = wait_state(cx, root, window, "menu query", |state| {
        state.focused
            && state.input_idle
            && matches!(
                state.menu_state,
                Some("ready" | "empty" | "failed" | "unavailable" | "input-too-long")
            )
    })
    .await?;
    if matches!(
        state.menu_state,
        Some("failed" | "unavailable" | "input-too-long")
    ) {
        return Err(io::Error::other("production suggestion menu query failed").into());
    }
    Ok(state)
}

/// 只在生产菜单已完成快照请求且 FIFO 空闲时发起一次独立读回；任何竞争都显式失败。
///
/// @author kongweiguang
async fn read_shell(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    before: &SuggestionsAcceptanceState,
) -> Result<ShellEditingSnapshot, Box<dyn Error>> {
    if !before.input_idle || !matches!(before.menu_state, Some("ready" | "empty")) {
        return Err(io::Error::other("Shell readback requires a settled production menu").into());
    }
    let binding = before.binding.clone();
    let sessions = services.sessions.clone();
    let snapshot = services
        .executor
        .run(move |cancellation| async move {
            sessions
                .request_shell_editing_snapshot(binding, cancellation)
                .await
        })
        .await?;
    let after = state(cx, root, window)?
        .ok_or_else(|| io::Error::other("active pane disappeared during Shell readback"))?;
    if after.binding != before.binding
        || !after.focused
        || !after.input_idle
        || after.context.as_ref().map(|context| context.version()) != Some(snapshot.context)
        || snapshot.binding != before.binding
    {
        return Err(io::Error::other("Shell readback no longer matches the active pane").into());
    }
    Ok(snapshot)
}

/// 首次 handler 可先于 Shell prompt 安装；就绪等待读取服务状态，不等待尚未发生输入 ACK 的 UI 缓存。
async fn wait_initial_prompt(
    services: &kerminal_ui::AppServices,
    initial: &SuggestionsAcceptanceState,
) -> Result<(), Box<dyn Error>> {
    let session_id = initial.binding.session_id.clone();
    let sessions = services.sessions.clone();
    let context = services
        .executor
        .run(move |_| async move {
            let deadline = tokio::time::Instant::now() + ACTION_TIMEOUT;
            loop {
                let context = sessions
                    .terminal_command_context(session_id.clone())
                    .await?;
                if context.prompt_ready || tokio::time::Instant::now() >= deadline {
                    return Ok(context);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?;
    if !context.prompt_ready {
        return Err(io::Error::other(format!(
            "initial Shell prompt unavailable: {:?}",
            context.availability
        ))
        .into());
    }
    Ok(())
}

/// 三主题在同一个真实候选状态下截图，并对光标和 pane 几何进行断言。
/// 同帧网格度量留给 PNG 墨迹检查，避免把节点位置误当成文字基线的验收证据。
///
/// @author kongweiguang
async fn capture_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    menu: bool,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<(), Box<dyn Error>> {
    for (mode, name, _) in THEMES {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            });
        })?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.rendered_terminal_palette() == Some(state.terminal_palette())
        })
        .await?;
        let current = wait_state(cx, root, window, "theme overlay", |state| {
            state.focused
                && state.input_idle
                && if menu {
                    state.menu_state == Some("ready")
                        && state
                            .menu_candidates
                            .first()
                            .is_some_and(|item| item == COMPLETE)
                } else {
                    state.ghost.as_deref() == Some(SUFFIX)
                }
        })
        .await?;
        let selector = if menu {
            "terminal-command-suggestion-menu".to_owned()
        } else {
            format!("suggestion-ghost-{}", current.pane_id)
        };
        window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let actual_dark = gpui_kit::component::Theme::global(app).is_dark();
            let expected_dark = match mode {
                ThemeMode::Light => false,
                ThemeMode::Dark => true,
                ThemeMode::System => matches!(
                    window.appearance(),
                    gpui_pre::WindowAppearance::Dark | gpui_pre::WindowAppearance::VibrantDark
                ),
            };
            let bounds = selector_bounds(window, &selector)?;
            if actual_dark != expected_dark || !bounds_inside_viewport(bounds, current.pane_bounds)
            {
                return Err(io::Error::other(format!(
                    "suggestion theme or pane containment mismatch: menu={menu}, theme={name}, dark={actual_dark}/{expected_dark}, overlay={bounds:?}, pane={:?}",
                    current.pane_bounds
                )));
            }
            if menu {
                let top = bounds.origin.y.as_f32();
                let bottom = top + bounds.size.height.as_f32();
                if top + 1.0 < current.cursor_y + current.line_height
                    && bottom > current.cursor_y + 1.0
                {
                    return Err(io::Error::other("suggestion menu overlaps its cursor line"));
                }
            } else if (bounds.origin.x.as_f32() - current.cursor_x).abs() > 1.0
                || (bounds.origin.y.as_f32() - current.cursor_y).abs() > 1.0
            {
                return Err(io::Error::other(
                    "suggestion ghost is not anchored to the cursor",
                ));
            }
            Ok::<_, io::Error>(())
        })??;
        let kind = if menu { "menu" } else { "ghost" };
        screenshots.push(save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("suggestions-{kind}-{name}.png")),
        )?);
        if !menu {
            // 保留同帧真实网格度量，供 PNG 字形墨迹回归使用；节点边框对齐不能证明文字基线对齐。
            let scale = window.update(cx, |_, window, _| window.scale_factor())?;
            let geometry = serde_json::json!({
                "cursorX": current.cursor_x,
                "cursorY": current.cursor_y,
                "cellWidth": current.cell_width,
                "lineHeight": current.line_height,
                "paneRight": current.pane_bounds.right().as_f32(),
                "scaleFactor": scale,
                "prefix": PREFIX,
                "suffix": SUFFIX,
            });
            fs::write(
                context
                    .output_directory
                    .join(format!("suggestions-ghost-{name}-geometry.json")),
                serde_json::to_vec_pretty(&geometry)?,
            )?;
        }
    }
    Ok(())
}

/// 从真实键盘派发到当前帧 ghost 布局采样；独立标明隐藏窗口与轮询边界，不冒充物理显示延迟。
async fn measure_warm_latency(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<Vec<f64>, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(20);
    for _ in 0..20 {
        press_key(cx, window, "backspace")?;
        wait_state(cx, root, window, "latency prefix setup", |state| {
            state.input_idle && state.ghost.as_deref() == Some("rsuggestionfixture")
        })
        .await?;
        let started = type_text(cx, window, "r")?;
        wait_state(cx, root, window, "latency ghost", |state| {
            state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
        })
        .await?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(samples)
}

/// 原生 Tab 与左右移动必须经过生产按键路由；菜单只用于读回，不能替 Shell 修复光标或输入。
/// 失败时先只读能力状态，再回读 Shell；诊断只比较绑定和输出安全元数据，不记录输入正文。
///
/// @author kongweiguang
async fn verify_native_edit_recovery(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    prompt_epoch: u64,
    baseline: &ShellEditingSnapshot,
) -> Result<(), Box<dyn Error>> {
    press_key(cx, window, "tab")?;
    let recovered = wait_state(cx, root, window, "native Tab recovery", |state| {
        state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await;
    if let Err(error) = recovered {
        let before =
            state(cx, root, window)?.ok_or_else(|| io::Error::other("Tab pane missing"))?;
        let same_binding = before.binding == baseline.binding;
        let same_session_generation =
            before.binding.session_generation == baseline.binding.session_generation;
        let availability_binding = before.binding.clone();
        let sessions = services.sessions.clone();
        let availability = services
            .executor
            .run(move |_| async move {
                sessions
                    .shell_editing_availability(availability_binding)
                    .await
            })
            .await
            .map_err(|error| error.public_code());
        let sessions = services.sessions.clone();
        let result = services
            .executor
            .run(move |cancellation| async move {
                sessions
                    .request_shell_editing_snapshot(before.binding, cancellation)
                    .await
            })
            .await;
        let detail = result.map(|snapshot| {
            (
                snapshot.input.chars().count(),
                snapshot.cursor,
                snapshot.input.chars().any(char::is_control),
                snapshot.input == PREFIX,
            )
        });
        return Err(io::Error::other(format!(
            "{error}; baseline_shell={:?}, same_binding={same_binding}, same_session_generation={same_session_generation}, shell_editing_availability={availability:?}; Tab readback metadata={detail:?}",
            baseline.shell,
        ))
        .into());
    }
    let menu = open_menu(cx, root, window).await?;
    let snapshot = read_shell(cx, root, window, services, &menu).await?;
    if snapshot.input != PREFIX
        || !snapshot.cursor_at_end()
        || snapshot.context.prompt_epoch != prompt_epoch
    {
        return Err(
            io::Error::other("native Tab changed the unmatched fixture or submitted it").into(),
        );
    }
    press_key(cx, window, "escape")?;
    press_key(cx, window, "left")?;
    let menu = open_menu(cx, root, window).await?;
    let snapshot = read_shell(cx, root, window, services, &menu).await?;
    if snapshot.input != PREFIX
        || snapshot.cursor + 1 != PREFIX.chars().count()
        || snapshot.context.prompt_epoch != prompt_epoch
        || !menu.menu_candidates.is_empty()
        || menu.ghost.is_some()
    {
        return Err(io::Error::other("mid-line editing did not preserve Shell ownership").into());
    }
    press_key(cx, window, "escape")?;
    press_key(cx, window, "right")?;
    wait_state(cx, root, window, "cursor recovery", |state| {
        state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    Ok(())
}

/// 鼠标必须经过真实 hit-test 与事件传播；关闭和接受后继续输入可证明终端焦点没有被按钮偷走。
async fn verify_pointer_interactions(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    prompt_epoch: u64,
) -> Result<(), Box<dyn Error>> {
    open_menu(cx, root, window).await?;
    click_selector(cx, window, "suggestion-menu-dismiss")?;
    wait_state(cx, root, window, "mouse dismiss retains focus", |state| {
        state.focused && state.input_idle && state.menu_state.is_none() && state.ghost.is_none()
    })
    .await?;
    press_key(cx, window, "backspace")?;
    wait_state(
        cx,
        root,
        window,
        "input changes after mouse dismiss",
        |state| {
            state.focused
                && state.input_idle
                && state.ghost.as_deref() == Some("rsuggestionfixture")
        },
    )
    .await?;
    type_text(cx, window, "r")?;
    wait_state(cx, root, window, "typing after mouse dismiss", |state| {
        state.focused && state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    let menu = open_menu(cx, root, window).await?;
    let snapshot = read_shell(cx, root, window, services, &menu).await?;
    if snapshot.input != PREFIX
        || !snapshot.cursor_at_end()
        || snapshot.context.prompt_epoch != prompt_epoch
        || menu
            .menu_candidates
            .first()
            .is_none_or(|candidate| candidate != COMPLETE)
    {
        return Err(
            io::Error::other("mouse dismiss lost input or changed the selected fixture").into(),
        );
    }
    let generation = snapshot.context.input_generation;
    click_selector(cx, window, "suggestion-menu-candidate-0")?;
    wait_state(
        cx,
        root,
        window,
        "mouse acceptance retains focus",
        |state| {
            state.focused
                && state.input_idle
                && state.menu_state.is_none()
                && state.context.as_ref().is_some_and(|context| {
                    context.prompt_epoch == prompt_epoch && context.input_generation > generation
                })
        },
    )
    .await?;
    let menu = open_menu(cx, root, window).await?;
    let accepted = read_shell(cx, root, window, services, &menu).await?;
    if accepted.input != COMPLETE
        || !accepted.cursor_at_end()
        || accepted.context.prompt_epoch != prompt_epoch
    {
        return Err(
            io::Error::other("mouse acceptance executed or did not append exactly once").into(),
        );
    }
    press_key(cx, window, "escape")?;
    type_text(cx, window, "x")?;
    let menu = open_menu(cx, root, window).await?;
    let continued = read_shell(cx, root, window, services, &menu).await?;
    if continued.input != format!("{COMPLETE}x")
        || !continued.cursor_at_end()
        || continued.context.prompt_epoch != prompt_epoch
    {
        return Err(
            io::Error::other("typing after mouse acceptance did not reach the same Shell").into(),
        );
    }
    press_key(cx, window, "escape")?;
    press_key(cx, window, "backspace")?;
    erase_suffix(cx, window)?;
    wait_state(cx, root, window, "pointer fixture restored", |state| {
        state.focused && state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    Ok(())
}

/// 固定复现用户的 Docker 前缀，先等自动 ghost 再查菜单，防止只覆盖主动触发而漏掉原始故障。
/// 只查询内置规格、核对 Shell 输入，不执行任何 Docker 命令。
///
/// @author kongweiguang
async fn verify_ssh_docker_suggestions(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    previous: &SuggestionsAcceptanceState,
    prompt_epoch: u64,
    context: &AcceptanceContext,
) -> Result<Vec<CaptureRecord>, Box<dyn Error>> {
    let mut screenshots = Vec::new();
    for _ in 0..PREFIX.chars().count() {
        press_key(cx, window, "backspace")?;
    }
    type_text(cx, window, "docker ")?;
    wait_state(
        cx,
        root,
        window,
        "SSH docker space automatic ghost",
        |state| {
            state.ssh_fixture
                && state.binding == previous.binding
                && state.input_idle
                && state.ghost.as_ref().is_some_and(|ghost| !ghost.is_empty())
        },
    )
    .await?;
    screenshots.push(save_frame(
        cx,
        window,
        context
            .output_directory
            .join("suggestions-docker-ghost.png"),
    )?);
    open_menu(cx, root, window).await?;
    let menu = wait_state(cx, root, window, "SSH docker space candidates", |state| {
        state.ssh_fixture
            && state.binding == previous.binding
            && state.input_idle
            && state.menu_state == Some("ready")
            && state
                .menu_candidates
                .iter()
                .any(|candidate| candidate == "ps")
    })
    .await?;
    let snapshot = read_shell(cx, root, window, services, &menu).await?;
    if snapshot.input != "docker "
        || !snapshot.cursor_at_end()
        || snapshot.context.prompt_epoch != prompt_epoch
    {
        return Err(io::Error::other("SSH docker-space query changed the prompt or input").into());
    }
    screenshots.push(save_frame(
        cx,
        window,
        context.output_directory.join("suggestions-docker-menu.png"),
    )?);
    press_key(cx, window, "escape")?;
    type_text(cx, window, "p")?;
    wait_state(cx, root, window, "SSH docker p automatic ghost", |state| {
        state.ssh_fixture
            && state.binding == previous.binding
            && state.input_idle
            && matches!(state.ghost.as_deref(), Some("s" | "ull" | "ush"))
    })
    .await?;
    open_menu(cx, root, window).await?;
    let menu = wait_state(cx, root, window, "SSH docker p candidate", |state| {
        state.ssh_fixture
            && state.binding == previous.binding
            && state.input_idle
            && state.menu_state == Some("ready")
            && state
                .menu_candidates
                .iter()
                .any(|candidate| candidate == "ps")
    })
    .await?;
    let snapshot = read_shell(cx, root, window, services, &menu).await?;
    if snapshot.input != "docker p"
        || !snapshot.cursor_at_end()
        || snapshot.context.prompt_epoch != prompt_epoch
    {
        return Err(io::Error::other("SSH docker-p query changed the prompt or input").into());
    }
    press_key(cx, window, "escape")?;
    for _ in 0.."docker p".chars().count() {
        press_key(cx, window, "backspace")?;
    }
    type_text(cx, window, PREFIX)?;
    wait_state(cx, root, window, "SSH history fixture restored", |state| {
        state.ssh_fixture
            && state.binding == previous.binding
            && state.input_idle
            && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    press_key(cx, window, "escape")?;
    Ok(screenshots)
}

/// 只在隔离 Bash 中执行无副作用的 `:` 命令，等待真实历史落盘后验证下一提示符恢复。
///
/// @author kongweiguang
async fn verify_next_ssh_prompt(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    host_id: &HostId,
    previous: &SuggestionsAcceptanceState,
    old_epoch: u64,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..PREFIX.chars().count() {
        press_key(cx, window, "backspace")?;
    }
    let command = format!(": {COMPLETE}");
    type_text(cx, window, &command)?;
    let menu = open_menu(cx, root, window).await?;
    let before_execution = read_shell(cx, root, window, services, &menu).await?;
    if before_execution.input != command || before_execution.context.prompt_epoch != old_epoch {
        return Err(
            io::Error::other("SSH fixture command was not staged at the original prompt").into(),
        );
    }
    press_key(cx, window, "escape")?;
    press_key(cx, window, "enter")?;
    let sessions = services.sessions.clone();
    let session_id = previous.binding.session_id.clone();
    services
        .executor
        .run(move |_| async move {
            let deadline = tokio::time::Instant::now() + ACTION_TIMEOUT;
            loop {
                let context = sessions
                    .terminal_command_context(session_id.clone())
                    .await?;
                if context.prompt_ready && context.prompt_epoch > old_epoch {
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(kerminal_application::AppError::infrastructure(
                        "SSH next prompt did not become ready",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?;
    let commands = services
        .commands
        .clone()
        .ok_or_else(|| io::Error::other("SSH fixture requires command history"))?;
    let history_host = host_id.clone();
    services
        .executor
        .run(move |_| async move {
            let deadline = tokio::time::Instant::now() + ACTION_TIMEOUT;
            loop {
                let entries = commands
                    .list_history(CommandHistoryListRequest {
                        query: Some(command.clone()),
                        target: Some(CommandHistoryTarget::Ssh),
                        remote_host_id: Some(history_host.clone()),
                        ..Default::default()
                    })
                    .await?;
                if entries.iter().any(|entry| entry.command == command) {
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(kerminal_application::AppError::infrastructure(
                        "SSH fixture command was not recorded",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?;
    let next_prefix = format!(": {PREFIX}");
    type_text(cx, window, &next_prefix)?;
    let next = wait_state(cx, root, window, "SSH next prompt ghost", |state| {
        state.ssh_fixture
            && state.binding == previous.binding
            && state.input_idle
            && state.ghost.as_deref() == Some(SUFFIX)
            && state
                .context
                .as_ref()
                .is_some_and(|context| context.prompt_epoch > old_epoch)
    })
    .await?;
    let menu = open_menu(cx, root, window).await?;
    let snapshot = read_shell(cx, root, window, services, &menu).await?;
    if next.binding != menu.binding
        || snapshot.input != next_prefix
        || !snapshot.cursor_at_end()
        || snapshot.context.prompt_epoch <= old_epoch
    {
        return Err(io::Error::other("SSH suggestion did not recover on the next prompt").into());
    }
    press_key(cx, window, "escape")?;
    Ok(())
}

/// 连接就绪使用启动预算而非输入交互预算，保留生产创建回执以区分拒绝、连接失败与尚未就绪。
/// 只观察本轮隔离配置中的状态，不重试连接或切换为其它目标。
///
/// @author kongweiguang
async fn wait_fixture_pane(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    ssh: bool,
) -> Result<SuggestionsAcceptanceState, Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last_status = None;
    loop {
        let current = state(cx, root, window)?;
        if current
            .as_ref()
            .is_some_and(|state| state.input_idle && state.ssh_fixture == ssh)
        {
            return Ok(current.expect("ready fixture pane"));
        }
        let (busy, owner, status) = window.update(cx, |_, _, app| {
            root.read(app)
                .workbench_acceptance_terminal_creation_state()
        })?;
        if status.is_some() {
            last_status = status;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "fixture pane unavailable: ssh={ssh}, creating={busy}, owner={owner}, status={last_status:?}"
            ))
            .into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
}

/// 通过真实输入和 Shell 回读验证追加与未提交；任何阶段失败都不生成通过报告。
/// 基线快照仅用于关联同一 prompt 和失败时的绑定诊断，不输出输入正文。
///
/// @author kongweiguang
pub(super) async fn capture_only(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
) -> Result<(), Box<dyn Error>> {
    let ssh_host_id = prepare_ssh_fixture(services, context).await?;
    seed_history(services, ssh_host_id.as_ref()).await?;
    if let Some(host_id) = ssh_host_id.as_ref() {
        let requested = window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.suggestions_acceptance_open_ssh(host_id.clone(), window, cx)
            })
        })?;
        if !requested {
            return Err(io::Error::other("SSH fixture terminal creation was not accepted").into());
        }
    }
    let initial = wait_fixture_pane(cx, root, window, ssh_host_id.is_some()).await?;
    wait_initial_prompt(services, &initial).await?;
    // 已聚焦的空 prompt 不再点击画布中心：该手势会合法创建选区，菜单必须为选区让位。
    if !initial.focused {
        click_selector(
            cx,
            window,
            &format!("workspace-terminal-canvas-{}", initial.pane_id),
        )?;
        press_key(cx, window, "escape")?;
    }
    wait_state(cx, root, window, "terminal focus", |state| {
        state.focused && state.input_idle
    })
    .await?;
    let baseline_menu = open_menu(cx, root, window).await?;
    let baseline = read_shell(cx, root, window, services, &baseline_menu).await?;
    if !baseline.input.is_empty() || !baseline.cursor_at_end() {
        return Err(io::Error::other("isolated Shell did not start at an empty prompt").into());
    }
    if ssh_host_id.is_some() && baseline.shell != ShellEditingShell::Bash {
        return Err(io::Error::other("SSH fixture did not start Bash").into());
    }
    press_key(cx, window, "escape")?;
    let prompt_epoch = baseline.context.prompt_epoch;
    type_text(cx, window, PREFIX)?;
    wait_state(cx, root, window, "inline ghost", |state| {
        state.focused && state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    verify_native_edit_recovery(cx, root, window, services, prompt_epoch, &baseline).await?;
    verify_pointer_interactions(cx, root, window, services, prompt_epoch).await?;
    let latency_samples = measure_warm_latency(cx, root, window).await?;
    let ghost =
        state(cx, root, window)?.ok_or_else(|| io::Error::other("latency pane disappeared"))?;
    let mut screenshots = Vec::new();
    capture_themes(cx, root, window, context, false, &mut screenshots).await?;
    let before_generation = ghost
        .context
        .as_ref()
        .ok_or_else(|| io::Error::other("missing prompt"))?
        .input_generation;
    press_key(cx, window, "right")?;
    wait_state(cx, root, window, "right-arrow acceptance", |state| {
        state.input_idle
            && state.context.as_ref().is_some_and(|context| {
                context.prompt_epoch == prompt_epoch && context.input_generation > before_generation
            })
    })
    .await?;
    cx.background_executor().timer(Duration::from_secs(2)).await;
    let menu = open_menu(cx, root, window).await?;
    let right_readback = read_shell(cx, root, window, services, &menu).await?;
    if right_readback.input != COMPLETE
        || !right_readback.cursor_at_end()
        || right_readback.context.prompt_epoch != prompt_epoch
    {
        return Err(io::Error::other(
            "right-arrow acceptance did not append exactly once at the same prompt",
        )
        .into());
    }
    press_key(cx, window, "escape")?;
    erase_suffix(cx, window)?;
    wait_state(cx, root, window, "ghost restored", |state| {
        state.focused && state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    let menu = open_menu(cx, root, window).await?;
    if menu
        .menu_candidates
        .first()
        .is_none_or(|candidate| candidate != COMPLETE)
    {
        return Err(io::Error::other("fixture is not the selected first menu candidate").into());
    }
    capture_themes(cx, root, window, context, true, &mut screenshots).await?;
    let before_generation = menu
        .context
        .as_ref()
        .ok_or_else(|| io::Error::other("missing menu prompt"))?
        .input_generation;
    press_key(cx, window, "enter")?;
    wait_state(cx, root, window, "menu acceptance", |state| {
        state.input_idle
            && state.context.as_ref().is_some_and(|context| {
                context.prompt_epoch == prompt_epoch && context.input_generation > before_generation
            })
    })
    .await?;
    cx.background_executor().timer(Duration::from_secs(2)).await;
    let menu = open_menu(cx, root, window).await?;
    let menu_readback = read_shell(cx, root, window, services, &menu).await?;
    if menu_readback.input != COMPLETE
        || !menu_readback.cursor_at_end()
        || menu_readback.context.prompt_epoch != prompt_epoch
    {
        return Err(io::Error::other("menu Enter executed or did not append exactly once").into());
    }
    press_key(cx, window, "escape")?;
    erase_suffix(cx, window)?;
    wait_state(cx, root, window, "dismiss fixture", |state| {
        state.focused && state.input_idle && state.ghost.as_deref() == Some(SUFFIX)
    })
    .await?;
    let menu = open_menu(cx, root, window).await?;
    let prefix_readback = read_shell(cx, root, window, services, &menu).await?;
    if prefix_readback.input != PREFIX
        || !prefix_readback.cursor_at_end()
        || prefix_readback.context.prompt_epoch != prompt_epoch
    {
        return Err(io::Error::other("dismiss fixture did not retain the same prompt").into());
    }
    press_key(cx, window, "escape")?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    let dismissed = state(cx, root, window)?
        .ok_or_else(|| io::Error::other("pane disappeared after Escape"))?;
    if dismissed.menu_state.is_some()
        || dismissed.ghost.is_some()
        || !dismissed.focused
        || !dismissed.input_idle
    {
        return Err(
            io::Error::other("Escape did not suppress the current suggestion state").into(),
        );
    }
    if let Some(host_id) = ssh_host_id.as_ref() {
        screenshots.extend(
            verify_ssh_docker_suggestions(
                cx,
                root,
                window,
                services,
                &dismissed,
                prompt_epoch,
                context,
            )
            .await?,
        );
        verify_next_ssh_prompt(
            cx,
            root,
            window,
            services,
            host_id,
            &dismissed,
            prompt_epoch,
        )
        .await?;
    }
    verify_capture_dimensions(cx, window, context.width, context.height, &screenshots)?;
    let (visible, active, bounds, isolation) = window.update(cx, |_, window, _| {
        let isolation = crate::right_rail_acceptance::isolation::capture()?;
        isolation.validate_window_state(window.is_visible(), window.is_window_active())?;
        Ok::<_, io::Error>((
            window.is_visible(),
            window.is_window_active(),
            window.bounds(),
            isolation,
        ))
    })??;
    let mut sorted_latency = latency_samples.clone();
    sorted_latency.sort_by(f64::total_cmp);
    let p95 = sorted_latency[18];
    let mut checks = vec![
        "history-recorded",
        "ghost-at-cursor",
        "native-tab-recovers",
        "cursor-move-recovers",
        "right-append-same-prompt",
        "menu-inside-pane",
        "enter-append-same-prompt",
        "escape-suppresses",
        "mouse-dismiss-keeps-input",
        "mouse-append-same-prompt",
        "mouse-accept-keeps-input",
    ];
    if ssh_host_id.is_some() {
        checks.push("ssh-docker-prefix-suggestions");
        checks.push("ssh-next-prompt-recovers");
    }
    let report = serde_json::json!({
        "schemaVersion": 6,
        "scope": "suggestions",
        "captureMode": "hidden_native_window",
        "captureBackend": "gpui_window_render_to_image",
        "nativeIsolation": isolation,
        "configDirectory": context.output_directory.join("config"),
        "windowVisible": visible,
        "windowActive": active,
        "windowWidth": bounds.size.width.as_f32(),
        "windowHeight": bounds.size.height.as_f32(),
        "themes": ["light", "dark", "system"],
        "target": if ssh_host_id.is_some() { "isolated-ssh" } else { "local" },
        "shell": format!("{:?}", baseline.shell),
        "shellVersion": baseline.shell_version,
        "nativePredictionState": baseline.shell_prediction_visible,
        "warmCandidateLatency": {
            "boundary": "GPUI key dispatch to hidden native ghost layout; includes polling",
            "samplesMs": latency_samples,
            "p95Ms": p95,
            "targetMs": if ssh_host_id.is_none() { Some(200) } else { None },
            "meetsTarget": if ssh_host_id.is_none() { Some(p95 <= 200.0) } else { None }
        },
        "checks": checks,
        "screenshots": screenshots,
    });
    fs::write(
        context.output_directory.join(REPORT_FILE),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
