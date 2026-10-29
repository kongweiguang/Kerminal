//! 隔离配置根上的 Profile 设置真实控件和持久化验收。
//!
//! @author kongweiguang

use super::*;

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
    Ok([
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

/// 文件 watcher 的保存回声可能在持久化读回之后暂时禁用字段；等待真实控件可编辑再输入，
/// 不向禁用控件盲发按键，也不重放任何保存或默认切换请求。
///
/// @author kongweiguang
async fn fill_profile_field(
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
async fn read_profile(
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
