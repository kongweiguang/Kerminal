//! 仅在私有 Windows 桌面运行的真实系统拖出验收入口。
//!
//! @author kongweiguang

use super::*;

/// 创建一次性配置根并要求 Win32 先证明当前线程属于非输入私有桌面。
///
/// @author kongweiguang
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let isolation = isolation::capture()?;
    if !isolation.isolation_proven {
        return Err(
            io::Error::other("真实拖拽验收必须从非输入的 KerminalRail 私有桌面启动").into(),
        );
    }
    let fixture_run_id = std::env::var("KERMINAL_DRAG_FIXTURE_RUN_ID")?;
    if fixture_run_id.len() != 12
        || !fixture_run_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::other("拖拽验收 fixture 身份无效").into());
    }
    let options = parse_options(std::env::args_os().skip(2))?;
    if options.capture_only || options.functional_only || options.sftp_functional_only {
        return Err(io::Error::other("拖拽专项不支持其他验收子模式").into());
    }
    let manifest = read_fixture_manifest()?;
    if manifest.remote_sftp_directory
        != format!("/run/kerminal-right-rail-{fixture_run_id}/home/sftp")
    {
        return Err(io::Error::other("拖拽验收目录与 fixture 身份不匹配").into());
    }
    let fixture_source =
        resolve_fixture_config_directory(options.fixture_config_directory, &manifest)?;
    let output = options
        .output_directory
        .ok_or_else(|| io::Error::other("拖拽专项必须提供全新输出目录"))?;
    reject_user_configuration_output_path(&output)?;
    reject_fixture_output_overlap(&fixture_source, &output)?;
    let output = create_requested_output_directory(&output)?;
    let config = output.join("config");
    fs::create_dir(&config)?;
    copy_fixture_configuration(&fixture_source, &config)?;
    let fixture_host = HostId::parse(FIXTURE_HOST_ID)?;

    let runtime = ApplicationRuntime::new()?;
    let mut composition = runtime.block_on(DesktopComposition::build(runtime.handle(), &config))?;
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_window(
            composition.app_services(),
            output.clone(),
            fixture_host,
            manifest.remote_sftp_directory,
            fixture_run_id,
            options.window_width,
            options.window_height,
        )
    }))
    .unwrap_or_else(|_| Err(io::Error::other("真实拖拽验收事件循环意外终止").into()));
    let shutdown = runtime
        .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
    runtime.shutdown();
    result?;
    shutdown?;
    Ok(())
}

/// 普通应用窗口不受影响；仅此入口在已验证的私有桌面创建可见窗口供系统拖放使用。
///
/// @author kongweiguang
fn run_window(
    services: kerminal_ui::AppServices,
    output: PathBuf,
    fixture_host: HostId,
    remote_directory: String,
    fixture_run_id: String,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(), Box<dyn Error>> {
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let bounds = match acceptance_window_bounds(cx, width, height) {
                Ok(bounds) => bounds,
                Err(error) => {
                    set_failure(&failure_for_app, error.to_string());
                    cx.quit();
                    return;
                }
            };
            let options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: bounds,
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal Drag Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: true,
                focus: true,
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                let root = cx.new(|cx| {
                    let mut root = RootView::new(services.clone(), window, cx);
                    root.enable_right_rail_acceptance_activity(cx);
                    root
                });
                let handle = window.window_handle();
                let owner = root.clone();
                let run_failure = Arc::clone(&failure_for_app);
                cx.spawn(async move |cx| {
                    if let Err(error) = run_ready_loop(
                        cx,
                        owner,
                        handle,
                        output,
                        fixture_host,
                        remote_directory,
                        fixture_run_id,
                    )
                    .await
                    {
                        set_failure(&run_failure, error.to_string());
                    }
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = opened {
                set_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });
    let message = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(message) = message {
        return Err(io::Error::other(message).into());
    }
    Ok(())
}

/// 通过生产 Root 建立 fixture 连接并暴露本轮文件行实际几何，驱动器据此发送系统输入。
///
/// @author kongweiguang
async fn run_ready_loop(
    cx: &mut AsyncApp,
    root: Entity<RootView>,
    window: AnyWindowHandle,
    output: PathBuf,
    fixture_host: HostId,
    remote_directory: String,
    fixture_run_id: String,
) -> Result<(), Box<dyn Error>> {
    wait_for_fixture_connection(cx, &root, window, &fixture_host).await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, root_cx)
        });
        // 先绘制一次，SFTP 面板由生产 Root 在本帧惰性装配。
        let clear = window.draw(app);
        clear.clear(app);
    })?;
    window
        .update(cx, |_, _, app| {
            root.update(app, |root, root_cx| {
                root.navigate_right_rail_acceptance_sftp_directory(
                    &fixture_host,
                    &remote_directory,
                    root_cx,
                )
            })
        })?
        .map_err(io::Error::other)?;
    let deadline = Instant::now() + FIXTURE_READY_TIMEOUT;
    let names = [
        format!("drag-a-{fixture_run_id}.txt"),
        format!("drag-b-{fixture_run_id}.txt"),
        format!("drag-dir-{fixture_run_id}"),
        format!("drag-large-{fixture_run_id}.bin"),
    ];
    let ready = loop {
        let state = window
            .update(cx, |_, window, app| {
                let ready = root.update(app, |root, root_cx| {
                    root.right_rail_acceptance_sftp_directory_ready(
                        &fixture_host,
                        &remote_directory,
                        root_cx,
                    )
                });
                let clear = window.draw(app);
                clear.clear(app);
                ready.map(|ready| {
                    let rows = names
                        .iter()
                        .map(|name| {
                            window
                                .debug_selector_bounds(&format!("files-row-1-{name}"))
                                .map(|bounds| {
                                    (bounds.center().x.as_f32(), bounds.center().y.as_f32())
                                })
                        })
                        .collect::<Option<Vec<_>>>();
                    (ready, rows, window.scale_factor())
                })
            })?
            .map_err(io::Error::other)?;
        if let (true, Some(rows), scale) = state {
            break (rows, scale);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待拖拽 fixture 文件行超时").into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    };
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "pid": std::process::id(),
        "fixtureRunId": fixture_run_id,
        "scale": ready.1,
        "rows": names.into_iter().zip(ready.0).map(|(name, (x, y))| {
            serde_json::json!({ "name": name, "x": x, "y": y })
        }).collect::<Vec<_>>(),
    }))?;
    fs::write(output.join("ready.json"), bytes)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if output.join("done").is_file() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待拖拽驱动器回执超时").into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}
