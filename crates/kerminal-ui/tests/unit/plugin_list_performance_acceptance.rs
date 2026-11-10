//! 宿主列表分页必须与真实 renderer 的身份和关闭边界一致。
//! @author kongweiguang

use super::*;
use std::time::Duration;

/// 验收适配器不借用外部插件进程，身份只表示当前宿主实例。
fn fixture() -> HostListRuntime {
    HostListRuntime {
        identity: PluginViewIdentity {
            plugin: ExternalPluginId::parse("fixture.host-list").unwrap(),
            view: "records".into(),
            instance: "fixture-instance".into(),
            generation: 1,
        },
        epoch: 1,
        telemetry: Arc::new(Mutex::new(
            json!({"phase":"created","instance":"fixture-instance","instanceEpoch":1,"fetches":[]}),
        )),
    }
}

/// 每次请求显式固定身份、期限和代次，测试不能依赖默认成功。
fn request(
    runtime: &HostListRuntime,
    method: &str,
    generation: Option<u64>,
    params: Value,
) -> RuntimeRequest {
    RuntimeRequest {
        identity: runtime.identity.clone(),
        process_generation: generation,
        method: method.into(),
        params,
        deadline: Instant::now() + Duration::from_secs(5),
        cancellation: CancellationToken::new(),
    }
}

/// 行只按半开范围生成；关闭后旧实例不能继续读取或伪造回执。
#[tokio::test]
async fn pages_and_close_enforce_instance_lifecycle() {
    let runtime = fixture();
    assert!(runtime.cached_data(&runtime.identity).unwrap().is_none());
    runtime
        .request(request(&runtime, "view.open", None, json!({})))
        .await
        .unwrap();
    assert_eq!(
        runtime.cached_data(&runtime.identity).unwrap(),
        Some(json!({}))
    );
    let reply = runtime
        .request(request(
            &runtime,
            "view.fetch",
            Some(HOST_LIST_GENERATION),
            json!({"node":"records","range":[99998,100003],"parent_key":null,"sort":null}),
        ))
        .await
        .unwrap();
    assert_eq!(
        reply.value,
        json!({"total":100000,"rows":[{"id":"record-99998","name":"Record 99998"},{"id":"record-99999","name":"Record 99999"}]})
    );
    assert_eq!(
        runtime.telemetry.lock().unwrap()["fetches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        runtime
            .request(request(
                &runtime,
                "view.fetch",
                Some(HOST_LIST_GENERATION),
                json!({"node":"records","range":[0,501],"parent_key":null,"sort":null})
            ))
            .await
            .is_err()
    );
    runtime
        .request(request(
            &runtime,
            "view.close",
            Some(HOST_LIST_GENERATION),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(runtime.telemetry.lock().unwrap()["phase"], "closed");
    assert!(
        runtime
            .request(request(
                &runtime,
                "view.fetch",
                Some(HOST_LIST_GENERATION),
                json!({"node":"records","range":[0,1],"parent_key":null,"sort":null})
            ))
            .await
            .is_err()
    );
}

/// 取消、过期和错误实例都不应产生任何成功遥测。
#[tokio::test]
async fn cancelled_expired_and_stale_requests_are_rejected() {
    let runtime = fixture();
    let cancelled = request(&runtime, "view.open", None, json!({}));
    cancelled.cancellation.cancel();
    assert!(runtime.request(cancelled).await.is_err());
    let mut expired = request(&runtime, "view.open", None, json!({}));
    expired.deadline = Instant::now();
    assert!(runtime.request(expired).await.is_err());
    let mut stale = request(&runtime, "view.open", None, json!({}));
    stale.identity.instance = "another-instance".into();
    assert!(runtime.request(stale).await.is_err());
    assert_eq!(runtime.telemetry.lock().unwrap()["phase"], "created");
}

/// 挂载前先占位；失败可回滚重试，并发或重复创建在挂载前被拒绝，打开可先于确认到达。
#[tokio::test]
async fn creation_reservation_rolls_back_and_rejects_duplicates() {
    let telemetry = Arc::new(Mutex::new(
        json!({"phase":"new","instance":null,"instanceEpoch":0}),
    ));
    let creation = HostListCreation {
        telemetry: Arc::clone(&telemetry),
    };
    let epoch_a = creation.reserve("instance-a").unwrap();
    assert_eq!(telemetry.lock().unwrap()["phase"], "creating");
    assert_eq!(telemetry.lock().unwrap()["instanceEpoch"], epoch_a);
    assert!(creation.reserve("instance-b").is_err());
    // 旧世代不能确认或回滚当前占位；两者都必须保持 no-op 或失败。
    assert!(creation.confirm("instance-b", epoch_a).is_err());
    creation.rollback("instance-b", epoch_a).unwrap();
    assert_eq!(telemetry.lock().unwrap()["instance"], "instance-a");
    creation.rollback("instance-a", epoch_a).unwrap();
    assert_eq!(telemetry.lock().unwrap()["phase"], "new");
    assert!(telemetry.lock().unwrap()["instance"].is_null());

    let epoch_b = creation.reserve("instance-b").unwrap();
    assert!(epoch_b > epoch_a);
    let runtime = HostListRuntime {
        identity: PluginViewIdentity {
            plugin: ExternalPluginId::parse("fixture.host-list").unwrap(),
            view: "records".into(),
            instance: "instance-b".into(),
            generation: 1,
        },
        epoch: epoch_b,
        telemetry: Arc::clone(&telemetry),
    };
    // 打开请求可能在挂载返回前到达；确认必须接受已打开状态而不是丢弃实例。
    runtime
        .request(request(&runtime, "view.open", None, json!({})))
        .await
        .unwrap();
    creation.confirm("instance-b", epoch_b).unwrap();
    assert_eq!(telemetry.lock().unwrap()["phase"], "open");
    assert!(creation.reserve("instance-c").is_err());
    creation.rollback("instance-b", epoch_b).unwrap();
    assert_eq!(telemetry.lock().unwrap()["phase"], "open");
}

/// 正常关闭后同一工厂必须能按新身份重开；旧实例回执和错误代次仍不能命中新状态。
///
/// 该用例特别覆盖旧 runtime 在“新实例已占位但尚未打开”的 creating/created 窗口
/// 发送迟到 open/close/notify 的场景：只核对自身 identity 的实现会让这些回执污染
/// 新实例，因此必须依赖共享遥测的实例世代。
#[tokio::test]
async fn closed_creation_reopens_without_accepting_stale_instance_replies() {
    let telemetry = Arc::new(Mutex::new(
        json!({"phase":"new","instance":null,"instanceEpoch":0,"visibility":null,"fetches":[]}),
    ));
    let creation = HostListCreation {
        telemetry: Arc::clone(&telemetry),
    };
    let epoch_a = creation.reserve("instance-a").unwrap();
    let runtime_a = HostListRuntime {
        identity: PluginViewIdentity {
            plugin: ExternalPluginId::parse("fixture.host-list").unwrap(),
            view: "records".into(),
            instance: "instance-a".into(),
            generation: 1,
        },
        epoch: epoch_a,
        telemetry: Arc::clone(&telemetry),
    };
    runtime_a
        .request(request(&runtime_a, "view.open", None, json!({})))
        .await
        .unwrap();
    runtime_a
        .request(request(
            &runtime_a,
            "view.fetch",
            Some(HOST_LIST_GENERATION),
            json!({"node":"records","range":[0,40],"parent_key":null,"sort":null}),
        ))
        .await
        .unwrap();
    creation.confirm("instance-a", epoch_a).unwrap();
    runtime_a
        .request(request(
            &runtime_a,
            "view.close",
            Some(HOST_LIST_GENERATION),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(telemetry.lock().unwrap()["phase"], "closed");
    assert!(runtime_a.subscribe(&runtime_a.identity).is_err());

    // 挂载失败的回滚也必须回到可重试状态，且不保留上一实例的分页证据。
    let epoch_b = creation.reserve("instance-b").unwrap();
    assert!(epoch_b > epoch_a);
    creation.rollback("instance-b", epoch_b).unwrap();
    let reset = telemetry.lock().unwrap().clone();
    assert_eq!(reset["phase"], "new");
    assert!(reset["instance"].is_null());
    assert_eq!(reset["fetches"].as_array().unwrap().len(), 0);

    let epoch_c = creation.reserve("instance-b").unwrap();
    assert!(epoch_c > epoch_b);
    creation.confirm("instance-b", epoch_c).unwrap();
    assert_eq!(telemetry.lock().unwrap()["instance"], "instance-b");
    assert_eq!(telemetry.lock().unwrap()["phase"], "created");
    assert_eq!(telemetry.lock().unwrap()["instanceEpoch"], epoch_c);
    assert_eq!(
        telemetry.lock().unwrap()["fetches"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(telemetry.lock().unwrap()["visibility"].is_null());
    let runtime_b = HostListRuntime {
        identity: PluginViewIdentity {
            plugin: ExternalPluginId::parse("fixture.host-list").unwrap(),
            view: "records".into(),
            instance: "instance-b".into(),
            generation: 1,
        },
        epoch: epoch_c,
        telemetry: Arc::clone(&telemetry),
    };

    // creating/created 窗口最危险：旧 open 若只比较自身 identity，会把新实例
    // 直接推进到 open；它必须因世代不符失败，且不能改变新实例阶段。
    assert!(
        runtime_a
            .request(request(&runtime_a, "view.open", None, json!({})))
            .await
            .is_err()
    );
    assert_eq!(telemetry.lock().unwrap()["phase"], "created");
    assert!(runtime_a.cached_data(&runtime_a.identity).is_err());
    assert!(runtime_a.cached_state(&runtime_a.identity).is_err());
    assert!(runtime_a.subscribe(&runtime_a.identity).is_err());

    runtime_b
        .request(request(&runtime_b, "view.open", None, json!({})))
        .await
        .unwrap();
    assert_eq!(telemetry.lock().unwrap()["phase"], "open");

    // 旧实例的打开、分页、关闭与显隐回执都带旧身份和旧世代，不能影响新实例。
    for (method, generation, params) in [
        ("view.open", None, json!({})),
        (
            "view.fetch",
            Some(HOST_LIST_GENERATION),
            json!({"node":"records","range":[0,1],"parent_key":null,"sort":null}),
        ),
        ("view.close", Some(HOST_LIST_GENERATION), json!({})),
    ] {
        assert!(
            runtime_a
                .request(request(&runtime_a, method, generation, params))
                .await
                .is_err()
        );
    }
    assert!(
        runtime_a
            .notify(request(
                &runtime_a,
                "view.visibility",
                Some(HOST_LIST_GENERATION),
                json!({"visible":true,"focused":true}),
            ))
            .await
            .is_err()
    );
    // 旧 close 不能关闭新实例，旧显隐也不能写入新实例的证据。
    let after_stale = telemetry.lock().unwrap().clone();
    assert_eq!(after_stale["phase"], "open");
    assert_eq!(after_stale["instance"], "instance-b");
    assert!(after_stale["visibility"].is_null());
    assert_eq!(after_stale["fetches"].as_array().unwrap().len(), 0);
    // 新实例仍拒绝错误进程代次，旧代次不能伪装成新实例回执。
    assert!(
        runtime_b
            .request(request(
                &runtime_b,
                "view.fetch",
                Some(HOST_LIST_GENERATION + 1),
                json!({"node":"records","range":[0,1],"parent_key":null,"sort":null}),
            ))
            .await
            .is_err()
    );
    // 旧回执被拒后，新实例仍能正常分页并留下属于新实例的证据。
    assert!(
        runtime_b
            .request(request(
                &runtime_b,
                "view.fetch",
                Some(HOST_LIST_GENERATION),
                json!({"node":"records","range":[0,1],"parent_key":null,"sort":null}),
            ))
            .await
            .is_ok()
    );
    assert!(runtime_b.subscribe(&runtime_b.identity).is_ok());
    assert_eq!(
        telemetry.lock().unwrap()["fetches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(creation.reserve("instance-c").is_err());
}

/// 活动标签、内容作用域、注册表提供方和内容身份必须同时指向本轮外部列表；
/// 标签切换、宿主 fixture 或其它提供方都不能返回可用来源。
#[test]
fn list_source_binding_requires_active_scope_and_external_provider() {
    let expected = TabId::parse("tab-list").expect("tab id");
    let scope = ContentScope::Tab(expected.clone());
    assert_eq!(
        list_source_binding(
            &expected,
            Some(&expected),
            Some(&scope),
            Some(LIST_PERFORMANCE_PLUGIN_ID),
            LIST_PERFORMANCE_CONTENT_ID,
        ),
        (true, true, None)
    );
    assert!(
        !list_source_binding(
            &expected,
            Some(&expected),
            Some(&scope),
            Some("fixture.host-list"),
            HOST_LIST_ID,
        )
        .1
    );
    assert!(
        list_source_binding(
            &expected,
            Some(&expected),
            Some(&scope),
            Some("fixture.host-list"),
            HOST_LIST_ID,
        )
        .2
        .is_some()
    );
    assert!(
        list_source_binding(
            &expected,
            None,
            Some(&scope),
            Some(LIST_PERFORMANCE_PLUGIN_ID),
            LIST_PERFORMANCE_CONTENT_ID,
        )
        .2
        .is_some()
    );
    let other = TabId::parse("tab-other").expect("tab id");
    assert!(
        !list_source_binding(
            &expected,
            Some(&other),
            Some(&scope),
            Some(LIST_PERFORMANCE_PLUGIN_ID),
            LIST_PERFORMANCE_CONTENT_ID,
        )
        .0
    );
}
