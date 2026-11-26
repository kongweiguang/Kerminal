//! 验证集合缓存身份不因树父项或排序方向不同而互相覆盖。
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_plugin_protocol::runtime::{ViewFetchSort, ViewSortDirection};

use super::{CollectionQuery, collection_cache_key};

/// 只有实际确认的新值才能产生动作；控件同步不能冒充用户交互。
#[gpui_kit::test]
fn select_on_change_ignores_synchronization_and_dispatches_confirmed_value(
    app: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::component::select::SelectEvent;
    use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginViewIdentity};
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    let runtime_guard = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared = pending_open_plugin();
    let identity = PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: "main".into(),
        instance: "on-change".into(),
        generation: 1,
    };
    let view = Arc::new(kerminal_plugin_protocol::native::parse_native_view(
        br#"{"schema":1,"data":{"address":"a","options":[{"label":"A","value":"a"},{"label":"B","value":"b"}]},"root":{"type":"input","bind":"address","x-kerminal-select-options":"$options","x-kerminal-on-change":"changed"}}"#,
    ).unwrap());
    let backend = Arc::new(PendingOpenRuntime::default());
    let config = super::NativeRendererConfig {
        prepared,
        view,
        identity,
        scope: json!({}),
        saved_state: None,
        initial_data: None,
        runtime: backend.clone(),
        executor: ApplicationTaskExecutor::new(runtime_guard.handle().clone()),
        locale: "en".into(),
        content_identity: None,
        embedded_form: None,
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let renderer = super::NativeRenderer::mount(config, window, cx).unwrap();
        *mounted.borrow_mut() = Some(renderer.clone());
        gpui_kit::base::Root::new(renderer, window, cx)
    });
    let renderer = slot.borrow_mut().take().unwrap();
    visual.update(|window, cx| {
        renderer.update(cx, |renderer, cx| {
            renderer.process_generation = Some(1);
            renderer.status = super::OpenStatus::Ready;
            renderer.edit_field("address", json!("b"), cx);
            renderer.sync_controls(window, cx);
        })
    });
    visual.run_until_parked();
    runtime_guard.block_on(async { tokio::task::yield_now().await });
    assert!(backend.actions.lock().unwrap().is_empty());
    visual.update(|_, cx| {
        renderer.update(cx, |renderer, cx| {
            let super::FieldControl::Select { state, .. } = &renderer.fields["root"] else {
                panic!("expected select")
            };
            assert_eq!(
                state.read(cx).selected_value().map(String::as_str),
                Some("b")
            );
            state.update(cx, |_, cx| {
                cx.emit(SelectEvent::Confirm(Some("a".to_owned())))
            });
        })
    });
    visual.run_until_parked();
    runtime_guard.block_on(async { tokio::task::yield_now().await });
    let actions = backend.actions.lock().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["action"], json!("changed"));
    assert_eq!(actions[0]["data"]["address"], json!("a"));
    drop(actions);
    visual.update(|_, cx| renderer.update(cx, |renderer, _| renderer.cancellation.cancel()));
}

/// 只有与当前业务回执一致的内联内容才能代替宿主反馈。
#[test]
fn inline_action_error_must_match_current_business_reply() {
    use kerminal_application::{AppError, plugin_ports::PluginBusinessError};
    use serde_json::json;

    use super::{inline_action_error_path, matches_inline_action_error};

    let root = serde_json::from_value(json!({
        "type": "scroll",
        "child": { "type": "text", "text": "$feedback.error" },
        "x-kerminal-action-error": "$feedback.error"
    }))
    .expect("valid scroll");
    let path = inline_action_error_path(&root).expect("explicit error binding");
    assert_eq!(path.segments(), &["feedback", "error"]);
    let inline = json!("请选择有效的共享目录");
    let business = AppError::infrastructure_with_source(
        "插件请求失败",
        PluginBusinessError::new(-32103, "请选择有效的共享目录").expect("business code"),
    );
    assert!(matches_inline_action_error(Some(&inline), &business));
    assert!(!matches_inline_action_error(
        Some(&json!("旧提示")),
        &business
    ));
    assert!(!matches_inline_action_error(None, &business));
    assert!(!matches_inline_action_error(
        Some(&inline),
        &AppError::unsupported("插件权限不足")
    ));
}

/// 父节点、排序列和方向都属于查询身份，比较规则必须与相等判断一致。
#[test]
fn collection_queries_keep_parent_and_sort_as_separate_keys() {
    let root = CollectionQuery::default();
    let child = CollectionQuery {
        parent_key: Some("parent".into()),
        sort: None,
    };
    let ascending = CollectionQuery {
        parent_key: None,
        sort: Some(ViewFetchSort {
            column: "name".into(),
            direction: ViewSortDirection::Asc,
        }),
    };
    let descending = CollectionQuery {
        parent_key: None,
        sort: Some(ViewFetchSort {
            column: "name".into(),
            direction: ViewSortDirection::Desc,
        }),
    };
    let other_column = CollectionQuery {
        parent_key: None,
        sort: Some(ViewFetchSort {
            column: "size".into(),
            direction: ViewSortDirection::Asc,
        }),
    };
    let queries = [root, child, ascending.clone(), descending, other_column];
    let pages: BTreeMap<_, _> = queries
        .into_iter()
        .enumerate()
        .map(|(n, q)| (q, n))
        .collect();
    assert_eq!(pages.len(), 5);
    assert_eq!(pages.get(&ascending), Some(&2));
}

/// 分隔符、空父键、Unicode 和排序不能制造相同缓存键，原始节点 ID 不被改写。
#[test]
fn collection_cache_key_is_unambiguous_for_arbitrary_parent_keys() {
    let no_parent = CollectionQuery::default();
    let empty_parent = CollectionQuery {
        parent_key: Some(String::new()),
        sort: None,
    };
    let unusual_parent = CollectionQuery {
        parent_key: Some("|:中0:".into()),
        sort: None,
    };
    assert_ne!(
        collection_cache_key("node", &no_parent),
        collection_cache_key("node", &empty_parent)
    );
    assert_ne!(
        collection_cache_key("node", &unusual_parent),
        collection_cache_key("node|:中0:", &no_parent)
    );
    let sorted = CollectionQuery {
        parent_key: Some("|:中0:".into()),
        sort: Some(ViewFetchSort {
            column: "name".into(),
            direction: ViewSortDirection::Asc,
        }),
    };
    let descending = CollectionQuery {
        sort: Some(ViewFetchSort {
            column: "name".into(),
            direction: ViewSortDirection::Desc,
        }),
        ..sorted.clone()
    };
    assert_ne!(
        collection_cache_key("node", &sorted),
        collection_cache_key("node", &descending)
    );
}

/// 图标只接受内置名称和冻结包中的 SVG，不从路径或 URL 临时加载资源。
#[test]
fn icon_resolution_stays_inside_builtin_and_frozen_resources() {
    use super::super::layout::{PluginIconSource, icon_source};
    use std::{collections::BTreeMap, sync::Arc};
    let resources = BTreeMap::from([
        (
            "icons/custom.svg".to_owned(),
            Arc::<[u8]>::from(&b"<svg/>"[..]),
        ),
        (
            "../secret.svg".to_owned(),
            Arc::<[u8]>::from(&b"<svg/>"[..]),
        ),
        (
            "https://example.test/image.svg".to_owned(),
            Arc::<[u8]>::from(&b"<svg/>"[..]),
        ),
    ]);
    let lower = icon_source(&resources, "search");
    assert!(matches!(lower, Some(PluginIconSource::Builtin(_))));
    assert_eq!(lower, icon_source(&resources, "Search"));
    assert_eq!(
        icon_source(&resources, "icons/custom.svg"),
        Some(PluginIconSource::Package(b"<svg/>"))
    );
    for forbidden in [
        "../secret.svg",
        "https://example.test/image.svg",
        "missing.svg",
        "icons/custom.png",
        "not-a-real-icon",
    ] {
        assert!(icon_source(&resources, forbidden).is_none(), "{forbidden}");
    }
}

/// 挂起打开请求模拟进程缓慢启动，记录其他请求以检查是否发生抢先关闭；动作参数
/// 单独记录，用来核对运行时下拉的选中值确实进入动作快照。
#[derive(Default)]
struct PendingOpenRuntime {
    requests: std::sync::Mutex<Vec<String>>,
    actions: std::sync::Mutex<Vec<serde_json::Value>>,
}

struct PendingSubscription;

impl super::RuntimeSubscription for PendingSubscription {
    /// 订阅只等待实体取消，避免测试伪造插件事件或常驻后台任务。
    fn recv(
        &mut self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> super::super::runtime::RuntimeEventFuture<'_> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Ok(None)
        })
    }
}

impl super::AsyncViewRuntime for PendingOpenRuntime {
    /// 真实记录方法并等待取消，确保测试不会用立即返回掩盖打开与关闭竞态。
    fn request(
        &self,
        request: super::RuntimeRequest,
    ) -> super::super::runtime::RuntimeFuture<super::RuntimeReply> {
        self.requests.lock().unwrap().push(request.method.clone());
        if request.method == "view.action" {
            self.actions.lock().unwrap().push(request.params.clone());
        }
        Box::pin(async move {
            request.cancellation.cancelled().await;
            Err(kerminal_application::AppError::cancelled("测试请求已取消"))
        })
    }

    /// 此用例不发送通知，意外通知必须报错而非伪造成功。
    fn notify(&self, _: super::RuntimeRequest) -> super::super::runtime::RuntimeFuture<()> {
        Box::pin(async { Err(kerminal_application::AppError::unsupported("测试通知")) })
    }

    /// 每个实例拥有自己的订阅，生命周期由 renderer 的取消令牌结束。
    fn subscribe(
        &self,
        _: &kerminal_application::plugin_ports::PluginViewIdentity,
    ) -> kerminal_application::AppResult<Box<dyn super::RuntimeSubscription>> {
        Ok(Box::new(PendingSubscription))
    }

    /// 测试不提供已保存状态，避免把测试数据误认为真实关闭回执。
    fn cached_state(
        &self,
        _: &kerminal_application::plugin_ports::PluginViewIdentity,
    ) -> kerminal_application::AppResult<
        Option<kerminal_application::plugin_ports::PluginViewSavedState>,
    > {
        Ok(None)
    }

    /// 打开仍挂起时没有已确认快照，不能预先伪造成功。
    fn cached_data(
        &self,
        _: &kerminal_application::plugin_ports::PluginViewIdentity,
    ) -> kerminal_application::AppResult<Option<serde_json::Value>> {
        Ok(None)
    }

    /// 测试不写入凭据，阻止意外副作用被误判为成功。
    fn store_secret(
        &self,
        _: kerminal_application::plugin_ports::PluginViewIdentity,
        _: String,
        _: zeroize::Zeroizing<String>,
        _: std::time::Instant,
        _: tokio_util::sync::CancellationToken,
    ) -> super::super::runtime::RuntimeFuture<String> {
        Box::pin(async { Err(kerminal_application::AppError::unsupported("测试凭据")) })
    }
}

/// 使用协议解析过的最小清单，不让打开关闭测试依赖磁盘或真实插件。
fn pending_open_plugin() -> std::sync::Arc<kerminal_application::plugin_ports::PreparedPlugin> {
    use kerminal_application::plugin_ports::PreparedPlugin;
    use kerminal_domain::plugins::{PluginId, PluginVersion};
    use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};
    use std::{path::PathBuf, sync::Arc};

    let manifest = serde_json::json!({
        "schema":1, "id":"acme.notes", "name":{"en":"Notes"},
        "description":{"en":"Notes"}, "version":"1.0.0", "api":"^1.0",
        "publisher":"acme", "runtime":{"kind":"none"},
        "views":{"main":{"kind":"native","file":"views/main.json"}}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&manifest).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    Arc::new(PreparedPlugin {
        id: PluginId::parse("acme.notes").unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        directory: PathBuf::from("fixture"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    })
}

/// 即使进程先报告失败，尚未返回的 open 仍禁止发送 close，避免迟到登记泄漏。
#[gpui_kit::test]
fn close_waits_for_open_receipt_after_process_failure(app: &mut gpui_kit::TestAppContext) {
    use kerminal_application::{
        ApplicationTaskExecutor, PublicErrorCode, plugin_ports::PluginViewIdentity,
    };
    use kerminal_domain::plugins::PluginRuntimeState;
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::Arc,
        time::{Duration, Instant},
    };

    let runtime_guard = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = ApplicationTaskExecutor::new(runtime_guard.handle().clone());
    let backend = Arc::new(PendingOpenRuntime::default());
    let prepared = pending_open_plugin();
    let identity = PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: "main".into(),
        instance: "instance-1".into(),
        generation: 1,
    };
    let view = Arc::new(
        kerminal_plugin_protocol::native::parse_native_view(
            br#"{"schema":1,"root":{"type":"text","text":"Hello"}}"#,
        )
        .unwrap(),
    );
    let config = super::NativeRendererConfig {
        prepared,
        view,
        identity: identity.clone(),
        scope: serde_json::json!({}),
        saved_state: None,
        initial_data: None,
        runtime: backend.clone(),
        executor,
        locale: "en".into(),
        content_identity: None,
        embedded_form: None,
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let renderer = super::NativeRenderer::mount(config, window, cx).unwrap();
        *mounted.borrow_mut() = Some(renderer.clone());
        gpui_kit::base::Root::new(renderer, window, cx)
    });
    let renderer = slot.borrow_mut().take().expect("renderer mounted");
    visual.run_until_parked();
    runtime_guard.block_on(async { tokio::task::yield_now().await });
    assert!(
        backend
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "view.open")
    );
    visual.update(|window, cx| {
        renderer.update(cx, |renderer, cx| {
            renderer.event(
                super::RuntimeEvent::ProcessState {
                    plugin: identity.plugin,
                    state: PluginRuntimeState::Failed,
                },
                window,
                cx,
            );
        });
        let future = renderer
            .read(cx)
            .close(Instant::now() + Duration::from_secs(5));
        let error = futures::executor::block_on(future).unwrap_err();
        assert_eq!(error.public_code(), PublicErrorCode::Conflict);
        renderer.update(cx, |renderer, _| renderer.cancellation.cancel());
    });
    runtime_guard.block_on(async { tokio::task::yield_now().await });
    visual.run_until_parked();
    assert!(
        !backend
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "view.close")
    );
}

/// 单实例恢复失败必须可见，迟到失败不得覆盖已恢复代次，其他实例不能影响当前草稿。
#[gpui_kit::test]
fn reopen_failure_is_scoped_to_the_instance_and_preserves_drafts(
    app: &mut gpui_kit::TestAppContext,
) {
    use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginViewIdentity};
    use std::{cell::RefCell, rc::Rc, sync::Arc};
    let runtime_guard = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared = pending_open_plugin();
    let identity = PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: "main".into(),
        instance: "instance-1".into(),
        generation: 1,
    };
    let view = Arc::new(
        kerminal_plugin_protocol::native::parse_native_view(
            br#"{"schema":1,"data":{"name":"old"},"root":{"type":"text","text":"Hello"}}"#,
        )
        .unwrap(),
    );
    let config = super::NativeRendererConfig {
        prepared,
        view,
        identity: identity.clone(),
        scope: serde_json::json!({}),
        saved_state: None,
        initial_data: None,
        runtime: Arc::new(PendingOpenRuntime::default()),
        executor: ApplicationTaskExecutor::new(runtime_guard.handle().clone()),
        locale: "en".into(),
        content_identity: None,
        embedded_form: None,
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let renderer = super::NativeRenderer::mount(config, window, cx).unwrap();
        *mounted.borrow_mut() = Some(renderer.clone());
        gpui_kit::base::Root::new(renderer, window, cx)
    });
    let renderer = slot.borrow_mut().take().unwrap();
    visual.update(|window, cx| {
        renderer.update(cx, |renderer, cx| {
            renderer.opened = true;
            renderer.open_in_flight = false;
            renderer.status = super::OpenStatus::Recovering;
            let path = super::super::state::BindingPath::parse("name").unwrap();
            renderer
                .projection
                .state_mut()
                .edit(&path, serde_json::json!("draft"))
                .unwrap();
            let mut other = identity.clone();
            other.instance = "instance-2".into();
            renderer.event(
                super::RuntimeEvent::ViewReopenFailed {
                    identity: other,
                    process_generation: 2,
                    message: "other".into(),
                },
                window,
                cx,
            );
            assert_eq!(renderer.status, super::OpenStatus::Recovering);
            renderer.event(
                super::RuntimeEvent::ViewReopenFailed {
                    identity: identity.clone(),
                    process_generation: 2,
                    message: "failed".into(),
                },
                window,
                cx,
            );
            assert_eq!(renderer.status, super::OpenStatus::Failed("failed".into()));
            assert_eq!(
                renderer.projection.state().value(&path),
                Some(&serde_json::json!("draft"))
            );
            renderer.process_generation = Some(2);
            renderer.status = super::OpenStatus::Ready;
            renderer.event(
                super::RuntimeEvent::ViewReopenFailed {
                    identity: identity.clone(),
                    process_generation: 2,
                    message: "late".into(),
                },
                window,
                cx,
            );
            assert_eq!(renderer.status, super::OpenStatus::Ready);
            renderer.cancellation.cancel();
        });
    });
}

/// 两个真实原生实体的 textarea 草稿跨重新挂载恢复；插件的 opaque 状态不因宿主输入而改变。
#[gpui_kit::test]
fn native_content_restart_restores_two_drafts_without_saving_plugin_action(
    app: &mut gpui_kit::TestAppContext,
) {
    use kerminal_application::{
        ApplicationTaskExecutor,
        plugin_ports::{PluginViewIdentity, PluginViewSavedState},
    };
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc, sync::Arc};
    let runtime_guard = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared = pending_open_plugin();
    let view = Arc::new(kerminal_plugin_protocol::native::parse_native_view(
        br#"{"schema":1,"data":{"text":"default","count":0},"root":{"type":"stack","children":[{"type":"textarea","bind":"text"},{"type":"secret","bind":"password"},{"type":"number","bind":"count"}]}}"#,
    ).unwrap());
    let executor = ApplicationTaskExecutor::new(runtime_guard.handle().clone());
    let backend = Arc::new(PendingOpenRuntime::default());
    let config = move |instance: &str, saved_state| super::NativeRendererConfig {
        prepared: prepared.clone(),
        view: view.clone(),
        identity: PluginViewIdentity {
            plugin: prepared.id.clone(),
            view: "main".into(),
            instance: instance.into(),
            generation: 1,
        },
        scope: json!({"kind":"tab","tab":instance}),
        saved_state,
        initial_data: None,
        runtime: backend.clone(),
        executor: executor.clone(),
        locale: "en".into(),
        content_identity: None,
        embedded_form: None,
    };
    let plugin_state = PluginViewSavedState {
        state_version: 1,
        state: json!(["opaque", {"stored":"old"}]),
    };
    let first_configs = [
        config("tab-one", Some(plugin_state.clone())),
        config("tab-two", Some(plugin_state.clone())),
    ];
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(Vec::new()));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let renderers: Vec<_> = first_configs
            .into_iter()
            .map(|config| super::NativeRenderer::mount(config, window, cx).unwrap())
            .collect();
        *mounted.borrow_mut() = renderers.clone();
        gpui_kit::base::Root::new(renderers[0].clone(), window, cx)
    });
    let renderers = slot.borrow_mut().drain(..).collect::<Vec<_>>();
    let snapshots = visual.update(|window, cx| {
        renderers
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                entity.update(cx, |renderer, cx| {
                    renderer.edit_field("text", json!(format!("RESTART_DRAFT_{index}")), cx);
                    renderer.edit_field("count", json!(7), cx);
                    renderer
                        .field_errors
                        .insert("count".into(), "invalid number".into());
                    if let super::FieldControl::Input { state, .. } = &renderer.fields["root.1"] {
                        state.update(cx, |state, cx| {
                            state.set_value("NEVER_PERSIST_SECRET", window, cx)
                        });
                    } else {
                        panic!("secret input missing");
                    }
                    let snapshot = renderer.content_saved_state().unwrap().unwrap();
                    assert_eq!(snapshot.state["plugin_state"]["state"], plugin_state.state);
                    assert_eq!(
                        snapshot.state["drafts"],
                        json!([{"path":"text","value":format!("RESTART_DRAFT_{index}")} ])
                    );
                    assert!(!snapshot.state.to_string().contains("NEVER_PERSIST_SECRET"));
                    renderer.cancellation.cancel();
                    snapshot
                })
            })
            .collect::<Vec<_>>()
    });
    visual.update(|window, cx| {
        for (index, snapshot) in snapshots.into_iter().enumerate() {
            let restored = super::NativeRenderer::mount(
                config(&format!("new-tab-{index}"), Some(snapshot)),
                window,
                cx,
            )
            .unwrap();
            restored.update(cx, |renderer, cx| {
                assert_eq!(renderer.saved_state.as_ref(), Some(&plugin_state));
                let path = super::super::state::BindingPath::parse("text").unwrap();
                renderer
                    .projection
                    .state_mut()
                    .replace_remote(json!({"text":"REMOTE_UPDATE","count":0}))
                    .unwrap();
                renderer.sync_controls(window, cx);
                assert!(renderer.projection.state().is_dirty(&path));
                assert!(renderer.projection.state().has_new_value(&path));
                if let super::FieldControl::Textarea { state, .. } = &renderer.fields["root.0"] {
                    assert_eq!(
                        state.read(cx).value().as_ref(),
                        format!("RESTART_DRAFT_{index}")
                    );
                } else {
                    panic!("textarea missing");
                }
                assert_eq!(renderer.cached_state().unwrap(), Some(plugin_state.clone()));
                renderer.cancellation.cancel();
            });
        }
    });
    runtime_guard.block_on(async { tokio::task::yield_now().await });
}

/// 输入节点声明运行时选项后投影为原生 Select；补丁替换选项时只保留仍匹配的选择，
/// 确认事件直接写回真实字符串值，不再依赖装载时捕获的选项索引。
#[gpui_kit::test]
fn runtime_select_tracks_patched_options_and_commits_confirmed_value(
    app: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::component::select::SelectEvent;
    use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginViewIdentity};
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    let runtime_guard = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared = pending_open_plugin();
    let identity = PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: "main".into(),
        instance: "runtime-select".into(),
        generation: 1,
    };
    let view = Arc::new(
        kerminal_plugin_protocol::native::parse_native_view(
            br#"{"schema":1,"data":{"address":"192.168.1.10","address_options":[{"label":"Ethernet","value":"192.168.1.10"},{"label":"Wi-Fi","value":"10.0.0.2"}]},"root":{"type":"input","bind":"address","label":"Listen address","placeholder":"Select an address","x-kerminal-select-options":"$address_options"}}"#,
        )
        .unwrap(),
    );
    let backend = Arc::new(PendingOpenRuntime::default());
    let runtime = backend.clone();
    let config = super::NativeRendererConfig {
        prepared,
        view,
        identity: identity.clone(),
        scope: json!({}),
        saved_state: None,
        initial_data: None,
        runtime,
        executor: ApplicationTaskExecutor::new(runtime_guard.handle().clone()),
        locale: "en".into(),
        content_identity: None,
        embedded_form: None,
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let renderer = super::NativeRenderer::mount(config, window, cx).unwrap();
        *mounted.borrow_mut() = Some(renderer.clone());
        gpui_kit::base::Root::new(renderer, window, cx)
    });
    let renderer = slot.borrow_mut().take().expect("renderer mounted");
    visual.update(|_, cx| {
        renderer.update(cx, |renderer, cx| {
            let state = match &renderer.fields["root"] {
                super::FieldControl::Select { state, .. } => state.clone(),
                _ => panic!("runtime select was not projected"),
            };
            assert_eq!(
                state.read(cx).selected_value().map(String::as_str),
                Some("192.168.1.10")
            );
            state.update(cx, |_, cx| {
                cx.emit(SelectEvent::Confirm(Some("10.0.0.2".to_owned())))
            });
        });
    });
    // 事件通过待处理效果分发，先让测试窗口跑完这一拍再检查草稿。
    visual.run_until_parked();

    visual.update(|window, cx| {
        renderer.update(cx, |renderer, cx| {
            assert_eq!(renderer.field_value("address"), Some(&json!("10.0.0.2")));
            renderer.process_generation = Some(1);
            renderer.status = super::OpenStatus::Ready;
            renderer.event(
                super::RuntimeEvent::ViewUpdate {
                    identity: identity.clone(),
                    process_generation: 1,
                    patch: json!({"address_options":[{"label":"Ethernet","value":"192.168.1.10"}]}),
                },
                window,
                cx,
            );
            let super::FieldControl::Select { state, .. } = &renderer.fields["root"] else {
                panic!("runtime select was not projected");
            };
            assert_eq!(state.read(cx).selected_value(), None);
            assert_eq!(renderer.field_value("address"), Some(&json!("10.0.0.2")));
            assert!(renderer.is_stale_runtime_binding("address"));
            // 失效下拉不阻断动作，但动作快照中该绑定必须是 null，不能把已经
            // 从下拉消失的旧值继续交给插件。
            assert_eq!(renderer.action_data()["address"], json!(null));

            // 候选暂时清空同样不能把旧值当成仍然有效；等待下一次补丁再恢复。
            renderer.event(
                super::RuntimeEvent::ViewUpdate {
                    identity: identity.clone(),
                    process_generation: 1,
                    patch: json!({"address_options":[]}),
                },
                window,
                cx,
            );
            assert!(renderer.is_stale_runtime_binding("address"));
            assert_eq!(renderer.action_data()["address"], json!(null));
        });
    });

    visual.update(|window, cx| {
        renderer.update(cx, |renderer, cx| {
            renderer.event(
                super::RuntimeEvent::ViewUpdate {
                    identity: identity.clone(),
                    process_generation: 1,
                    patch: json!({"address_options":[{"label":"Ethernet","value":"192.168.1.10"},{"label":"Wi-Fi 6","value":"10.0.0.2"}]}),
                },
                window,
                cx,
            );
            let super::FieldControl::Select { state, .. } = &renderer.fields["root"] else {
                panic!("runtime select was not projected");
            };
            assert!(!renderer.is_stale_runtime_binding("address"));
            assert_eq!(
                state.read(cx).selected_value().map(String::as_str),
                Some("10.0.0.2")
            );
            state.update(cx, |_, cx| {
                cx.emit(SelectEvent::Confirm(Some("192.168.1.10".to_owned())))
            });
        });
    });
    visual.run_until_parked();

    visual.update(|_, cx| {
        renderer.update(cx, |renderer, cx| {
            assert_eq!(
                renderer.field_value("address"),
                Some(&json!("192.168.1.10"))
            );
            assert_eq!(renderer.action_data()["address"], json!("192.168.1.10"));
            renderer.action("start".into(), "start".into(), None, cx);
        });
    });
    // 动作任务先由 GPUI 执行器轮询，再交给应用 runtime 记录请求参数。
    visual.run_until_parked();
    runtime_guard.block_on(async { tokio::task::yield_now().await });
    let actions = backend.actions.lock().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["action"], json!("start"));
    assert_eq!(actions[0]["data"]["address"], json!("192.168.1.10"));
    drop(actions);
    visual.update(|_, cx| {
        renderer.update(cx, |renderer, _| renderer.cancellation.cancel());
    });
}

/// 静区固定四个模块，模块尺寸与原点按设备像素取整；矩阵内容只影响深浅，不参与尺寸计算。
#[test]
fn qr_layout_adds_quiet_zone_and_snaps_modules_to_device_pixels() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let side = 21;
    let matrix = (0..side)
        .map(|row| {
            (0..side)
                .map(|column| if (row + column) % 2 == 0 { '1' } else { '0' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(145.0), px(145.0)));
    let layout = qr_layout(&matrix, bounds, 2.0).expect("valid layout");
    assert_eq!(layout.matrix_side(), 21);
    assert_eq!(layout.total_modules(), 29);
    assert!(layout.module_px() > 0.0);
    assert!((layout.module_px() * 2.0 - (layout.module_px() * 2.0).round()).abs() < 1e-6);
    let (x, y) = layout.origin();
    // 原点也是整数设备像素，整幅二维码连同静区留在画布内。
    assert!((x * 2.0 - (x * 2.0).round()).abs() < 1e-6);
    assert!((y * 2.0 - (y * 2.0).round()).abs() < 1e-6);
    let qr = layout.total_modules() as f32 * layout.module_px();
    assert!(x >= 0.0 && y >= 0.0);
    assert!(x + qr <= 145.0 + 1e-3 && y + qr <= 145.0 + 1e-3);
    for (column, row) in [(0, 0), (3, 3), (25, 25), (28, 0), (29, 29)] {
        assert!(!layout.is_dark(column, row), "{column},{row}");
    }
    assert!(layout.is_dark(4, 4));
    assert!(!layout.is_dark(4, 5));
    assert!(qr_layout("not a matrix", bounds, 1.0).is_none());
    let empty = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(145.0)));
    assert!(qr_layout(&matrix, empty, 1.0).is_none());
}

/// 模块不足一个设备像素、或最后一个设备像素也无法保证整数原点时必须失败，
/// 而不是把分数模块画成空白/截断却仍显示成二维码。
///
/// 覆盖版本 40 的 185 个模块：画布设备像素小于 185 时没有可用的整数模块宽，应当返回
/// `None`；185 设备像素时模块只能取 1 像素、余量为 0，容器查询阶段又拿不到画布的绝对
/// 原点，若画布落在分数设备像素上就没有可用的整数原点，因此同样判为空间不足；186 设备
/// 像素留出 1 像素余量后必定存在整数原点，必须完整可绘制。
#[test]
fn qr_layout_rejects_canvas_too_small_for_one_device_pixel_module() {
    use super::super::layout::{QrLayout, qr_layout};
    use gpui_kit::{Bounds, point, px, size};

    /// 用固定原点构造正方形画布，避免闭包返回借用时触发捕获变量逃逸限制。
    fn layout(matrix: &str, side_px: f32, scale: f32) -> Option<QrLayout<'_>> {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(side_px), px(side_px)));
        qr_layout(matrix, bounds, scale)
    }

    let matrix = patterned_qr(177, 2);
    // 185 模块 → 至少 185 个设备像素；低 DPI 与非整数 DPR 都按设备空间判定下限。
    assert!(
        layout(&matrix, 184.0, 1.0).is_none(),
        "184 dev px must fail"
    );
    assert!(
        layout(&matrix, 144.0, 1.0).is_none(),
        "narrow sidebar @1x must fail"
    );
    // 1.25 DPR 下 147.2 逻辑像素只有 184 设备像素，同样不足一个模块宽。
    assert!(
        layout(&matrix, 147.2, 1.25).is_none(),
        "184 dev px @1.25x must fail"
    );
    // 恰好一个设备像素/模块但零余量：无法保证分数画布原点也能落格，必须走可见失败。
    assert!(
        layout(&matrix, 185.0, 1.0).is_none(),
        "185 dev px leaves no room for an integer origin"
    );
    // 186 设备像素：1 像素/模块 + 1 像素余量，整数原点必定存在。
    let minimal = layout(&matrix, 186.0, 1.0).expect("186 dev px guarantees an integer origin");
    assert_eq!(minimal.total_modules(), 185);
    assert!((minimal.module_px() - 1.0).abs() < 1e-6);
    // 2x DPR 下 280 逻辑像素 = 560 设备像素，185 模块每块 3 设备像素，仍是整数且不为 0。
    let scaled = layout(&matrix, 280.0, 2.0).expect("280px @2x fits");
    let module_dev = scaled.module_px() * 2.0;
    assert!((module_dev - 3.0).abs() < 1e-3);
    // 非正方形画布取较短边，高度不足同样失败。
    let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(280.0), px(120.0)));
    assert!(qr_layout(&matrix, narrow, 1.0).is_none());
}

/// 收集布局实际输出的绘制段，供回归断言真实绘制范围而非只检查 `is_dark`。
fn qr_runs(layout: &super::super::layout::QrLayout<'_>) -> Vec<super::super::layout::QrRun> {
    let mut runs = Vec::new();
    layout.for_each_run(|run| runs.push(run));
    runs
}

/// 判断某个含静区坐标的模块是否真的被绘制段覆盖。
///
/// 回归必须断言「画出来了」而不是「查出来是深色」：旧实现的 `is_dark` 本身正确，
/// 错的是遍历范围，只有检查绘制输出才能抓住这类截断缺陷。
fn painted_module(
    layout: &super::super::layout::QrLayout<'_>,
    runs: &[super::super::layout::QrRun],
    column: usize,
    row: usize,
) -> bool {
    let module = layout.module_px();
    let (origin_x, origin_y) = layout.origin();
    let x = origin_x + column as f32 * module;
    let y = origin_y + row as f32 * module;
    runs.iter().any(|run| {
        (run.y - y).abs() < 1e-3 && run.x <= x + 1e-3 && run.x + run.width >= x + module - 1e-3
    })
}

/// 生成 `(row + column) % period == 0` 为深色的方阵，便于覆盖任意版本的角与边缘。
fn patterned_qr(side: usize, period: usize) -> String {
    (0..side)
        .map(|row| {
            (0..side)
                .map(|column| {
                    if (row + column) % period == 0 {
                        '1'
                    } else {
                        '0'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 构造带三个定位图案与时间图案的 21×21 矩阵，接近真实二维码结构。
///
/// 定位图案与右下数据角是扫码最先依赖的区域，也正是旧实现截断时受影响的位置，
/// 因此回归用结构矩阵而不是纯棋盘格。
fn realistic_qr() -> String {
    let side = 21;
    let mut cells = vec![vec!['0'; side]; side];
    for (top, left) in [(0, 0), (0, 14), (14, 0)] {
        for row in 0..7 {
            for column in 0..7 {
                let border = row == 0 || row == 6 || column == 0 || column == 6;
                let center = (2..=4).contains(&row) && (2..=4).contains(&column);
                cells[top + row][left + column] = if border || center { '1' } else { '0' };
            }
        }
    }
    // 时间图案连接定位图案，占据第 6 行与第 6 列；同时跨行跨列写入，
    // 用单轴迭代器表达反而要拆成两次遍历，这里保留按索引的双轴写法。
    #[allow(clippy::needless_range_loop)]
    for index in 8..13 {
        cells[6][index] = if index % 2 == 0 { '1' } else { '0' };
        cells[index][6] = if index % 2 == 0 { '1' } else { '0' };
    }
    // 右下数据角：旧实现正是丢掉了这一整块。
    cells[20][20] = '1';
    cells[20][19] = '1';
    cells
        .into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 绘制必须覆盖含静区的完整边长：矩阵四个角与右下数据角都要真正落在绘制段内。
///
/// 旧实现用原始边长（21）遍历含静区坐标（29），右下 4 个模块被整体截断；这里同时
/// 断言定位图案合并后的绘制宽度，确保修复没有破坏连续段合并。
#[test]
fn qr_paint_covers_finder_patterns_and_bottom_right_corner() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let matrix = realistic_qr();
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(145.0), px(145.0)));
    let layout = qr_layout(&matrix, bounds, 1.0).expect("valid layout");
    assert_eq!(layout.matrix_side(), 21);
    assert_eq!(layout.total_modules(), 29);
    let runs = qr_runs(&layout);
    let module = layout.module_px();
    let (origin_x, origin_y) = layout.origin();

    // 三个定位图案的外角：矩阵 (0,0)、(0,20)、(20,0) 对应绘制坐标加静区。
    for (column, row) in [(4, 4), (24, 4), (4, 24)] {
        assert!(
            painted_module(&layout, &runs, column, row),
            "finder corner {column},{row} not painted"
        );
    }
    // 右下数据角：旧实现下这里必然缺失。
    for (column, row) in [(24, 24), (23, 24)] {
        assert!(
            painted_module(&layout, &runs, column, row),
            "bottom-right module {column},{row} not painted"
        );
    }
    // 定位图案顶部横边必须合并为 7 个模块宽的一段。
    let finder_row = origin_y + 4.0 * module;
    let merged = runs.iter().any(|run| {
        (run.y - finder_row).abs() < 1e-3
            && (run.x - (origin_x + 4.0 * module)).abs() < 1e-3
            && (run.width - 7.0 * module).abs() < 1e-3
    });
    assert!(
        merged,
        "finder border row should merge into one 7-module run"
    );
    // 静区必须留空：任何绘制段都不允许进入左/上各 4 个模块。
    assert!(
        runs.iter()
            .all(|run| run.x >= origin_x + 4.0 * module - 1e-4
                && run.y >= origin_y + 4.0 * module - 1e-4)
    );
    // 右侧/底部同样保留 4 个模块静区，不能把矩阵一直画到画布边缘。
    let quiet_end = origin_x + (layout.total_modules() - 4) as f32 * module;
    assert!(runs.iter().all(|run| run.x + run.width <= quiet_end + 1e-4));
    assert!(runs.iter().all(
        |run| run.y + module <= origin_y + (layout.total_modules() - 4) as f32 * module + 1e-4
    ));
    // 含静区的整幅二维码不得越出画布。
    let qr = layout.total_modules() as f32 * module;
    assert!(origin_x >= 0.0 && origin_y >= 0.0);
    assert!(origin_x + qr <= 145.0 + 1e-3 && origin_y + qr <= 145.0 + 1e-3);
}

/// 版本 40 的最大矩阵同样要完整覆盖：177 边长、280px 画布下不能被裁切或丢边。
#[test]
fn qr_paint_runs_cover_the_largest_supported_matrix() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let side = 177;
    let matrix = patterned_qr(side, 2);
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(280.0), px(280.0)));
    let layout = qr_layout(&matrix, bounds, 1.0).expect("valid layout");
    assert_eq!(layout.matrix_side(), 177);
    assert_eq!(layout.total_modules(), 185);
    let runs = qr_runs(&layout);
    let module = layout.module_px();
    let (origin_x, origin_y) = layout.origin();
    let last = 176 + 4;
    // 棋盘格在 (176,176) 为深色，最后一行/列都必须被真实绘制。
    for (column, row) in [(last, last), (4, last), (last, 4)] {
        assert!(
            painted_module(&layout, &runs, column, row),
            "module {column},{row} not painted"
        );
    }
    // 含静区的整幅二维码不得越出画布，也不得被裁掉右侧或底部。
    let qr = layout.total_modules() as f32 * module;
    assert!(origin_x >= 0.0 && origin_y >= 0.0);
    assert!(origin_x + qr <= 280.0 + 1e-3);
    assert!(origin_y + qr <= 280.0 + 1e-3);
}

/// 原点按画布绝对坐标吸附到设备像素：非整数 DPR 与奇数剩余像素都不能产生半像素偏移。
///
/// 画布原点、画布尺寸和模块宽都换算到设备空间后再取整：模块宽为整数设备像素，原点为
/// 画布内的整数设备像素。曾经在「画布宽恰好等于整幅二维码宽」时找不到整数原点，实现
/// 退回原始分数坐标，所有模块落在半设备像素上，产生灰边且扫码不稳；这里把该输入作为
/// 回归固定为「缩小一级模块换取严格对齐」。
#[test]
fn qr_origin_snaps_to_absolute_device_pixels() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let matrix = patterned_qr(21, 2);
    for (origin, side_px, scale) in [
        (0.0_f32, 145.0_f32, 1.25_f32),
        (0.0, 280.0, 1.0),
        (0.0, 175.0, 1.0),
        (0.5, 150.0, 1.0),
        // 画布宽恰好等于 29 × 5 个设备像素：必须缩到 4 像素/模块后严格对齐，
        // 不能保留 0.5 逻辑像素原点把整幅码摊在半设备像素上。
        (0.5, 145.0, 1.0),
        (0.3, 145.0, 1.5),
        (10.75, 233.0, 1.25),
        (1.5, 175.0, 2.0),
        (2.5, 177.0, 1.0),
        // 宽高不同：两轴独立取整，较短边决定模块宽，较长边只影响居中余量。
        (3.25, 160.0, 1.5),
    ] {
        let bounds = Bounds::new(
            point(px(origin), px(origin)),
            size(px(side_px), px(side_px)),
        );
        let layout = qr_layout(&matrix, bounds, scale).expect("valid layout");
        let (x, y) = layout.origin();
        let qr = layout.total_modules() as f32 * layout.module_px();
        for value in [x, y] {
            let device = value * scale;
            // 无论剩余像素多少都必须落在设备像素网格上：宁可缩小一级模块，也不接受半像素。
            assert!(
                (device - device.round()).abs() < 1e-3,
                "origin {value} at scale {scale} is off the device grid"
            );
        }
        // 吸附不能把内容推出画布：左右上下都保留在边界内。
        assert!(x >= origin - 1e-3 && y >= origin - 1e-3);
        assert!(x + qr <= origin + side_px + 1e-3);
        assert!(y + qr <= origin + side_px + 1e-3);
        // 模块尺寸按设备像素取整，边缘不会落在半像素上。
        let module_dev = layout.module_px() * scale;
        assert!((module_dev - module_dev.round()).abs() < 1e-3);
        assert!(module_dev >= 1.0);
    }
}

/// 对齐只应让模块缩小到必要程度：有余量时不能无谓地少画一级。
///
/// 画布 150 设备像素、29 个模块，理论最大整数模块是 5；只有画布恰好整除、余量为 0 时
/// 才需要让出一级。这里同时验证宽高独立：窄边限制模块宽，长边不影响。
#[test]
fn qr_layout_keeps_the_largest_aligned_module() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let matrix = patterned_qr(21, 2);
    let square = Bounds::new(point(px(0.0), px(0.0)), size(px(150.0), px(150.0)));
    let layout = qr_layout(&matrix, square, 1.0).expect("valid layout");
    assert!(
        (layout.module_px() - 5.0).abs() < 1e-6,
        "unused slack exists"
    );

    // 恰好整除（29 × 5 = 145）时让出一级，换取任意分数原点也能对齐。
    let exact = Bounds::new(point(px(0.5), px(0.5)), size(px(145.0), px(145.0)));
    let layout = qr_layout(&matrix, exact, 1.0).expect("valid layout");
    assert!((layout.module_px() - 4.0).abs() < 1e-6);
    let (x, y) = layout.origin();
    assert!((x - x.round()).abs() < 1e-6 && (y - y.round()).abs() < 1e-6);

    // 宽 300、高 160：模块宽由高度决定，宽度只影响水平居中余量。
    let wide = Bounds::new(point(px(0.0), px(0.0)), size(px(300.0), px(160.0)));
    let layout = qr_layout(&matrix, wide, 1.0).expect("valid layout");
    assert!((layout.module_px() - 5.0).abs() < 1e-6);
    let (x, y) = layout.origin();
    assert!(x > 0.0, "horizontal slack must be centred");
    assert!((x - x.round()).abs() < 1e-6 && (y - y.round()).abs() < 1e-6);
}

/// 容器查询按零原点预判、绘制按真实原点落格，两者结论必须一致。
///
/// 预判阶段拿不到画布绝对原点，若它放行而绘制阶段因找不到整数原点返回空，界面会只剩
/// 标签和一块空白画布，看起来像二维码丢了。这里枚举整除边界与非整除尺寸、整数与非整数
/// DPR，以及各种分数原点，要求预判通过后绘制必定成功，并且严格对齐、完整留在画布内。
#[test]
fn qr_layout_probe_guarantees_fractional_origin_paint() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let matrix = patterned_qr(21, 2);
    for side in [145.0_f32, 146.0, 150.0, 174.0, 175.0, 176.0, 203.0] {
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let probe = Bounds::new(point(px(0.0), px(0.0)), size(px(side), px(side)));
            if qr_layout(&matrix, probe, scale).is_none() {
                continue;
            }
            for eighth in 0..8 {
                let offset = eighth as f32 / 8.0;
                let bounds = Bounds::new(point(px(offset), px(offset)), size(px(side), px(side)));
                let layout = qr_layout(&matrix, bounds, scale).unwrap_or_else(|| {
                    panic!("probe passed but paint failed at side {side}, scale {scale}, offset {offset}")
                });
                let (x, y) = layout.origin();
                let qr = layout.total_modules() as f32 * layout.module_px();
                for value in [x, y] {
                    assert!(
                        (value * scale - (value * scale).round()).abs() < 1e-3,
                        "offset {offset} scale {scale} produced a fractional device origin"
                    );
                }
                assert!(x >= offset - 1e-3 && y >= offset - 1e-3);
                assert!(x + qr <= offset + side + 1e-3);
                assert!(y + qr <= offset + side + 1e-3);
            }
        }
    }
}

/// 两轴原点独立取整：x/y 落在不同分数位置时各自吸附，且都留在画布内。
///
/// 画布通常是正方形，但侧栏收缩、标签或内边距会让两轴起点不同；共用一次吸附结果会在
/// 一轴上越界或留下半像素，因此必须分别求解。
#[test]
fn qr_origin_aligns_each_axis_independently() {
    use super::super::layout::qr_layout;
    use gpui_kit::{Bounds, point, px, size};

    let matrix = patterned_qr(21, 2);
    // 宽 150 → 5 设备像素/模块、整幅 145；高 170 只影响纵向居中余量。
    let bounds = Bounds::new(point(px(4.5), px(9.25)), size(px(150.0), px(170.0)));
    let layout = qr_layout(&matrix, bounds, 1.0).expect("valid layout");
    assert!((layout.module_px() - 5.0).abs() < 1e-6);
    let (x, y) = layout.origin();
    assert!((x - x.round()).abs() < 1e-6, "x is off the device grid");
    assert!((y - y.round()).abs() < 1e-6, "y is off the device grid");
    assert!(x >= 4.5 && x + 145.0 <= 154.5 + 1e-3);
    assert!(y >= 9.25 && y + 145.0 <= 179.25 + 1e-3);
}

/// 目录模式必须关闭文件选择并保留目录选择；open/save 既有行为不变。
#[test]
fn directory_picker_disables_files_and_keeps_directory_selection() {
    use super::super::layout::path_prompt_options;
    use kerminal_plugin_protocol::native::PickerMode;

    let directory = path_prompt_options(PickerMode::Directory).expect("directory prompt");
    assert!(!directory.files);
    assert!(directory.directories);
    assert!(!directory.multiple);
    let open = path_prompt_options(PickerMode::Open).expect("open prompt");
    assert!(open.files && open.directories && !open.multiple);
    assert!(path_prompt_options(PickerMode::Save).is_none());
}

/// 目录 Path 与二维码必须经真实渲染实体绘制；无静区矩阵由宿主验证并补静区。
#[gpui_kit::test]
fn qrcode_and_directory_path_render_through_real_renderer(app: &mut gpui_kit::TestAppContext) {
    use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginViewIdentity};
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    let runtime_guard = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = ApplicationTaskExecutor::new(runtime_guard.handle().clone());
    let prepared = pending_open_plugin();
    let identity = PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: "main".into(),
        instance: "instance-qrcode".into(),
        generation: 1,
    };
    let matrix = (0..21)
        .map(|row| {
            (0..21)
                .map(|column| if (row + column) % 3 == 0 { '1' } else { '0' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let view_json = serde_json::json!({
        "schema":1,
        "data":{"qr":matrix,"share":{"dir":""}},
        "root":{"type":"stack","gap":"md","children":[
            {"type":"path","bind":"share.dir","label":"共享目录","mode":"directory"},
            {"type":"qrcode","matrix":"$qr","label":"扫码打开"}
        ]}
    });
    let view = Arc::new(
        kerminal_plugin_protocol::native::parse_native_view(
            &serde_json::to_vec(&view_json).unwrap(),
        )
        .unwrap(),
    );
    let config = super::NativeRendererConfig {
        prepared,
        view,
        identity,
        scope: serde_json::json!({}),
        saved_state: None,
        initial_data: None,
        runtime: Arc::new(PendingOpenRuntime::default()),
        executor,
        locale: "en".into(),
        content_identity: None,
        embedded_form: None,
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let renderer = super::NativeRenderer::mount(config, window, cx).unwrap();
        *mounted.borrow_mut() = Some(renderer.clone());
        gpui_kit::base::Root::new(renderer, window, cx)
    });
    let renderer = slot.borrow_mut().take().expect("renderer mounted");
    visual.run_until_parked();
    visual.update(|_, cx| {
        assert!(renderer.read(cx).error.is_none());
        renderer.update(cx, |renderer, _| renderer.cancellation.cancel());
    });
}
