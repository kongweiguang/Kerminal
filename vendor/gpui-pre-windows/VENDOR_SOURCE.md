<!-- @author kongweiguang -->

# Vendored source

`gpui-pre-windows` 0.3.9 is copied from the resolved Cargo registry source at
`gpui-pre-windows-0.3.9`. Its package metadata identifies the upstream Zed
snapshot as commit `93d745668805029476e6d7fbb19652116fed51de`:

- Upstream: https://github.com/zed-industries/zed
- Crate version: `gpui-pre-windows 0.3.9`
- Upstream source commit: `93d745668805029476e6d7fbb19652116fed51de`
- License: Apache-2.0; see `LICENSE-APACHE`

The local patch retains the Windows IME composition handler ownership changes
and their regression tests. It also implements per-window visibility control
in `src/window.rs`: the backend checks that the HWND is live, hides it with
`SetWindowPos(SWP_HIDEWINDOW)` without changing its placement, and restores an
iconic window with `ShowWindowAsync(SW_RESTORE)` or shows a hidden non-iconic
window with `SetWindowPos(SWP_SHOWWINDOW)`. This avoids using minimize as a
substitute for hiding and preserves the normal/maximized placement state. A
successful call reports that Windows accepted the request; final visibility
still comes from the existing window visibility observer. The local Windows
crate tests passed 28/28 on 2026-10-17; this does not constitute a real tray
interaction acceptance test. The original crate manifest, source files, and
license are retained.
