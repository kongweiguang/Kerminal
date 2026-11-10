//! 插件主机表单的注册与实例边界，宿主保留草稿和写入生命周期。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use gpui_kit::{AnyView, App, SharedString, Window};
use kerminal_application::{
    AppError, AppResult,
    connections::host_types::{HostConfigurationCatalog, PluginHost, PluginHostDraft},
};
use tokio_util::sync::CancellationToken;

/// 已保存投影与新建默认分组由宿主提供，工厂无需访问工作区或 Root。
pub struct HostFormSeed {
    pub original: Option<PluginHost>,
    pub default_group: Option<String>,
}

/// 测试未来值由共享执行器运行，取消令牌必须传入实际后台工作。
pub type HostFormTestFuture = Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'static>>;

/// 测试构造器在共享执行器内接收受监督取消令牌，覆盖宿主关闭与用户取消。
pub type HostFormTest = Box<dyn FnOnce(CancellationToken) -> HostFormTestFuture + Send + 'static>;

/// 保存前在共享执行器内准备 Vault 候选，返回只含可持久化引用的草稿。
pub type HostFormPrepareFuture =
    Pin<Box<dyn Future<Output = AppResult<PluginHostDraft>> + Send + 'static>>;
pub type HostFormPrepare =
    Box<dyn FnOnce(PluginHostDraft, CancellationToken) -> HostFormPrepareFuture + Send + 'static>;

/// 工厂捕获插件服务，只能为已登记的提供方和类型创建窗口线程内的实例。
pub trait HostFormFactory: Send + Sync + 'static {
    /// 提供方身份与冻结的配置目录保持一致。
    fn provider_id(&self) -> &str;

    /// 类型身份不由本地化名称或显示顺序推导。
    fn type_id(&self) -> &str;

    /// 协议栏使用工厂提供的名称，而不在中心分派添加插件分支。
    fn display_name(&self) -> SharedString;

    /// 每个编辑任务创建独立实体；服务由工厂显式捕获。
    fn create(
        &self,
        seed: &HostFormSeed,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn HostFormInstance>>;
}

/// 实例拥有视图和短期输入；宿主负责保存、回读、取消与关闭门禁。
pub trait HostFormInstance {
    /// 每次返回同一个实体句柄，重绘不得重建控件或丢失焦点。
    fn view(&self) -> AnyView;

    /// 草稿只包含可持久化的非秘密数据，凭据由已有服务单独管理。
    fn snapshot(&self, cx: &App) -> AppResult<PluginHostDraft>;

    /// 默认草稿无需额外准备；秘密输入由具体表单捕获并在执行器中转换为引用。
    fn begin_prepare(&mut self, _cx: &App) -> AppResult<HostFormPrepare> {
        Ok(Box::new(|draft, _| Box::pin(async move { Ok(draft) })))
    }

    /// 包含所有尚未提交的输入，切换协议时实例仍由宿主持有。
    fn is_dirty(&self, cx: &App) -> bool;

    /// 保存和测试期间冻结输入，避免回执覆盖异步操作开始后的编辑。
    fn set_enabled(&mut self, enabled: bool, window: &mut Window, cx: &mut App);

    /// 默认不显示测试按钮，避免没有实际探测能力时伪装成功。
    fn supports_test(&self) -> bool {
        false
    }

    /// 插件可读取短期输入并构造测试任务，但不能在窗口线程执行 I/O。
    fn begin_test(&mut self, _cx: &mut App) -> AppResult<HostFormTest> {
        Err(AppError::unsupported("主机表单测试不可用"))
    }

    /// 只在显式切换或恢复焦点时调用，普通重绘不抢占焦点。
    fn focus(&mut self, _window: &mut Window, _cx: &mut App) {}

    /// 原生验收按字段路径写入真实输入控件；生产表单默认拒绝，避免绕过用户输入。
    #[cfg(feature = "right-rail-acceptance")]
    fn acceptance_set_field(
        &mut self,
        _path: &str,
        _value: &str,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AppResult<()> {
        Err(AppError::unsupported("主机表单不支持验收写入"))
    }

    /// 原生验收读取控件与草稿；只返回已挂载字段，不伪造未提供的值。
    #[cfg(feature = "right-rail-acceptance")]
    fn acceptance_snapshot(&self, _cx: &App) -> AppResult<serde_json::Value> {
        Err(AppError::unsupported("主机表单不支持验收读取"))
    }
}

/// 注册表仅保存工厂；表单实体由各窗口和编辑任务独立持有。
#[derive(Default)]
pub struct HostFormFactories {
    factories: BTreeMap<String, Arc<dyn HostFormFactory>>,
}

impl HostFormFactories {
    /// 先核对冻结身份并拒绝重复类型，防止另一个插件接管已有配置。
    pub fn new(
        catalog: &HostConfigurationCatalog,
        factories: impl IntoIterator<Item = Arc<dyn HostFormFactory>>,
    ) -> AppResult<Self> {
        let mut registered = BTreeMap::new();
        for factory in factories {
            let provider_id = factory.provider_id();
            let type_id = factory.type_id();
            if provider_id == "kerminal" || catalog.get(provider_id, type_id).is_none() {
                return Err(AppError::invalid("主机表单与配置目录不匹配"));
            }
            if registered.contains_key(type_id) {
                return Err(AppError::conflict("主机表单重复注册"));
            }
            registered.insert(type_id.to_owned(), factory);
        }
        Ok(Self {
            factories: registered,
        })
    }

    /// 保持冻结类型顺序，使协议栏在语言切换和刷新后稳定。
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn HostFormFactory>> {
        self.factories.values()
    }

    /// 编辑时同时核对提供方身份，缺失的插件不能被其他表单接管。
    pub fn get(&self, provider_id: &str, type_id: &str) -> Option<Arc<dyn HostFormFactory>> {
        self.factories
            .get(type_id)
            .filter(|factory| factory.provider_id() == provider_id)
            .cloned()
    }

    /// 新建时类型在注册阶段已唯一，可直接按协议栏选择定位工厂。
    pub fn by_type(&self, type_id: &str) -> Option<Arc<dyn HostFormFactory>> {
        self.factories.get(type_id).cloned()
    }
}
