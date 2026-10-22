# @author kongweiguang

# Vendored upstream patches

## `alacritty_terminal` 0.26.0

- Upstream source revision: <https://github.com/alacritty/alacritty/tree/94e7c8874e526b1e67b349d9ba30ddf81669119e/alacritty_terminal> (recorded in `.cargo_vcs_info.json`).
- Crate: `alacritty_terminal` 0.26.0, Apache-2.0; upstream license files are kept in `vendor/alacritty_terminal/`.
- Local changes are limited to coordinate metadata. Full-screen upward grid rotation increments a monotonic scroll origin, including rows discarded after scrollback reaches its capacity. Non-translational grid operations advance a coordinate epoch so consumers reject snapshots that cannot be mapped safely.
- The patch does not change VT parsing, terminal rendering, input encoding, in-range cell movement, or scrollback capacity. Direct `Grid::scroll_up` calls above the region height are clamped once so storage movement and the reported origin agree; normal `Term` callers already apply this bound.
- Regression command for the upstream unit tests added with this patch: `cargo test --manifest-path vendor/alacritty_terminal/Cargo.toml coordinate_watermarks --lib --offline`.
