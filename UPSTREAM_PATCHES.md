# @author kongweiguang

# Vendored upstream patches

## `alacritty_terminal` 0.26.0

- Upstream source revision: <https://github.com/alacritty/alacritty/tree/94e7c8874e526b1e67b349d9ba30ddf81669119e/alacritty_terminal> (recorded in `.cargo_vcs_info.json`).
- Crate: `alacritty_terminal` 0.26.0, Apache-2.0; upstream license files are kept in `vendor/alacritty_terminal/`.
- Local changes are limited to coordinate metadata. Full-screen upward grid rotation increments a monotonic scroll origin, including rows discarded after scrollback reaches its capacity. Non-translational grid operations advance a coordinate epoch so consumers reject snapshots that cannot be mapped safely.
- The patch does not change VT parsing, terminal rendering, input encoding, in-range cell movement, or scrollback capacity. Direct `Grid::scroll_up` calls above the region height are clamped once so storage movement and the reported origin agree; normal `Term` callers already apply this bound.
- Regression command for the upstream unit tests added with this patch: `cargo test --manifest-path vendor/alacritty_terminal/Cargo.toml coordinate_watermarks --lib --offline`.

## `gpui-component` 0.7.3

- Upstream source revision: <https://github.com/longbridge/gpui-kit/tree/67265a36e260ffe03c5aaebdcd8d654dbf892eab> (recorded in `.agents/skills/gpui-kit/SKILL.md`).
- Crate: `gpui-component` 0.7.3, Apache-2.0; upstream license files are kept in `vendor/gpui-component/`.
- Local changes are menu presentation hooks required by the Kerminal theme: `PopupMenu::surface_style` / `items_style` / `item_style` / `item_checked_style` / `item_hover_style` / `separator_style` / `render_surface`, `MenuItemElement::interactive_style`, per-item `aria_label`, test observation, and the `PopupMenuItem::danger()` destructive-command state.
- `PopupMenuItem::danger()` colors the label and icon with `theme().danger` so delete, stop, and close commands read as one family; hover and keyboard selection use a 12% danger background instead of the menu's neutral or accent highlight, and while the item is disabled it falls back to `theme().muted_foreground` with no highlight, because an unavailable destructive command must not look actionable.
- The patch does not change menu navigation, focus transfer and restoration, submenu anchoring, dismissal, shortcuts, or item geometry.
- Regression coverage: the vendored unit test `danger_items_use_theme_danger_color_only_when_actionable` in `vendor/gpui-component/src/menu/popup_menu.rs`.
- `Popover::trigger_on_mouse_down(false)` delegates input-owned opening to the caller while preserving the existing dismissal and focus state machine. Its default is `true`, so existing button triggers retain their behavior. The corresponding `gpui-base` adapter owns the optional mouse listener; the component only forwards the setting.
- The file transfer host search uses this option so clicking inside an already focused search field keeps the results open. Pointer regressions live with the file host picker tests under `crates/kerminal-ui/tests/unit/`.

## `gpui-pre` / `gpui-pre-windows` 0.3.9

- The local Windows file-drag extension adds `external_drag_payload` to the existing internal drag gesture. Local files use `CF_HDROP`; deferred files expose `FileGroupDescriptorW` and read-only `FileContents` streams. Other platforms keep the existing internal drag path.
- Payload ownership and gesture generations prevent stale completion from clearing a newer drag. Same-window re-entry retains internal drop routing; rejected targets return `DROPEFFECT_NONE`.
- The dedicated OLE STA temporarily attaches its input queue to the verified source UI thread only during `DoDragDrop`, then detaches before asynchronous copying. Initial key-state handling accounts for `AttachThreadInput` resetting queued state; physical release and Escape still end the gesture.
- Escape consumption is scoped to the active native gesture so cancelling a drag does not also close its source panel. Dropping outside an accepting target is cancellation only when no copy receipt, preparation failure, or internal acceptance exists.
- Deferred providers retain staging until asynchronous target completion and final stream release. Unknown delivery preserves recovery data; cancellation and stream operations use bounded memory and continue dispatching COM calls.
- Implementation: `vendor/gpui-pre/src/interactive.rs`, `vendor/gpui-pre/src/window.rs`, `vendor/gpui-pre-windows/src/external_drag.rs`, and `vendor/gpui-pre-windows/src/external_drag/stream.rs`. File validation and staging policy stay in the application layer.
- Focused platform regression command: `cargo test --locked -p gpui-pre-windows --lib external_drag::`.

## `gpui-pre-linux` 0.3.9

- On X11, window creation checks the compositor selection owner for the target screen. When no compositor is confirmed, it prefers an opaque visual so a transparent visual cannot produce a black window; a present or unknown compositor keeps the upstream transparent-visual behavior and fallback.
- This only affects visual selection when creating an X11 window. Wayland is unchanged; adding a compositor after window creation requires a new window to select a transparent visual.
- Selection-policy regressions are in `vendor/gpui-pre-linux/src/linux/x11/window.rs`; native verification uses an isolated Xvfb display.
