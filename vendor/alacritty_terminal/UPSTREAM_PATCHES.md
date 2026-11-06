# Upstream patches

<!-- @author kongweiguang -->

`alacritty_terminal` is based on upstream Alacritty 0.26.0 and retains its upstream license headers.

## ConPTY row growth

The local `ResizeStrategy::ConPty` path preserves the existing grid row coordinates and scrollback when the visible row count grows. It adds blank rows below the old viewport, keeps the cursor/selection/vi position attached to existing cells, and applies the same operation to active and inactive grids. Generic callers continue to use upstream Alacritty history-pull behavior for row growth. Column growth has the narrowly scoped adjustment below; shrink behavior, parser behavior, and rendering remain upstream.

The strategy is selected by infrastructure only when the opened transport reports the Windows native PTY capability. Unix PTYs, SSH and remote container transports retain the generic strategy; a local container exec inherits the local PTY capability because it uses that same transport.

## Blank history during column growth

When column growth merges wrapped rows at the live bottom viewport, upstream can pull default blank history rows above the existing visible content. The local patch records the old viewport boundary during reflow and returns only a newly pulled, contiguous prefix of strictly default cells to history by adding equal blank capacity at the screen bottom. It moves the active cursor with its content and retains DEC saved screen coordinates. No history rows are deleted.

Existing visible blank rows, nondefault cells (including styles, backgrounds and links), nonblank historical output, scrolled viewports, and a soft wrap crossing the history/screen boundary retain their existing behavior. Alternate grids do not reflow; inactive primary grids use the same guarded adjustment. Regression coverage is in `crates/terminal/tests/column_reflow.rs`.
