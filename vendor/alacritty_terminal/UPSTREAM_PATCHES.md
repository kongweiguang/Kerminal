# Upstream patches

<!-- @author kongweiguang -->

`alacritty_terminal` is based on upstream Alacritty 0.26.0 and retains its upstream license headers.

## ConPTY row growth

The local `ResizeStrategy::ConPty` path preserves the existing grid row coordinates and scrollback when the visible row count grows. It adds blank rows below the old viewport, keeps the cursor/selection/vi position attached to existing cells, and applies the same operation to active and inactive grids. Generic callers continue to use upstream Alacritty history-pull behavior. Column reflow, shrink behavior, parser behavior, and rendering remain upstream.

The strategy is selected by infrastructure only when the opened transport reports the Windows native PTY capability. Unix PTYs, SSH and remote container transports retain the generic strategy; a local container exec inherits the local PTY capability because it uses that same transport.
