<!-- @author kongweiguang -->

# Vendored source

`gpui-pre` 0.3.9 is copied from the resolved Cargo registry source at
`gpui-pre-0.3.9`. Its package metadata identifies the upstream Zed snapshot as
commit `93d745668805029476e6d7fbb19652116fed51de`:

- Upstream: https://github.com/zed-industries/zed
- Crate version: `gpui-pre 0.3.9`
- Upstream source commit: `93d745668805029476e6d7fbb19652116fed51de`
- License: Apache-2.0; see `LICENSE-APACHE`

The local patch adds shared input-handler ownership and focus identity for the
Windows IME composition pin and platform-specific per-focus input routing. It
also adds `PlatformWindow::supports_visibility_control` and
`PlatformWindow::set_window_visibility`, with forwarding methods on `Window`.
The default backend reports the capability as unsupported and returns an
explicit error; it never treats an unimplemented hide request as success. A
successful request means the platform accepted it, while the resulting state
is observed through the existing visibility API and callback. Backend details
are recorded in the corresponding vendored package notes. The source and
license are retained; the package manifest adds only the explicit tooltip
integration-test target described below.

The tooltip patch cancels a pending delayed show on any mouse-down, including a
press on the tooltip's source element. It also keeps the source bounds current
across layout passes and rechecks them when the delay expires, so moving a
same-identity control does not leave a stale hover region behind. These checks
prevent a tooltip from appearing after a click unmounts its source or after a
layout change moves it away from the pointer. An already-visible hoverable
tooltip keeps its existing behavior. Before retaining or showing a tooltip,
the source must also remain in the current frame's hover hit-test stack; this
clears a visible tooltip when a modal or another blocking hitbox occludes its
source, without treating pointer capture as hover. The public-API integration
tests are in `tests/unit/tooltip_mouse_down.rs`, behind the `test-support`
feature; run them
with `cargo test -p gpui-pre --test tooltip_mouse_down --no-default-features --features test-support`.
They exercise pending-show cancellation through source unmount, moved-bounds
revalidation, occlusion invalidation, persistent-entity keyed and manually
materialized `AnyElement` view unmount/remount, and visible hoverable
preservation against upstream commit `93d745668805029476e6d7fbb19652116fed51de`.

The frame records stable IDs and hover hitboxes only for visible elements with
an actual hover listener. Deferred tooltip prepaint checks those records after
the root and reused view prepaint ranges have completed. The registration list
is included in prepaint cache reuse and transaction rollback so an unchanged
cached source remains live while an absent or occluded source invalidates its
request. The corresponding managed-tooltip integration target is owned by
`gpui-base`; see that crate's `VENDOR_SOURCE.md` and
`tests/tooltip_source_lifecycle.rs`.
