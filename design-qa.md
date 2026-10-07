<!-- @author kongweiguang -->

# Background image design QA

## Evidence

- Source visual truth: `C:\Users\24052\AppData\Local\Temp\kerminal-background-audit-20260820\01-termora-reference.png`
- Rendered implementation: `C:\Users\24052\AppData\Local\Temp\kerminal-background-audit-20260820\02-kerminal-connected-terminal.png`
- Full-view comparison: `C:\Users\24052\AppData\Local\Temp\kerminal-background-audit-20260820\03-full-comparison.png`
- Focused terminal comparison: `C:\Users\24052\AppData\Local\Temp\kerminal-background-audit-20260820\04-terminal-surface-comparison.png`
- Source pixels: 2441 x 1416. The external reference does not expose its CSS viewport or device density.
- Implementation pixels: 3840 x 2088. The current Windows display uses 150% scaling, corresponding to an approximately 2560 x 1392 CSS viewport at device scale factor 1.5.
- Density normalization: each full screenshot was proportionally scaled and padded to 1280 x 720 before horizontal comparison. The terminal regions were cropped from the actual source and implementation and independently normalized to 1280 x 800 before comparison.
- State: connected SSH terminal, dark theme, compact density, background enabled, `cover`, terminal font size 15, background opacity 65%, window opacity 69%.

## Full-view comparison

- Image treatment: Kerminal now keeps the wallpaper subject, texture, and crop visibly intact. The previous stacked gradients and large-area blur no longer erase the artwork.
- Surface continuity: the wallpaper passes continuously through the workspace and xterm viewport. There is no opaque black rectangle or straight-edged inner terminal surface.
- Layout and rhythm: the title tab, terminal header, terminal body, sidebar, and right tool rail remain aligned. Kerminal intentionally keeps a wider persistent host sidebar than the Termora reference; this is a product-layout difference rather than a background defect.
- Shape and elevation: the terminal card has a subtle border and consistent rounded outer clipping at all four corners. The header adds only a restrained darker layer instead of becoming a separate solid block.
- Colors and tokens: navigation remains darker for scanning and selection contrast, while the terminal body stays lightly tinted. The cyan active states and existing dark-theme tokens remain consistent.
- Typography: Kerminal's compact density produces smaller text relative to the larger viewport than the reference, but the native 15px terminal font remains crisp, evenly spaced, and readable. No wrapping, clipping, or baseline defect is visible.
- Image quality: the source wallpaper is sharp with no blur halo, transparency seam, stretching, or masking artifact. The stronger image visibility is intentional because the reported defect was that the background content could not be seen.
- Copy and content: terminal content is real connected-session output rather than placeholder copy. Shell labels and actions remain coherent in the connected state.
- Icons: existing Kerminal icon family, stroke weight, active state, and alignment are preserved; no replacement or fabricated asset was introduced.

## Focused terminal comparison

- The focused crop confirms that xterm's viewport, canvas area, and scroll region are transparent over the same wallpaper plane.
- The former `#000` viewport rectangle is absent. Terminal text begins directly over the themed veil without an inner hard edge.
- The terminal header/body transition is visible but restrained, and the outer border owns the rounding and clipping.
- White terminal text remains readable across both the light wall and dark character regions in the supplied state. Screenshot evidence cannot establish full WCAG contrast for every possible wallpaper or ANSI color.
- The scrollbar remains visible without restoring an opaque track. Its current contrast is acceptable; reducing the thumb prominence later would be a P3 polish option, not an acceptance blocker.

## Findings

1. Resolved P1, terminal surface: xterm's packaged `.xterm-viewport` black background created a straight-edged rectangle. Wallpaper mode now enables transparent xterm rendering, supplies a transparent terminal theme, and scopes a transparent viewport override to the wallpaper state.
2. Resolved P1, image treatment: stacked vignette, side, horizon, workspace-blur, and terminal-blur layers hid the wallpaper subject. A single uniform veil now controls readability while retaining image detail.
3. Resolved P2, material hierarchy: navigation, workspace, terminal header, and terminal body previously stacked near-opaque surfaces into mismatched color blocks. They now use separate opacity roles on one continuous background plane.
4. Accepted product difference: Kerminal's sidebar is wider and more opaque than Termora's. It preserves host-list readability and does not reintroduce the reported workspace defect.
5. Follow-up P3: the xterm scrollbar thumb is slightly more prominent than the reference and may be softened if a quieter chrome treatment is desired.

No actionable P0, P1, or P2 finding remains.

## Comparison history

1. Initial comparison found the P1 black xterm rectangle and P1 wallpaper-detail loss. The implementation was blocked.
2. The shell wallpaper was reduced to a uniform veil, wallpaper-mode surface opacity was separated by role, workspace and terminal-body blur were removed, and xterm transparency was enabled and scoped.
3. A real Tauri empty-workspace screenshot confirmed image visibility and shell continuity, but connected-terminal acceptance remained blocked because no xterm tab was open.
4. The user opened a real SSH terminal and supplied the 3840 x 2088 connected-state screenshot. Full-view and focused same-state comparisons confirm that the black inner surface is gone, the wallpaper remains visible, text is usable, and outer rounding is intact.

## Final result

passed
