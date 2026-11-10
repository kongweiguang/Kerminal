# 在私有桌面比较真实启动和内存；不将汇总统计冒充原始帧数据。
# @author kongweiguang
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$EmptyConfigTemplate,
    [Parameter(Mandatory)][string]$ThreePluginConfigTemplate,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateRange(5, 300)][int]$WarmupSeconds = 5,
    # 515 个固定采样槽、列表前的三插件激活、两段 warmup 和 13 秒洪泛结算必须同时落入帧窗。
    # 上限绑定桌面端 10,000 个 raw 帧容量与 16ms 刷新节拍；120 秒约 7500 帧，
# 加上 515 个采样槽仍留有余量，更长时长会在窗口结束前丢帧。
    [ValidateRange(65, 120)][int]$DurationSeconds = 70,
    # 桌面端自身等待为 warmup + duration + 30 秒；脚本再留出报告落盘与正式退出回收余量。
    [ValidateRange(30, 900)][int]$TimeoutSeconds = 150
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# raw 帧容量为 10,000；按 16ms 刷新节拍 120 秒约 7500 帧，加 515 槽仍低于容量。
$MaximumDurationSeconds = 120
# fixture main.js 的发送窗口与速率常量；短于该值的 burst 不能代表持续插件负载。
$FloodMinimumSendMs = 12000
$FloodMinimumAttempts = 12000
$ActivationReserveMs = 6000

function Assert-PerformanceBudget {
    # 与桌面端同一预算公式；不足时拒绝运行，避免三插件激活或洪泛把列表挤出实际帧窗。
    # @author kongweiguang
    param([int]$DurationSeconds, [int]$WarmupSeconds)
    if ($DurationSeconds -gt $MaximumDurationSeconds) {
        # 时长超过 raw 容量时，帧报告会先丢样本；必须在这里拒绝，而不是运行后误报“帧丢失”。
        throw "Duration $DurationSeconds s exceeds the $MaximumDurationSeconds s cap derived from the bounded raw-frame capacity and refresh cadence."
    }
    # 两段 warmup、列表前置 1 秒、洪泛尾巴 16 秒、激活预留 6 秒、调度余量 2 秒、515*55 毫秒。
    $requiredMs = 2 * $WarmupSeconds * 1000 + 1000 + 16000 + $ActivationReserveMs + 2000 + 515 * 55
    if (($DurationSeconds * 1000) -lt $requiredMs) {
        throw "Duration $DurationSeconds s and warmup $WarmupSeconds s do not fit the 515-slot list plan, activation reserve and plugin flood inside the frame window (need $requiredMs ms)."
    }
}

function Initialize-PerformanceLauncher {
    # 复用已验证的挂起启动与 Job 绑定块；保留私有桌面，确保成功时能核对全部后代自然退出。
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'webview-native.ps1'), [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw 'Cannot parse existing Job launcher.' }
    $parts = [Collections.Generic.List[string]]::new()
    foreach ($name in @('Get-WebViewLauncherSource', 'Replace-WebViewLauncherPart')) {
        $matches = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true))
        if ($matches.Count -ne 1) { throw "Launcher function is ambiguous: $name" }
        $parts.Add($matches[0].Extent.Text)
    }
    $jobs = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$jobSource' }, $true))
    $launch = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.StartsWith("if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher'", [StringComparison]::Ordinal) }, $true))
    if ($jobs.Count -ne 1 -or $launch.Count -ne 1) { throw 'The complete Job launcher cannot be identified.' }
    $parts.Add($jobs[0].Extent.Text)
    $skip = @('String.IsNullOrWhiteSpace(desktopName)', 'EntryPoint = "CreateDesktopW"', 'private static extern IntPtr CreateDesktop(', 'CreateDesktop(desktopName, IntPtr.Zero', 'startupInfo.StartupInfo.Flags = StartfUseStdHandles;')
    $parts.Add((($launch[0].Extent.Text -split "`r?`n") | Where-Object { $line = $_; -not ($line.Contains('Replace-WebViewLauncherPart $source') -and @($skip | Where-Object { $line.Contains($_) }).Count -gt 0) }) -join "`n")
    $source = ($parts -join "`n").Replace('$PSScriptRoot', ("'" + $PSScriptRoot.Replace("'", "''") + "'"))
    $command = '+ " --webview-probe --output " + QuoteArgument(outputDirectory) + " --theme " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_WEBVIEW_PROBE_THEME") ?? "system")'
    if ($source.Split([string[]]@($command), [StringSplitOptions]::None).Count -ne 2) { throw 'The Job launcher command changed.' }
    $source = $source.Replace($command, '').Replace('Kerminal.WebViewProbe', 'Kerminal.PluginPerformanceAcceptance')
    . ([scriptblock]::Create($source))
}

function Get-PluginState {
    # 待应用安装不能当作已启用插件；空配置允许首次启动时自动创建状态。
    param([string]$Root, [int]$Expected)
    $path = Join-Path $Root 'plugins/state.json'
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) {
        if ($Expected -eq 0) { return $null }
        throw "Missing plugin state: $path"
    }
    $state = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
    $enabled = @($state.plugins.PSObject.Properties | Where-Object { $_.Value.enabled })
    if ($enabled.Count -ne $Expected -or @($state.pending).Count -ne 0) { throw "Expected $Expected enabled plugins and no pending operations in $path" }
    return $state
}

function Get-TemplateVisualState {
    # 只提取会影响列表 viewport 的几何以及决定明暗/配色的设置。
    # 终端字体不属于宿主列表字体：Windows 上界面主题固定使用 Segoe UI，
    # 因此这里不解析 [terminal] 字体，也避免 TOML 转义引号造成误判。
    # @author kongweiguang
    param([string]$Root)
    $settingsPath = Join-Path $Root 'settings.toml'
    $settingsText = if (Test-Path -LiteralPath $settingsPath -PathType Leaf) { Get-Content -Raw -LiteralPath $settingsPath } else { $null }
    $themeMode = $null; $colorScheme = $null; $interfaceDensity = $null
    if ($null -ne $settingsText) {
        $section = ''
        foreach ($line in ($settingsText -split "`r?`n")) {
            $trimmed = $line.Trim()
            if ($trimmed.StartsWith('#')) { continue }
            if ($trimmed -match '^\[([^\]]+)\]$') { $section = $matches[1]; continue }
            if ($section -ceq '' -and $trimmed -match '^themeMode\s*=\s*"([^"]*)"') { $themeMode = $matches[1]; continue }
            if ($section -ceq '' -and $trimmed -match '^interfaceDensity\s*=\s*"([^"]*)"') { $interfaceDensity = $matches[1]; continue }
            if ($section -ceq 'appearance' -and $trimmed -match '^colorScheme\s*=\s*"([^"]*)"') { $colorScheme = $matches[1]; continue }
        }
    }
    $windowStatePath = Join-Path $Root 'window-state.json'
    $windowStatePresent = Test-Path -LiteralPath $windowStatePath -PathType Leaf
    $bounds = $null; $maximized = $null; $fullscreen = $null
    if ($windowStatePresent) {
        try { $window = Get-Content -Raw -LiteralPath $windowStatePath | ConvertFrom-Json } catch { throw "Template window state is invalid JSON: $windowStatePath" }
        # 与正式 reader 一样先核对 schema/revision/几何范围；损坏模板不能进入采样。
        foreach ($name in @('schema_version', 'revision')) {
            $property = $window.PSObject.Properties[$name]
            if ($null -eq $property -or $property.Value -is [bool] -or $property.Value -is [string] -or $property.Value -isnot [ValueType]) { throw "Template window state is invalid: $windowStatePath" }
            $value = [double]$property.Value
            if (![double]::IsFinite($value) -or $value -ne [math]::Floor($value)) { throw "Template window state is invalid: $windowStatePath" }
        }
        if ($window.schema_version -ne 1 -or [double]$window.revision -lt 1) { throw "Template window state schema or revision is invalid: $windowStatePath" }
        if ($null -eq $window.PSObject.Properties['state'] -or $null -eq $window.state -or $null -eq $window.state.PSObject.Properties['normal_bounds']) { throw "Template window state is invalid: $windowStatePath" }
        $normal = $window.state.normal_bounds
        foreach ($name in @('x', 'y', 'width', 'height')) {
            $property = $normal.PSObject.Properties[$name]
            if ($null -eq $property -or $property.Value -is [bool] -or $property.Value -is [string] -or $property.Value -isnot [ValueType]) { throw "Template window bounds are invalid: $windowStatePath" }
            $value = [double]$property.Value
            if (![double]::IsFinite($value) -or $value -ne [math]::Floor($value)) { throw "Template window bounds are invalid: $windowStatePath" }
            if ($name -in @('width', 'height') -and ($value -lt 640 -or $value -gt 16384)) { throw "Template window bounds are outside the persisted window range: $windowStatePath" }
            if ($name -in @('x', 'y') -and ($value -lt -2147483648 -or $value -gt 2147483647)) { throw "Template window position is outside the persisted range: $windowStatePath" }
        }
        if ($normal.height -lt 420) { throw "Template window height is outside the persisted window range: $windowStatePath" }
        foreach ($name in @('maximized', 'fullscreen')) {
            if ($null -eq $window.state.PSObject.Properties[$name] -or $window.state.PSObject.Properties[$name].Value -isnot [bool]) { throw "Template window state is invalid: $windowStatePath" }
        }
        $bounds = '{0},{1},{2},{3}' -f $normal.x, $normal.y, $normal.width, $normal.height
        $maximized = [bool]$window.state.maximized
        $fullscreen = [bool]$window.state.fullscreen
    }
    return [pscustomobject][ordered]@{
        # 应用默认主题是 Dark；缺失字段必须按真实默认归一，否则缺失与显式 dark
        # 会被误判为不同，而缺失与显式 system 又可能被错误放过。
        themeMode = if ($null -ne $themeMode) { $themeMode } else { 'dark' }
        colorScheme = if ($null -ne $colorScheme) { $colorScheme } else { 'kerminal' }
        interfaceDensity = if ($null -ne $interfaceDensity) { $interfaceDensity } else { 'compact' }
        windowStatePresent = $windowStatePresent
        normalBounds = $bounds; maximized = $maximized; fullscreen = $fullscreen
    }
}

function Assert-PerformanceTemplateParity {
    # 比较窗口几何和会影响明暗/配色的设置；只声明这些已比较的范围，
    # 不把背景图、工作台布局等未解析字段宣称为相同视觉状态。
    # @author kongweiguang
    param([string]$Empty, [string]$Three)
    # 局部变量不能叫 $three/$empty：PowerShell 变量名大小写不敏感，会覆盖同名参数。
    $emptyState = Get-TemplateVisualState $Empty
    $threeState = Get-TemplateVisualState $Three
    foreach ($field in @('themeMode', 'colorScheme', 'interfaceDensity', 'windowStatePresent', 'normalBounds', 'maximized', 'fullscreen')) {
        $emptyProperty = $emptyState.PSObject.Properties[$field]
        $threeProperty = $threeState.PSObject.Properties[$field]
        if ($null -eq $emptyProperty -or $null -eq $threeProperty -or $emptyProperty.Value -cne $threeProperty.Value) {
            throw "Performance templates differ in $field; viewport/theme comparison would not be valid. Normalize both templates' window-state.json and theme/density/color-scheme settings before running."
        }
    }
    return [ordered]@{
        matched = $true; themeMode = $emptyState.themeMode; colorScheme = $emptyState.colorScheme; interfaceDensity = $emptyState.interfaceDensity
        normalBounds = $emptyState.normalBounds; maximized = $emptyState.maximized; fullscreen = $emptyState.fullscreen
        scope = 'window geometry plus theme mode, interface density and color scheme only; terminal font is not compared because the host list uses the platform UI theme font, and background/workbench-layout keys are outside this preflight'
    }
}

function Get-OwnedProcessTreeSample {
    # 仅沿当前父子关系采集；同时保留 PID 与创建时间，避免把 PID 复用误当成原进程。
    # 墙钟时间用于和宿主列表窗口对齐，证明三插件进程确实在列表帧期间存活。
    param([int]$RootId, [double]$ElapsedMs)
    $sampledUnixMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $rows = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, CreationDate, WorkingSetSize, PrivatePageCount, Name)
    $ids = [Collections.Generic.HashSet[uint32]]::new()
    $null = $ids.Add([uint32]$RootId)
    do {
        $added = $false
        foreach ($row in $rows) {
            if ($ids.Contains([uint32]$row.ParentProcessId) -and $ids.Add([uint32]$row.ProcessId)) { $added = $true }
        }
    } while ($added)
    $members = @($rows | Where-Object { $ids.Contains([uint32]$_.ProcessId) } | ForEach-Object {
        [ordered]@{ pid = $_.ProcessId; parentPid = $_.ParentProcessId; created = $_.CreationDate; name = $_.Name; workingSetBytes = $_.WorkingSetSize; privateBytes = $_.PrivatePageCount }
    })
    # 退出与 CIM 枚举之间允许主进程消失；最终结论由原生句柄和 Job 计数决定。
    return [ordered]@{ elapsedMs = $ElapsedMs; unixMs = $sampledUnixMs; processes = $members }
}

function Install-PerformanceFixture {
    # 只改写本轮隔离副本里明确命名的验收插件文件；模板和真实用户目录不受影响。
    param([string]$Config)
    $state = Get-PluginState $Config 3
    $entry = $state.plugins.PSObject.Properties['acme.notes']
    if ($null -eq $entry -or !$entry.Value.enabled -or $entry.Value.active -cne '1.0.0') { throw 'Three-plugin template must contain enabled acme.notes version 1.0.0.' }
    $package = Join-Path $Config 'plugins/packages/acme.notes/1.0.0'
    if (!(Test-Path -LiteralPath $package -PathType Container)) { throw 'Fixture package directory is missing.' }
    foreach ($path in @($Config, (Join-Path $Config 'plugins'), (Join-Path $Config 'plugins/packages'), (Join-Path $Config 'plugins/packages/acme.notes'), $package)) {
        if ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Fixture path must not be a reparse point: $path" }
    }
    $views = Join-Path $package 'views'
    if (Test-Path -LiteralPath $views) {
        if ((Get-Item -LiteralPath $views -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Fixture views directory must not be a reparse point.' }
    } else { New-Item -ItemType Directory -Path $views | Out-Null }
    $fixture = Join-Path $PSScriptRoot '../../tests/fixtures/plugins/performance'
    foreach ($name in @('kerminal-plugin.json', 'main.js', 'views/renderer.json')) {
        $destination = Join-Path $package $name
        if ((Test-Path -LiteralPath $destination) -and ((Get-Item -LiteralPath $destination -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Fixture destination must not be a reparse point.' }
        Copy-Item -LiteralPath (Join-Path $fixture $name) -Destination $destination -Force
    }
    $scriptPath = Join-Path $package 'main.js'
    $source = Get-Content -Raw -LiteralPath $scriptPath
    # 洪泛门禁的 12 秒/至少 12,000 次必须与 fixture 常量同源；fixture 被改动时
    # 直接拒绝，避免短 burst 或降速版本冒充持续负载。
    $sendMarker = "sendElapsed >= $FloodMinimumSendMs"
    if ($source.Split([string[]]@($sendMarker), [StringSplitOptions]::None).Count -ne 2 -or $source.Split([string[]]@('UPDATES_PER_TICK = 24'), [StringSplitOptions]::None).Count -ne 2) {
        throw 'Performance fixture flood constants changed; the sustained send window and update rate no longer match the acceptance gate.'
    }
    $delay = 'const START_DELAY_MS = 8000;'
    if ($source.Split([string[]]@($delay), [StringSplitOptions]::None).Count -ne 2) { throw 'Fixture warmup marker changed.' }
    $source.Replace($delay, "const START_DELAY_MS = $($WarmupSeconds * 1000 + 1000);") | Set-Content -Encoding utf8 -LiteralPath $scriptPath
}

function Get-PerformanceProcessSample {
    # 退出与进程表采样并不同步；缺失样本不代表成功，必须继续等待原生句柄的实际退出码。
    param([int]$OwnedId, [double]$ElapsedMs)
    $owned = $null
    try {
        $owned = [Diagnostics.Process]::GetProcessById($OwnedId)
        $owned.Refresh()
        return [ordered]@{ elapsedMs = $ElapsedMs; workingSetBytes = $owned.WorkingSet64; privateBytes = $owned.PrivateMemorySize64 }
    } catch {
        $cause = $_.Exception.GetBaseException()
        if ($cause -is [ArgumentException]) { return $null }
        if ($cause -is [InvalidOperationException] -and $null -ne $owned -and $owned.HasExited) { return $null }
        throw
    } finally { if ($null -ne $owned) { $owned.Dispose() } }
}

function Get-RequiredNumber {
    # 拒绝缺失、字符串、非有限数及非整数，避免 PowerShell 将空值转换成合法零。
    # @author kongweiguang
    param($Object, [string]$Name, [bool]$AllowNegative = $false)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing numeric field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($null -eq $value -or $value -is [bool] -or $value -is [string] -or $value -isnot [ValueType]) { throw "Invalid numeric field: $Name" }
    try { $number = [double]$value } catch { throw "Invalid numeric field: $Name" }
    if (![double]::IsFinite($number) -or [math]::Abs($number) -gt 9007199254740991 -or $number -ne [math]::Floor($number) -or (!$AllowNegative -and $number -lt 0)) { throw "Invalid numeric field: $Name" }
    return $number
}

function Get-RequiredBoolean {
    # StrictMode 下缺失属性和字符串 "true" 都不能冒充真实布尔证据。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name] -or $Object.PSObject.Properties[$Name].Value -isnot [bool]) { throw "Host list field '$Name' is missing or invalid." }
    return [bool]$Object.PSObject.Properties[$Name].Value
}

function Get-RequiredFiniteNumber {
    # 几何字段允许 DPI 小数，但仍拒绝缺失、字符串、布尔、NaN 和非正值。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing numeric field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($null -eq $value -or $value -is [bool] -or $value -is [string] -or $value -isnot [ValueType]) { throw "Invalid numeric field: $Name" }
    $number = [double]$value
    if (![double]::IsFinite($number) -or $number -le 0 -or $number -gt 9007199254740991) { throw "Invalid numeric field: $Name" }
    return $number
}

function Assert-RawFrameIntegrity {
    # 整个采样窗和 journal 必须完整；任何丢失或异常都不能用某个交集样本掩盖。
    # @author kongweiguang
    param($FrameReport)
    if ($null -eq $FrameReport -or $null -eq $FrameReport.PSObject.Properties['framePhases'] -or $null -eq $FrameReport.framePhases -or $null -eq $FrameReport.framePhases.PSObject.Properties['rawFrames'] -or $null -eq $FrameReport.framePhases.rawFrames) { throw 'Missing raw frame report.' }
    if ($null -eq $FrameReport.PSObject.Properties['status'] -or $FrameReport.status -cne 'frame_samples_collected') { throw 'GPUI frame histogram has insufficient samples.' }
    $phases = $FrameReport.framePhases
    $raw = $phases.rawFrames
    $origin = Get-RequiredNumber $raw 'originUnixMs'
    $originDelay = Get-RequiredNumber $raw 'originClockReadDelayNs'
    $endNs = Get-RequiredNumber $raw 'measurementEndNs'
    $endUnix = Get-RequiredNumber $raw 'measurementEndUnixMs'
    $endDelay = Get-RequiredNumber $raw 'endClockReadDelayNs'
    $duration = Get-RequiredNumber $FrameReport 'actualMeasurementDurationMs'
    $target = Get-RequiredNumber $FrameReport 'targetMeasurementDurationMs'
    if ($origin -le 0 -or $endUnix -le $origin -or $endNs -le 0 -or $target -le 0 -or $duration -lt $target -or [math]::Floor($endNs / 1000000) -ne $duration) { throw 'Invalid or incomplete measurement window.' }
    if ((Get-RequiredNumber $phases 'journalEntriesLost') -ne 0 -or (Get-RequiredNumber $raw 'dropped') -ne 0 -or (Get-RequiredNumber $raw 'invalid') -ne 0) { throw 'Journal entries or raw frames were lost or invalid.' }
    if ((Get-RequiredNumber $phases 'journalCrossBoundaryFrames') -ne 0) { throw 'A raw frame crossed the measurement boundary; timestamps remain in the frame report.' }
    if ($null -eq $phases.PSObject.Properties['journalFenceObserved'] -or $phases.journalFenceObserved -isnot [bool] -or !$phases.journalFenceObserved) { throw 'Journal publication fence was not observed after the measurement window.' }
    $fenceStart = Get-RequiredNumber $phases 'journalFenceDrawStartNs'
    $fenceEnd = Get-RequiredNumber $phases 'journalFencePresentEndNs'
    if ($fenceStart -le $endNs -or $fenceEnd -lt $fenceStart) { throw 'Journal fence timestamps do not follow the measurement window.' }
    if ($null -eq $phases.PSObject.Properties['journalFenceDirtyAtNs']) { throw 'Missing journal fence dirty timestamp field.' }
    if ($null -ne $phases.journalFenceDirtyAtNs -and (Get-RequiredNumber $phases 'journalFenceDirtyAtNs' $true) -gt $fenceStart) { throw 'Journal fence dirty timestamp is after its draw start.' }
    if ((Get-RequiredNumber $phases 'powerTransitions') -ne 0) { throw 'Power transition may have interrupted the measurement; interruption cannot be excluded.' }
    if ($null -eq $raw.PSObject.Properties['samples'] -or $null -eq $raw.samples) { throw 'Missing raw frame samples.' }
    $frames = @($raw.samples)
    if ($frames.Count -eq 0 -or (Get-RequiredNumber $phases 'presentedFrameCount') -ne $frames.Count) { throw 'Raw frame count does not match presented frame count.' }
    $previousEnd = $null
    foreach ($frame in $frames) {
        $drawStart = Get-RequiredNumber $frame 'drawStartNs' $true
        $drawEnd = Get-RequiredNumber $frame 'drawEndNs' $true
        $presentStart = Get-RequiredNumber $frame 'presentStartNs' $true
        $presentEnd = Get-RequiredNumber $frame 'presentEndNs' $true
        if ($null -eq $frame.PSObject.Properties['windowIdsMatch'] -or $frame.windowIdsMatch -isnot [bool] -or !$frame.windowIdsMatch -or $null -eq $frame.PSObject.Properties['dirtyAtNs']) { throw 'Frame has missing or inconsistent identity or dirty timestamp field.' }
        if ($null -ne $frame.dirtyAtNs -and (Get-RequiredNumber $frame 'dirtyAtNs' $true) -gt $drawStart) { throw 'Frame dirty timestamp is after draw start.' }
        if ($drawStart -gt $drawEnd -or $drawEnd -gt $presentStart -or $presentStart -gt $presentEnd -or $presentEnd -gt $endNs -or ($null -ne $previousEnd -and $presentEnd -lt $previousEnd)) { throw 'Raw frame timestamps are invalid or out of order.' }
        $previousEnd = $presentEnd
    }
    # 墙钟可能被调整；端点不一致时拒绝，端点一致也不能证明中途没有往返跳变。
    $driftMs = [math]::Abs(($endUnix - $origin) - $endNs / 1000000)
    $readDelayMs = ($originDelay + $endDelay) / 1000000
    if ($driftMs -gt ($readDelayMs + 2)) { throw 'Wall-clock and monotonic measurement windows disagree.' }
    return [ordered]@{ originUnixMs = $origin; endUnixMs = $endUnix; endNs = $endNs; alignmentMarginMs = 2 + $readDelayMs + $driftMs; wallMonotonicDifferenceMs = $driftMs }
}

function Get-HostListSummary {
    # 验收源、分页、逐目标的真实构造行和实际采样窗均需一致，不能以滚动请求冒充可视结果。
    # @author kongweiguang
    param($List, $FrameReport)
    if ($null -eq $List -or $null -eq $List.fixture -or $List.fixture.fixtureId -cne 'host-list-100000-v1' -or $List.fixture.source -cne 'host-in-process' -or $List.fixture.externalPlugin -isnot [bool] -or $List.fixture.externalPlugin -or $List.fixture.pluginFactoryOrIpc -isnot [bool] -or $List.fixture.pluginFactoryOrIpc -or $List.fixture.phase -cne 'open' -or [string]::IsNullOrWhiteSpace($List.fixture.instance)) { throw 'Host list fixture identity or lifecycle is invalid.' }
    $visibilityProperty = $List.fixture.PSObject.Properties['visibility']
    $visibility = if ($null -ne $visibilityProperty) { $visibilityProperty.Value } else { $null }
    # 私有桌面可能报告 inactive，因此 focused 可能为 false；内容可见仍必须是真实通知，
    # 实际渲染由每个目标行的 visible range 证据单独成立。
    if ($null -eq $visibility -or !(Get-RequiredBoolean $visibility 'visible')) { throw 'Host list real visibility notification is missing or not visible.' }
    $focusedProperty = $visibility.PSObject.Properties['focused']
    $visibilityFocused = if ($null -ne $focusedProperty -and $focusedProperty.Value -is [bool]) { [bool]$focusedProperty.Value } else { $null }
    if ((Get-RequiredNumber $List 'sweepTargets') -ne 500 -or (Get-RequiredNumber $List 'roundtripTargets') -ne 15) { throw 'Host list target counts are invalid.' }
    # 固定节奏必须由采样预算推导并显式记录；脚本不自行选择间隔，也不接受缺失的计划字段。
    $intervalMs = Get-RequiredNumber $List 'scheduleIntervalMs'
    $lateToleranceMs = Get-RequiredNumber $List 'scheduleLateToleranceMs'
    if ($intervalMs -lt 55 -or $lateToleranceMs -gt 100 -or (Get-RequiredNumber $List 'scheduleSlots') -ne 515) { throw 'Host list sampling plan budget is invalid.' }
    $clock = Assert-RawFrameIntegrity $FrameReport
    $start = Get-RequiredNumber $List 'startedUnixMs'
    $end = Get-RequiredNumber $List 'finishedUnixMs'
    $elapsed = Get-RequiredNumber $List 'elapsedMs'
    if ($end -le $start -or [math]::Abs(($end - $start) - $elapsed) -gt 10 -or $start -lt ($clock.originUnixMs + $clock.alignmentMarginMs) -or $end -gt ($clock.endUnixMs - $clock.alignmentMarginMs)) { throw 'Host list interval was not fully captured or its clocks disagree.' }
    $fetches = @($List.fixture.fetches)
    $fetchStart = Get-RequiredNumber $List 'fixtureFetchStart'
    if ($fetchStart -ge $fetches.Count) { throw 'Host list has no measured paging requests.' }
    foreach ($fetch in $fetches) {
        $from = Get-RequiredNumber $fetch 'start'; $to = Get-RequiredNumber $fetch 'end'; $returned = Get-RequiredNumber $fetch 'returned'
        if ($to -le $from -or $to - $from -gt 500 -or $returned -ne [math]::Max(0, [math]::Min($to, 100000) - $from)) { throw 'Host list paging evidence is invalid.' }
    }
    $observations = @($List.observations)
    if ($observations.Count -ne 515) { throw 'Host list business row observations are incomplete.' }
    $expectedRoundtrip = @(0,49999,99999,49999,0,49999,0,99999,0,49999,0,99999,49999,0,49999)
    $previous = 0
    $previousRequested = $null
    $expectedPrevious = [int]::MinValue
    $intervalNs = $intervalMs * 1000000
    $lateToleranceNs = $lateToleranceMs * 1000000
    $lates = [Collections.Generic.List[double]]::new()
    $requestOffsets = [Collections.Generic.List[double]]::new()
    for ($i = 0; $i -lt $observations.Count; $i++) {
        $observation = $observations[$i]
        $expected = if ($i -lt 500) { ($i * 4096) % 100000 } else { $expectedRoundtrip[$i - 500] }
        $phase = if ($i -lt 500) { 'sweep' } else { 'roundtrip' }
        # 相邻目标相同只能是原地空滚动，不能计入真实滚动次数。
        if ($expected -eq $expectedPrevious) { throw 'Host list target sequence repeats an index without movement.' }
        $expectedPrevious = $expected
        $requested = Get-RequiredNumber $observation 'requestedNs'; $observed = Get-RequiredNumber $observation 'observedNs'
        if ((Get-RequiredNumber $observation 'index') -ne $expected -or $observation.phase -cne $phase -or $requested -lt $previous -or $observed -lt $requested -or $observed -gt ($elapsed + 1) * 1000000) { throw 'Host list scroll order or timing is invalid.' }
        # 槽号、计划时刻和实际请求共同证明既没有跳步/重复，也没有在等待后集中补发。
        if ((Get-RequiredNumber $observation 'slot') -ne $i) { throw 'Host list sampling slot was skipped, duplicated or reordered.' }
        $planned = Get-RequiredNumber $observation 'plannedNs'
        $expectedPlanned = $i * $intervalNs
        if ($planned -ne $expectedPlanned) { throw 'Host list sampling plan differs from the recorded interval.' }
        $late = $requested - $planned
        $recordedLate = Get-RequiredNumber $observation 'lateMs'
        # 单调时钟与调度器允许极小提前；明显早发仍拒绝。记录值与实测差同样受 2ms 上限。
        if ($late -lt -1000000 -or $late -gt ($lateToleranceNs + 1000000) -or [math]::Abs($recordedLate * 1000000 - $late) -gt 2000000) { throw 'Host list sampling request was late, early or misrecorded.' }
        if ($null -ne $previousRequested) {
            $gap = $requested - $previousRequested
            # 容忍两端各自的抖动；零间隔或跨两槽的突发追赶都超出固定节奏范围。
            if ($gap -lt ($intervalNs - 2 * $lateToleranceNs - 1000000) -or $gap -gt ($intervalNs + 2 * $lateToleranceNs + 1000000)) { throw 'Host list sampling gap indicates a burst, skip or duplicate.' }
        }
        $previousRequested = $requested
        $lates.Add($late / 1000000)
        $requestOffsets.Add($requested)
        $observedList = $observation.list
        if ($null -eq $observedList -or $observedList.status -cne 'Ready' -or $observedList.visible -isnot [bool] -or !$observedList.visible -or (Get-RequiredNumber $observedList.collection 'total') -ne 100000 -or @($observedList.collection.visible_rows | Where-Object { $_.index -eq $expected -and $_.key -ceq "record-$expected" }).Count -eq 0) { throw 'Host list target was not observed in the real renderer.' }
        if ($observedList.instance -cne $List.fixture.instance -or (Get-RequiredNumber $observedList 'generation') -ne 1) { throw 'Host list observation is not from the same open instance.' }
        $visibleStart = Get-RequiredNumber $observedList.collection 'visible_start'
        $visibleEnd = Get-RequiredNumber $observedList.collection 'visible_end'
        $baselineStart = Get-RequiredNumber $observation 'baselineVisibleStart'
        $baselineEnd = Get-RequiredNumber $observation 'baselineVisibleEnd'
        if ($visibleEnd -le $visibleStart -or $visibleEnd -gt 100000 -or $expected -lt $visibleStart -or $expected -ge $visibleEnd) { throw 'Host list visible range does not contain the target row.' }
        if ($baselineEnd -le $baselineStart -or ($visibleStart -eq $baselineStart -and $visibleEnd -eq $baselineEnd)) { throw 'Host list target was observed without a new visible range after the scroll request.' }
        $previous = $observed
    }
    $metadata = $List.metadata
    if ($null -eq $metadata -or @($metadata.viewport).Count -ne 2) { throw 'Host list viewport metadata is missing.' }
    foreach ($coordinate in @($metadata.viewport)) {
        if ($null -eq $coordinate -or $coordinate -is [bool] -or $coordinate -is [string] -or $coordinate -isnot [ValueType] -or ![double]::IsFinite([double]$coordinate) -or $coordinate -le 0) { throw 'Host list viewport metadata is invalid.' }
    }
    if ((Get-RequiredFiniteNumber $metadata 'scaleFactor') -le 0 -or (Get-RequiredFiniteNumber $metadata 'fontSize') -le 0) { throw 'Host list DPI or font size metadata is invalid.' }
    $fontFamilyProperty = $metadata.PSObject.Properties['fontFamily']
    if ($null -eq $fontFamilyProperty -or [string]::IsNullOrWhiteSpace([string]$fontFamilyProperty.Value)) { throw 'Host list font family metadata is missing.' }
    $themeProperty = $metadata.PSObject.Properties['themeMode']
    if ($null -eq $themeProperty -or ($themeProperty.Value -cne 'light' -and $themeProperty.Value -cne 'dark')) { throw 'Host list theme identity is invalid.' }
    # 私有桌面可能报告窗口非活动；这里只要求两项状态是真实布尔值并记录，
    # 内容可见和实际渲染仍由列表 visible 与目标行证据单独成立。
    $windowVisible = Get-RequiredBoolean $metadata 'windowVisible'
    $windowActive = Get-RequiredBoolean $metadata 'windowActive'
    # 激活耗时写入比较：3 插件启动天然晚于 0 组，只有实际耗时超过共享预留才是预算失败。
    $activationProperty = $List.PSObject.Properties['activation']
    if ($null -eq $activationProperty -or $null -eq $activationProperty.Value) { throw 'Host list activation evidence is missing.' }
    $activation = $activationProperty.Value
    $activationElapsed = Get-RequiredNumber $activation 'elapsedMs'
    $activationReserve = Get-RequiredNumber $activation 'reserveMs'
    if ($activationReserve -ne $ActivationReserveMs) { throw 'Host list activation reserve differs from the shared budget.' }
    $themeScopeProperty = $List.PSObject.Properties['themeScope']
    $windowStateScopeProperty = $List.PSObject.Properties['windowStateScope']
    if ($null -eq $themeScopeProperty -or [string]::IsNullOrWhiteSpace([string]$themeScopeProperty.Value)) { throw 'Host list theme sampling scope is missing.' }
    if ($null -eq $windowStateScopeProperty -or [string]::IsNullOrWhiteSpace([string]$windowStateScopeProperty.Value)) { throw 'Host list window state sampling scope is missing.' }
    # 目标序列由实际观测生成，跨轮比较的是记录而非夹具声明。
    $sequence = ($observations | ForEach-Object { "$($_.phase):$($_.index)" }) -join ','
    $startNs = ($start - $clock.originUnixMs - $clock.alignmentMarginMs) * 1000000
    $endNs = ($end - $clock.originUnixMs + $clock.alignmentMarginMs) * 1000000
    # 保留相对采样窗的位置，供跨轮校验是否在可比较的同一阶段执行。
    $startOffsetMs = $start - $clock.originUnixMs
    $endOffsetMs = $clock.endUnixMs - $end
    if ($null -eq $FrameReport.framePhases.journalFenceDirtyAtNs -or (Get-RequiredNumber $FrameReport.framePhases 'journalFenceDirtyAtNs' $true) -le $endNs) { throw 'Host list frame publication fence is incomplete.' }
    $frames = [Collections.Generic.List[object]]::new()
    foreach ($frame in @($FrameReport.framePhases.rawFrames.samples)) {
        if ($frame.presentEndNs -ge $startNs) {
            if ($null -eq $frame.dirtyAtNs) { throw 'Host list frame has no dirty timestamp.' }
            if ($frame.dirtyAtNs -ge $startNs -and $frame.dirtyAtNs -le $endNs) { $frames.Add($frame) }
        }
    }
    if ($frames.Count -eq 0) { throw 'Host list interval has no invalidated frames.' }
    $latency = @($frames | ForEach-Object { ([double]$_.presentEndNs - [double]$_.dirtyAtNs) / 1000000 } | Sort-Object)
    $draw = @($frames | ForEach-Object { ([double]$_.drawEndNs - [double]$_.drawStartNs) / 1000000 } | Sort-Object)
    $p95 = [int][math]::Ceiling($frames.Count * 0.95) - 1
    $lateP95Index = [int][math]::Ceiling($lates.Count * 0.95) - 1
    $sortedLates = @($lates | Sort-Object)
    $maxLateMs = ($lates | Measure-Object -Maximum).Maximum
    return [pscustomobject][ordered]@{ fixtureId = $List.fixture.fixtureId; source = $List.fixture.source; instance = $List.fixture.instance; observations = $observations.Count; fetches = $fetches.Count; measuredFetches = $fetches.Count - $fetchStart; startedUnixMs = $start; finishedUnixMs = $end; elapsedMs = $elapsed; startOffsetMs = $startOffsetMs; endOffsetMs = $endOffsetMs; frameOriginUnixMs = $clock.originUnixMs; frameEndUnixMs = $clock.endUnixMs; targetSequence = $sequence; metadata = $metadata; windowVisible = $windowVisible; windowActive = $windowActive; themeScope = [string]$themeScopeProperty.Value; windowStateScope = [string]$windowStateScopeProperty.Value; activationElapsedMs = $activationElapsed; activationReserveMs = $activationReserve; visibilityVisible = $true; visibilityFocused = $visibilityFocused; frameCount = $frames.Count; drawP95Ms = $draw[$p95]; invalidationToPresentP95Ms = $latency[$p95]; scheduleIntervalMs = $intervalMs; scheduleLateToleranceMs = $lateToleranceMs; maxScheduleLateMs = $maxLateMs; p95ScheduleLateMs = $sortedLates[$lateP95Index]; requestedOffsetsNs = @($requestOffsets); physicalDisplayMeasured = $false; pluginIpcMeasured = $false; scope = 'window invalidation to platform draw return; same fixture targets and absolute monotonic sampling plan; no physical display or plugin IPC evidence' }
}

function Assert-HostListComparison {
    # 只有两轮实际记录的身份、显隐、主题、目标序列、绝对采样计划和采样窗相对位置一致才可比较；
    # 任一计划、迟到、跳步、帧窗或实际节奏偏差都失败关闭，不能把追赶突发描述成固定速率。
    # @author kongweiguang
    param($Zero, $Three)
    if ($null -eq $Zero -or $null -eq $Three) { throw 'Host list comparison requires both observed summaries.' }
    if ($Zero.fixtureId -cne $Three.fixtureId -or $Zero.source -cne $Three.source) { throw 'Host list fixture or data source differs between 0 and 3 external plugins.' }
    if ($Zero.fixtureId -cne 'host-list-100000-v1' -or $Zero.source -cne 'host-in-process') { throw 'Host list comparison is not using the host acceptance fixture.' }
    $zeroMeta = $Zero.metadata; $threeMeta = $Three.metadata
    if ($null -eq $zeroMeta -or $null -eq $threeMeta) { throw 'Host list geometry metadata is missing.' }
    $zeroViewport = @($zeroMeta.viewport); $threeViewport = @($threeMeta.viewport)
    if ($zeroViewport.Count -ne 2 -or $threeViewport.Count -ne 2 -or $zeroViewport[0] -ne $threeViewport[0] -or $zeroViewport[1] -ne $threeViewport[1]) { throw 'Host list viewport differs between 0 and 3 external plugins.' }
    $zeroScale = Get-RequiredFiniteNumber $zeroMeta 'scaleFactor'
    $threeScale = Get-RequiredFiniteNumber $threeMeta 'scaleFactor'
    $zeroFont = Get-RequiredFiniteNumber $zeroMeta 'fontSize'
    $threeFont = Get-RequiredFiniteNumber $threeMeta 'fontSize'
    if ($zeroScale -ne $threeScale -or $zeroFont -ne $threeFont -or $zeroMeta.fontFamily -cne $threeMeta.fontFamily -or $zeroMeta.themeMode -cne $threeMeta.themeMode) { throw 'Host list DPI, font or theme differs between 0 and 3 external plugins.' }
    # 私有桌面可能不把窗口标记为活动；只要求真实布尔值并保留两侧值，
    # 不再把 active 当作可比前提，避免把私有桌面状态误报成采样失败。
    $zeroWindowVisible = Get-RequiredBoolean $zeroMeta 'windowVisible'
    $threeWindowVisible = Get-RequiredBoolean $threeMeta 'windowVisible'
    $zeroWindowActive = Get-RequiredBoolean $zeroMeta 'windowActive'
    $threeWindowActive = Get-RequiredBoolean $threeMeta 'windowActive'
    if ([string]::IsNullOrWhiteSpace($Zero.targetSequence) -or $Zero.targetSequence -cne $Three.targetSequence) { throw 'Host list observed target sequence differs between 0 and 3 external plugins.' }
    # 计划必须逐槽相同：间隔、容差、槽数以及每个计划时刻都来自同一预算公式。
    $zeroInterval = Get-RequiredNumber $Zero 'scheduleIntervalMs'
    $threeInterval = Get-RequiredNumber $Three 'scheduleIntervalMs'
    $zeroTolerance = Get-RequiredNumber $Zero 'scheduleLateToleranceMs'
    $threeTolerance = Get-RequiredNumber $Three 'scheduleLateToleranceMs'
    if ($zeroInterval -ne $threeInterval -or $zeroTolerance -ne $threeTolerance) { throw 'Host list sampling plan differs between 0 and 3 external plugins.' }
    if ($null -eq $Zero.PSObject.Properties['requestedOffsetsNs'] -or $null -eq $Three.PSObject.Properties['requestedOffsetsNs']) { throw 'Host list raw sampling plan evidence is incomplete.' }
    $zeroOffsets = @($Zero.requestedOffsetsNs); $threeOffsets = @($Three.requestedOffsetsNs)
    if ($zeroOffsets.Count -ne 515 -or $threeOffsets.Count -ne 515) { throw 'Host list raw sampling plan evidence is incomplete.' }
    $intervalNs = $zeroInterval * 1000000
    $maxDeviationNs = 0
    for ($i = 0; $i -lt 515; $i++) {
        $zeroOffset = [double]$zeroOffsets[$i]
        $threeOffset = [double]$threeOffsets[$i]
        # 与逐槽门禁一致，允许单调时钟/调度 1ms 内提前；明显早发仍拒绝。
        if ($zeroOffset -lt $i * $intervalNs - 1000000 -or $zeroOffset -gt $i * $intervalNs + $zeroTolerance * 1000000 + 1000000 -or $threeOffset -lt $i * $intervalNs - 1000000 -or $threeOffset -gt $i * $intervalNs + $threeTolerance * 1000000 + 1000000) { throw 'Host list raw sampling offset is outside the recorded plan.' }
        $deviation = [math]::Abs($zeroOffset - $threeOffset)
        if ($deviation -gt $maxDeviationNs) { $maxDeviationNs = $deviation }
    }
    # 两组实际请求时刻的相对偏差必须落在各自容差之内，否则不能声称同一目标节奏。
    if ($maxDeviationNs -gt (2 * $zeroTolerance + 2) * 1000000) { throw 'Host list observed pacing deviates beyond the shared plan tolerance.' }
    $zeroStart = Get-RequiredNumber $Zero 'startOffsetMs'
    $threeStart = Get-RequiredNumber $Three 'startOffsetMs'
    $offsetDelta = [math]::Abs($zeroStart - $threeStart)
    $zeroElapsed = Get-RequiredNumber $Zero 'elapsedMs'
    $threeElapsed = Get-RequiredNumber $Three 'elapsedMs'
    $elapsedDelta = [math]::Abs($zeroElapsed - $threeElapsed)
    # 3 插件激活天然让列表起点后移；先把实际激活耗时与共享预留比较，
    # 超预留时明确报“激活超预算”，而不是让偏移门禁误报“采样窗不可比”。
    $zeroActivation = Get-RequiredNumber $Zero 'activationElapsedMs'
    $threeActivation = Get-RequiredNumber $Three 'activationElapsedMs'
    $zeroReserve = Get-RequiredNumber $Zero 'activationReserveMs'
    $threeReserve = Get-RequiredNumber $Three 'activationReserveMs'
    if ($zeroReserve -ne $threeReserve) { throw 'Host list activation reserve differs between 0 and 3 external plugins.' }
    if ($zeroActivation -gt $zeroReserve -or $threeActivation -gt $threeReserve) { throw "插件激活超预算：0 组 $zeroActivation ms / 3 组 $threeActivation ms，预留 $zeroReserve ms。" }
    # 激活差是列表窗无法避免的启动偏移，可计入起点门禁；515 槽计划、
    # 每槽节奏和列表持续时间仍必须一致，不能用激活差掩盖真实采样差异。
    $activationDelta = [math]::Abs($threeActivation - $zeroActivation)
    $allowedOffsetDelta = 2000 + $activationDelta
    if ($offsetDelta -gt $allowedOffsetDelta -or $elapsedDelta -gt 2000) { throw 'Host list relative sampling windows differ beyond the activation-adjusted 2000 ms gate between cases.' }
    if ((Get-RequiredNumber $Zero 'frameOriginUnixMs') -ge (Get-RequiredNumber $Zero 'frameEndUnixMs') -or (Get-RequiredNumber $Three 'frameOriginUnixMs') -ge (Get-RequiredNumber $Three 'frameEndUnixMs')) { throw 'Host list frame window is missing or invalid.' }
    if ((Get-RequiredNumber $Zero 'frameCount') -le 0 -or (Get-RequiredNumber $Three 'frameCount') -le 0) { throw 'Host list case has no frames inside its observed interval.' }
    # 可比条件满足后才计算同口径 p95；没有用户确认的预算，只给原值、差值和适用时的比值。
    $zeroLatency = Get-RequiredNumber $Zero 'invalidationToPresentP95Ms'
    $threeLatency = Get-RequiredNumber $Three 'invalidationToPresentP95Ms'
    $zeroDraw = Get-RequiredNumber $Zero 'drawP95Ms'
    $threeDraw = Get-RequiredNumber $Three 'drawP95Ms'
    $latencyRatio = if ($zeroLatency -gt 0) { [math]::Round($threeLatency / $zeroLatency, 4) } else { $null }
    $drawRatio = if ($zeroDraw -gt 0) { [math]::Round($threeDraw / $zeroDraw, 4) } else { $null }
    return [ordered]@{
        fixtureMatched = $true; targetSequenceMatched = $true; geometryMatched = $true
        windowStateRecorded = $true; windowActiveMatched = ($zeroWindowActive -eq $threeWindowActive)
        windowVisibleMatched = ($zeroWindowVisible -eq $threeWindowVisible)
        themeMatched = $true; samplingWindowCaptured = $true; samplingPlanMatched = $true
        pacingComparable = $true
        pacingComparableScope = 'sampling plan only; runtime load comparability is reported separately as loadComparable'
        pacingMethodology = 'both cases use the same 515-slot absolute monotonic plan, interval, tolerance and target sequence; per-slot requested offsets are compared and late slots fail closed'
        scheduleIntervalMs = $zeroInterval
        scheduleLateToleranceMs = $zeroTolerance
        maxScheduleLateMs = @((Get-RequiredNumber $Zero 'maxScheduleLateMs'), (Get-RequiredNumber $Three 'maxScheduleLateMs'))
        p95ScheduleLateMs = @((Get-RequiredNumber $Zero 'p95ScheduleLateMs'), (Get-RequiredNumber $Three 'p95ScheduleLateMs'))
        maxRequestOffsetDeviationMs = $maxDeviationNs / 1000000
        activationReserveMs = $zeroReserve
        activationElapsedMs = @($zeroActivation, $threeActivation)
        activationDeltaMs = $activationDelta
        activationBudgetScope = 'real plugin activation completes before the host list window; over-reserve activation fails as a budget error instead of an incomparable sampling window'
        windowVisible = @($zeroWindowVisible, $threeWindowVisible)
        windowActive = @($zeroWindowActive, $threeWindowActive)
        windowActiveScope = 'point-in-time at sampling start on a private desktop; window activity may be false and is not a gate, and continuity across the interval is not proven'
        visibilityScope = 'last accepted host visibility notification for this instance; visible is required, focused and window active may be false and are not gates, and this is not a continuous focus trace'
        themeScope = 'themeMode sampled once at the list interval start; not a light/dark/system sweep'
        targetEvidence = 'target row rendered in a visible range that differs from the pre-request baseline'
        listStartOffsetMs = @($zeroStart, $threeStart); listStartOffsetDeltaMs = $offsetDelta
        listStartOffsetGateMs = $allowedOffsetDelta
        elapsedMs = @($zeroElapsed, $threeElapsed); elapsedDeltaMs = $elapsedDelta
        p95Comparison = [ordered]@{
            latency = [ordered]@{ unit = 'ms'; zero = $zeroLatency; three = $threeLatency; delta = $threeLatency - $zeroLatency; ratio = $latencyRatio; ratioApplicable = ($zeroLatency -gt 0) }
            draw = [ordered]@{ unit = 'ms'; zero = $zeroDraw; three = $threeDraw; delta = $threeDraw - $zeroDraw; ratio = $drawRatio; ratioApplicable = ($zeroDraw -gt 0) }
            frameCount = @((Get-RequiredNumber $Zero 'frameCount'), (Get-RequiredNumber $Three 'frameCount'))
            windowOffsetScope = 'each case list window is anchored independently and may be offset by up to the activation-adjusted start gate; p95 values share the same filter and formula, not absolute timestamps'
            frameScope = 'frames are selected whose dirtyAtNs falls inside that case own list interval'
            methodology = 'same script filter: frames whose dirtyAtNs falls inside each case list interval; latency = presentEnd - dirtyAt, draw = drawEnd - drawStart; p95 index ceil(n*0.95)-1'
            budgetEvaluated = $false; physicalDisplayMeasured = $false; pluginIpcMeasured = $false
        }
        workloadScope = '500 large-jump sweep targets plus 15 roundtrip targets on the host in-process list; not continuous RPC scrolling'
        physicalDisplayMeasured = $false; pluginIpcMeasured = $false
    }
}

function Get-PluginProcessEvidence {
    # 列表帧必须由同窗真实进程负载支撑；只用外部插件宿主进程，不从报告字段推断。
    # @author kongweiguang
    param($TreeSamples, $ListSummary, [int]$Expected)
    if ($null -eq $ListSummary) { throw 'Plugin process evidence requires the host list summary.' }
    $start = Get-RequiredNumber $ListSummary 'startedUnixMs'
    $end = Get-RequiredNumber $ListSummary 'finishedUnixMs'
    if ($end -le $start) { throw 'Host list window for plugin process evidence is invalid.' }
    # 只接受采样时刻真正落在列表窗内的进程树快照；窗外样本不能证明同窗负载。
    $inside = @($TreeSamples | Where-Object { $null -ne $_.unixMs -and [double]$_.unixMs -ge $start -and [double]$_.unixMs -le $end })
    if ($inside.Count -eq 0) { throw 'No process tree sample intersects the host list window.' }
    $hostCount = {
        param($Sample)
        @($Sample.processes | Where-Object { $_.name -ieq 'kerminal-plugin-host.exe' -or $_.name -ieq 'kerminal-plugin-host' }).Count
    }
    $counts = @($inside | ForEach-Object { & $hostCount $_ })
    $ordered = @($inside | Sort-Object { [double]$_.unixMs })
    $minHosts = ($counts | Measure-Object -Minimum).Minimum
    $maxHosts = ($counts | Measure-Object -Maximum).Maximum
    if ($Expected -eq 0) {
        # 0 组没有外部插件：仍要求窗口内有进程树样本，并确认没有插件宿主混入该负载。
        if ($maxHosts -ne 0) { throw 'Zero-plugin case ran plugin host processes during the host list window.' }
        return [pscustomobject][ordered]@{ expected = 0; applicable = $false; status = 'not-applicable'; reason = 'no external plugins are declared for this case'; samplesInWindow = $inside.Count; pluginHostProcessesMin = 0; pluginHostProcessesMax = 0; windowStartUnixMs = $start; windowEndUnixMs = $end; fieldMeaning = 'idle resident plugin host process count in periodic process-tree samples; not plugin RPC concurrency or per-request load' }
    }
    if ($Expected -ne 3) { throw 'Plugin process evidence requires an explicit 0 or 3 plugin expectation.' }
    # 窗口内每个进程树样本都必须含 3 个宿主进程；只看首末会漏掉中途退出或重启。
    if ($minHosts -lt 3) { throw 'Three live plugin host processes were not present across every process tree sample inside the host list window.' }
    return [pscustomobject][ordered]@{ expected = 3; applicable = $true; status = 'observed'; reason = 'every process tree sample inside the host list window contains three plugin host processes'; samplesInWindow = $inside.Count; pluginHostProcessesMin = $minHosts; pluginHostProcessesMax = $maxHosts; firstSampleUnixMs = [double]$ordered[0].unixMs; lastSampleUnixMs = [double]$ordered[-1].unixMs; windowStartUnixMs = $start; windowEndUnixMs = $end; fieldMeaning = 'idle resident plugin host process count in periodic process-tree samples; not plugin RPC concurrency or per-request load' }
}

function Assert-ListLoadComparison {
    # 负载可比性独立于采样计划：0 组必须无插件进程，3 组必须在列表窗内确有 3 个宿主进程。
    # @author kongweiguang
    param($Zero, $Three)
    if ($null -eq $Zero -or $null -eq $Three) { throw 'List load comparison requires both process evidence records.' }
    if ($Zero.expected -ne 0 -or $Zero.applicable -isnot [bool] -or $Zero.applicable -or $Zero.status -cne 'not-applicable') { throw 'Zero-plugin list load evidence is invalid or fabricated.' }
    if ($Three.expected -ne 3 -or $Three.applicable -isnot [bool] -or !$Three.applicable -or $Three.status -cne 'observed' -or (Get-RequiredNumber $Three 'pluginHostProcessesMin') -lt 3) { throw 'Three-plugin list load evidence does not prove three live plugin hosts in every sample during the list window.' }
    return [ordered]@{
        comparable = $true
        scope = 'host list sampling window only; zero case has no external plugin processes, three case requires three live plugin host processes in every intersecting process tree sample'
        fieldMeaning = 'idle resident plugin host process count; this is not plugin RPC concurrency or per-request load, and the plugin flood runs after the list window'
        zero = $Zero; three = $Three
    }
}

function Get-StartupMemoryComparison {
    # 启动与内存只有同启动器、同轮询口径的描述性对照；没有确认预算就不制造通过/失败结论。
    # @author kongweiguang
    param($ZeroCase, $ThreeCase)
    if ($null -eq $ZeroCase -or $null -eq $ThreeCase) { throw 'Startup and memory comparison requires both case results.' }
    $zeroFirst = Get-RequiredFiniteNumber $ZeroCase 'firstFrameObservedMs'
    $threeFirst = Get-RequiredFiniteNumber $ThreeCase 'firstFrameObservedMs'
    $zeroSamples = @($ZeroCase.samples); $threeSamples = @($ThreeCase.samples)
    if ($zeroSamples.Count -eq 0 -or $threeSamples.Count -eq 0) { throw 'Startup and memory comparison requires process samples from both cases.' }
    $statistics = {
        param($Samples, [string]$Field)
        $values = @($Samples | ForEach-Object { Get-RequiredNumber $_ $Field } | Sort-Object)
        $mid = [int][math]::Floor($values.Count / 2)
        $median = if ($values.Count % 2 -eq 0) { ($values[$mid - 1] + $values[$mid]) / 2 } else { $values[$mid] }
        [ordered]@{ count = $values.Count; median = [double]$median; max = [double]$values[-1] }
    }
    return [ordered]@{
        applicable = $true; budgetEvaluated = $false
        methodology = 'same private-desktop launcher, same 100ms process polling and first-frame state polling; root application process only; no plugin host working sets included; descriptive values only'
        firstFrameObservedMs = [ordered]@{ zero = $zeroFirst; three = $threeFirst; delta = $threeFirst - $zeroFirst }
        workingSetBytes = [ordered]@{ zero = (& $statistics $zeroSamples 'workingSetBytes'); three = (& $statistics $threeSamples 'workingSetBytes') }
        privateBytes = [ordered]@{ zero = (& $statistics $zeroSamples 'privateBytes'); three = (& $statistics $threeSamples 'privateBytes') }
        limitations = @('Polling intervals include observation latency and asynchronous state commit.', 'Root process memory excludes plugin host processes; it is not a total workload memory comparison.')
    }
}

function Get-FloodComparison {
    # 0 组不声明外部插件，因此没有洪泛基线；保留三组原始值并明确不可比，不用零值补造。
    # @author kongweiguang
    param($ZeroCase, $ThreeCase)
    if ($null -eq $ZeroCase -or $null -eq $ThreeCase) { throw 'Flood comparison requires both case results.' }
    if ($null -ne $ZeroCase.pluginWorkload.data.renderer) { throw 'Zero-plugin case unexpectedly contains an external plugin flood.' }
    return [ordered]@{
        applicable = $false; comparable = $false
        reason = 'the zero-plugin case declares no external plugin and therefore has no plugin flood baseline'
        zero = [ordered]@{ applicable = $false; status = 'not-applicable'; throughput = $null; stressFrameSummary = $null }
        three = [ordered]@{ applicable = $true; throughput = $ThreeCase.throughput; stressFrameSummary = $ThreeCase.stressFrameSummary }
        methodology = 'plugin flood exists only in the three-plugin case; no synthesized zero baseline'
    }
}

function Get-StressFrameSummary {
    # 仅从同一原始帧计算首次失效到平台调用返回；边界外晚回帧仍须计入。
    # @author kongweiguang
    param($FrameReport, $Data)
    $clock = Assert-RawFrameIntegrity $FrameReport
    $start = Get-RequiredNumber $Data 'started_unix_ms'
    $sendEnd = Get-RequiredNumber $Data 'send_finished_unix_ms'
    $completionEnd = Get-RequiredNumber $Data 'finished_unix_ms'
    $margin = $clock.alignmentMarginMs
    if ($start -le 0 -or $sendEnd -le $start -or $completionEnd -lt $sendEnd) { throw 'Invalid plugin flood window.' }
    if ($start -lt ($clock.originUnixMs + $margin) -or $completionEnd -gt ($clock.endUnixMs - $margin)) { throw 'Complete plugin flood and acknowledgement window was not captured.' }
    # 扩大而非裁掉模糊边界，以免少算真实洪泛时失效或发送结束后才返回的慢帧。
    $startNs = ($start - $clock.originUnixMs - $margin) * 1000000
    $endNs = ($sendEnd - $clock.originUnixMs + $margin) * 1000000
    if ($endNs -le $startNs) { throw 'Plugin send window is invalid after clock alignment.' }
    if ($null -eq $FrameReport.framePhases.PSObject.Properties['journalFenceDirtyAtNs'] -or $null -eq $FrameReport.framePhases.journalFenceDirtyAtNs) { throw 'Journal fence has no valid dirty timestamp to exclude missing flood frames.' }
    $fenceDirty = Get-RequiredNumber $FrameReport.framePhases 'journalFenceDirtyAtNs' $true
    if ($fenceDirty -le $endNs) { throw 'A flood-period frame remained incomplete after the measurement window.' }
    $frames = [Collections.Generic.List[object]]::new()
    $completedAfterSend = 0
    foreach ($frame in $FrameReport.framePhases.rawFrames.samples) {
        if ($frame.presentEndNs -ge $startNs) {
            if ($null -eq $frame.dirtyAtNs) { throw 'A frame at or after the plugin flood has no dirty timestamp.' }
            if ($frame.dirtyAtNs -ge $startNs -and $frame.dirtyAtNs -le $endNs) {
                $frames.Add($frame)
                if ($frame.presentEndNs -gt $endNs) { $completedAfterSend++ }
            }
        }
    }
    if ($frames.Count -eq 0) { throw 'No dirty-to-present frames were invalidated in the plugin flood window.' }
    $draw = @($frames | ForEach-Object { ([double]$_.drawEndNs - [double]$_.drawStartNs) / 1000000 } | Sort-Object)
    $present = @($frames | ForEach-Object { ([double]$_.presentEndNs - [double]$_.presentStartNs) / 1000000 } | Sort-Object)
    $latency = @($frames | ForEach-Object { ([double]$_.presentEndNs - [double]$_.dirtyAtNs) / 1000000 } | Sort-Object)
    $p95 = [int][math]::Ceiling($frames.Count * 0.95) - 1
    return [ordered]@{
        frames = @($frames.ToArray())
        summary = [ordered]@{
            scope = 'same-frame first invalidation in expanded send window through platform draw call return, including post-send returns'
            boundary = 'gpui_first_window_invalidation_to_platform_draw_call_return'; physicalDisplayMeasured = $false; platformSubmissionSuccessVerified = $false
            startUnixMs = $start; sendEndUnixMs = $sendEnd; completionEndUnixMs = $completionEnd; alignmentMarginMs = $margin
            sendWindowCaptured = $true; completionWindowCaptured = $true
            count = $frames.Count; completedAfterSendMargin = $completedAfterSend; invalidationToPresentP95Ms = $latency[$p95]; invalidationToPresentMaxMs = $latency[-1]
            drawP95Ms = $draw[$p95]; drawMaxMs = $draw[-1]; presentP95Ms = $present[$p95]; presentMaxMs = $present[-1]
        }
    }
}

function Get-FloodThroughput {
    # 确认吞吐包含最后一个 ACK 的等待时间，失败或未达到 1000/s 不得被视为验收通过。
    # @author kongweiguang
    param($Data)
    if ($null -eq $Data -or $null -eq $Data.PSObject.Properties['done'] -or $Data.done -isnot [bool] -or !$Data.done -or $null -eq $Data.PSObject.Properties['completed'] -or $Data.completed -isnot [bool] -or !$Data.completed) { throw 'Plugin flood completion is missing or invalid.' }
    $attempted = Get-RequiredNumber $Data 'attempted'
    $acknowledged = Get-RequiredNumber $Data 'acknowledged'
    $failed = Get-RequiredNumber $Data 'failed'
    $inFlight = Get-RequiredNumber $Data 'in_flight'
    $skipped = Get-RequiredNumber $Data 'skipped_due_to_in_flight_limit'
    $seq = Get-RequiredNumber $Data 'seq'
    $sendElapsed = Get-RequiredNumber $Data 'send_elapsed_ms'
    $completionElapsed = Get-RequiredNumber $Data 'completion_elapsed_ms'
    $start = Get-RequiredNumber $Data 'started_unix_ms'
    $sendEnd = Get-RequiredNumber $Data 'send_finished_unix_ms'
    $completionEnd = Get-RequiredNumber $Data 'finished_unix_ms'
    if ($start -le 0 -or $attempted -le 0 -or $seq -le 0 -or $sendElapsed -le 0 -or $completionElapsed -lt $sendElapsed -or $completionEnd - $start -ne $completionElapsed -or $sendEnd - $start -ne $sendElapsed -or $acknowledged -ne $attempted -or $inFlight -ne 0 -or $failed -ne 0) { throw 'Plugin flood did not complete with consistent send and acknowledgement timestamps.' }
    # 发送窗和尝试次数必须达到 fixture 声明的持续负载语义；高瞬时速率的短 burst
    # 不能证明 12 秒持续洪泛，也不能与接收确认吞吐混为一谈。
    if ($attempted -lt $FloodMinimumAttempts) { throw "Plugin flood attempted only $attempted updates; sustained load requires at least $FloodMinimumAttempts." }
    if ($sendElapsed -lt $FloodMinimumSendMs) { throw "Plugin flood send window is $sendElapsed ms; sustained load requires at least $FloodMinimumSendMs ms and rejects short bursts." }
    $attemptRate = $attempted * 1000 / $sendElapsed
    $ackRate = $acknowledged * 1000 / $completionElapsed
    return [ordered]@{ attempted = $attempted; acknowledged = $acknowledged; skippedDueToInFlightLimit = $skipped; attemptedPerSecond = $attemptRate; acknowledgedPerSecond = $ackRate; sendElapsedMs = $sendElapsed; completionElapsedMs = $completionElapsed; targetAcknowledgedPerSecond = 1000; targetAchieved = ($ackRate -ge 1000) }
}

function Assert-FloodThroughput {
    # 将实际 ACK 速率作为硬门禁；目标值只作比较，不作为实测。
    # @author kongweiguang
    param($Throughput)
    if ($null -eq $Throughput -or $Throughput.targetAchieved -isnot [bool] -or !$Throughput.targetAchieved) { throw 'Acknowledged plugin flood throughput is below 1000 per second.' }
}

function Close-PerformanceProcess {
    # 即使 Job 查询或终止失败也继续尝试释放，任何错误都保留且不能声称已回收。
    # @author kongweiguang
    param($Native, [bool]$Failed)
    $errors = [Collections.Generic.List[string]]::new()
    $jobCleared = $null; $processExited = $null; $desktopClosed = $null
    try {
        if ($Failed) {
            $snapshot = $null
            try { $snapshot = $Native.GetJobSnapshot() } catch { $errors.Add("Initial Job query failed: $($_.Exception.Message)") }
            if ($null -eq $snapshot -or $snapshot.ActiveProcesses -gt 0) {
                try { $Native.TerminateJob(1) } catch { $errors.Add("Job termination failed: $($_.Exception.Message)") }
            }
            try { $processExited = $Native.WaitForExit(10000) } catch { $errors.Add("Process wait failed: $($_.Exception.Message)") }
            if ($processExited -cne $true) {
                try { $Native.Terminate(1) } catch { $errors.Add("Process termination failed: $($_.Exception.Message)") }
                try { $processExited = $Native.WaitForExit(10000) } catch { $errors.Add("Process wait after termination failed: $($_.Exception.Message)") }
                if ($processExited -cne $true) { $errors.Add('Owned process termination was not confirmed.') }
            }
            try {
                $cleanupDeadline = [DateTime]::UtcNow.AddSeconds(10)
                do {
                    $snapshot = $Native.GetJobSnapshot()
                    if ($snapshot.ActiveProcesses -eq 0) { break }
                    Start-Sleep -Milliseconds 100
                } while ([DateTime]::UtcNow -lt $cleanupDeadline)
                $jobCleared = ($snapshot.ActiveProcesses -eq 0)
                if (!$jobCleared) { $errors.Add('Owned Job still has running descendants.') }
            } catch { $errors.Add("Final Job query failed: $($_.Exception.Message)") }
        } else {
            # 正常退出及 Job 归零已由调用方在进入此分支前按原生句柄确认。
            $processExited = $true; $jobCleared = $true
        }
    } catch {
        $errors.Add("Unexpected cleanup failure: $($_.Exception.Message)")
    } finally {
        try { $Native.Dispose() } catch { $errors.Add("Dispose failed: $($_.Exception.Message)") }
        try {
            $desktopClosed = $Native.DesktopClosed
            if (!$desktopClosed) { $errors.Add('Private desktop closure was not confirmed.') }
        } catch { $errors.Add("Desktop closure query failed: $($_.Exception.Message)") }
    }
    return [ordered]@{ processExited = $processExited; jobCleared = $jobCleared; desktopClosed = $desktopClosed; errors = @($errors.ToArray()); reclaimed = ($errors.Count -eq 0 -and $processExited -ceq $true -and $jobCleared -ceq $true -and $desktopClosed -ceq $true) }
}

function Invoke-PerformanceCase {
    # 两组各用独立配置与 Job；只有正式退出、后代归零并回读报告后才交付成功结果。
    # @author kongweiguang
    param([string]$Name, [string]$Template, [int]$Expected)
    $case = Join-Path $output $Name
    $config = Join-Path $case 'config'
    New-Item -ItemType Directory -Path $config -Force | Out-Null
    Copy-Item -Path (Join-Path $Template '*') -Destination $config -Recurse -Force
    if ($Expected -eq 3) { Install-PerformanceFixture $config }
    $previous = Get-PluginState $config $Expected
    $previousId = if ($null -ne $previous -and $null -ne $previous.PSObject.Properties['boot'] -and $null -ne $previous.boot) { $previous.boot.id } else { '' }
    $statePath = Join-Path $config 'plugins/state.json'
    $reportPath = Join-Path $case 'frame-report.json'
    $env:KERMINAL_CONFIG_ROOT = $config
    $env:KERMINAL_PLUGIN_PERFORMANCE_ISOLATION_ROOT = $case
    $env:KERMINAL_PLUGIN_PERFORMANCE_EXPECTED_PLUGINS = [string]$Expected
    $env:KERMINAL_ACCEPTANCE_METRICS = '1'
    $env:KERMINAL_ACCEPTANCE_METRICS_RAW_FRAMES = '1'
    $env:KERMINAL_ACCEPTANCE_METRICS_DURATION_MS = [string]($DurationSeconds * 1000)
    $env:KERMINAL_ACCEPTANCE_METRICS_WARMUP_MS = [string]($WarmupSeconds * 1000)
    $env:KERMINAL_ACCEPTANCE_METRICS_OUTPUT = $reportPath
    $env:KERMINAL_ACCEPTANCE_METRICS_ENVIRONMENT = 'plugin-performance-private-desktop'
    $env:KERMINAL_ACCEPTANCE_METRICS_WORKLOAD = $Name
    $env:KERMINAL_ACCEPTANCE_WINDOW_TITLE = "Kerminal plugin performance $Name"
    $performancePath = Join-Path $case 'plugin-workload.json'
    $env:KERMINAL_PLUGIN_PERFORMANCE_REPORT = $performancePath
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $native = $null; $firstFrame = $null; $succeeded = $false; $failure = $null; $cleanup = $null
    $samples = [Collections.Generic.List[object]]::new()
    $treeSamples = [Collections.Generic.List[object]]::new()
    $result = [ordered]@{
        case = $Name; expectedEnabledPlugins = $Expected; status = 'failed'; processId = $null
        firstFrameObservedMs = $null; firstFrameMeasurement = 'CreateProcess call to first-frame state file observed; includes polling and asynchronous state commit'
        samples = $samples; processTreeSamples = $treeSamples; frameReport = 'frame-report.json'; frameIntegrity = $null; pluginWorkload = $null; hostListSummary = $null; pluginProcessEvidence = $null; throughput = $null; stressFrameSummary = $null
        normalExitCode = $null; normalExitJob = $null; failure = $null; cleanup = $null
        limitations = @('Process-tree snapshots are periodic; short-lived child processes may be missed. The Job counter verifies its own members reached zero, not whether system-brokered or escaped processes remain.', 'Both cases request a window refresh about every 16ms; GPUI platform draw call return does not verify submission success, system composition or physical display scanout.', 'First invalidation is window-wide and can coalesce periodic refreshes and plugin updates; the metric is not per-RPC latency or proof that a plugin update caused a frame.', 'Whole-window aggregates include idle time; use stressFrameSummary and stress-frames.json for load measurements. Cross-process wall-clock alignment has millisecond precision and cannot detect a clock adjustment that is later reversed.', 'The workload reports attempted, acknowledged, failed, skipped and still in-flight RPCs; target rate is not a measured achievement.')
    }
    $nextTreeSampleMs = 0
    try {
        $desktop = 'KerminalRail-settings-plugin-perf-' + [guid]::NewGuid().ToString('N')
        $native = [Kerminal.PluginPerformanceAcceptance.NativeDesktopLauncher]::Start($executable, $repository, $case, (Join-Path $case 'stdout.log'), (Join-Path $case 'stderr.log'), $desktop, 1440, 900)
        $result.processId = $native.ProcessId
        while (!$native.WaitForExit(100)) {
            if ($clock.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw 'Performance sampling or normal application shutdown timed out.' }
            if ($clock.ElapsedMilliseconds -ge $nextTreeSampleMs) {
                $treeSamples.Add((Get-OwnedProcessTreeSample $native.ProcessId ([math]::Round($clock.Elapsed.TotalMilliseconds, 3))))
                $nextTreeSampleMs = $clock.ElapsedMilliseconds + 500
            }
            $sample = Get-PerformanceProcessSample $native.ProcessId ([math]::Round($clock.Elapsed.TotalMilliseconds, 3))
            if ($null -ne $sample) { $samples.Add($sample) }
            if ($null -eq $firstFrame -and (Test-Path -LiteralPath $statePath)) {
                try {
                    $state = Get-Content -Raw -LiteralPath $statePath | ConvertFrom-Json
                    if ($null -ne $state.PSObject.Properties['boot'] -and $null -ne $state.boot -and $state.boot.id -cne $previousId -and $state.boot.phase -ceq 'first_frame') { $firstFrame = [math]::Round($clock.Elapsed.TotalMilliseconds, 3); $result.firstFrameObservedMs = $firstFrame }
                } catch { }
            }
        }
        $result.normalExitCode = $native.GetExitCode()
        if ($result.normalExitCode -ne 0) { throw "Performance candidate exited with code $($result.normalExitCode). See $(Join-Path $case 'stderr.log')" }
        $settleDeadline = [DateTime]::UtcNow.AddSeconds(10)
        while ($native.GetJobSnapshot().ActiveProcesses -gt 0 -and [DateTime]::UtcNow -lt $settleDeadline) { Start-Sleep -Milliseconds 100 }
        $result.normalExitJob = $native.GetJobSnapshot()
        if ($result.normalExitJob.ActiveProcesses -ne 0) { throw 'Normal shutdown left running plugin descendants.' }
        if ($null -eq $firstFrame -or !(Test-Path -LiteralPath $reportPath -PathType Leaf)) { throw 'First frame or native metrics report was not observed before timeout.' }
        $finalState = Get-PluginState $config $Expected
        if ($null -eq $finalState) { throw 'Plugin state was not observed after the performance case.' }
        if (@($finalState.failures.PSObject.Properties).Count -ne 0) { throw 'Plugin startup reported package failures; comparison is invalid.' }
        $frameReport = Get-Content -Raw -LiteralPath $reportPath | ConvertFrom-Json
        $result.frameIntegrity = Assert-RawFrameIntegrity $frameReport
        if ((Get-RequiredNumber $frameReport 'targetMeasurementDurationMs') -ne ($DurationSeconds * 1000)) { throw 'Frame report target does not match the requested sampling duration.' }
        $workload = Get-Content -Raw -LiteralPath $performancePath | ConvertFrom-Json
        $result.pluginWorkload = $workload
        if ($workload.status -cne 'observed' -or $workload.data.metricsObserved -isnot [bool] -or !$workload.data.metricsObserved -or @($workload.data.pluginsReady).Count -ne $Expected) { throw 'Actual metrics completion and plugin process count were not confirmed.' }
        if ((Get-RequiredNumber $workload.data 'expectedExternalPlugins') -ne $Expected) { throw 'App-reported external plugin expectation differs from the case.' }
        $scopeProperty = $workload.data.PSObject.Properties['comparisonScope']
        if ($null -eq $scopeProperty -or $scopeProperty.Value -isnot [string] -or !$scopeProperty.Value.Contains('script comparison')) { throw 'App report must scope a single case and defer cross-case comparison to the script.' }
        $meaningProperty = $workload.data.hostList.PSObject.Properties['observationMeaning']
        if ($null -eq $meaningProperty -or $meaningProperty.Value -isnot [string] -or !$meaningProperty.Value.Contains('visible range that changed')) { throw 'App report must describe target evidence as a new visible range, not a bare row hit.' }
        if ($null -eq $workload.data.PSObject.Properties['loadedExternalPlugins']) { throw 'Loaded external plugin identities are missing from the app report.' }
        $loadedIds = @($workload.data.loadedExternalPlugins | ForEach-Object { [string]$_ } | Sort-Object)
        if ($loadedIds.Count -ne $Expected) { throw 'Loaded external plugin count differs from the case expectation.' }
        $enabledIds = @($finalState.plugins.PSObject.Properties | Where-Object { $_.Value.enabled } | ForEach-Object { $_.Name } | Sort-Object)
        $readyIds = @($workload.data.pluginsReady | ForEach-Object { $_.plugin } | Sort-Object)
        if (($enabledIds -join "`n") -cne ($readyIds -join "`n") -or ($loadedIds -join "`n") -cne ($readyIds -join "`n")) { throw 'Enabled, loaded and activated external plugin identities differ.' }
        $result.hostListSummary = Get-HostListSummary $workload.data.hostList $frameReport
        # 列表窗内必须有同窗真实进程证据；3 组缺证据即失败关闭，0 组明确记录不可适用。
        $result.pluginProcessEvidence = Get-PluginProcessEvidence $treeSamples $result.hostListSummary $Expected
        if ($Expected -eq 3) {
            $data = $workload.data.renderer.data
            if ($null -eq $result.hostListSummary) { throw 'Host list summary is missing before real plugin flood.' }
            if ((Get-RequiredNumber $data 'started_unix_ms') -le $result.hostListSummary.finishedUnixMs) { throw 'Real plugin flood overlapped the common host list workload.' }
            $result.throughput = Get-FloodThroughput $data
            $problems = [Collections.Generic.List[string]]::new()
            try {
                $stress = Get-StressFrameSummary $frameReport $data
                ConvertTo-Json -InputObject $stress.frames -Depth 6 | Set-Content -Encoding utf8 (Join-Path $case 'stress-frames.json')
                $result.stressFrameSummary = $stress.summary
            } catch { $problems.Add($_.Exception.Message) }
            try { Assert-FloodThroughput $result.throughput } catch { $problems.Add($_.Exception.Message) }
            if ($problems.Count -gt 0) { throw ($problems -join '; ') }
        }
        $succeeded = $true
    } catch {
        $failure = $_.Exception.Message
    } finally {
        if ($null -ne $native) { $cleanup = Close-PerformanceProcess $native (!$succeeded) }
    }
    $result.cleanup = $cleanup
    if ($succeeded -and $null -ne $cleanup -and $cleanup.reclaimed) { $result.status = 'observed' }
    if ($null -ne $failure) { $result.failure = $failure }
    if ($null -ne $cleanup -and $cleanup.errors.Count -gt 0) {
        $cleanupFailure = 'Cleanup failed: ' + ($cleanup.errors -join '; ')
        $result.failure = if ($null -ne $failure) { "$failure; $cleanupFailure" } else { $cleanupFailure }
    }
    $result | ConvertTo-Json -Depth 20 | Set-Content -Encoding utf8 (Join-Path $case 'result.json')
    if ($result.status -cne 'observed') { throw "Performance case '$Name' failed: $($result.failure)" }
}

$repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$executable = [IO.Path]::GetFullPath($ExecutablePath)
$empty = [IO.Path]::GetFullPath($EmptyConfigTemplate)
$three = [IO.Path]::GetFullPath($ThreePluginConfigTemplate)
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (!(Test-Path -LiteralPath $executable -PathType Leaf)) { throw 'Executable does not exist.' }
foreach ($template in @($empty, $three)) { if (!(Test-Path -LiteralPath $template -PathType Container)) { throw "Template does not exist: $template" } }
if (Test-Path -LiteralPath $output) { throw 'Output directory must not exist.' }
foreach ($template in @($empty, $three)) {
    $prefix = $template.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if ($output.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Output directory must not be inside a template.' }
}
if ($TimeoutSeconds -le $WarmupSeconds + $DurationSeconds) { throw 'Timeout must exceed the warmup and measurement interval.' }
# 运行前用与桌面端相同的预算公式拒绝不足时长，避免激活或洪泛把列表采样挤出帧窗。
Assert-PerformanceBudget $DurationSeconds $WarmupSeconds
# 两个模板必须先满足窗口几何、主题和字体一致；不在启动后才发现不可比。
$templateParity = Assert-PerformanceTemplateParity $empty $three
Get-PluginState $empty 0 | Out-Null
Get-PluginState $three 3 | Out-Null
New-Item -ItemType Directory -Path $output | Out-Null
[ordered]@{
    executable = $executable
    executableSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $executable).Hash
    os = @(Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, OSArchitecture)
    cpu = @(Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors)
    gpu = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion)
    requestedWindow = 'application default; actual size and DPI in frame-report.json'
    templateParity = $templateParity
    measuredAtUtc = [DateTime]::UtcNow.ToString('o')
} | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 (Join-Path $output 'environment.json')
Initialize-PerformanceLauncher
$names = @('KERMINAL_CONFIG_ROOT','KERMINAL_PLUGIN_PERFORMANCE_ISOLATION_ROOT','KERMINAL_PLUGIN_PERFORMANCE_EXPECTED_PLUGINS','KERMINAL_ACCEPTANCE_METRICS','KERMINAL_ACCEPTANCE_METRICS_RAW_FRAMES','KERMINAL_ACCEPTANCE_METRICS_DURATION_MS','KERMINAL_ACCEPTANCE_METRICS_WARMUP_MS','KERMINAL_ACCEPTANCE_METRICS_OUTPUT','KERMINAL_ACCEPTANCE_METRICS_ENVIRONMENT','KERMINAL_ACCEPTANCE_METRICS_WORKLOAD','KERMINAL_ACCEPTANCE_WINDOW_TITLE','KERMINAL_PLUGIN_PERFORMANCE_REPORT')
$saved = @{}; foreach ($name in $names) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    Invoke-PerformanceCase 'no-plugins' $empty 0
    Invoke-PerformanceCase 'three-plugins' $three 3
    # 两轮应保持相同 viewport、缩放、字体、目标序列和绝对采样计划；跨轮比较同时核对逐槽实际请求偏差。
    $zeroCase = Get-Content -Raw -LiteralPath (Join-Path $output 'no-plugins/result.json') | ConvertFrom-Json
    $threeCase = Get-Content -Raw -LiteralPath (Join-Path $output 'three-plugins/result.json') | ConvertFrom-Json
    $zero = $zeroCase.hostListSummary
    $threeResult = $threeCase.hostListSummary
    $comparison = Assert-HostListComparison $zero $threeResult
    # 计划可比与负载可比分开：pacingComparable 只描述 515 槽计划，loadComparable 证明
    # 3 插件进程确实在列表窗内运行而 0 组没有插件进程。
    $load = Assert-ListLoadComparison $zeroCase.pluginProcessEvidence $threeCase.pluginProcessEvidence
    $comparison['loadComparable'] = $load.comparable
    $comparison['loadComparison'] = $load
    # 启动/内存只给同口径描述值；0 组没有洪泛，三组原始值保留但明确不可比。
    $startupMemory = Get-StartupMemoryComparison $zeroCase $threeCase
    $flood = Get-FloodComparison $zeroCase $threeCase
    [ordered]@{
        zeroExternalPlugins = $zero; threeExternalPlugins = $threeResult; comparison = $comparison
        startupAndMemoryComparison = $startupMemory; floodComparison = $flood
    } | ConvertTo-Json -Depth 20 | Set-Content -Encoding utf8 (Join-Path $output 'host-list-comparison.json')
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
