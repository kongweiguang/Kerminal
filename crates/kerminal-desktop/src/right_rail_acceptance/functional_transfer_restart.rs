//! 两个后台原生进程共享专用配置，验证独立传输标签冷重启恢复。
//!
//! @author kongweiguang

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    time::Instant,
};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, ElementId, Entity, InputEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent,
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    files::FileService,
    workspace::{WorkspaceSessionLoad, WorkspaceSessionService, WorkspaceSessionTab},
};
use kerminal_domain::{
    files::{DeleteTargetKind, FileEndpoint, FileEntryKind, WriteTextRequest, WriteTextStrategy},
    identity::{HostId, TabId},
};
use kerminal_ui::{QuitIntent, RootView};
use serde::{Deserialize, Serialize};

use super::{
    RightRailCaptureContext,
    functional::{self, FunctionalRecord},
    functional_transfer_workspace as workspace,
};

const SOURCE_HOST: &str = "transfer-source-fixture";
const PENDING_RECEIPT: &str = "transfer-restart-pending.json";
const PREPARED_RECEIPT: &str = "transfer-restart-prepared.json";

/// 只有成功退出并完成资源关闭的准备回执才能作为第二进程的输入。
pub(super) enum Phase {
    Prepare,
    Verify(Box<Receipt>),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Receipt {
    schema_version: u32,
    process_id: u32,
    manifest: PathBuf,
    config_directory: PathBuf,
    run_id: String,
    tabs: [ExpectedTab; 2],
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedTab {
    id: TabId,
    left: RemoteTarget,
    right: RemoteTarget,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RemoteTarget {
    host: HostId,
    path: String,
}

/// 重启模式只允许在本工作树验收根目录下创建全新直接子目录，并显式绑定 fixture 清单。
///
/// @author kongweiguang
pub(super) fn validate_output_request(
    output: &Option<PathBuf>,
    manifest: Option<&Path>,
) -> Result<(), io::Error> {
    let output = output
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "重启验收必须指定新输出目录"))?;
    if manifest.is_none() || !output.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "重启验收必须使用绝对输出目录和显式 fixture 清单",
        ));
    }
    let root = fs::canonicalize(Path::new("target").join("acceptance").join("right-rail"))?;
    let parent = output
        .parent()
        .ok_or_else(|| io::Error::other("重启验收输出缺少父目录"))?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if fs::canonicalize(parent)? != root
        || !name.starts_with("restart-")
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "重启验收输出必须是专用验收根的 restart-* 直接子目录",
        ));
    }
    Ok(())
}

/// 不跟随重解析点复用配置，避免验收输入通过目录链接转向用户文件。
///
/// @author kongweiguang
fn require_plain_entry(path: &Path, directory: bool) -> Result<(), io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    let linked = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        linked || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    if linked
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "重启验收拒绝链接或非普通文件",
        ));
    }
    Ok(())
}

/// 逐项检查配置树，不能只检查根目录而跟随内部链接写入其他目录。
///
/// @author kongweiguang
fn require_plain_tree(directory: &Path) -> Result<(), io::Error> {
    require_plain_entry(directory, true)?;
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            require_plain_tree(&path)?;
        } else {
            require_plain_entry(&path, false)?;
        }
    }
    Ok(())
}

/// 核验准备回执及目录身份，拒绝用户配置、不同 fixture、未完成退出或同一进程。
///
/// @author kongweiguang
pub(super) fn resolve_previous_run(
    previous: &Path,
    manifest: &Path,
) -> Result<(PathBuf, Receipt), Box<dyn Error>> {
    if !previous.is_absolute() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "准备阶段目录必须是绝对路径").into(),
        );
    }
    validate_output_request(&Some(previous.to_path_buf()), Some(manifest))?;
    require_plain_entry(previous, true)?;
    let previous = fs::canonicalize(previous)?;
    let config = previous.join("config");
    require_plain_tree(&config)?;
    let config = fs::canonicalize(config)?;
    super::reject_user_configuration_directory(&config)?;
    let receipt_path = previous.join(PREPARED_RECEIPT);
    require_plain_entry(&receipt_path, false)?;
    let receipt: Receipt = serde_json::from_slice(&fs::read(receipt_path)?)?;
    if receipt.schema_version != 1
        || receipt.process_id == std::process::id()
        || receipt.manifest != fs::canonicalize(manifest)?
        || receipt.config_directory != config
        || previous.file_name().and_then(|value| value.to_str()) != Some(receipt.run_id.as_str())
        || receipt.tabs[0].id == receipt.tabs[1].id
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "重启验收准备回执身份不匹配",
        )
        .into());
    }
    Ok((config, receipt))
}

/// 只有 Root 保存门禁和 Composition 收尾都成功后才发布回执，失败结果不可供重启阶段复用。
///
/// @author kongweiguang
pub(super) fn finish_prepare(output: &Path) -> Result<(), io::Error> {
    let pending = output.join(PENDING_RECEIPT);
    let completed = output.join(PREPARED_RECEIPT);
    require_plain_entry(&pending, false)?;
    if completed.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "准备回执已存在",
        ));
    }
    fs::rename(pending, completed)
}

/// 等待生产退出门禁确认，包括工作区保存；实际资源关闭由外层 Composition 执行。
///
/// @author kongweiguang
pub(super) async fn request_graceful_exit(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.request_quit(QuitIntent::Exit, cx);
        })
    })?;
    functional::poll_root(
        cx,
        root,
        window,
        "正常退出与工作区保存",
        |root, _, _| {
            root.transfer_workspace_acceptance_quit_ready()
                .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 准备与验证共用窗口、队列服务和证据格式，但冷启动不新建 fixture 终端或覆盖活动标签。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &kerminal_ui::AppServices,
    phase: &Phase,
) -> Result<(), Box<dyn Error>> {
    match phase {
        Phase::Prepare => prepare(cx, root, window, capture, records, services).await,
        Phase::Verify(receipt) => {
            verify(cx, root, window, capture, records, services, receipt).await
        }
    }
}

/// 基于当前 fixture 和运行名重建唯一允许的两个目标，拒绝回执扩大远端操作范围。
///
/// @author kongweiguang
fn expected_targets(
    capture: &RightRailCaptureContext,
    run_id: &str,
) -> Result<[(RemoteTarget, RemoteTarget); 2], Box<dyn Error>> {
    if !run_id.starts_with("restart-")
        || !run_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(io::Error::other("非法重启验收运行标识").into());
    }
    let source = HostId::parse(SOURCE_HOST)?;
    let target = capture.fixture_host_id.clone();
    let source_root = FileEndpoint::remote(source.clone(), &capture.remote_sftp_directory)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let target_root = FileEndpoint::remote(target.clone(), &capture.remote_sftp_directory)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let child_path = |endpoint: &FileEndpoint, suffix: &str| -> Result<String, io::Error> {
        let path = endpoint
            .remote_path()
            .ok_or_else(|| io::Error::other("fixture 路径不是远端目录"))?;
        Ok(path
            .join_name(&format!("rr-{run_id}-{suffix}"))
            .map_err(|error| io::Error::other(format!("{error:?}")))?
            .to_string())
    };
    let root_path = source_root
        .remote_path()
        .ok_or_else(|| io::Error::other("fixture 缺少远端根"))?
        .to_string();
    Ok([
        (
            RemoteTarget {
                host: source.clone(),
                path: child_path(&source_root, "a")?,
            },
            RemoteTarget {
                host: target.clone(),
                path: root_path.clone(),
            },
        ),
        (
            RemoteTarget {
                host: target,
                path: child_path(&target_root, "b")?,
            },
            RemoteTarget {
                host: source,
                path: root_path,
            },
        ),
    ])
}

/// 先建立彼此不同的目录，再经主机菜单与文件行输入建立两个真实独立标签。
///
/// @author kongweiguang
async fn prepare(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &kerminal_ui::AppServices,
) -> Result<(), Box<dyn Error>> {
    if !capture
        .fixture_config_source
        .join("hosts")
        .join(format!("{SOURCE_HOST}.toml"))
        .is_file()
    {
        return Err(io::Error::other("冷重启验收要求两个独立 SSH fixture 主机").into());
    }
    let run_id = capture
        .output_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("缺少运行标识"))?
        .to_owned();
    let targets = expected_targets(capture, &run_id)?;
    let files = services
        .files
        .as_ref()
        .ok_or_else(|| io::Error::other("缺少文件服务"))?;
    create_directories(&services.executor, files, &targets, &run_id).await?;
    let first = create_tab_from_host_menu(cx, root, window, &capture.fixture_host_id).await?;
    workspace::select_host(cx, root, window, capture, 0, SOURCE_HOST).await?;
    let first_root =
        FileEndpoint::remote(targets[0].0.host.clone(), &capture.remote_sftp_directory)
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
    workspace::wait_endpoint(cx, root, window, 0, &first_root).await?;
    let first_name = format!("rr-{run_id}-a");
    workspace::double_click_selector(
        cx,
        window,
        capture,
        0,
        &format!("central-files-row-0-{first_name}"),
        "restart-first-directory",
    )
    .await?;
    let first_expected = ExpectedTab {
        id: first,
        left: targets[0].0.clone(),
        right: targets[0].1.clone(),
    };
    assert_tab(cx, root, window, &first_expected).await?;
    workspace::wait_selector(cx, window, "central-files-row-0-restart-marker-a.txt").await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "transfer-restart-prepared-first",
        "first_tab_distinct_hosts_and_directory_ready",
    )
    .await?;

    let second = create_tab_from_host_menu(cx, root, window, &capture.fixture_host_id).await?;
    workspace::select_host(
        cx,
        root,
        window,
        capture,
        0,
        capture.fixture_host_id.as_str(),
    )
    .await?;
    let second_root =
        FileEndpoint::remote(targets[1].0.host.clone(), &capture.remote_sftp_directory)
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
    workspace::wait_endpoint(cx, root, window, 0, &second_root).await?;
    let second_name = format!("rr-{run_id}-b");
    workspace::double_click_selector(
        cx,
        window,
        capture,
        0,
        &format!("central-files-row-0-{second_name}"),
        "restart-second-directory",
    )
    .await?;
    workspace::select_host(cx, root, window, capture, 1, SOURCE_HOST).await?;
    let second_expected = ExpectedTab {
        id: second,
        left: targets[1].0.clone(),
        right: targets[1].1.clone(),
    };
    assert_tab(cx, root, window, &second_expected).await?;
    workspace::wait_selector(cx, window, "central-files-row-0-restart-marker-b.txt").await?;
    let tabs = [first_expected, second_expected];
    let sessions = services
        .workspace_sessions
        .as_ref()
        .ok_or_else(|| io::Error::other("缺少工作区会话仓储"))?;
    wait_persisted(cx, window, &services.executor, sessions, &tabs).await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "transfer-restart-prepared-second",
        "both_tabs_saved_with_independent_targets",
    )
    .await?;
    let receipt = Receipt {
        schema_version: 1,
        process_id: std::process::id(),
        manifest: capture
            .fixture_manifest_path
            .clone()
            .ok_or_else(|| io::Error::other("缺少 fixture 清单"))?,
        config_directory: fs::canonicalize(&capture.config_directory)?,
        run_id,
        tabs,
    };
    fs::write(
        capture.output_directory.join(PENDING_RECEIPT),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(())
}

/// 第二进程按回执激活两个真实标签并再次切回，读取面板目录而不是以磁盘快照冒充恢复。
///
/// @author kongweiguang
async fn verify(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &kerminal_ui::AppServices,
    receipt: &Receipt,
) -> Result<(), Box<dyn Error>> {
    let expected = expected_targets(capture, &receipt.run_id)?;
    if receipt
        .tabs
        .iter()
        .zip(expected.iter())
        .any(|(tab, (left, right))| &tab.left != left || &tab.right != right)
    {
        return Err(io::Error::other("准备回执中的端点不属于本轮 fixture").into());
    }
    let files = services
        .files
        .as_ref()
        .ok_or_else(|| io::Error::other("缺少文件服务"))?;
    for (index, operation) in [
        (0, "transfer-restart-restored-first"),
        (1, "transfer-restart-restored-second"),
        (0, "transfer-restart-restored-again"),
    ] {
        let tab = &receipt.tabs[index];
        super::functional_transfers::click_visible_workspace_tab(
            cx, window, capture, &tab.id, false, operation,
        )
        .await?;
        assert_tab(cx, root, window, tab).await?;
        let marker_name = marker_name(index);
        workspace::wait_selector(cx, window, &format!("central-files-row-0-{marker_name}")).await?;
        let marker = marker_endpoint(&tab.left, marker_name)?;
        workspace::assert_text(
            &services.executor,
            files,
            &marker,
            &marker_content(&receipt.run_id, index),
        )
        .await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "restored_tab_hosts_and_directories_loaded",
        )
        .await?;
    }
    fs::write(
        capture
            .output_directory
            .join("transfer-restart-verified.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({ "prepareProcessId": receipt.process_id, "verifyProcessId": std::process::id(), "tabs": &receipt.tabs }),
        )?,
    )?;
    remove_directories(&services.executor, files, &expected).await?;
    Ok(())
}

/// 每次等待真实标签处于活动状态且两侧目录均完成读取，区分加载态和恢复成功。
///
/// @author kongweiguang
async fn assert_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab: &ExpectedTab,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(cx, root, window, "活动传输标签", |root, _, _| {
        Ok(root
            .right_rail_acceptance_sftp_workspace_tab_selected(&tab.right.host, &tab.id)
            .then_some(()))
    })
    .await?;
    for (side, target) in [(0, &tab.left), (1, &tab.right)] {
        let endpoint = FileEndpoint::remote(target.host.clone(), &target.path)
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
        workspace::wait_endpoint(cx, root, window, side, &endpoint).await?;
    }
    Ok(())
}

/// 仓储回读只作为准备阶段保存门禁，恢复结论仍来自第二进程的真实面板。
///
/// @author kongweiguang
async fn wait_persisted(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    sessions: &WorkspaceSessionService,
    tabs: &[ExpectedTab; 2],
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let service = sessions.clone();
        let load = executor
            .run(move |cancellation| async move { service.load(cancellation).await })
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        match load {
            WorkspaceSessionLoad::Loaded { document, .. } => {
                let snapshot = document
                    .workspace_snapshot()
                    .map_err(|error| io::Error::other(format!("{error:?}")))?;
                if tabs.iter().all(|expected| snapshot.terminal_tabs.iter().any(|tab| matches!(tab, WorkspaceSessionTab::SftpTransfer { id, left_host_id: Some(left), right_host_id: Some(right), locked_left_host_id: None, initial_left_path: Some(left_path), initial_right_path: Some(right_path), .. } if id == &expected.id && left == &expected.left.host && right == &expected.right.host && left_path == &expected.left.path && right_path == &expected.right.path))) { return Ok(()); }
            }
            WorkspaceSessionLoad::Missing => {}
            WorkspaceSessionLoad::Invalid { .. } | WorkspaceSessionLoad::Unsupported { .. } => {
                return Err(io::Error::other("工作区仓储格式无效").into());
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("独立标签未按预期保存").into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 标记名称按端点固定，显示行和磁盘回读必须指向同一个目录项。
///
/// @author kongweiguang
fn marker_name(index: usize) -> &'static str {
    if index == 0 {
        "restart-marker-a.txt"
    } else {
        "restart-marker-b.txt"
    }
}

/// 每个目录的正文绑定本轮运行身份与标签次序，排除读取到另一轮或另一标签的结果。
///
/// @author kongweiguang
fn marker_content(run_id: &str, index: usize) -> String {
    format!("kerminal-restart-{run_id}-{index}\n")
}

/// 路径拼接采用远端路径 API，避免把标记名称作为操作系统路径重新解释。
///
/// @author kongweiguang
fn marker_endpoint(target: &RemoteTarget, name: &str) -> Result<FileEndpoint, io::Error> {
    let endpoint = FileEndpoint::remote(target.host.clone(), &target.path)
        .map_err(|_| io::Error::other("验收目录路径无效"))?;
    let FileEndpoint::Remote { host_id, path } = endpoint else {
        return Err(io::Error::other("验收目标不是远端目录"));
    };
    Ok(FileEndpoint::Remote {
        host_id,
        path: path
            .join_name(name)
            .map_err(|_| io::Error::other("验收标记名称无效"))?,
    })
}

/// 仅创建本轮唯一名称的目录和标记；目标已存在时停止，避免触碰未知 fixture 数据。
///
/// @author kongweiguang
async fn create_directories(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    targets: &[(RemoteTarget, RemoteTarget); 2],
    run_id: &str,
) -> Result<(), Box<dyn Error>> {
    let service = files.clone();
    let entries = [targets[0].0.clone(), targets[1].0.clone()]
        .into_iter()
        .enumerate()
        .map(|(index, target)| {
            Ok((
                FileEndpoint::remote(target.host.clone(), &target.path)
                    .map_err(|_| io::Error::other("验收目录路径无效"))?,
                marker_endpoint(&target, marker_name(index))?,
                marker_content(run_id, index),
            ))
        })
        .collect::<Result<Vec<_>, io::Error>>()?;
    executor
        .run(move |cancellation| async move {
            for (endpoint, marker, content) in entries {
                service
                    .create_directory(cancellation.clone(), endpoint)
                    .await?;
                service
                    .write_text_file(
                        cancellation.clone(),
                        WriteTextRequest {
                            endpoint: marker,
                            content,
                            strategy: WriteTextStrategy::CreateNew,
                        },
                    )
                    .await?;
            }
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// 仅清理由本轮运行名推导的标记和目录，遇到额外内容时保留整个目录。
///
/// @author kongweiguang
async fn remove_directories(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    targets: &[(RemoteTarget, RemoteTarget); 2],
) -> Result<(), Box<dyn Error>> {
    let service = files.clone();
    let entries = [targets[0].0.clone(), targets[1].0.clone()]
        .into_iter()
        .enumerate()
        .map(|(index, target)| {
            Ok((
                FileEndpoint::remote(target.host.clone(), &target.path)
                    .map_err(|_| io::Error::other("验收目录路径无效"))?,
                marker_endpoint(&target, marker_name(index))?,
            ))
        })
        .collect::<Result<Vec<_>, io::Error>>()?;
    executor
        .run(move |cancellation| async move {
            for (endpoint, marker) in entries {
                let listing = service
                    .list_directory(cancellation.clone(), endpoint.clone())
                    .await?;
                if listing.entries.len() != 1
                    || listing.entries[0].endpoint != marker
                    || listing.entries[0].kind != FileEntryKind::File
                {
                    return Err(AppError::conflict("验收目录内容与本轮标记不一致，拒绝清理"));
                }
                service
                    .delete(cancellation.clone(), marker, DeleteTargetKind::File)
                    .await?;
                service
                    .delete(cancellation.clone(), endpoint, DeleteTargetKind::Directory)
                    .await?;
            }
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// 主机行右键后按无障碍标签点击菜单，覆盖真实窗口事件和生产预检链路。
///
/// @author kongweiguang
async fn create_tab_from_host_menu(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
) -> Result<TabId, Box<dyn Error>> {
    let before = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_sftp_workspace_tab_ids(host)
    })?;
    let selector = format!("host-navigation-host-{host}");
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let found = window.update(cx, |_, window, app| {
            let id: ElementId = selector.clone().into();
            let Some(node) =
                gpui_kit::base::test_support::find(window, &[], &id).filter(|node| node.visible())
            else {
                return false;
            };
            let position = node.bounds().center();
            for event in [
                MouseMoveEvent {
                    position,
                    ..Default::default()
                }
                .to_platform_input(),
                MouseDownEvent {
                    button: MouseButton::Right,
                    position,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input(),
                MouseUpEvent {
                    button: MouseButton::Right,
                    position,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input(),
            ] {
                let _ = window.dispatch_event(event, app);
            }
            true
        })?;
        if found {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("fixture 主机行未显示").into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
    workspace::wait_selector(cx, window, "rr-popup-menu").await?;
    window.update(cx, |_, window, app| {
        let nodes = gpui_kit::base::test_support::snapshots(window);
        let matching = nodes
            .iter()
            .filter(|node| {
                node.visible()
                    && node.role() == Some(gpui_kit::Role::MenuItem)
                    && matches!(node.label(), Some("新建传输标签" | "New transfer tab"))
            })
            .collect::<Vec<_>>();
        if matching.len() != 1 || matching[0].disabled() == Some(true) {
            return Err(io::Error::other("主机菜单缺少可用的唯一新建传输标签命令"));
        }
        let position = matching[0].bounds().center();
        for event in [
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    functional::poll_root(
        cx,
        root,
        window,
        "主机菜单新建传输标签",
        |root, _, _| {
            let created = root
                .right_rail_acceptance_sftp_workspace_tab_ids(host)
                .into_iter()
                .filter(|id| !before.contains(id))
                .collect::<Vec<_>>();
            if created.len() > 1 {
                return Err("主机菜单创建了多个标签".to_owned());
            }
            Ok(created
                .into_iter()
                .next()
                .filter(|id| root.right_rail_acceptance_sftp_workspace_tab_selected(host, id)))
        },
    )
    .await
}

#[cfg(test)]
#[path = "../../tests/unit/right_rail_transfer_restart.rs"]
mod tests;
