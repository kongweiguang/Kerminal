<!-- @author kongweiguang -->

# Vendored source

`gpui-base 0.7.3` is copied from the resolved Cargo registry source. Its package
metadata identifies the upstream repository snapshot and source directory:

- Upstream: https://github.com/longbridge/gpui-kit
- Crate version: `gpui-base 0.7.3`
- Upstream source commit: `67265a36e260ffe03c5aaebdcd8d654dbf892eab`
- Source path: `crates/base`
- License: Apache-2.0; see `LICENSE-APACHE`

The local tooltip change binds a managed hover request to the originating
`GlobalElementId`. A deferred element validates that source against the
completed current-frame hover registration before painting its popup. The
registration is limited to visible elements with hover callbacks and is
preserved across cached prepaint reuse. An invalidated request is not revived
by remounting an element with the same ID; a fresh hover creates a new request.
The matching `gpui-pre` frame-registration patch is documented in
`../gpui-pre/VENDOR_SOURCE.md`.

The source lifecycle regression is an explicit integration target because this
crate disables Cargo's automatic test discovery. It exercises cached frames,
occlusion, grace movement, persistent entities, and keyed or manually rendered
view unmount/remount through GPUI's public event and draw paths. It requires the
`test-support` feature and runs with:

```text
cargo test -p gpui-base --test tooltip_source_lifecycle --no-default-features --features test-support
```

本地还为 `InputState` 增加只读 `is_composing()`，仅公开是否存在输入法组合文本，
不暴露组合文本或选区。标签重命名在按键 action 处理前读取该状态，避免输入组件
清除 marked range 后，外层将候选确认或取消误判为保存、关闭编辑。

ResizeHandle 在左键 MouseDown capture 阶段仅对实际顶层 hitbox 阻止默认聚焦，
避免拖动分隔线时焦点跳到父面板；覆盖层和相邻控件仍按自身 hitbox 正常取得焦点。
事件仍继续进入原有 bubble 按下、拖动和释放流程。回归用例放在
`src/resizable/resize_handle.rs` 的内嵌 GPUI headless tests 中。
当前应用 workspace 没有将 gpui-base 列为成员，不能从根目录使用 `-p gpui-base`
运行其测试。验证时在独立测试目录保留 `vendor/gpui-base` 层级并复制根 `README.md`
（现有 text-view 测试在编译时读取它），在副本 manifest 添加空 `[workspace]`，
并通过 Cargo 配置复用应用根 manifest 中的全部本地 `[patch.crates-io]`；
在这个独立包目录定向运行，保持生产 manifest 和锁文件不变：

```text
cargo --config <本地patch配置> test --no-default-features --features test-support --lib resize_handle_focus_guard_respects_top_hitbox_and_adjacent_controls
cargo --config <本地patch配置> test --no-default-features --features test-support --lib first_threshold_move_resizes_and_mouse_up_emits_once
```
