//! 标签事件必须来自真实 Root 导航，并沿正式应用事件服务交付。
//! @author kongweiguang

use super::*;
use crate::features::workspace_tabs::WorkspaceTabEvent;
use async_trait::async_trait;
use kerminal_application::{
    plugin_host_api::PluginEventSubscriptions,
    plugin_ports::{PluginHostRouter, PluginProcessMessage, PluginProcessPort},
};
use kerminal_domain::plugins::{PluginId as RuntimePluginId, PluginRuntimeState};
use serde_json::{Value, json};
use std::{sync::Weak, time::Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct EventProcess {
    sent: Mutex<Vec<Value>>,
}

#[async_trait]
impl PluginProcessPort for EventProcess {
    /// 本回归没有反向进程调用，拒绝意外装配避免把缺失桥接误判为成功。
    fn register_host_router(&self, _: Weak<dyn PluginHostRouter>) -> AppResult<()> {
        Err(AppError::unsupported("测试不建立进程路由"))
    }

    /// 订阅和正式发布共享固定代次，通知仍经过应用服务的代次检查。
    fn state(&self, _: &RuntimePluginId) -> AppResult<PluginRuntimeState> {
        Ok(PluginRuntimeState::Ready { generation: 1 })
    }

    /// 原生内存内容不启动外部进程，意外激活必须使测试失败。
    async fn activate(&self, _: &RuntimePluginId, _: Instant) -> AppResult<u64> {
        Err(AppError::unsupported("测试不启动进程"))
    }

    /// 导航只需单向事件；拒绝 RPC 防止测试绕过发布路径。
    async fn call(
        &self,
        _: &RuntimePluginId,
        _: u64,
        _: &str,
        _: Value,
        _: Instant,
        _: CancellationToken,
    ) -> AppResult<Value> {
        Err(AppError::unsupported("测试不调用进程"))
    }

    /// 只记录事件服务实际交付的通知，测试自身不调用发布或队列接口。
    async fn notify(
        &self,
        _: &RuntimePluginId,
        generation: u64,
        method: &str,
        params: Value,
        _: Instant,
    ) -> AppResult<()> {
        assert_eq!(generation, 1);
        assert_eq!(method, "event");
        self.sent.lock().unwrap().push(params);
        Ok(())
    }

    /// 测试不模拟进程回传；正式事件发布不应依赖此通道。
    fn subscribe(&self) -> broadcast::Receiver<PluginProcessMessage> {
        broadcast::channel(1).1
    }

    /// 替身不持有系统资源，关闭不会伪造任何运行中进程的回收。
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 实际打开、激活与重复刷新必须交付三个有序通知；测试窗口先取得模拟焦点，不绕过生产导航门禁。
#[gpui_kit::test]
fn root_navigation_publishes_ordered_tab_changes(app: &mut TestAppContext) {
    let provider = PluginId::new("fixture.recovery-provider").unwrap();
    let mut registry = PluginRegistryBuilder::new();
    registry.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    registry.register_content_type(ContentDescriptor::new(
        ContentTypeId::new("fixture.recovery-content").unwrap(),
        provider.clone(),
        "Event fixture",
    ));
    let registry = Arc::new(registry.freeze().unwrap());
    let factory = Arc::new(RecoveryFactory(Arc::new(FailureSwitches::default())));
    let process = Arc::new(EventProcess::default());
    let events = Arc::new(PluginEventSubscriptions::new(process.clone()));
    events
        .subscribe(
            &RuntimePluginId::parse(provider.as_str()).unwrap(),
            1,
            vec!["tab.changed".into()],
            CancellationToken::new(),
        )
        .unwrap();
    let (runtime, root, _, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |services| {
            services
                .with_plugins(registry)
                .with_content_factories(vec![factory])
        },
    );
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.activate_window();
        window.focus(&root.read(app).focus_handle.clone(), app);
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            root.workspace_controller = Some(WorkspaceController::from_missing().unwrap());
            root.workspace_load_pending = false;
            root.services.plugin_events = Some(events);
        });
    });
    let mut tabs = Vec::new();
    for title in ["First", "Second"] {
        tabs.push(visual.update(|window, app| {
            root.update(app, |root, cx| {
                let id = root
                    .open_plugin_tab(
                        &crate::plugins::actions::OpenContentTab {
                            content_id: "fixture.recovery-content".into(),
                            title: title.into(),
                            state_version: 1,
                            state: json!({"text": title}),
                        },
                        window,
                        cx,
                    )
                    .unwrap();
                root.refresh_workspace_views(cx);
                id
            })
        }));
        settle(&runtime, visual);
    }
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(root.handle_workspace_tab_event(
                &WorkspaceTabEvent::Activate(tabs[0].clone()),
                window,
                cx,
            ));
            root.activate_current_workspace_surface_after_close(window, cx);
            root.refresh_workspace_views(cx);
        });
    });
    settle(&runtime, visual);
    assert_eq!(
        *process.sent.lock().unwrap(),
        vec![
            json!({"name": "tab.changed", "payload": {"tab": tabs[0].as_str()}}),
            json!({"name": "tab.changed", "payload": {"tab": tabs[1].as_str()}}),
            json!({"name": "tab.changed", "payload": {"tab": tabs[0].as_str()}}),
        ]
    );
}
