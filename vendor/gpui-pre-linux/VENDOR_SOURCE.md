<!-- @author kongweiguang -->

# Vendored source

`gpui-pre-linux` 0.3.9 is copied from the resolved Cargo registry source at
`gpui-pre-linux-0.3.9`. Its package metadata identifies the upstream Zed
snapshot as commit `93d745668805029476e6d7fbb19652116fed51de`:

- Upstream: https://github.com/zed-industries/zed
- Crate version: `gpui-pre-linux 0.3.9`
- Upstream source commit: `93d745668805029476e6d7fbb19652116fed51de`
- License: Apache-2.0; see `LICENSE-APACHE`

The local patch covers Linux X11/Wayland IME focus ownership, Wayland
`done(serial)` routing, MIME-accurate Wayland clipboard reads and writes, and
reversible visibility for live XDG toplevels. Wayland clipboard sources only
advertise formats present in the item. Text MIME requests receive text bytes;
image requests receive the original encoded bytes from the matching image
entry (including `image/png`) without re-encoding or per-request image copies.
The calloop writer owns the request fd and shared image item until completion,
peer closure, write failure, or source removal. The visibility path uses a
null-buffer commit to unmap, replays role metadata after remap, waits for
configure/ack before submitting a buffer, and reports visible only after the
first successful frame. Previously visible child toplevels are restored after
the parent frame if they remain registered; XDG popups are left to their close
lifecycle. Pure state regressions are maintained in
`tests/wayland_visibility.rs` and `tests/wayland_clipboard.rs`, included by the
backend's test modules.
Protocol behavior was checked against the lockfile-resolved
`wayland-protocols 0.32.13` `xdg-shell.xml`. The original crate manifest, source
files, and license are retained.
