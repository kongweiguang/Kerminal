<!-- @author kongweiguang -->

# Vendored source

`gpui-pre-macos` 0.3.9 is copied from the resolved Cargo registry source
`gpui-pre-macos-0.3.9`. Its package metadata identifies the upstream Zed
snapshot as commit `93d745668805029476e6d7fbb19652116fed51de`:

- Upstream: https://github.com/zed-industries/zed
- Crate version: `gpui-pre-macos 0.3.9`
- Upstream source commit: `93d745668805029476e6d7fbb19652116fed51de`
- License: Apache-2.0; see `LICENSE-APACHE`

本地补丁保留原平台实现，并增加焦点绑定的 AppKit marked-text 路由及独立状态机回归测试。
同时，`src/window.rs` 实现窗口级 visibility capability：通过 `orderOut` 隐藏单个窗口，
通过 `makeKeyAndOrderFront` 恢复；调用前在锁内复制由窗口状态持有的 native handle，再释放锁
执行 AppKit 操作，避免同步 occlusion 通知重入时死锁。最终可见状态仍由现有 visibility observer
报告，API 成功只表示请求已接受。包元数据与依赖保持原样；manifest 仅额外声明外置状态机与 URL 队列测试目标。
AppKit 不会给任意异步回调附加来源标记，因此原生取消保护只覆盖同步 `discardMarkedText` 回调窗口。
`src/platform.rs` 另将 `application:openURLs:` 在 GPUI 回调注册前收到的 URL 批次暂存，回调注册后按到达顺序各投递一次；投递前释放平台状态锁，避免调用应用回调时同步重入。纯队列状态测试位于 `tests/open_url_events.rs`。
macOS 实际托盘隐藏/恢复、首次启动 URL 激活及 IME 原生交互尚未验收。
