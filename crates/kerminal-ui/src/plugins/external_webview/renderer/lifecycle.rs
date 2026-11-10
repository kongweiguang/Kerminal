//! 网页运行态恢复与订阅；与承载实体共享完整身份和代次。
//! @author kongweiguang

use super::*;

impl WebViewRenderer {
    /// 事件只进入订阅时的页面与进程代次；替换任务会 Drop 旧接收端并同步撤销槽位。
    pub(super) fn watch_page_events(
        &mut self,
        mut stream: Box<dyn WebViewEvents>,
        sequence: u64,
        epoch: u64,
        generation: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sequence < self.event_sequence {
            return;
        }
        self.event_sequence = sequence;
        self.event_watch = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let result = stream.recv().await;
                match result {
                    Ok(Some(event)) => {
                        let delivered = this.update_in(cx, |renderer, _, cx| {
                            if renderer.page_epoch != epoch
                                || renderer.process_generation != generation
                                || renderer.event_sequence != sequence
                                || renderer.closing.load(Ordering::Acquire)
                            {
                                return false;
                            }
                            let frame = renderer.bridge.notification("event", event);
                            renderer.post(&frame, cx);
                            true
                        });
                        if !matches!(delivered, Ok(true)) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let _ = this.update(cx, |renderer, cx| {
                            if renderer.page_epoch == epoch && renderer.event_sequence == sequence {
                                renderer.error = Some("网页事件连接已中断，请重新订阅".into());
                                cx.notify();
                            }
                        });
                        break;
                    }
                }
            }
        }));
    }

    /// 打开先登记运行服务，再初始化网页；页面缺少运行时仍允许宿主完成关闭回收。
    pub(super) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_in_flight || self.opened {
            return;
        }
        self.open_in_flight = true;
        let params = match &self.saved_state {
            Some(state) => {
                json!({"scope":self.scope,"state_version":state.state_version,"state":state.state})
            }
            None => json!({"scope":self.scope}),
        };
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: None,
            method: "view.open".into(),
            params,
            deadline: Instant::now() + OPEN_TIMEOUT,
            cancellation: self.cancellation.child_token(),
        };
        let runtime = self.runtime.clone();
        let executor = self.executor.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(request.cancellation.clone(), move |token| {
                    runtime.request(RuntimeRequest {
                        cancellation: token,
                        ..request
                    })
                })
                .await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                renderer.open_in_flight = false;
                match result {
                    Ok(reply) => {
                        renderer.opened = true;
                        renderer.process_generation = Some(reply.process_generation);
                        renderer.initialize_page(window, cx);
                    }
                    Err(error) => renderer.failure = Some(error.public_message().into()),
                }
                renderer.sync_visibility(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 所有运行消息仍校验完整身份；关闭和取消直接结束订阅，不遗留后台等待。
    pub(super) fn watch(
        &mut self,
        mut subscription: Box<dyn RuntimeSubscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cancellation = self.cancellation.child_token();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let event = subscription.recv(cancellation.clone()).await;
                if cancellation.is_cancelled() {
                    break;
                }
                match event {
                    Ok(Some(event)) => {
                        if this
                            .update_in(cx, |renderer, window, cx| {
                                renderer.runtime_event(event, window, cx)
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = this.update(cx, |renderer, cx| {
                            renderer.failure = Some("插件事件连接已中断，请重试".into());
                            cx.notify();
                        });
                        break;
                    }
                }
            }
        })
        .detach();
    }

    /// 统一检测网页进程退出或无响应；隐藏及系统睡眠期间不把定时器暂停误判为崩溃。
    pub(super) fn watch_health(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancellation = self.cancellation.child_token();
        cx.spawn_in(window, async move |this, cx| {
            let mut previous = Instant::now();
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if cancellation.is_cancelled() {
                    break;
                }
                let now = Instant::now();
                let resumed = now.duration_since(previous) > Duration::from_secs(5);
                previous = now;
                if this
                    .update_in(cx, |renderer, window, cx| {
                        if resumed {
                            renderer.last_heartbeat = now;
                        }
                        if renderer.visible
                            && renderer.page.is_some()
                            && renderer.failure.is_none()
                            && now.duration_since(renderer.last_heartbeat) > Duration::from_secs(15)
                        {
                            renderer.abort_page_requests(cx);
                            renderer.page_ready = false;
                            renderer.failure = Some("页面已停止响应，请重新加载".into());
                            renderer.sync_visibility(window, cx);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// 自动恢复只重新初始化已登记实例，不重放页面动作；资源重载才替换页面。
    pub(super) fn runtime_event(
        &mut self,
        event: RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            RuntimeEvent::ViewUpdate {
                identity,
                process_generation,
                patch,
            } if identity == self.identity
                && self.process_generation == Some(process_generation)
                && self.page_loaded =>
            {
                if self.page_ready {
                    let frame = self.bridge.notification(
                        "view.update",
                        json!({"instance":self.identity.instance,"patch":patch}),
                    );
                    self.post(&frame, cx);
                } else if self.page_open.is_some() {
                    if self.pending_updates.len() < 128 {
                        self.pending_updates.push(patch);
                    } else {
                        if let Some(id) = self.page_open.take() {
                            self.bridge.cancel_host_request(&id);
                        }
                        self.initialize_page(window, cx);
                    }
                }
            }
            RuntimeEvent::ViewState { identity, state } if identity == self.identity => {
                self.saved_state = Some(state);
                if let Some(identity) = &self.content_identity {
                    window.dispatch_action(
                        Box::new(ContentStateChanged {
                            identity: identity.clone(),
                        }),
                        cx,
                    );
                }
            }
            RuntimeEvent::Progress {
                identity,
                process_generation,
                token,
                message,
                percent,
            } if identity == self.identity
                && self.process_generation == Some(process_generation) =>
            {
                let frame = self.bridge.notification(
                    "$/progress",
                    json!({"token":token,"message":message,"percent":percent}),
                );
                self.post(&frame, cx);
            }
            RuntimeEvent::ProcessState { plugin, state } if plugin == self.identity.plugin => {
                match state {
                    PluginRuntimeState::Crashed { generation }
                        if self.process_generation == Some(generation) =>
                    {
                        self.process_generation = None;
                        self.retired_generation = Some(generation);
                        self.abort_page_requests(cx);
                        self.page_open = None;
                        self.page_ready = false;
                        self.error = Some("插件进程已停止，正在恢复；未保存输入仍保留".into());
                    }
                    PluginRuntimeState::Failed => {
                        self.process_generation = None;
                        self.abort_page_requests(cx);
                        self.page_open = None;
                        self.page_ready = false;
                        self.error = Some("插件进程不可用，请重试".into());
                    }
                    _ => {}
                }
            }
            RuntimeEvent::ResourcesReloaded { plugin, prepared }
                if plugin == self.identity.plugin =>
            {
                self.prepared = prepared;
                self.process_generation = None;
                self.replace_page(window, cx);
            }
            RuntimeEvent::ViewReopened {
                identity,
                process_generation,
            } if identity == self.identity
                && self
                    .retired_generation
                    .is_none_or(|retired| process_generation > retired)
                && self
                    .process_generation
                    .is_none_or(|current| process_generation >= current) =>
            {
                self.process_generation = Some(process_generation);
                self.opened = true;
                self.reopening = false;
                self.error = None;
                if self.page.is_none() {
                    self.replace_page(window, cx);
                } else {
                    self.failure = None;
                    self.initialize_page(window, cx);
                }
            }
            RuntimeEvent::ViewReopenFailed {
                identity, message, ..
            } if identity == self.identity => {
                self.reopening = false;
                self.error = Some(message);
            }
            RuntimeEvent::ResourcesReloadFailed { plugin, message }
                if plugin == self.identity.plugin =>
            {
                self.error = Some(message)
            }
            RuntimeEvent::ResyncRequired { .. } => {
                self.abort_page_requests(cx);
                self.page_open = None;
                self.page_ready = false;
                self.initialize_page(window, cx);
            }
            _ => {}
        }
        self.sync_visibility(window, cx);
        cx.notify();
    }

    /// 用户明确重试才恢复进程或重新创建页面，业务动作永不自动重放。
    pub(super) fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.opened {
            self.failure = None;
            if self.page.is_none() {
                self.replace_page(window, cx);
            }
            self.open(window, cx);
            return;
        }
        if self.process_generation.is_some() {
            self.replace_page(window, cx);
            return;
        }
        if self.reopening {
            return;
        }
        self.reopening = true;
        let runtime = self.runtime.clone();
        let executor = self.executor.clone();
        let identity = self.identity.clone();
        let cancellation = self.cancellation.child_token();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| {
                    runtime.retry_reopen(identity, Instant::now() + OPEN_TIMEOUT, token)
                })
                .await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                renderer.reopening = false;
                match result {
                    Ok(generation) => {
                        renderer.process_generation = Some(generation);
                        renderer.error = None;
                        renderer.failure = None;
                        if renderer.page.is_none() {
                            renderer.replace_page(window, cx);
                        } else {
                            renderer.initialize_page(window, cx);
                        }
                    }
                    Err(error) => renderer.error = Some(error.public_message().into()),
                }
                renderer.sync_visibility(window, cx);
                cx.notify();
            });
        })
        .detach();
    }
}
