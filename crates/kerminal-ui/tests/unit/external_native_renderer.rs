//! 验证集合缓存身份不因树父项或排序方向不同而互相覆盖。
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_plugin_protocol::runtime::{ViewFetchSort, ViewSortDirection};

use super::{CollectionQuery, collection_cache_key};

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

/// 挂起打开请求模拟进程缓慢启动，记录其他请求以检查是否发生抢先关闭。
#[derive(Default)]
struct PendingOpenRuntime {
    requests: std::sync::Mutex<Vec<String>>,
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
        self.requests.lock().unwrap().push(request.method);
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
