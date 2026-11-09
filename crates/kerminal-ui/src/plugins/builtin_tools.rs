//! 内置工具只借用宿主已经持有的实体，不另建业务资源或订阅。
//!
//! @author kongweiguang

use std::time::Instant;

use gpui_kit::{AnyView, AnyWeakView, App, Window};
use kerminal_application::{AppError, AppResult, preferences::ToolRailToolId};

use super::{ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPresentation, ToolScope};

pub(super) struct BuiltinToolFactory {
    pub(super) tool: ToolRailToolId,
    pub(super) scope: ToolScope,
    pub(super) presentation: ToolPresentation,
    pub(super) view: Option<AnyWeakView>,
}

impl ToolFactory for BuiltinToolFactory {
    /// 沿用配置中的稳定身份，使内置工具与扩展工具使用相同的宿主索引。
    ///
    /// @author kongweiguang
    fn tool_id(&self) -> &str {
        self.tool.as_str()
    }

    /// 显示信息来自内置目录，不由视图或标签状态推导。
    ///
    /// @author kongweiguang
    fn presentation(&self) -> ToolPresentation {
        self.presentation.clone()
    }

    /// 实例范围由原有实体所有者决定，容器导航与其他标签工具可以不同。
    ///
    /// @author kongweiguang
    fn scope(&self) -> ToolScope {
        self.scope
    }

    /// 工厂不延长原实体寿命；创建时才获取强引用，失效或未装配时明确报错。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        _context: &ToolContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        let view = self
            .view
            .as_ref()
            .and_then(AnyWeakView::upgrade)
            .ok_or_else(|| AppError::not_found("内置工具服务尚未装配"))?;
        Ok(Box::new(BuiltinToolInstance { view }))
    }
}

struct BuiltinToolInstance {
    view: AnyView,
}

impl ToolInstance for BuiltinToolInstance {
    /// 保留原实体身份，移动宿主时继续使用同一份草稿和订阅。
    ///
    /// @author kongweiguang
    fn view(&self) -> AnyView {
        self.view.clone()
    }

    /// 此适配器只释放视图引用；实体、任务和订阅由原工作区关闭流程回收。
    ///
    /// @author kongweiguang
    fn begin_close(&mut self, _deadline: Instant, _cx: &mut App) -> AppResult<ToolCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}
