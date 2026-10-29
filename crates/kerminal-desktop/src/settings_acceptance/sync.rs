//! Native acceptance for the production settings sync action against an isolated local Git remote.
//!
//! @author kongweiguang

use super::*;
use std::{
    ffi::OsString,
    process::{Output, Stdio},
};

use kerminal_application::configuration_sync::{
    ConfigurationSyncOperationKind as SyncKind, ConfigurationSyncOutcome as SyncOutcome,
    ConfigurationSyncRunResult, ConfigurationSyncStatus,
};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const BASELINE_LOCAL_MARKER: &str = "settings acceptance local marker baseline\n";
const REMOTE_MARKER: &str = "settings acceptance remote marker\n";
const LOCAL_MARKER: &str = "settings acceptance local marker\n";

#[derive(Clone)]
pub(super) struct SyncFixture {
    root: PathBuf,
    bare_remote: PathBuf,
    peer: PathBuf,
    unavailable_remote: PathBuf,
    global_git_config: PathBuf,
}

/// Isolates the desktop's actual configuration root before making its Git history entirely local.
///
/// @author kongweiguang
pub(super) async fn prepare(
    services: &kerminal_ui::AppServices,
    output_directory: &Path,
) -> Result<SyncFixture, Box<dyn Error>> {
    let sync_service = services
        .configuration_sync
        .clone()
        .ok_or_else(|| io::Error::other("configuration sync service is unavailable"))?;
    let workspace = sync_service
        .workspace_info()
        .await?
        .ok_or_else(|| io::Error::other("configuration sync workspace path is unavailable"))?;
    let output_root = output_directory.canonicalize()?;
    let expected_root = output_root.join("config").canonicalize()?;
    // Canonical paths prove ownership, while Git for Windows needs ordinary paths rather than Win32's verbatim prefix.
    let root = PathBuf::from(workspace.workspace_root);
    let canonical_root = root.canonicalize()?;
    if canonical_root != expected_root || !canonical_root.starts_with(&output_root) {
        return Err(io::Error::other(
            "configuration sync workspace is outside this acceptance run",
        )
        .into());
    }

    let settings_path = root.join("settings.toml");
    if !settings_path.is_file() {
        let preferences = services.preferences.clone();
        let initial = services
            .executor
            .run(move |_| async move { preferences.load_settings().await })
            .await?;
        let preferences = services.preferences.clone();
        services
            .executor
            .run(move |_| async move {
                preferences
                    .save_settings(initial.settings, initial.revision)
                    .await
            })
            .await?;
    }
    if root.join(".git").exists() {
        return Err(io::Error::other(
            "acceptance configuration root already contains Git metadata",
        )
        .into());
    }

    let fixture_directory = root.join(".acceptance-sync");
    fs::create_dir(&fixture_directory)?;
    let global_git_config = fixture_directory.join("global.gitconfig");
    // Run-specific diagnostics paths can exceed Win32 MAX_PATH when Git adds template hook names.
    fs::write(&global_git_config, "[core]\n\tlongpaths = true\n")?;
    let empty_hooks = fixture_directory.join("empty-hooks");
    fs::create_dir(&empty_hooks)?;
    fs::write(
        root.join("acceptance-local-marker.txt"),
        BASELINE_LOCAL_MARKER,
    )?;
    ensure_fixture_is_ignored(&root)?;

    let fixture = SyncFixture {
        root: root.clone(),
        bare_remote: fixture_directory.join("origin.git"),
        peer: fixture_directory.join("peer"),
        unavailable_remote: fixture_directory.join("unavailable.git"),
        global_git_config,
    };
    fs::create_dir(&fixture.bare_remote)?;
    checked_git(
        &root,
        git_args(&["--version"]),
        &fixture.global_git_config,
        "checking Git availability",
    )
    .await?;
    checked_git(
        &fixture.bare_remote,
        git_args(&["init", "--bare", "--quiet"]),
        &fixture.global_git_config,
        "initializing the local bare remote",
    )
    .await?;
    checked_git(
        &fixture.bare_remote,
        git_args(&["symbolic-ref", "HEAD", "refs/heads/main"]),
        &fixture.global_git_config,
        "selecting the fixture remote branch",
    )
    .await?;

    checked_git(
        &root,
        git_args(&["init", "--quiet"]),
        &fixture.global_git_config,
        "initializing the isolated configuration repository",
    )
    .await?;
    configure_fixture_repository(&root, &empty_hooks, &fixture.global_git_config).await?;
    checked_git(
        &root,
        git_args(&["symbolic-ref", "HEAD", "refs/heads/main"]),
        &fixture.global_git_config,
        "selecting the local fixture branch",
    )
    .await?;
    checked_git(
        &root,
        git_args(&["add", "--force", "--", "acceptance-local-marker.txt"]),
        &fixture.global_git_config,
        "tracking the local fixture marker",
    )
    .await?;
    // Use the production allowlist and managed ignores so the fixture never tracks local keys or runtime files.
    let baseline = sync_service
        .commit(tokio_util::sync::CancellationToken::new())
        .await?;
    if baseline.outcome != SyncOutcome::Succeeded {
        return Err(io::Error::other(format!(
            "fixture baseline commit failed: {:?}",
            baseline.outcome
        ))
        .into());
    }
    let mut origin_args = git_args(&["remote", "add", "origin"]);
    origin_args.push(fixture.bare_remote.as_os_str().to_os_string());
    checked_git(
        &root,
        origin_args,
        &fixture.global_git_config,
        "adding the local bare remote",
    )
    .await?;
    checked_git(
        &root,
        git_args(&["push", "--quiet", "--set-upstream", "origin", "main"]),
        &fixture.global_git_config,
        "publishing the baseline to the local bare remote",
    )
    .await?;

    let mut clone_args = git_args(&["clone", "--quiet"]);
    clone_args.push(fixture.bare_remote.as_os_str().to_os_string());
    clone_args.push(fixture.peer.as_os_str().to_os_string());
    checked_git(
        &fixture_directory,
        clone_args,
        &fixture.global_git_config,
        "cloning the local bare remote for the peer change",
    )
    .await?;
    configure_fixture_repository(&fixture.peer, &empty_hooks, &fixture.global_git_config).await?;
    let peer_marker = fixture.peer.clone();
    fs::create_dir_all(&peer_marker)?;
    fs::write(
        peer_marker.join("acceptance-remote-marker.txt"),
        REMOTE_MARKER,
    )?;
    checked_git(
        &fixture.peer,
        git_args(&["add", "--force", "--", "acceptance-remote-marker.txt"]),
        &fixture.global_git_config,
        "staging the peer marker",
    )
    .await?;
    checked_git(
        &fixture.peer,
        git_args(&["commit", "--quiet", "-m", "settings acceptance peer change"]),
        &fixture.global_git_config,
        "committing the peer marker",
    )
    .await?;
    checked_git(
        &fixture.peer,
        git_args(&["push", "--quiet", "origin", "main"]),
        &fixture.global_git_config,
        "publishing the peer marker to the local bare remote",
    )
    .await?;

    sync_service.status().await.map_err(|error| {
        io::Error::other(format!(
            "production Git cannot read the prepared fixture: {error:?}"
        ))
    })?;
    Ok(fixture)
}

/// Drives the production modal button through commit/pull/push, a failed fetch, and a confirmed retry.
///
/// @author kongweiguang
pub(super) async fn verify(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let fixture = &context.sync_fixture;
    ensure_page(cx, root, window, "appearance", "appearance").await?;
    let current = load_persisted_settings(services).await?.settings.theme_mode;
    let target = if current == ThemeMode::Dark {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    let target_id = theme_mode_id(target);
    let theme_selector = format!("theme-{target_id}");
    reveal_settings_selector(cx, root, window, "appearance", &theme_selector).await?;
    click_selector(cx, window, &theme_selector)?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "saving a local settings change before synchronization",
        |state| {
            state.draft_ready()
                && !state.draft_dirty()
                && state.draft_theme_mode() == Some(target_id)
        },
    )
    .await?;
    let persisted = load_persisted_settings(services).await?;
    if persisted.settings.theme_mode != target {
        return Err(io::Error::other("local settings change did not persist").into());
    }

    ensure_page(cx, root, window, "sync", "sync").await?;
    let success = click_sync_and_wait(cx, root, window).await?;
    let success_status = load_sync_status(services).await?;
    let local_head = read_branch_oid(services, &fixture.root.join(".git")).await?;
    let remote_head = read_branch_oid(services, &fixture.bare_remote).await?;
    let remote_marker =
        read_fixture_text(services, &fixture.root.join("acceptance-remote-marker.txt")).await?;
    let persisted = load_persisted_settings(services).await?;
    let success_stages = stage_kinds(&success);
    let success_passed = success.outcome == SyncOutcome::Succeeded
        && success_stages == vec![SyncKind::Commit, SyncKind::Pull, SyncKind::Push]
        && success_status.local_changes.is_empty()
        && success_status.ahead == Some(0)
        && success_status.behind == Some(0)
        && local_head == remote_head
        && remote_marker == REMOTE_MARKER
        && persisted.settings.theme_mode == target;
    let success_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-sync-local-remote-roundtrip.png"),
    )?;
    screenshots.push(success_capture.clone());
    let success_check = sync_check(
        "sync-local-remote-roundtrip",
        success_passed,
        "Commit, Pull, Push; clean status; local and bare HEAD match; peer marker and settings read back",
        format!(
            "outcome={:?}; stages={success_stages:?}; local_changes={}; ahead={:?}; behind={:?}; heads_match={}; marker_matches={}; theme={}",
            success.outcome,
            success_status.local_changes.len(),
            success_status.ahead,
            success_status.behind,
            local_head == remote_head,
            remote_marker == REMOTE_MARKER,
            theme_mode_id(persisted.settings.theme_mode),
        ),
        success_capture,
    );

    let remote_before_failure = remote_head;
    write_fixture_text(
        services,
        &fixture.root.join("acceptance-local-marker.txt"),
        LOCAL_MARKER,
    )
    .await?;
    set_origin_from_ui(services, fixture, &fixture.unavailable_remote).await?;
    let failure = click_sync_and_wait(cx, root, window).await?;
    let failure_status = load_sync_status(services).await?;
    let failure_local_head = read_branch_oid(services, &fixture.root.join(".git")).await?;
    let failure_remote_head = read_branch_oid(services, &fixture.bare_remote).await?;
    let local_marker =
        read_fixture_text(services, &fixture.root.join("acceptance-local-marker.txt")).await?;
    let failure_stages = stage_kinds(&failure);
    let failure_passed = failure.outcome == SyncOutcome::Blocked
        && failure_stages == vec![SyncKind::Commit]
        && failure
            .stages
            .first()
            .is_some_and(|stage| stage.outcome == SyncOutcome::Succeeded)
        && local_marker == LOCAL_MARKER
        && failure_local_head != remote_before_failure
        && failure_remote_head == remote_before_failure
        && failure_status.local_changes.is_empty()
        && failure_status.ahead == Some(1)
        && failure_status.behind == Some(0);
    let failure_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-sync-failure-preserves-local.png"),
    )?;
    screenshots.push(failure_capture.clone());
    let failure_check = sync_check(
        "sync-failure-preserves-local",
        failure_passed,
        "failed fetch keeps the successful local commit and leaves the bare remote unchanged",
        format!(
            "outcome={:?}; stages={failure_stages:?}; local_marker_matches={}; local_commit_preserved={}; remote_unchanged={}; ahead={:?}; behind={:?}",
            failure.outcome,
            local_marker == LOCAL_MARKER,
            failure_local_head != remote_before_failure,
            failure_remote_head == remote_before_failure,
            failure_status.ahead,
            failure_status.behind,
        ),
        failure_capture,
    );

    set_origin_from_ui(services, fixture, &fixture.bare_remote).await?;
    let retry = click_sync_and_wait(cx, root, window).await?;
    let retry_status = load_sync_status(services).await?;
    let retry_local_head = read_branch_oid(services, &fixture.root.join(".git")).await?;
    let retry_remote_head = read_branch_oid(services, &fixture.bare_remote).await?;
    let retry_local_marker =
        read_fixture_text(services, &fixture.root.join("acceptance-local-marker.txt")).await?;
    let retry_remote_marker =
        read_fixture_text(services, &fixture.root.join("acceptance-remote-marker.txt")).await?;
    let retry_stages = stage_kinds(&retry);
    let retry_passed = retry.outcome == SyncOutcome::Succeeded
        && retry_stages == vec![SyncKind::Pull, SyncKind::Push]
        && !retry_stages.contains(&SyncKind::Commit)
        && retry_local_head == failure_local_head
        && retry_remote_head == retry_local_head
        && retry_local_marker == LOCAL_MARKER
        && retry_remote_marker == REMOTE_MARKER
        && retry_status.local_changes.is_empty()
        && retry_status.ahead == Some(0)
        && retry_status.behind == Some(0);
    let retry_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-sync-retry-confirms-remote.png"),
    )?;
    screenshots.push(retry_capture.clone());
    let retry_check = sync_check(
        "sync-retry-confirms-remote",
        retry_passed,
        "retry runs Pull and Push without another Commit, then local and bare HEAD match",
        format!(
            "outcome={:?}; stages={retry_stages:?}; commit_not_repeated={}; heads_match={}; local_changes={}; ahead={:?}; behind={:?}; markers_match={}",
            retry.outcome,
            retry_local_head == failure_local_head,
            retry_local_head == retry_remote_head,
            retry_status.local_changes.len(),
            retry_status.ahead,
            retry_status.behind,
            retry_local_marker == LOCAL_MARKER && retry_remote_marker == REMOTE_MARKER,
        ),
        retry_capture,
    );

    Ok(vec![success_check, failure_check, retry_check])
}

/// Runs Git without a shell, under the acceptance-only config and a kill-on-timeout Windows-safe process policy.
///
/// @author kongweiguang
async fn run_git(
    working_directory: &Path,
    arguments: &[OsString],
    global_git_config: &Path,
) -> io::Result<Output> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(arguments)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .env("GIT_CONFIG_GLOBAL", global_git_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.as_std_mut().creation_flags(0x0800_0000);
    }
    tokio::time::timeout(GIT_COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Git fixture command timed out"))?
}

/// Gives each temporary commit an explicit identity and disables hooks outside the fixture directory.
///
/// @author kongweiguang
async fn configure_fixture_repository(
    repository: &Path,
    empty_hooks: &Path,
    global_git_config: &Path,
) -> Result<(), Box<dyn Error>> {
    for (arguments, operation) in [
        (
            git_args(&["config", "user.name", "Kerminal Acceptance"]),
            "setting fixture author",
        ),
        (
            git_args(&["config", "user.email", "acceptance@example.invalid"]),
            "setting fixture email",
        ),
        (
            git_args(&["config", "commit.gpgSign", "false"]),
            "disabling fixture signing",
        ),
    ] {
        checked_git(repository, arguments, global_git_config, operation).await?;
    }
    let mut hooks_args = git_args(&["config", "core.hooksPath"]);
    hooks_args.push(empty_hooks.as_os_str().to_os_string());
    checked_git(
        repository,
        hooks_args,
        global_git_config,
        "isolating fixture hooks",
    )
    .await?;
    Ok(())
}

/// Restores or removes reachability without letting Git commands leave the application runtime.
///
/// @author kongweiguang
async fn set_origin_from_ui(
    services: &kerminal_ui::AppServices,
    fixture: &SyncFixture,
    remote: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut arguments = git_args(&["remote", "set-url", "origin"]);
    arguments.push(remote.as_os_str().to_os_string());
    checked_git_from_ui(
        services,
        fixture,
        &fixture.root,
        arguments,
        "setting the fixture origin",
    )
    .await?;
    Ok(())
}

/// Keeps the generated fixture repositories outside the synchronizable path set.
///
/// @author kongweiguang
fn ensure_fixture_is_ignored(root: &Path) -> io::Result<()> {
    let path = root.join(".gitignore");
    let mut contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if !contents
        .lines()
        .any(|line| line.trim() == ".acceptance-sync/")
    {
        if !contents.is_empty() && !contents.ends_with('\n') && !contents.ends_with('\r') {
            contents.push('\n');
        }
        contents.push_str(".acceptance-sync/\n");
        fs::write(path, contents)?;
    }
    Ok(())
}

/// Converts fixed Git arguments to owned values so paths can be appended without lossy encoding.
///
/// @author kongweiguang
fn git_args(arguments: &[&str]) -> Vec<OsString> {
    arguments.iter().map(OsString::from).collect()
}

/// Turns unexpected Git exit codes into compact fixture diagnostics without echoing config paths.
///
/// @author kongweiguang
async fn checked_git(
    working_directory: &Path,
    arguments: Vec<OsString>,
    global_git_config: &Path,
    operation: &str,
) -> Result<Output, Box<dyn Error>> {
    let output = run_git(working_directory, &arguments, global_git_config).await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Git fixture operation failed: {operation} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(2000)
                .collect::<String>()
        ))
        .into());
    }
    Ok(output)
}

/// Sends fixture Git commands through the same Tokio executor used by production panel work.
///
/// @author kongweiguang
async fn checked_git_from_ui(
    services: &kerminal_ui::AppServices,
    fixture: &SyncFixture,
    working_directory: &Path,
    arguments: Vec<OsString>,
    operation: &'static str,
) -> Result<Output, Box<dyn Error>> {
    let working_directory = working_directory.to_path_buf();
    let global_git_config = fixture.global_git_config.clone();
    let output = services
        .executor
        .run(move |_| async move {
            Ok(run_git(&working_directory, &arguments, &global_git_config).await)
        })
        .await??;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Git fixture operation failed: {operation} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(2000)
                .collect::<String>()
        ))
        .into());
    }
    Ok(output)
}

/// Waits for a new settled result so an old operation receipt cannot satisfy the next button click.
///
/// @author kongweiguang
async fn click_sync_and_wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<ConfigurationSyncRunResult, Box<dyn Error>> {
    ensure_page(cx, root, window, "sync", "sync").await?;
    reveal_settings_selector(cx, root, window, "sync", "workspace-sync-run").await?;
    let previous = root_state(cx, root, window)?.sync_result().cloned();
    click_selector(cx, window, "workspace-sync-run")?;
    let state = wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "waiting for the production synchronization action",
        |state| {
            !state.sync_busy()
                && (state.sync_error().is_some()
                    || state
                        .sync_result()
                        .is_some_and(|result| previous.as_ref() != Some(result)))
        },
    )
    .await?;
    if let Some(error) = state.sync_error() {
        return Err(io::Error::other(format!("settings sync failed to start: {error}")).into());
    }
    state
        .sync_result()
        .cloned()
        .ok_or_else(|| io::Error::other("settings sync returned no operation receipt").into())
}

/// Reads the settled repository status through the application executor instead of blocking GPUI.
///
/// @author kongweiguang
async fn load_sync_status(
    services: &kerminal_ui::AppServices,
) -> Result<ConfigurationSyncStatus, Box<dyn Error>> {
    let service = services
        .configuration_sync
        .clone()
        .ok_or_else(|| io::Error::other("configuration sync service is unavailable"))?;
    Ok(services
        .executor
        .run(move |_| async move { service.status().await })
        .await?)
}

/// Reads loose or packed branch refs through the application runtime for independent remote proof.
///
/// @author kongweiguang
async fn read_branch_oid(
    services: &kerminal_ui::AppServices,
    git_directory: &Path,
) -> Result<String, Box<dyn Error>> {
    let git_directory = git_directory.to_path_buf();
    Ok(services
        .executor
        .run(move |_| async move {
            let reference = "refs/heads/main";
            Ok(async move {
                match tokio::fs::read_to_string(git_directory.join(reference)).await {
                    Ok(value) => Ok(value.trim().to_owned()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let packed_refs =
                            tokio::fs::read_to_string(git_directory.join("packed-refs")).await?;
                        packed_refs
                            .lines()
                            .filter_map(|line| {
                                let mut fields = line.split_ascii_whitespace();
                                Some((fields.next()?, fields.next()?))
                            })
                            .find_map(|(oid, name)| (name == reference).then(|| oid.to_owned()))
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::NotFound,
                                    "fixture branch ref is missing",
                                )
                            })
                    }
                    Err(error) => Err(error),
                }
            }
            .await)
        })
        .await??)
}

/// Uses Tokio file operations inside the production executor so the GPUI future needs no reactor.
///
/// @author kongweiguang
async fn read_fixture_text(
    services: &kerminal_ui::AppServices,
    path: &Path,
) -> Result<String, Box<dyn Error>> {
    let path = path.to_path_buf();
    Ok(services
        .executor
        .run(move |_| async move { Ok(tokio::fs::read_to_string(path).await) })
        .await??)
}

/// Writes only a marker under the run-scoped config root to create a deterministic local Git change.
///
/// @author kongweiguang
async fn write_fixture_text(
    services: &kerminal_ui::AppServices,
    path: &Path,
    content: &'static str,
) -> Result<(), Box<dyn Error>> {
    let path = path.to_path_buf();
    services
        .executor
        .run(move |_| async move { Ok(tokio::fs::write(path, content).await) })
        .await??;
    Ok(())
}

/// Exposes Git stage order separately from the on-screen summary, which can describe partial success.
///
/// @author kongweiguang
fn stage_kinds(result: &ConfigurationSyncRunResult) -> Vec<SyncKind> {
    result.stages.iter().map(|stage| stage.kind).collect()
}

/// Builds report evidence for each recovery boundary without converting an expected failure into a test abort.
///
/// @author kongweiguang
fn sync_check(
    action: &'static str,
    passed: bool,
    expected: &str,
    observed: String,
    capture: CaptureRecord,
) -> SettingCheck {
    let expected = expected.to_owned();
    SettingCheck {
        action,
        passed,
        observed: if passed { expected.clone() } else { observed },
        expected,
        capture: Some(capture),
    }
}
