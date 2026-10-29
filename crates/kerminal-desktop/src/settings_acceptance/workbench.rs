//! Isolated native acceptance for host selection, local Profile retry, and terminal-tab creation.
//!
//! @author kongweiguang

use super::*;
use gpui_pre::WindowBounds;
use kerminal_application::{
    connections::management::{
        HostAuthenticationMode, HostDraft, HostGroupDraft, HostRouteSettings, HostSftpOptions,
        HostTerminalOptions, RdpDisplayOptions,
    },
    preferences::{ProfileDraft, ProfileRevision, VersionedProfile},
};
use kerminal_domain::{
    agents::AgentTargetBinding, connections::HostEndpoint, identity::HostId,
    preferences::ConfigRevision,
};
use serde::Serialize;

const WORKBENCH_ACCEPTANCE_ENV: &str = "KERMINAL_WORKBENCH_ACCEPTANCE";
const WORKBENCH_POWERSHELL_ENV: &str = "KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL";
const WORKBENCH_REPORT_FILE: &str = "workbench-report.json";
const WORKBENCH_PROFILE_ID: &str = "acceptance-default";
const WORKBENCH_PROFILE_NAME: &str = "Acceptance Local Profile";
const WORKBENCH_CONFLICT_PROFILE_ID: &str = "acceptance-conflicting-default";
const WORKBENCH_HOST_ID: &str = "acceptance-host";
const WORKBENCH_HOST_NAME: &str = "Acceptance Host Details";
const WORKBENCH_UNGROUPED_HOST_ID: &str = "acceptance-ungrouped-host";
const WORKBENCH_SFTP_HOST_ID: &str = "acceptance-sftp-host";
const WORKBENCH_GROUP_ID: &str = "acceptance-pinned-group";

#[path = "workbench_sidebar_menus.rs"]
mod sidebar_menus;
const WORKBENCH_READY_TIMEOUT: Duration = Duration::from_secs(45);
const WORKBENCH_ACTION_TIMEOUT: Duration = Duration::from_secs(25);
const REPEATED_PROFILE_OPENS: usize = 10;

#[derive(Clone)]
struct AcceptanceFixture {
    host_id: HostId,
    profile_id: String,
}

#[derive(Clone, Serialize)]
struct WorkbenchState {
    ready: bool,
    host_selected: bool,
    connections_modal_open: bool,
    connections_panel_present: bool,
    local_profile_selected: bool,
    agent_target_key: Option<AgentTargetBinding>,
    local_tab_count: usize,
    local_pane_count: usize,
    ready_session_ids: Vec<String>,
    failed_pane_ids: Vec<String>,
    terminal_creation_busy: bool,
    pending_terminal_creation_owner: bool,
    terminal_status: Option<String>,
    active_local_pane_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkbenchAcceptanceReport {
    schema_version: u32,
    capture_mode: &'static str,
    capture_backend: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    config_directory: String,
    local_profile_shell: String,
    window_visible: bool,
    window_active: bool,
    window_width: f32,
    window_height: f32,
    host_selection: AcceptanceCheck,
    local_profile_selection: AcceptanceCheck,
    host_selection_preserved_agent_target: AcceptanceCheck,
    profile_selection_preserved_agent_target: AcceptanceCheck,
    default_profile_retry: AcceptanceCheck,
    profile_resolution_failure_recovery: AcceptanceCheck,
    profile_enter_opens_one_session: AcceptanceCheck,
    repeated_profile_tabs: AcceptanceCheck,
    split_then_plus: AcceptanceCheck,
    closing_new_tab_preserves_other_sessions: AcceptanceCheck,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptanceCheck {
    action: &'static str,
    passed: bool,
    expected: String,
    observed: String,
}

struct DuplicateDefaultProfileFixture {
    path: PathBuf,
}

impl DuplicateDefaultProfileFixture {
    /// 在本次隔离配置根中模拟外部编辑器写入第二个默认 Profile，以触发真实异步 resolver 冲突。
    fn install(config_directory: &Path) -> Result<Self, Box<dyn Error>> {
        let path = config_directory
            .join("profiles")
            .join(format!("{WORKBENCH_CONFLICT_PROFILE_ID}.toml"));
        fs::write(
            &path,
            concat!(
                "schema_version = 1\n",
                "id = \"acceptance-conflicting-default\"\n",
                "name = \"Acceptance Conflicting Default\"\n",
                "shell = \"cmd.exe\"\n",
                "args = [\"/Q\"]\n",
                "env = {}\n",
                "is_default = true\n",
                "sort_order = 1\n",
                "created_at = \"acceptance\"\n",
                "updated_at = \"acceptance\"\n",
            ),
        )?;
        Ok(Self { path })
    }

    /// 显式修复隔离 Profile 冲突；Drop 继续兜底清理任何提前返回路径。
    fn remove(&self) -> Result<(), Box<dyn Error>> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for DuplicateDefaultProfileFixture {
    /// 验收中断时也删除临时冲突文件，避免失败报告留下半修复的配置样本。
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 只有调用方显式设置验收环境变量时才启动独立 Root，避免普通设置验收意外创建工作区。
pub(super) fn requested() -> bool {
    std::env::var_os(WORKBENCH_ACCEPTANCE_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// 使用全新配置根覆盖 fixture、终端和关闭流程，避免触碰用户配置并保证资源可独立回收。
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = requested_options()?;
    let output_directory = match options.output_directory {
        Some(path) => create_requested_output_directory(&path)?,
        None => create_unique_output_directory(
            Path::new("target")
                .join("diagnostics")
                .join("workbench-acceptance"),
        )?,
    };
    let config_directory = output_directory.join("config");
    fs::create_dir(&config_directory)?;
    let runtime = ApplicationRuntime::new()?;
    let mut composition = match runtime.block_on(DesktopComposition::build(
        runtime.handle(),
        &config_directory,
    )) {
        Ok(composition) => composition,
        Err(error) => {
            runtime.shutdown();
            return Err(error.into());
        }
    };
    let services = composition.app_services();
    let ui_result = match runtime.block_on(prepare_fixture(&services)) {
        Ok(fixture) => {
            let context = WorkbenchContext {
                output_directory: output_directory.clone(),
                config_directory: config_directory.clone(),
                width: options.width.unwrap_or(DEFAULT_WIDTH),
                height: options.height.unwrap_or(DEFAULT_HEIGHT),
                fixture,
            };
            catch_unwind(AssertUnwindSafe(|| run_hidden_window(services, context))).unwrap_or_else(
                |_| Err(io::Error::other("workbench acceptance event loop panicked").into()),
            )
        }
        Err(error) => Err(error),
    };
    let shutdown_result = runtime
        .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
    runtime.shutdown();

    ui_result?;
    shutdown_result?;
    println!(
        "Workbench acceptance report: {}",
        output_directory.join(WORKBENCH_REPORT_FILE).display()
    );
    Ok(())
}

struct WorkbenchContext {
    output_directory: PathBuf,
    config_directory: PathBuf,
    width: u32,
    height: u32,
    fixture: AcceptanceFixture,
}

/// 通过应用服务创建隔离主机、置顶分组和默认 Profile，同时核对真实目录样式与连接生命周期。
/// 加入未分组与 SFTP 样本覆盖菜单差异，所有网络地址均为未连接的隔离 fixture。
/// 可显式选用 Store PowerShell 的真实安装入口，避免 cmd 样本掩盖应用别名启动故障。
async fn prepare_fixture(
    services: &kerminal_ui::AppServices,
) -> Result<AcceptanceFixture, Box<dyn Error>> {
    let host_id = HostId::parse(WORKBENCH_HOST_ID)?;
    let host_service = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("host management service is unavailable"))?;
    let group_service = host_service.clone();
    services
        .executor
        .clone()
        .run(move |_| async move {
            let groups = group_service.load_groups().await?;
            group_service
                .save_group(
                    HostGroupDraft {
                        id: Some(WORKBENCH_GROUP_ID.to_owned()),
                        name: "默认分组".to_owned(),
                        sort_order: -1,
                    },
                    groups.revision,
                )
                .await
        })
        .await?;
    let draft = HostDraft {
        id: Some(host_id.clone()),
        group_id: Some(WORKBENCH_GROUP_ID.to_owned()),
        name: WORKBENCH_HOST_NAME.to_owned(),
        note: Some("Isolated host-details acceptance fixture; never connected.".to_owned()),
        protocol: kerminal_domain::connections::RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: "acceptance".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 15_000,
        keepalive_interval_ms: None,
        terminal_options: HostTerminalOptions::default(),
        sftp_options: HostSftpOptions::default(),
        rdp_display: RdpDisplayOptions::default(),
    };
    services
        .executor
        .clone()
        .run(move |_| async move {
            host_service.save_host(draft.clone(), None).await?;
            let mut ungrouped = draft.clone();
            ungrouped.id = Some(HostId::parse(WORKBENCH_UNGROUPED_HOST_ID)?);
            ungrouped.name = "Acceptance Ungrouped Host".to_owned();
            ungrouped.group_id = None;
            host_service.save_host(ungrouped, None).await?;
            let mut sftp = draft;
            sftp.id = Some(HostId::parse(WORKBENCH_SFTP_HOST_ID)?);
            sftp.name = "Acceptance SFTP Host".to_owned();
            sftp.protocol = kerminal_domain::connections::RemoteProtocol::Sftp;
            host_service.save_host(sftp, None).await
        })
        .await?;

    let profile_id = WORKBENCH_PROFILE_ID.to_owned();
    let profile_service = services.profiles.clone();
    let initial_profile_id = profile_id.clone();
    services
        .executor
        .clone()
        .run(move |_| async move {
            profile_service
                .save_profile(
                    ProfileDraft {
                        id: Some(initial_profile_id),
                        name: WORKBENCH_PROFILE_NAME.to_owned(),
                        shell: acceptance_profile_shell(),
                        args: if std::env::var_os(WORKBENCH_POWERSHELL_ENV).is_some() {
                            vec!["-NoLogo".to_owned(), "-NoProfile".to_owned()]
                        } else {
                            vec!["/D".to_owned(), "/Q".to_owned()]
                        },
                        cwd: None,
                        env: std::collections::BTreeMap::new(),
                        sidebar_group_id: Some(WORKBENCH_GROUP_ID.to_owned()),
                        sort_order: 0,
                    },
                    None,
                )
                .await
        })
        .await?;

    let profile_service = services.profiles.clone();
    let profile_rows = services
        .executor
        .clone()
        .run(move |_| async move { profile_service.list_profiles().await })
        .await?;
    let expected_revisions = profile_rows
        .iter()
        .map(|row| ProfileRevision {
            profile_id: row.profile.id.clone(),
            revision: row.revision.clone(),
        })
        .collect();
    let profile_service = services.profiles.clone();
    let default_profile_id = profile_id.clone();
    services
        .executor
        .clone()
        .run(move |_| async move {
            profile_service
                .set_default_profile(&default_profile_id, expected_revisions)
                .await
        })
        .await?;

    Ok(AcceptanceFixture {
        host_id,
        profile_id,
    })
}

/// 在隐藏隔离窗口运行生产 Root，避免验收抢占用户前台并沿用真实事件和渲染路径。
fn run_hidden_window(
    services: kerminal_ui::AppServices,
    context: WorkbenchContext,
) -> Result<(), Box<dyn Error>> {
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let bounds = match acceptance_bounds(cx, context.width, context.height) {
                Ok(bounds) => bounds,
                Err(error) => {
                    set_failure(&failure_for_app, error.to_string());
                    cx.quit();
                    return;
                }
            };
            let window_options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(context.width as f32), px(context.height as f32))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal Workbench Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_background_appearance(WindowBackgroundAppearance::Transparent);
                window.resize(size(px(context.width as f32), px(context.height as f32)));
                let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
                if let Err(error) =
                    crate::right_rail_acceptance::isolation::capture().and_then(|evidence| {
                        evidence
                            .validate_window_state(window.is_visible(), window.is_window_active())
                    })
                {
                    set_failure(
                        &failure_for_app,
                        format!("workbench acceptance window isolation failed: {error}"),
                    );
                    cx.quit();
                    return root;
                }
                let window_handle = window.window_handle();
                let capture_root = root.clone();
                let capture_services = services.clone();
                let capture_failure = Arc::clone(&failure_for_app);
                cx.spawn(async move |cx| {
                    let result = capture_workbench(
                        cx,
                        capture_root,
                        window_handle,
                        capture_services,
                        context,
                    )
                    .await;
                    if let Err(error) = result {
                        set_failure(&capture_failure, error.to_string());
                    }
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = result {
                set_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });
    if let Some(message) = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Err(io::Error::other(message).into());
    }
    Ok(())
}

/// 默认 cmd 样本之外允许精确选择调用方提供的 PowerShell 入口，配置仍只写入隔离根。
///
/// @author kongweiguang
fn acceptance_profile_shell() -> String {
    std::env::var(WORKBENCH_POWERSHELL_ENV).unwrap_or_else(|_| "cmd.exe".to_owned())
}

/// 用真实控件覆盖单击保持终端、双击及键盘连接、失败恢复、分屏与独立关闭，不借助中央详情按钮。
/// 失败重试恢复本轮真实 shell；报告保留入口，区分普通 cmd 与 Store 应用别名验收。
async fn capture_workbench(
    cx: &mut AsyncApp,
    root: Entity<RootView>,
    window: AnyWindowHandle,
    services: kerminal_ui::AppServices,
    context: WorkbenchContext,
) -> Result<(), Box<dyn Error>> {
    let fixture = context.fixture.clone();
    let deadline = Instant::now() + WORKBENCH_READY_TIMEOUT;
    wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        deadline,
        "directory readiness",
        |state| state.ready,
    )
    .await?;

    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let first_session = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "initial local terminal",
        |state| state.ready_session_ids.len() == 1 && state.agent_target_key.is_some(),
    )
    .await?;
    let initial_agent_key = first_session
        .agent_target_key
        .clone()
        .ok_or_else(|| io::Error::other("initial Agent target candidate is unavailable"))?;
    click_selector(cx, window, "host-navigation-refresh")?;

    click_selector(
        cx,
        window,
        &format!("host-navigation-host-{}", fixture.host_id.as_str()),
    )?;
    let host_selection_state = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "remote host selection without center navigation",
        |state| state.host_selected && state.agent_target_key.as_ref() == Some(&initial_agent_key),
    )
    .await?;
    let _ = save_frame(
        cx,
        window,
        context.output_directory.join("remote-host-selection.png"),
    )?;
    let host_selection_observation = workbench_state(cx, &root, window, &fixture)?;
    fs::write(
        context
            .output_directory
            .join("remote-host-selection-state.json"),
        serde_json::to_vec_pretty(&host_selection_observation)?,
    )?;
    let host_detail_visible = workbench_selector_visible(cx, window, "connections-edit-host")?;
    let host_selection = AcceptanceCheck {
        action: "select-remote-host-without-details",
        passed: host_selection_state.host_selected && !host_detail_visible
            && host_selection_state.active_local_pane_id == first_session.active_local_pane_id
            && host_selection_state.ready_session_ids == first_session.ready_session_ids,
        expected:
            "selecting a remote host preserves the center terminal and session without showing details"
                .to_owned(),
        observed: format!(
            "centerSelected={}, detailVisible={}, readyLocalSessions={}",
            host_selection_state.host_selected,
            host_detail_visible,
            host_selection_state.ready_session_ids.len()
        ),
    };
    capture_sidebar_themes(cx, &root, window, &services, &context.output_directory).await?;

    click_selector(
        cx,
        window,
        &format!("host-navigation-profile-{}", fixture.profile_id),
    )?;
    let local_details_state = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "local Profile selection without center navigation",
        |state| {
            state.local_profile_selected
                && state.agent_target_key.as_ref() == Some(&initial_agent_key)
        },
    )
    .await?;
    let local_detail_visible =
        workbench_selector_visible(cx, window, "connections-local-host-connect")?;
    let _ = save_frame(
        cx,
        window,
        context.output_directory.join("local-profile-selection.png"),
    )?;
    let local_profile_selection = AcceptanceCheck {
        action: "select-local-profile-without-details",
        passed: local_details_state.local_profile_selected && !local_detail_visible
            && local_details_state.active_local_pane_id == first_session.active_local_pane_id
            && local_details_state.ready_session_ids == first_session.ready_session_ids,
        expected: "selecting a local Profile preserves the center terminal and session without showing details"
            .to_owned(),
        observed: format!(
            "centerSelected={}, detailVisible={}",
            local_details_state.local_profile_selected, local_detail_visible
        ),
    };
    let host_selection_preserved_agent_target = AcceptanceCheck {
        action: "host-selection-preserves-agent-target",
        passed: host_selection_state.agent_target_key.as_ref() == Some(&initial_agent_key),
        expected: "the Agent target identity remains bound to the active terminal".to_owned(),
        observed: if host_selection_state.agent_target_key.as_ref() == Some(&initial_agent_key) {
            "unchanged".to_owned()
        } else {
            "changed".to_owned()
        },
    };
    let profile_selection_preserved_agent_target = AcceptanceCheck {
        action: "profile-selection-preserves-agent-target",
        passed: local_details_state.agent_target_key.as_ref() == Some(&initial_agent_key),
        expected: "the Agent target identity remains bound to the active terminal".to_owned(),
        observed: if local_details_state.agent_target_key.as_ref() == Some(&initial_agent_key) {
            "unchanged".to_owned()
        } else {
            "changed".to_owned()
        },
    };

    let before_failed_default = workbench_state(cx, &root, window, &fixture)?;
    update_profile_shell(&services, &fixture, acceptance_missing_shell()).await?;
    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let failed_default = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "failed default Profile startup",
        |state| {
            !state.terminal_creation_busy
                && !state.pending_terminal_creation_owner
                && state.terminal_status != before_failed_default.terminal_status
                && state.local_tab_count == before_failed_default.local_tab_count
                && state.local_pane_count == before_failed_default.local_pane_count
                && state.ready_session_ids == before_failed_default.ready_session_ids
                && state.failed_pane_ids.is_empty()
        },
    )
    .await?;
    update_profile_shell(&services, &fixture, acceptance_profile_shell()).await?;
    // 修复默认 shell 后仍从原加号入口重试，确认失败 reservation 已完整回滚。
    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let recovered = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "default Profile retry",
        |state| {
            state.ready_session_ids.len() == before_failed_default.ready_session_ids.len() + 1
                && state.local_tab_count == before_failed_default.local_tab_count + 1
                && state.local_pane_count == before_failed_default.local_pane_count + 1
                && state.failed_pane_ids.is_empty()
        },
    )
    .await?;
    let default_profile_retry = AcceptanceCheck {
        action: "default-profile-failure-retry",
        passed: !failed_default.terminal_creation_busy
            && !failed_default.pending_terminal_creation_owner
            && failed_default.terminal_status != before_failed_default.terminal_status
            && failed_default.local_tab_count == before_failed_default.local_tab_count
            && failed_default.local_pane_count == before_failed_default.local_pane_count
            && failed_default.ready_session_ids == before_failed_default.ready_session_ids
            && failed_default.failed_pane_ids.is_empty()
            && recovered.ready_session_ids.len()
                == before_failed_default.ready_session_ids.len() + 1
            && recovered.local_tab_count == before_failed_default.local_tab_count + 1
            && recovered.local_pane_count == before_failed_default.local_pane_count + 1
            && recovered.failed_pane_ids.is_empty(),
        expected: "failed startup releases its exact creation owner and rolls back the empty reservation; after fixing the Profile, the original plus action creates one independent session".to_owned(),
        observed: format!(
            "busy={}, ownerPending={}, failedPanes={}, failedTabs={}, failedPanesTotal={}, recoveredSessions={}, recoveredPanes={}",
            failed_default.terminal_creation_busy,
            failed_default.pending_terminal_creation_owner,
            failed_default.failed_pane_ids.len(),
            failed_default.local_tab_count,
            failed_default.local_pane_count,
            recovered.ready_session_ids.len(),
            recovered.local_pane_count
        ),
    };

    let before_profile_conflict = workbench_state(cx, &root, window, &fixture)?;
    let duplicate_profile = DuplicateDefaultProfileFixture::install(&context.config_directory)?;
    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let conflicted = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "multiple default Profile resolution failure",
        |state| {
            !state.terminal_creation_busy
                && !state.pending_terminal_creation_owner
                && state.terminal_status != before_profile_conflict.terminal_status
                && state.local_tab_count == before_profile_conflict.local_tab_count
                && state.local_pane_count == before_profile_conflict.local_pane_count
                && state.ready_session_ids == before_profile_conflict.ready_session_ids
                && state.failed_pane_ids.is_empty()
        },
    )
    .await?;
    duplicate_profile.remove()?;
    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let profile_recovered = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "new default Profile tab after conflict recovery",
        |state| {
            state.failed_pane_ids.is_empty()
                && state.ready_session_ids.len()
                    == before_profile_conflict.ready_session_ids.len() + 1
                && state.local_tab_count == before_profile_conflict.local_tab_count + 1
                && state.local_pane_count == before_profile_conflict.local_pane_count + 1
        },
    )
    .await?;
    let profile_resolution_failure_recovery = AcceptanceCheck {
        action: "default-profile-resolution-failure-recovery",
        passed: !conflicted.terminal_creation_busy
            && !conflicted.pending_terminal_creation_owner
            && conflicted.terminal_status != before_profile_conflict.terminal_status
            && conflicted.local_tab_count == before_profile_conflict.local_tab_count
            && conflicted.local_pane_count == before_profile_conflict.local_pane_count
            && conflicted.ready_session_ids == before_profile_conflict.ready_session_ids
            && conflicted.failed_pane_ids.is_empty()
            && profile_recovered.failed_pane_ids.is_empty()
            && profile_recovered.ready_session_ids.len()
                == before_profile_conflict.ready_session_ids.len() + 1
            && profile_recovered.local_tab_count == before_profile_conflict.local_tab_count + 1
            && profile_recovered.local_pane_count == before_profile_conflict.local_pane_count + 1,
        expected: "a duplicate-default resolver failure releases its owner and rolls back the empty reservation; removing the conflict lets the next plus action create one independent session".to_owned(),
        observed: format!(
            "busy={}, ownerPending={}, failedPanes={}, failedTabs={}, failedPanesTotal={}, recoveredTabs={}, recoveredPanes={}, readySessions={}",
            conflicted.terminal_creation_busy,
            conflicted.pending_terminal_creation_owner,
            conflicted.failed_pane_ids.len(),
            conflicted.local_tab_count,
            conflicted.local_pane_count,
            profile_recovered.local_tab_count,
            profile_recovered.local_pane_count,
            profile_recovered.ready_session_ids.len()
        ),
    };

    let mut repeated = profile_recovered.clone();
    let mut profile_enter_opens_one_session = None;
    for open_index in 1..=REPEATED_PROFILE_OPENS {
        click_selector(
            cx,
            window,
            &format!("host-navigation-profile-{}", fixture.profile_id),
        )?;
        let previous = repeated.clone();
        if open_index == 1 {
            dispatch_workbench_key(cx, window, "enter")?;
        } else {
            dispatch_workbench_second_click(
                cx,
                window,
                &format!("host-navigation-profile-{}", fixture.profile_id),
            )?;
        }
        let opened = wait_for_workbench_state(
            cx,
            &root,
            window,
            &fixture,
            Instant::now() + WORKBENCH_ACTION_TIMEOUT,
            "repeated default Profile open",
            |state| {
                state.ready_session_ids.len() == previous.ready_session_ids.len() + 1
                    && state.local_tab_count == previous.local_tab_count + 1
                    && state.local_pane_count == previous.local_pane_count + 1
                    && state.failed_pane_ids.is_empty()
                    && !state.terminal_creation_busy
                    && !state.pending_terminal_creation_owner
            },
        )
        .await?;
        if open_index == 1 {
            profile_enter_opens_one_session = Some(AcceptanceCheck {
                action: "enter-opens-local-profile",
                passed: opened.ready_session_ids.len() == previous.ready_session_ids.len() + 1
                    && opened.local_tab_count == previous.local_tab_count + 1
                    && opened.local_pane_count == previous.local_pane_count + 1,
                expected: "clicking the local Profile row and dispatching one GPUI Enter key pair opens exactly one new session and tab".to_owned(),
                observed: format!(
                    "sessionsBefore={}, sessionsAfter={}, tabsBefore={}, tabsAfter={}, panesBefore={}, panesAfter={}",
                    previous.ready_session_ids.len(),
                    opened.ready_session_ids.len(),
                    previous.local_tab_count,
                    opened.local_tab_count,
                    previous.local_pane_count,
                    opened.local_pane_count
                ),
            });
        }
        repeated = opened;
    }
    let profile_enter_opens_one_session = profile_enter_opens_one_session
        .ok_or_else(|| io::Error::other("keyboard Profile opening was not exercised"))?;
    let repeated_profile_tabs = AcceptanceCheck {
        action: "ten-repeated-profile-independent-tabs",
        passed: repeated.ready_session_ids.len()
            == profile_recovered.ready_session_ids.len() + REPEATED_PROFILE_OPENS
            && repeated.local_pane_count
                == profile_recovered.local_pane_count + REPEATED_PROFILE_OPENS
            && repeated.local_tab_count
                == profile_recovered.local_tab_count + REPEATED_PROFILE_OPENS
            && repeated.failed_pane_ids.is_empty(),
        expected:
            "ten repeated connections to the same local Profile, including keyboard Enter, each own a new tab, pane, and session"
                .to_owned(),
        observed: format!(
            "opens={}, tabs={}, panes={}, distinctSessions={}",
            REPEATED_PROFILE_OPENS,
            repeated.local_tab_count,
            repeated.local_pane_count,
            repeated.ready_session_ids.len()
        ),
    };

    let active_pane_id = repeated
        .active_local_pane_id
        .ok_or_else(|| io::Error::other("active local pane is unavailable for split acceptance"))?;
    click_selector(
        cx,
        window,
        &format!("pane-split-horizontal-after-{active_pane_id}-trigger"),
    )?;
    let split_ready = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "split local pane",
        |state| {
            state.local_tab_count == repeated.local_tab_count
                && state.local_pane_count == repeated.local_pane_count + 1
                && state.ready_session_ids.len() == repeated.ready_session_ids.len() + 1
        },
    )
    .await?;
    let _ = save_frame(
        cx,
        window,
        context.output_directory.join("split-workspace.png"),
    )?;
    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let after_split_plus = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "plus after split",
        |state| {
            state.local_tab_count == repeated.local_tab_count + 1
                && state.local_pane_count == repeated.local_pane_count + 2
                && state.ready_session_ids.len() == repeated.ready_session_ids.len() + 2
                && state.active_local_pane_id.is_some()
                && state.failed_pane_ids.is_empty()
        },
    )
    .await?;
    let split_then_plus = AcceptanceCheck {
        action: "plus-after-split",
        passed: split_ready.local_tab_count == repeated.local_tab_count
            && split_ready.local_pane_count == repeated.local_pane_count + 1
            && split_ready.ready_session_ids.len() == repeated.ready_session_ids.len() + 1
            && after_split_plus.local_tab_count == repeated.local_tab_count + 1
            && after_split_plus.local_pane_count == repeated.local_pane_count + 2
            && after_split_plus.ready_session_ids.len() == repeated.ready_session_ids.len() + 2,
        expected:
            "splitting adds one pane; the top plus button still adds an independent tab and session"
                .to_owned(),
        observed: format!(
            "splitPanes={}, finalTabs={}, finalPanes={}, distinctSessions={}",
            split_ready.local_pane_count,
            after_split_plus.local_tab_count,
            after_split_plus.local_pane_count,
            after_split_plus.ready_session_ids.len()
        ),
    };

    let close_pane_id = after_split_plus
        .active_local_pane_id
        .clone()
        .ok_or_else(|| io::Error::other("new terminal tab has no active pane to close"))?;
    let plus_session_id = after_split_plus
        .ready_session_ids
        .iter()
        .find(|session_id| !split_ready.ready_session_ids.contains(*session_id))
        .cloned()
        .ok_or_else(|| io::Error::other("new terminal tab session was not identified"))?;
    click_selector(cx, window, &format!("pane-close-{close_pane_id}"))?;
    wait_for_selector(cx, window, "ok", "terminal tab close confirmation").await?;
    click_selector(cx, window, "ok")?;
    let after_close = wait_for_workbench_state(
        cx,
        &root,
        window,
        &fixture,
        Instant::now() + WORKBENCH_ACTION_TIMEOUT,
        "close the new terminal tab",
        |state| {
            state.ready_session_ids.len() + 1 == after_split_plus.ready_session_ids.len()
                && state.local_tab_count + 1 == after_split_plus.local_tab_count
                && state.local_pane_count + 1 == after_split_plus.local_pane_count
                && !state.ready_session_ids.contains(&plus_session_id)
                && split_ready
                    .ready_session_ids
                    .iter()
                    .all(|session_id| state.ready_session_ids.contains(session_id))
                && state.failed_pane_ids.is_empty()
        },
    )
    .await?;
    let closing_new_tab_preserves_other_sessions = AcceptanceCheck {
        action: "close-new-tab-preserves-other-sessions",
        passed: after_close.ready_session_ids.len() + 1
            == after_split_plus.ready_session_ids.len()
            && after_close.local_tab_count + 1 == after_split_plus.local_tab_count
            && after_close.local_pane_count + 1 == after_split_plus.local_pane_count
            && !after_close.ready_session_ids.contains(&plus_session_id)
            && split_ready
                .ready_session_ids
                .iter()
                .all(|session_id| after_close.ready_session_ids.contains(session_id)),
        expected: "closing the new tab through its active pane button removes exactly its Ready session and tab while every prior session and tab remains".to_owned(),
        observed: format!(
            "closedSessionRemoved={}, sessionsBefore={}, sessionsAfter={}, tabsBefore={}, tabsAfter={}, panesBefore={}, panesAfter={}, priorSessionsPreserved={}",
            !after_close.ready_session_ids.contains(&plus_session_id),
            after_split_plus.ready_session_ids.len(),
            after_close.ready_session_ids.len(),
            after_split_plus.local_tab_count,
            after_close.local_tab_count,
            after_split_plus.local_pane_count,
            after_close.local_pane_count,
            split_ready
                .ready_session_ids
                .iter()
                .all(|session_id| after_close.ready_session_ids.contains(session_id))
        ),
    };

    let (window_visible, window_active, window_width, window_height) =
        window.update(cx, |_, window, _| {
            ensure_hidden_window(window)?;
            Ok::<_, io::Error>((
                window.is_visible(),
                window.is_window_active(),
                window.bounds().size.width.as_f32(),
                window.bounds().size.height.as_f32(),
            ))
        })??;
    let report = WorkbenchAcceptanceReport {
        schema_version: 4,
        capture_mode: "hidden-native-window",
        capture_backend: "gpui-root-click-and-state-readback",
        native_isolation: crate::right_rail_acceptance::isolation::capture()?,
        config_directory: context.config_directory.to_string_lossy().into_owned(),
        local_profile_shell: acceptance_profile_shell(),
        window_visible,
        window_active,
        window_width,
        window_height,
        host_selection,
        local_profile_selection,
        host_selection_preserved_agent_target,
        profile_selection_preserved_agent_target,
        default_profile_retry,
        profile_resolution_failure_recovery,
        profile_enter_opens_one_session,
        repeated_profile_tabs,
        split_then_plus,
        closing_new_tab_preserves_other_sessions,
    };
    let report_path = context.output_directory.join(WORKBENCH_REPORT_FILE);
    fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    // 终端焦点可激活私有桌面的隐藏窗口；统一策略仍要求其绝不显示且与用户输入桌面隔离。
    report
        .native_isolation
        .validate_window_state(window_visible, window_active)?;
    if [
        &report.host_selection,
        &report.local_profile_selection,
        &report.host_selection_preserved_agent_target,
        &report.profile_selection_preserved_agent_target,
        &report.default_profile_retry,
        &report.profile_resolution_failure_recovery,
        &report.profile_enter_opens_one_session,
        &report.repeated_profile_tabs,
        &report.split_then_plus,
        &report.closing_new_tab_preserves_other_sessions,
    ]
    .iter()
    .any(|check| !check.passed)
    {
        return Err(io::Error::other("workbench acceptance contains a failed check").into());
    }
    Ok(())
}

/// 用当前 revision 更新 shell，保留 Profile 其余字段以便恢复只修正导致启动失败的值。
async fn update_profile_shell(
    services: &kerminal_ui::AppServices,
    fixture: &AcceptanceFixture,
    shell: String,
) -> Result<(), Box<dyn Error>> {
    let profile_service = services.profiles.clone();
    let profile_rows = services
        .executor
        .clone()
        .run(move |_| async move { profile_service.list_profiles().await })
        .await?;
    let profile = profile_rows
        .into_iter()
        .find(|row| row.profile.id == fixture.profile_id)
        .ok_or_else(|| io::Error::other("default acceptance Profile disappeared"))?;
    let expected_revision: ConfigRevision = profile.revision.clone();
    let draft = profile_draft_with_shell(&profile, shell);
    let profile_service = services.profiles.clone();
    services
        .executor
        .clone()
        .run(move |_| async move {
            profile_service
                .save_profile(draft, Some(expected_revision))
                .await
        })
        .await?;
    Ok(())
}

/// 将完整已保存 Profile 投影回草稿，避免验收修复 shell 时误改未参与故障的字段。
fn profile_draft_with_shell(profile: &VersionedProfile, shell: String) -> ProfileDraft {
    ProfileDraft {
        id: Some(profile.profile.id.clone()),
        name: profile.profile.name.clone(),
        shell,
        args: profile.profile.args.clone(),
        cwd: profile.profile.cwd.clone(),
        env: profile.profile.env.clone(),
        sidebar_group_id: profile.profile.sidebar_group_id.clone(),
        sort_order: profile.profile.sort_order,
    }
}

/// 使用进程唯一且不存在的可执行文件名，确定性触发失败而不运行任何测试命令。
fn acceptance_missing_shell() -> String {
    format!(
        "kerminal-acceptance-missing-shell-{}.exe",
        std::process::id()
    )
}

/// 先驱动隐藏窗口回调再读 Root 状态，避免将尚未绘制的异步变更误判为成功。
fn workbench_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: &AcceptanceFixture,
) -> Result<WorkbenchState, Box<dyn Error>> {
    let fixture = fixture.clone();
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok(root.update(app, |root, cx| {
            let (local_tab_count, local_pane_count, ready_session_ids, failed_pane_ids) =
                root.workbench_acceptance_local_state();
            let (terminal_creation_busy, pending_terminal_creation_owner, terminal_status) =
                root.workbench_acceptance_terminal_creation_state();
            WorkbenchState {
                ready: root.workbench_acceptance_ready(&fixture.host_id, cx),
                host_selected: root.workbench_acceptance_host_selected(&fixture.host_id, cx),
                connections_modal_open: root.workbench_acceptance_connections_modal_open(),
                connections_panel_present: root.workbench_acceptance_connections_panel_present(),
                local_profile_selected: root.workbench_acceptance_local_profile_selected(),
                agent_target_key: root.workbench_acceptance_agent_target_key(),
                local_tab_count,
                local_pane_count,
                ready_session_ids,
                failed_pane_ids,
                terminal_creation_busy,
                pending_terminal_creation_owner,
                terminal_status,
                active_local_pane_id: root.workbench_acceptance_active_local_pane_id(),
            }
        }))
    })?
}

/// 以有界轮询等待真实 Root 投影，并在超时时保留当前阶段及计数便于定位挂起环节。
async fn wait_for_workbench_state<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: &AcceptanceFixture,
    deadline: Instant,
    stage: &str,
    mut condition: F,
) -> Result<WorkbenchState, Box<dyn Error>>
where
    F: FnMut(&WorkbenchState) -> bool,
{
    loop {
        let state = workbench_state(cx, root, window, fixture)?;
        if condition(&state) {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "workbench acceptance timed out during {stage}: ready={}, host_selected={}, local_profile_selected={}, tabs={}, panes={}, ready_sessions={}, failed_panes={}, creating={}, owner_pending={}, status={:?}",
                state.ready,
                state.host_selected,
                state.local_profile_selected,
                state.local_tab_count,
                state.local_pane_count,
                state.ready_session_ids.len(),
                state.failed_pane_ids.len(),
                state.terminal_creation_busy,
                state.pending_terminal_creation_owner,
                state.terminal_status,
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 从真实设置入口切换三种明暗模式并关闭模态后截图，避免只验证默认主题或绕过保存链路。
/// 每个主题随后验证对象/背景菜单归属、Escape 焦点恢复和新建连接实际打开编辑器。
///
/// @author kongweiguang
async fn capture_sidebar_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    output_directory: &Path,
) -> Result<(), Box<dyn Error>> {
    for (theme, name, selector) in THEMES {
        press_key(cx, window, "ctrl-alt-s")?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.modal_open() && state.settings_ready() && state.draft_ready()
        })
        .await?;
        ensure_page(cx, root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, root, window, "appearance", selector).await?;
        click_selector(cx, window, selector)?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.draft_theme_mode() == Some(name) && !state.draft_dirty() && state.draft_ready()
        })
        .await?;
        if load_persisted_settings(services).await?.settings.theme_mode != theme {
            return Err(io::Error::other(format!("sidebar theme {name} was not saved")).into());
        }
        click_selector(cx, window, "preferences-close")?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            !state.modal_open()
        })
        .await?;
        let _ = save_frame(
            cx,
            window,
            output_directory.join(format!("sidebar-{name}.png")),
        )?;
        sidebar_menus::capture(cx, root, window, output_directory, name).await?;
    }
    Ok(())
}

/// 在同一帧检查详情动作是否实际挂载，证明目录单击没有暗中替换中央终端。
///
/// @author kongweiguang
fn workbench_selector_visible(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<bool, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(selector_bounds(window, selector).is_ok())
    })??)
}

/// 与前一个单击组成真实的第二击事件，只驱动私有窗口事件树，不使用操作系统鼠标。
///
/// @author kongweiguang
fn dispatch_workbench_second_click(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        let bounds = selector_bounds(window, selector)?;
        if !bounds_inside_viewport(bounds, window.visual_viewport_bounds()) {
            return Err(io::Error::other(
                "double-click target is outside the viewport",
            ));
        }
        for event in [
            MouseDownEvent {
                button: MouseButton::Left,
                position: bounds.center(),
                click_count: 2,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position: bounds.center(),
                click_count: 2,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 按键直接进入私有窗口的生产焦点与事件树，验证真实键盘路径而不注入操作系统键鼠。
///
/// @author kongweiguang
fn dispatch_workbench_key(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    key: &str,
) -> Result<(), Box<dyn Error>> {
    let keystroke = gpui_kit::Keystroke::parse(key)
        .map_err(|error| io::Error::other(format!("invalid workbench key {key}: {error}")))?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        let _ = window.dispatch_event(
            gpui_kit::KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(gpui_kit::KeyUpEvent { keystroke }.to_platform_input(), app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 等待详情页独有的可交互控件，避免用状态镜像或非交互容器代替实际可见性验证。
async fn wait_for_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    stage: &str,
) -> Result<bool, Box<dyn Error>> {
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    let mut last_observation = String::from("selector was not inspected");
    loop {
        let (visible, observation) = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            let bounds = selector_bounds(window, selector);
            let observation = bounds.as_ref().err().map(|error| {
                format!(
                    "{error}; registered paths: {}",
                    gpui_kit::base::test_support::registered_paths(window)
                )
            });
            Ok::<_, io::Error>((bounds.is_ok(), observation))
        })??;
        if visible {
            return Ok(true);
        }
        if let Some(observation) = observation {
            last_observation = observation;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "workbench acceptance timed out waiting for {stage} element {selector:?}: {last_observation}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}
