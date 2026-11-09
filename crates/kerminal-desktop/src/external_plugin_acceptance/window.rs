//! 两次真实进程共用的原生交互序列，成功条件来自外部运行实例而非验收 fixture。
//! @author kongweiguang

use super::*;

/// 原生窗口只运行在已证明隔离的桌面；失败截图和最终退出沿用同一个窗口句柄。
pub(super) fn run_window(
    services: AppServices,
    startup: Arc<StartupPlugins>,
    options: &Options,
    output: &Path,
) -> Result<UiResult, Box<dyn Error>> {
    let options = options.clone();
    let output = output.to_path_buf();
    let result = Arc::new(Mutex::new(None::<Result<Value, String>>));
    let result_for_app = result.clone();
    let deadline = Arc::new(Mutex::new(None::<Instant>));
    let deadline_for_app = deadline.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let Some(display) = cx.primary_display() else {
                *result_for_app
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some(Err("无可用显示设备".into()));
                cx.quit();
                return;
            };
            let bounds = Bounds::centered_at(
                display.visible_bounds().center(),
                size(px(options.width as f32), px(options.height as f32)),
            );
            let window_options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(options.width as f32), px(options.height as f32))),
                titlebar: Some(TitlebarOptions {
                    title: Some(TITLE.into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.resize(size(px(options.width as f32), px(options.height as f32)));
                let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
                cx.subscribe(&root, move |_, event: &RootViewEvent, _| {
                    if let RootViewEvent::QuitReady { deadline, .. } = event {
                        *deadline_for_app
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(*deadline);
                    }
                })
                .detach();
                let handle = window.window_handle();
                let owned_root = root.clone();
                let completed = result_for_app.clone();
                cx.spawn(async move |cx| {
                    let outcome = exercise(
                        cx,
                        &owned_root,
                        handle,
                        &services,
                        startup,
                        &options,
                        &output,
                    )
                    .await;
                    if outcome.is_err() {
                        let _ = capture_frame(
                            cx,
                            handle,
                            &output.join(format!("{}-failure.png", options.phase)),
                        )
                        .await;
                    }
                    *completed.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(outcome.map_err(|error| error.to_string()));
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = opened {
                *result_for_app
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(Err(error.to_string()));
                cx.quit();
            }
        });
    let report = result
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("原生验收未返回回执"))?
        .map_err(io::Error::other)?;
    let deadline = deadline
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("未收到生产退出截止时间"))?;
    Ok(UiResult { report, deadline })
}

/// 先证明真实窗口和生产服务就绪，再验证实例隔离、草稿、保存、设置及跨进程恢复。
/// 重启只恢复标签草稿和配置；侧栏工具沿正式导航按需挂载，不把可用性误当作自动打开承诺。
async fn exercise(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &AppServices,
    startup: Arc<StartupPlugins>,
    options: &Options,
    output: &Path,
) -> Result<Value, Box<dyn Error>> {
    let isolation = window.update(cx, |_, window, _| checked_isolation(window))??;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.enable_right_rail_acceptance_activity(cx)
        })
    })?;
    wait(
        cx,
        root,
        window,
        "工作区和正式插件目录",
        |snapshot| {
            snapshot["workspaceReady"] == true
                && snapshot["settingsReady"] == true
                && snapshot["registeredContent"] == true
        },
    )
    .await?;
    #[cfg(windows)]
    crate::settings_acceptance::prepare_capture_surface_for_title(
        cx,
        window,
        options.width,
        options.height,
        TITLE,
    )
    .await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(
                match options.theme.as_str() {
                    "light" => ThemeMode::Light,
                    "dark" => ThemeMode::Dark,
                    _ => ThemeMode::System,
                },
                cx,
            )
        })
    })?;
    // 首帧已经在 wait 中真实 draw，健康提交与普通启动使用同一存储方法和共享执行器。
    services
        .executor
        .run(move |_| async move {
            tokio::task::spawn_blocking(move || {
                startup
                    .store
                    .mark_boot_phase(
                        &startup.snapshot.boot_id,
                        kerminal_domain::plugins::PluginBootPhase::FirstFrame,
                    )
                    .map_err(|source| {
                        kerminal_application::AppError::infrastructure_with_source(
                            "外部插件首帧提交失败",
                            source,
                        )
                    })
            })
            .await
            .map_err(|source| {
                kerminal_application::AppError::infrastructure_with_source(
                    "首帧提交任务失败",
                    source,
                )
            })??;
            Ok(())
        })
        .await?;
    let mut steps = Vec::<Value>::new();
    if options.phase == "prepare" {
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.external_plugin_acceptance_open_notes(window, cx)
            })
        })??;
        let first = wait(cx, root, window, "首个笔记实例", |snapshot| {
            active(snapshot).is_some_and(|tab| tab["renderer"]["status"] == "Ready")
        })
        .await?;
        let first_tab = active(&first).ok_or_else(|| io::Error::other("首标签缺失"))?;
        let first_id = first_tab["tabId"]
            .as_str()
            .ok_or_else(|| io::Error::other("首标签身份缺失"))?
            .to_owned();
        let first_view = first_tab["viewId"].clone();
        let first_tab_id = TabId::parse(&first_id)?;
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.external_plugin_acceptance_toggle_notes_tool(window, cx)
            })
        })??;
        wait(cx, root, window, "外部工具实例", |snapshot| {
            snapshot["toolMounted"] == true && snapshot["tool"]["status"] == "Ready"
        })
        .await?;
        edit_input(cx, window, "external-field-root.0", DRAFT)?;
        let before = wait(cx, root, window, "首标签草稿", |snapshot| {
            active_text(snapshot) == Some(DRAFT)
        })
        .await?;
        steps.push(json!({"step":"draft-entered","snapshot":before}));
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.external_plugin_acceptance_open_notes(window, cx)
            })
        })??;
        wait(cx, root, window, "第二笔记实例", |snapshot| {
            snapshot["activeTabId"] != first_id
                && active(snapshot).is_some_and(|tab| tab["renderer"]["status"] == "Ready")
        })
        .await?;
        edit_input(cx, window, "external-field-root.0", SAVED)?;
        wait(cx, root, window, "第二标签草稿", |snapshot| {
            active_text(snapshot) == Some(SAVED)
        })
        .await?;
        wait_selector(cx, window, "external-button-root.1.0").await?;
        click_selector(cx, window, "external-button-root.1.0", false)?;
        wait_note_state(cx, window, services, SAVED).await?;
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.plugin_acceptance_tab_action(first_tab_id.clone(), false, window, cx)
            })
        })?;
        let returned = wait(cx, root, window, "切回首标签草稿", |snapshot| {
            snapshot["activeTabId"] == first_id && active_text(snapshot) == Some(DRAFT)
        })
        .await?;
        if active(&returned).is_none_or(|tab| tab["viewId"] != first_view) {
            return Err(io::Error::other("切换标签重建了首个外部实体").into());
        }
        steps.push(json!({"step":"draft-preserved-after-other-instance-save","snapshot":returned}));
        click_selector(cx, window, "external-button-root.1.0", false)?;
        wait_note_state(cx, window, services, DRAFT).await?;
        wait(cx, root, window, "侧栏保存结果", |snapshot| {
            snapshot["toolMounted"] == true && snapshot["tool"]["data"]["saved"] == DRAFT
        })
        .await?;
        capture_frame(cx, window, &output.join("prepare-tool-and-tab.png")).await?;
        edit_input(cx, window, "external-field-root.0", RESTART_DRAFT)?;
        wait(cx, root, window, "未提交重启草稿", |snapshot| {
            active_text(snapshot) == Some(RESTART_DRAFT)
        })
        .await?;
    } else {
        let restored = wait(cx, root, window, "新进程恢复草稿", |snapshot| {
            active_text(snapshot) == Some(RESTART_DRAFT)
        })
        .await?;
        steps.push(json!({"step":"restored-tab-draft","snapshot":restored}));
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.external_plugin_acceptance_toggle_notes_tool(window, cx)
            })
        })??;
        let tool = wait(
            cx,
            root,
            window,
            "重启后按需挂载笔记工具",
            |snapshot| snapshot["toolMounted"] == true && snapshot["tool"]["status"] == "Ready",
        )
        .await?;
        steps.push(json!({"step":"reopened-tool-after-restart","snapshot":tool}));
    }
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.external_plugin_acceptance_open_settings(window, cx)
        })
    })??;
    wait(cx, root, window, "正式设置页加载", |snapshot| {
        let page = &snapshot["settings"]["page"];
        page["loaded"] == true
            && page["ready"] == true
            && page["busy"] == false
            && page["dirty"] == false
            && page["draft"]["showPreview"] == (options.phase == "prepare")
    })
    .await?;
    wait_selector(cx, window, "plugin-setting-/showPreview").await?;
    if options.phase == "prepare" {
        click_selector(cx, window, "plugin-setting-/showPreview", false)?;
        wait_selector(cx, window, "plugin-settings-save").await?;
        click_selector(cx, window, "plugin-settings-save", false)?;
    }
    let settings = wait(
        cx,
        root,
        window,
        "设置确认和保留笔记草稿",
        |snapshot| {
            snapshot["settings"]["page"]["loaded"] == true
                && snapshot["settings"]["page"]["draft"]["showPreview"] == false
                && snapshot["settings"]["page"]["busy"] == false
                && snapshot["settings"]["page"]["dirty"] == false
        },
    )
    .await?;
    steps.push(json!({"step":"settings-persisted","snapshot":settings}));
    capture_frame(
        cx,
        window,
        &output.join(format!("{}-settings.png", options.phase)),
    )
    .await?;
    window.update(cx, |_, window, app| {
        window.dispatch_keystroke(Keystroke::parse("escape").map_err(io::Error::other)?, app);
        Ok::<_, io::Error>(())
    })??;
    let final_state = wait(cx, root, window, "关闭设置后恢复草稿", |snapshot| {
        snapshot["preferencesOpen"] == false
            && active_text(snapshot) == Some(RESTART_DRAFT)
            && snapshot["toolMounted"] == true
            && snapshot["tool"]["data"]["showPreview"] == false
    })
    .await?;
    let (viewport, scale) = window.update(cx, |_, window, _| {
        viewport_evidence(window, [options.width, options.height])
    })??;
    let capture = format!("{}-final.png", options.phase);
    capture_frame(cx, window, &output.join(&capture)).await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))
    })?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let ready = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            root.update(app, |root, _| {
                root.transfer_workspace_acceptance_quit_ready()
            })
            .map_err(io::Error::other)
        })??;
        if ready {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("正式工作区和插件退出保存超时").into());
        }
        cx.background_executor().timer(POLL).await;
    }
    Ok(
        json!({"schemaVersion":1,"phase":options.phase,"theme":options.theme,"processId":std::process::id(),
        "nativeIsolation":isolation,"requestedViewport":[options.width,options.height],"viewport":viewport,"scaleFactor":scale,
        "capture":capture,"steps":steps,"finalState":final_state,"firstFrameMarked":true,
        "inputMode":"synthetic GPUI pointer and keystroke dispatch; production navigation helpers",
        "nativeImeTested":false,"dpiSwitchTested":false,"sleepResumeTested":false}),
    )
}

/// 等待的每轮都推进原生绘制并核对桌面身份，超时保留末次真实快照帮助定位。
async fn wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    mut predicate: impl FnMut(&Value) -> bool,
) -> Result<Value, Box<dyn Error>> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            Ok::<_, io::Error>(root.update(app, |root, cx| {
                root.external_plugin_acceptance_snapshot(window, cx)
            }))
        })??;
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{stage}超时：{snapshot}")).into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 只读取活动的正式笔记标签，避免另一个隐藏实例的值满足当前操作等待。
fn active(snapshot: &Value) -> Option<&Value> {
    snapshot["tabs"]
        .as_array()?
        .iter()
        .find(|tab| tab["tabId"] == snapshot["activeTabId"] && tab["contentId"] == CONTENT)
}

/// 合并状态与真实输入实体必须一致，避免投影更新却没有同步到屏幕控件仍被判为成功。
fn active_text(snapshot: &Value) -> Option<&str> {
    let tab = active(snapshot)?;
    let renderer = &tab["renderer"];
    let text = renderer["data"]["text"].as_str()?;
    let field = renderer["fields"]
        .as_array()?
        .iter()
        .find(|field| field["binding"] == "text" && field["kind"] == "textarea")?;
    (renderer["status"] == "Ready" && field["value"] == text).then_some(text)
}

/// 保存按钮的成功以独立应用端口回读为准，不能只凭 toast、忙碌图标或RPC受理回执。
async fn wait_note_state(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &AppServices,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let settings = services
        .external_plugin_settings
        .clone()
        .ok_or_else(|| io::Error::other("插件状态服务缺失"))?;
    let plugin = PluginId::parse("acme.notes")?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let settings = settings.clone();
        let plugin = plugin.clone();
        let state = services
            .executor
            .run(move |cancel| async move { settings.state_get(&plugin, cancel).await })
            .await?;
        if state
            .as_ref()
            .is_some_and(|state| state["text"] == expected)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("保存后插件状态回读超时").into());
        }
        window.update(cx, |_, window, app| draw(window, app))?;
        cx.background_executor().timer(POLL).await;
    }
}

#[cfg(test)]
#[path = "../../tests/support/external_plugin_acceptance_unit.rs"]
mod tests;
