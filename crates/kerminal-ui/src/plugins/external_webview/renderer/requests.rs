//! 网页请求分派和页面初始化；与承载实体共享完整身份和代次。
//! @author kongweiguang

use super::*;

impl WebViewRenderer {
    /// 页面动作固定到同一运行实例；宿主能力交给应用层既有权限路由。
    pub(super) fn page_request(
        &mut self,
        request: PageRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PageRequest {
            id,
            sequence,
            method,
            params,
            cancellation,
        } = request;
        let epoch = self.page_epoch;
        let generation = self.process_generation;
        if self.closing.load(Ordering::Acquire)
            || generation.is_none()
            || (!self.page_ready && method.starts_with("view."))
        {
            if let Some(reply) =
                self.bridge
                    .finish(&id, sequence, Err((-32004, "view unavailable".into())))
            {
                self.post(&reply, cx);
            }
            return;
        }
        let runtime = self.runtime.clone();
        let executor = self.executor.clone();
        let identity = self.identity.clone();
        let lifetime = self.page_cancellation.child_token();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    let routed = if method == "events.subscribe" {
                        let target = PluginWebViewEventTarget::new(
                            identity,
                            generation.unwrap_or(0),
                            epoch,
                            sequence,
                        );
                        runtime
                            .subscribe_webview_events(target, params, token, lifetime)
                            .await
                            .map(|stream| (json!({}), Some(stream)))
                            .map_err(|error| (error.code, error.message))
                    } else if matches!(method.as_str(), "view.action" | "view.fetch") {
                        runtime
                            .request(RuntimeRequest {
                                identity,
                                process_generation: generation,
                                method,
                                params,
                                deadline: Instant::now() + REQUEST_TIMEOUT,
                                cancellation: token,
                            })
                            .await
                            .and_then(|reply| {
                                if Some(reply.process_generation) == generation {
                                    Ok(reply.value)
                                } else {
                                    Err(AppError::conflict("插件进程代次已改变"))
                                }
                            })
                            .map(|value| (value, None))
                            .map_err(rpc_error)
                    } else {
                        runtime
                            .route_webview(identity, method, params, token)
                            .await
                            .map(|value| (value, None))
                            .map_err(|error| (error.code, error.message))
                    };
                    Ok(routed)
                })
                .await
                .unwrap_or_else(|error| Err(rpc_error(error)));
            let _ = this.update_in(cx, |renderer, window, cx| {
                if renderer.page_epoch != epoch || renderer.process_generation != generation {
                    return;
                }
                match result {
                    Ok((value, stream)) => {
                        if let Some(frame) = renderer.bridge.finish(&id, sequence, Ok(value)) {
                            if let Some(stream) = stream {
                                renderer.watch_page_events(
                                    stream, sequence, epoch, generation, window, cx,
                                );
                            }
                            renderer.post(&frame, cx);
                        }
                    }
                    Err(error) => {
                        if let Some(frame) = renderer.bridge.finish(&id, sequence, Err(error)) {
                            renderer.post(&frame, cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// 外链沿用 shell.openExternal 权限及窗口绑定，不能调用系统打开接口绕过授权。
    pub(super) fn open_external(
        &mut self,
        url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        let runtime = self.runtime.clone();
        let identity = self.identity.clone();
        let executor = self.executor.clone();
        let token = self.page_cancellation.child_token();
        let epoch = self.page_epoch;
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(token, move |token| async move {
                    Ok(runtime
                        .route_webview(
                            identity,
                            "shell.openExternal".into(),
                            json!({"url":url}),
                            token,
                        )
                        .await)
                })
                .await;
            let message = match result {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(error.message),
                Err(error) => Some(error.public_message().into()),
            };
            if let Some(message) = message {
                let _ = this.update(cx, |renderer, cx| {
                    if renderer.page_epoch == epoch {
                        renderer.error = Some(message);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// 宿主请求有明确超时；页面未注册生命周期处理器时显示恢复入口而不永远等待。
    pub(super) fn initialize_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.page_loaded || self.process_generation.is_none() || self.page_open.is_some() {
            return;
        }
        let data = match self.runtime.cached_data(&self.identity) {
            Ok(Some(data)) => data,
            Ok(None) => {
                self.failure = Some("页面数据尚未准备好，请重试".into());
                return;
            }
            Err(error) => {
                self.failure = Some(error.public_message().into());
                return;
            }
        };
        let params = json!({"instance":self.identity.instance,"scope":self.scope,"data":data,"state":self.saved_state.as_ref().map(|state| &state.state),"state_version":self.saved_state.as_ref().map(|state| state.state_version)});
        let (id, frame) = match self.bridge.begin_host_request("view.open", params) {
            Ok(request) => request,
            Err(error) => {
                self.failure = Some(error.public_message().into());
                return;
            }
        };
        self.page_open = Some(id.clone());
        self.pending_updates.clear();
        self.post(&frame, cx);
        let epoch = self.page_epoch;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(OPEN_TIMEOUT).await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                if renderer.page_epoch == epoch && renderer.page_open.as_ref() == Some(&id) {
                    renderer.bridge.cancel_host_request(&id);
                    renderer.page_open = None;
                    renderer.failure = Some("页面初始化超时，请重新加载".into());
                    renderer.sync_visibility(window, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
