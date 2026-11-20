//! 退出前等待插件内容和工具资源的关闭回执。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc, time::Instant};

use gpui_kit::{App, Context, SharedString};
use kerminal_application::{AppError, AppResult};

use super::{QuitIntent, RootHostLifecycle, RootView};
use crate::plugins::{
    content::{ContentCloseFuture, ContentCloseToken, ContentHost, ContentPhase},
    tools::{PluginTools, ToolCloseToken, ToolPhase},
};

enum PluginCloseToken {
    Content(ContentCloseToken),
    Tool(ToolCloseToken),
}

struct PluginCloseBatch {
    intent: QuitIntent,
    deadline: Instant,
    remaining: usize,
    first_error: Option<SharedString>,
}

/// 一次遍历启动全部可关闭实例；单个同步失败不能丢弃其他已启动的关闭任务。
///
/// @author kongweiguang
fn begin_plugin_closes(
    contents: &mut ContentHost,
    tools: &mut PluginTools,
    deadline: Instant,
    cx: &mut App,
) -> (
    Vec<(PluginCloseToken, ContentCloseFuture)>,
    Option<SharedString>,
) {
    let mut jobs = Vec::new();
    let mut first_error = None;
    for scope in contents.scopes() {
        match contents.phase(&scope) {
            None | Some(ContentPhase::Closed) => {}
            Some(ContentPhase::Closing) => {
                first_error.get_or_insert_with(|| {
                    crate::locale::text(
                        "内容仍在关闭，请等待原操作完成后重试。",
                        "Content is still closing. Retry after the previous operation finishes.",
                    )
                });
            }
            Some(ContentPhase::Open | ContentPhase::CloseFailed) => {
                match contents.begin_close(&scope, deadline, cx) {
                    Ok((token, future)) => jobs.push((PluginCloseToken::Content(token), future)),
                    Err(error) => {
                        first_error.get_or_insert_with(|| error.public_message().to_owned().into());
                    }
                }
            }
        }
    }
    for (tool, context) in tools.all_instance_contexts() {
        match tools.phase(tool, &context) {
            None | Some(ToolPhase::Closed) => {}
            Some(ToolPhase::Closing) => {
                first_error.get_or_insert_with(|| {
                    crate::locale::text(
                        "工具仍在关闭，请等待原操作完成后重试。",
                        "A tool is still closing. Retry after the previous operation finishes.",
                    )
                });
            }
            Some(ToolPhase::Open | ToolPhase::CloseFailed) => {
                match tools.begin_close(tool, &context, deadline, cx) {
                    Ok((token, future)) => jobs.push((PluginCloseToken::Tool(token), future)),
                    Err(error) => {
                        first_error.get_or_insert_with(|| error.public_message().to_owned().into());
                    }
                }
            }
        }
    }
    (jobs, first_error)
}

/// 每个异步回执立即交还原 owner，执行器拒绝任务也要解除该代次的 Closing。
///
/// @author kongweiguang
fn complete_plugin_close(
    contents: &mut ContentHost,
    tools: &mut PluginTools,
    token: PluginCloseToken,
    result: AppResult<()>,
) -> AppResult<()> {
    match token {
        PluginCloseToken::Content(token) => contents.complete_close(token, result),
        PluginCloseToken::Tool(token) => tools.complete_close(token, result),
    }
}

/// 所有实例均收到成功回执后才释放；未完成或失败的实例继续保留原 owner。
///
/// @author kongweiguang
fn finalize_plugin_closes(contents: &mut ContentHost, tools: &mut PluginTools) -> AppResult<()> {
    let scopes = contents.scopes();
    let contexts = tools.all_instance_contexts();
    if scopes
        .iter()
        .any(|scope| contents.phase(scope) != Some(ContentPhase::Closed))
        || contexts
            .iter()
            .any(|(tool, context)| tools.phase(*tool, context) != Some(ToolPhase::Closed))
    {
        return Err(AppError::conflict("插件资源尚未全部完成关闭"));
    }
    for scope in scopes {
        contents.finalize_close(&scope)?;
    }
    for (tool, context) in contexts {
        tools.finalize_close(tool, &context)?;
    }
    Ok(())
}

impl RootView {
    /// 提交收尾后拒绝新资源；保留原意图让窗口在插件回执前也能显示正确的操作。
    ///
    /// @author kongweiguang
    pub(super) fn begin_content_shutdown(
        &mut self,
        intent: QuitIntent,
        deadline: Instant,
        cx: &mut Context<Self>,
    ) {
        self.cancel_plugin_host_open();
        self.host_lifecycle = RootHostLifecycle::Stopping;
        self.plugin_commands.cancel();
        self.host_shutdown_requested = true;
        self.pending_quit_intent = None;
        self.quit_ready_intent = Some(intent.clone());
        let (jobs, first_error) =
            begin_plugin_closes(&mut self.content_host, &mut self.plugin_tools, deadline, cx);
        if jobs.is_empty() {
            self.finish_plugin_closes(intent, deadline, first_error, cx);
            return;
        }
        let batch = Rc::new(RefCell::new(PluginCloseBatch {
            intent,
            deadline,
            remaining: jobs.len(),
            first_error,
        }));
        for (token, future) in jobs {
            let batch = Rc::clone(&batch);
            let executor = self.services.executor.clone();
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run(move |_| {
                        kerminal_application::executor::await_until(
                            deadline,
                            "关闭插件资源",
                            future,
                        )
                    })
                    .await;
                let _ = this.update(cx, |root, cx| {
                    let result = complete_plugin_close(
                        &mut root.content_host,
                        &mut root.plugin_tools,
                        token,
                        result,
                    );
                    let finished = {
                        let mut batch = batch.borrow_mut();
                        if let Err(error) = result {
                            batch
                                .first_error
                                .get_or_insert_with(|| error.public_message().to_owned().into());
                        }
                        batch.remaining -= 1;
                        (batch.remaining == 0).then(|| {
                            (
                                batch.intent.clone(),
                                batch.deadline,
                                batch.first_error.take(),
                            )
                        })
                    };
                    if let Some((intent, deadline, error)) = finished {
                        root.finish_plugin_closes(intent, deadline, error, cx);
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// 失败保留完整收尾身份；只有全部回执成功才进入桌面资源收尾。
    ///
    /// @author kongweiguang
    fn finish_plugin_closes(
        &mut self,
        intent: QuitIntent,
        deadline: Instant,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let error = error.or_else(|| {
            finalize_plugin_closes(&mut self.content_host, &mut self.plugin_tools)
                .err()
                .map(|error| error.public_message().to_owned().into())
        });
        if let Some(error) = error {
            self.fail_content_shutdown(intent, error, cx);
            return;
        }
        self.failed_quit_intent = None;
        self.finish_quit_after_content_shutdown(intent, deadline, cx);
    }

    /// 部分关闭不可撤销，独立保留首个错误和原意图供用户显式重试。
    ///
    /// @author kongweiguang
    pub(super) fn fail_content_shutdown(
        &mut self,
        intent: QuitIntent,
        message: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.pending_quit_intent = None;
        self.quit_ready_emitted = false;
        self.quit_ready_intent = None;
        self.failed_quit_intent = Some(intent);
        self.host_lifecycle = RootHostLifecycle::StopFailed;
        self.plugin_commands.cancel();
        self.host_shutdown_requested = true;
        self.host_failure_message = Some(message.clone());
        self.status = Some(message);
        cx.notify();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_shutdown.rs"]
mod tests;
