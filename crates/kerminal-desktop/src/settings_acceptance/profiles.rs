//! 隔离配置根上的 Profile 设置真实控件和持久化验收。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::component::WindowExt as _;
use kerminal_application::preferences::ProfileDraft;

/// 新建、修改和设置默认均经生产界面操作，再从同一 ProfileService 读回确认。
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
    let result = verify_flow(cx, root, window, services, context, screenshots).await;
    if result.is_err() {
        let _ = save_frame(
            cx,
            window,
            context
                .output_directory
                .join("settings-profile-failure.png"),
        );
    }
    result
}

/// 失败阶段单独标记，避免将控件输入、保存回执和默认事务的超时混为同一个问题。
///
/// @author kongweiguang
async fn verify_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    ensure_page(cx, root, window, "profiles", "profiles").await?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-list-ready",
        |state| state.profile_ready(),
    )
    .await?;
    click_selector(cx, window, "profile-create")?;
    let created = wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-create-draft",
        |state| state.profile_id().is_some() && state.profile_name() == Some(""),
    )
    .await?;
    let id = created
        .profile_id()
        .ok_or_else(|| io::Error::other("new Profile has no stable ID"))?
        .to_owned();
    fill_profile_field(cx, root, window, "profile-name-field", "Acceptance Profile").await?;
    fill_profile_field(cx, root, window, "profile-shell-field", "cmd.exe").await?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-input",
        |state| {
            state.profile_id() == Some(id.as_str())
                && state.profile_name() == Some("Acceptance Profile")
                && state.profile_shell() == Some("cmd.exe")
        },
    )
    .await?;
    reveal_settings_selector(cx, root, window, "profiles", "profile-save").await?;
    click_selector(cx, window, "profile-save")?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-save",
        |state| {
            state.profile_id() == Some(id.as_str())
                && state.profile_name() == Some("Acceptance Profile")
                && state.profile_ready()
        },
    )
    .await?;
    let first = read_profile(services, &id).await?;
    if first.profile.name != "Acceptance Profile" || first.profile.shell != "cmd.exe" {
        return Err(io::Error::other("new Profile readback mismatch").into());
    }
    fill_profile_field(cx, root, window, "profile-name-field", "Edited Profile").await?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-edit-input",
        |state| state.profile_name() == Some("Edited Profile"),
    )
    .await?;
    reveal_settings_selector(cx, root, window, "profiles", "profile-save").await?;
    click_selector(cx, window, "profile-save")?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-edit-save",
        |state| {
            state.profile_id() == Some(id.as_str())
                && state.profile_name() == Some("Edited Profile")
                && state.profile_ready()
        },
    )
    .await?;
    let edited = read_profile(services, &id).await?;
    if edited.profile.name != "Edited Profile" || edited.revision == first.revision {
        return Err(io::Error::other("edited Profile readback mismatch").into());
    }
    reveal_settings_selector(cx, root, window, "profiles", "profile-set-default").await?;
    click_selector(cx, window, "profile-set-default")?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-set-default",
        |state| {
            state.profile_id() == Some(id.as_str())
                && state.profile_is_default()
                && state.profile_ready()
        },
    )
    .await?;
    let default = read_profile(services, &id).await?;
    if !default.profile.is_default {
        return Err(io::Error::other("default Profile readback mismatch").into());
    }
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-profile.png"),
    )?;
    screenshots.push(capture.clone());
    verify_rescan(cx, root, window, services, context, screenshots).await?;
    let profile = format!("profile-select-{id}");
    wait_for_profile_scan_control(cx, window, &profile, true).await?;
    click_selector(cx, window, &profile)?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-after-rescan",
        |state| state.profile_id() == Some(id.as_str()) && state.profile_ready(),
    )
    .await?;
    Ok([
        ("rescan-profile-readback", "added-and-confirmed"),
        ("create-profile-readback", "created"),
        ("edit-profile-readback", "edited"),
        ("set-default-profile-readback", "default"),
    ]
    .into_iter()
    .map(|(action, result)| SettingCheck {
        action,
        passed: true,
        expected: result.to_owned(),
        observed: result.to_owned(),
        capture: None,
    })
    .collect())
}

/// 用隔离仓储模拟新增 Shell 和本地自定义参数，验证扫描、取消确认和显式覆盖。
/// @author kongweiguang
async fn verify_rescan(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<(), Box<dyn Error>> {
    let profiles = services.profiles.clone();
    let (custom, added_id, expected_args) = services
        .executor
        .run(move |cancel| async move {
            let mut candidates = profiles.discover_local_shells(cancel).await?;
            let rows = profiles.list_profiles().await?;
            candidates.sort_by_key(|candidate| {
                let id = candidate.standard_profile_id();
                !rows
                    .iter()
                    .any(|row| row.profile.id == id && row.profile.is_default)
            });
            let mut eligible = candidates.into_iter();
            let changed = eligible.next().ok_or_else(|| {
                kerminal_application::AppError::not_found("验收缺少第一个系统 Shell")
            })?;
            let added = eligible.next().ok_or_else(|| {
                kerminal_application::AppError::not_found("验收缺少第二个系统 Shell")
            })?;
            let added_id = added.standard_profile_id();
            if let Some(row) = rows.iter().find(|row| row.profile.id == added_id) {
                profiles
                    .delete_profile(&added_id, row.revision.clone())
                    .await?;
            }
            let id = changed.standard_profile_id();
            let revision = rows
                .iter()
                .find(|row| row.profile.id == id)
                .map(|row| row.revision.clone());
            let custom = profiles
                .save_profile(
                    ProfileDraft {
                        id: Some(id),
                        name: "Custom scan fixture".into(),
                        shell: changed.program().to_string_lossy().into_owned(),
                        args: vec!["kerminal-custom-argument".into()],
                        cwd: Some(r"C:\Windows".into()),
                        env: std::collections::BTreeMap::from([(
                            "K_SCAN_FIXTURE".into(),
                            "keep".into(),
                        )]),
                        sidebar_group_id: None,
                        sort_order: 1,
                    },
                    revision,
                )
                .await?;
            let expected_args = changed
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            Ok((custom, added_id, expected_args))
        })
        .await?;
    wait_for_profile_scan_control(cx, window, "profile-rescan", true).await?;
    click_selector(cx, window, "profile-rescan")?;
    wait_for_profile_scan_control(cx, window, "profile-scan-review", true).await?;
    let added = read_profile(services, &added_id).await?;
    if added.profile.is_default || read_profile(services, &custom.profile.id).await? != custom {
        return Err(io::Error::other(
            "scan changed an existing or default profile before confirmation",
        )
        .into());
    }

    let overwrite = format!("profile-scan-overwrite-{}", custom.profile.id);
    click_selector(cx, window, "profile-scan-review")?;
    wait_for_profile_scan_control(cx, window, &overwrite, true).await?;
    screenshots.push(save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-profile-rescan.png"),
    )?);
    press_key(cx, window, "escape")?;
    wait_for_profile_scan_control(cx, window, &overwrite, false).await?;
    if !root_state(cx, root, window)?.modal_open()
        || read_profile(services, &custom.profile.id).await? != custom
    {
        return Err(io::Error::other("closing scan review changed the saved profile").into());
    }
    click_selector(cx, window, "profile-scan-review")?;
    wait_for_profile_scan_control(cx, window, &overwrite, true).await?;
    click_selector(cx, window, &overwrite)?;
    wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "profile-scan-overwrite",
        |state| state.profile_id() == Some(custom.profile.id.as_str()) && state.profile_ready(),
    )
    .await?;
    let saved = read_profile(services, &custom.profile.id).await?;
    let mut expected = custom.profile;
    expected.args = expected_args;
    expected.updated_at = saved.profile.updated_at.clone();
    if saved.profile != expected || saved.revision == custom.revision {
        return Err(
            io::Error::other("confirmed scan update did not preserve profile settings").into(),
        );
    }
    let profiles = services.profiles.clone();
    let before = services
        .executor
        .run(move |_| async move { profiles.list_profiles().await })
        .await?;
    wait_for_profile_scan_control(cx, window, "profile-rescan", true).await?;
    let previous_notice = window.update(cx, |_, window, app| {
        window.notifications(app).last().map(Entity::entity_id)
    })?;
    click_selector(cx, window, "profile-rescan")?;
    // 用新的完成通知确认扫描确实执行；短任务不保证绘制出禁用态。
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let notice = window.update(cx, |_, window, app| {
            window.notifications(app).last().map(Entity::entity_id)
        })?;
        if notice.is_some() && notice != previous_notice {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("repeat scan did not publish a completion notice").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    wait_for_profile_scan_control(cx, window, "profile-rescan", true).await?;
    let profiles = services.profiles.clone();
    let after = services
        .executor
        .run(move |_| async move { profiles.list_profiles().await })
        .await?;
    if before != after {
        return Err(io::Error::other("repeat scan changed the profile directory").into());
    }
    Ok(())
}

/// 在扫描、重载和对话框都完成绘制后才发送点击，避免重复启动真实写入。
/// @author kongweiguang
async fn wait_for_profile_scan_control(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    available: bool,
) -> Result<(), Box<dyn Error>> {
    let id: ElementId = selector.to_owned().into();
    let deadline = Instant::now() + ACTION_TIMEOUT;
    while Instant::now() < deadline {
        if window.update(cx, |_, window, app| {
            window.refresh();
            draw_acceptance_frame(window, app);
            gpui_kit::base::test_support::find(window, &[], &id)
                .is_some_and(|item| item.visible() && item.disabled() != Some(true))
        })? == available
        {
            return Ok(());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Err(io::Error::other(format!(
        "scan control {selector:?} availability did not become {available}"
    ))
    .into())
}

/// 文件 watcher 的保存回声可能在持久化读回之后暂时禁用字段；等待真实控件可编辑再输入，
/// 不向禁用控件盲发按键，也不重放任何保存或默认切换请求。
///
/// @author kongweiguang
pub(super) async fn fill_profile_field(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    selector: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    reveal_settings_selector(cx, root, window, "profiles", selector).await?;
    loop {
        let state = root_state(cx, root, window)?;
        if state.profile_input_ready() {
            let expected = match selector {
                "profile-name-field" => state.profile_name_focus(),
                "profile-shell-field" => state.profile_shell_focus(),
                _ => return Err(io::Error::other("Unknown Profile input").into()),
            }
            .clone();
            click_selector(cx, window, selector)?;
            let focused = window.update(cx, |_, window, app| {
                ensure_hidden_window(window)?;
                window.refresh();
                draw_acceptance_frame(window, app);
                Ok::<_, io::Error>(window.focused(app).as_ref() == Some(&expected))
            })??;
            if focused && root_state(cx, root, window)?.profile_input_ready() {
                return click_and_type(cx, window, selector, value);
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "Profile input {selector} did not acquire editable focus"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 读回使用应用执行器和同一配置根；不从 UI 草稿推断磁盘状态。
///
/// @author kongweiguang
pub(super) async fn read_profile(
    services: &kerminal_ui::AppServices,
    id: &str,
) -> Result<kerminal_application::preferences::VersionedProfile, Box<dyn Error>> {
    let profiles = services.profiles.clone();
    let id = id.to_owned();
    services
        .executor
        .run(move |_| async move { profiles.get_profile(&id).await })
        .await?
        .ok_or_else(|| io::Error::other("Profile missing from readback").into())
}
