# @author kongweiguang
param(
    [Parameter(Mandatory = $true)]
    [string]$ReportDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# 比较相同字母 r 的真实 PNG 墨迹；归一化消除浅深色及 ghost 颜色差异，不修改截图或使用控件边框代替字形。
function Get-GlyphInk {
    param(
        [System.Drawing.Bitmap]$Bitmap,
        [double]$Left,
        [double]$Top,
        [double]$Width,
        [double]$Height
    )
    $x0 = [int][Math]::Ceiling($Left)
    $x1 = [int][Math]::Floor($Left + $Width) - 1
    $y0 = [int][Math]::Floor($Top)
    $y1 = [int][Math]::Ceiling($Top + $Height) - 1
    if ($x0 -lt 0 -or $y0 -lt 0 -or $x1 -ge $Bitmap.Width -or $y1 -ge $Bitmap.Height) {
        throw 'Glyph measurement is outside the native screenshot.'
    }
    $background = $Bitmap.GetPixel($x0, $y0)
    $rows = [Collections.Generic.List[double]]::new()
    for ($y = $y0; $y -le $y1; $y++) {
        $strength = 0.0
        for ($x = $x0; $x -le $x1; $x++) {
            $pixel = $Bitmap.GetPixel($x, $y)
            $strength += [Math]::Max(
                [Math]::Abs([double]$pixel.R - $background.R),
                [Math]::Max([Math]::Abs([double]$pixel.G - $background.G), [Math]::Abs([double]$pixel.B - $background.B))
            )
        }
        $rows.Add($strength)
    }
    $peak = ($rows | Measure-Object -Maximum).Maximum
    if ($peak -lt 50) { throw 'Expected glyph ink is absent or too faint to measure.' }
    $sum = 0.0
    $moment = 0.0
    $inkRows = [Collections.Generic.List[int]]::new()
    for ($index = 0; $index -lt $rows.Count; $index++) {
        $normalized = $rows[$index] / $peak
        $sum += $normalized
        $moment += ($y0 + $index) * $normalized
        if ($normalized -ge 0.1) { $inkRows.Add($y0 + $index) }
    }
    [pscustomobject]@{
        top = $inkRows[0]
        bottom = $inkRows[$inkRows.Count - 1]
        centroid = $moment / $sum
    }
}

$directory = (Resolve-Path -LiteralPath $ReportDirectory).Path
$samples = foreach ($theme in @('light', 'dark', 'system')) {
    $geometry = Get-Content -Raw -LiteralPath (Join-Path $directory "suggestions-ghost-$theme-geometry.json") | ConvertFrom-Json
    if ($geometry.prefix -cne 'zqkmr' -or $geometry.suffix -cne 'suggestionfixture') {
        throw 'Glyph comparison requires the known non-executed native fixture.'
    }
    $scale = [double]$geometry.scaleFactor
    $width = [double]$geometry.cellWidth
    # 已输入前缀末尾 r 与后缀第 16 个字母 r 都避开光标方块；右侧完整可见是测量的前提。
    $typedX = ([double]$geometry.cursorX - $width) * $scale
    $ghostX = ([double]$geometry.cursorX + 15 * $width) * $scale
    if ($ghostX + $width * $scale -gt [double]$geometry.paneRight * $scale) {
        throw 'The comparison glyph is clipped; use a wider native acceptance window.'
    }
    $bitmap = [System.Drawing.Bitmap]::new((Join-Path $directory "suggestions-ghost-$theme.png"))
    try {
        $typed = Get-GlyphInk $bitmap $typedX ($geometry.cursorY * $scale) ($width * $scale) ($geometry.lineHeight * $scale)
        $ghost = Get-GlyphInk $bitmap $ghostX ($geometry.cursorY * $scale) ($width * $scale) ($geometry.lineHeight * $scale)
        $delta = [Math]::Abs($typed.centroid - $ghost.centroid)
        [pscustomobject]@{
            theme = $theme
            scaleFactor = $scale
            typed = $typed
            ghost = $ghost
            centroidDeltaPhysicalPixels = $delta
            passed = ($typed.top -eq $ghost.top -and $typed.bottom -eq $ghost.bottom -and $delta -le 0.35)
        }
    }
    finally { $bitmap.Dispose() }
}
$report = [pscustomobject]@{
    boundary = 'Identical r glyph ink in unmodified hidden-native PNGs; not monitor scanout'
    maximumCentroidDeltaPhysicalPixels = 0.35
    samples = @($samples)
    passed = (@($samples | Where-Object { -not $_.passed }).Count -eq 0)
}
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $directory 'glyph-alignment-report.json') -Encoding utf8
$report | ConvertTo-Json -Depth 8
if (-not $report.passed) { throw 'Terminal and inline suggestion glyph baselines differ.' }
