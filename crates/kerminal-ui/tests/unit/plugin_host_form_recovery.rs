//! 未知保存的读回采用必须能结束旧表单，即使外部已更换提供方。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::{AnyView, AppContext as _, IntoElement, Render, SharedString, TestAppContext, div};
use kerminal_application::AppResult;

use crate::features::connections::panel::connections_editor_dirty_tests::{fixture, update_panel};

struct FormView;

impl Render for FormView {
    /// 回归只需真实保留实体，不依赖插件的具体表单控件。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct OriginalFactory;

impl HostFormFactory for OriginalFactory {
    /// 固定原提供方，验证采用新存储状态时不会借它解释其它类型。
    fn provider_id(&self) -> &str {
        "original.provider"
    }

    /// 原表单类型与外部更新后的记录刻意不同。
    fn type_id(&self) -> &str {
        "original.type"
    }

    /// 显示文案不参与主机身份判定。
    fn display_name(&self) -> SharedString {
        "Original".into()
    }

    /// 错误类型不得交给原工厂；调用本身说明恢复边界已经失效。
    fn create(
        &self,
        _: &HostFormSeed,
        _: &mut Window,
        _: &mut App,
    ) -> AppResult<Box<dyn HostFormInstance>> {
        panic!("外部新类型不能由旧工厂创建")
    }
}

struct OriginalInstance(AnyView);

impl HostFormInstance for OriginalInstance {
    /// 同一实例保持原视图，直到用户明确采用存储状态。
    fn view(&self) -> AnyView {
        self.0.clone()
    }

    /// 采用读回不应再次保存旧候选。
    fn snapshot(&self, _: &App) -> AppResult<PluginHostDraft> {
        panic!("采用读回不能重建保存请求")
    }

    /// 原候选仍是脏稿，不能靠将脏状态清零掩盖关闭死锁。
    fn is_dirty(&self, _: &App) -> bool {
        true
    }

    /// 此路径直接释放旧实例，无需改变输入控件状态。
    fn set_enabled(&mut self, _: bool, _: &mut Window, _: &mut App) {}
}

/// 显式采用更换类型、提供方或已不可用的读回，均解除未知结果门禁且不重新写配置。
#[gpui_kit::test]
fn adopting_changed_plugin_readback_releases_unknown_save_guard(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    for (provider, type_id, available) in [
        ("new.provider", "original.type", true),
        ("original.provider", "new.type", true),
        ("original.provider", "original.type", false),
    ] {
        update_panel(&panel, visual, |panel, window, cx| {
            let host_id = HostId::new();
            let host = PluginHost {
                id: host_id.clone(),
                provider_id: provider.into(),
                type_id: type_id.into(),
                name: "Stored host".into(),
                group_id: None,
                sort_order: 0,
                configuration_version: 1,
                configuration: available
                    .then(|| toml::from_str("text = 'fixture'").expect("合法测试配置")),
                runtime_available: available,
                revision: ConfigRevision::from_sha256("stored-revision".into()),
            };
            let view = cx.new(|_| FormView);
            panel.plugin_forms.active = Some("original.type".into());
            panel.plugin_forms.forms.insert(
                "original.type".into(),
                HostedForm {
                    factory: Arc::new(OriginalFactory),
                    instance: Box::new(OriginalInstance(view.into())),
                    original: None,
                    host_id,
                    generation: 1,
                    phase: Phase::Unknown,
                    cancellation: None,
                    pending_save: None,
                    readback: Some(Some(host)),
                },
            );
            assert!(panel.plugin_forms.write_blocked());
            panel.use_plugin_readback(window, cx);
            assert!(
                !panel.plugin_forms.write_blocked(),
                "采用存储状态后应解除门禁"
            );
            assert!(
                !panel.plugin_forms.is_open(),
                "旧表单应关闭，交由目录提供新类型入口"
            );
            assert!(!panel.editor_open());
            assert!(panel.status.is_some());
        });
    }
}
