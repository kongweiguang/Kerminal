# 只验证列表验收的纯证据门禁，不启动候选进程或写入用户目录。
# @author kongweiguang
#Requires -Version 7.0
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    # 使用显式布尔断言，避免空值或错误类型被当作成功。
    # @author kongweiguang
    param([bool]$Value, [string]$Message)
    if (!$Value) { throw $Message }
}

function Assert-Throws {
    # 同时核对失败原因，防止其他异常意外覆盖目标门禁。
    # @author kongweiguang
    param([scriptblock]$Action, [string]$Expected)
    $failure = $null
    try { & $Action } catch { $failure = $_.Exception.Message }
    Assert-True ($null -ne $failure -and $failure.Contains($Expected)) "Expected '$Expected'; actual: $failure"
}

class FailingCleanupProcess {
    # 模拟 Job 查询、等待和 Dispose 全部失败，验证清理继续执行并聚合后续错误。
    # @author kongweiguang
    [int] $SnapshotCalls = 0
    [int] $DisposeAttempts = 0
    [bool] $Terminated = $false
    [object] GetJobSnapshot() {
        $this.SnapshotCalls++
        throw 'snapshot unavailable'
    }
    [void] TerminateJob([int] $Code) {
        $this.Terminated = $true
    }
    [bool] WaitForExit([int] $Milliseconds) {
        throw 'wait unavailable'
    }
    [void] Dispose() {
        $this.DisposeAttempts++
        throw 'dispose unavailable'
    }
}

$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'plugin-list-performance.ps1'), [ref]$tokens, [ref]$errors)
Assert-True ($errors.Count -eq 0) "Acceptance script syntax errors: $($errors -join '; ')"
foreach ($name in @('Get-RequiredNumber', 'Get-RequiredBoolean', 'Get-RequiredFiniteNumber', 'Get-RequiredString', 'Get-RequiredObject', 'Get-RequiredArray', 'Get-RequiredElementNumber', 'Get-PluginDigestRule', 'ConvertTo-UInt64LittleEndian', 'Compare-Utf8Ordinal', 'Get-PluginContentDigest', 'Complete-OwnedProcessCleanup', 'Get-CleanupFailureMessage', 'Get-ListTargetOs', 'Get-ListTargetArch', 'ConvertTo-NormalListPath', 'Assert-PluginListPluginCopy', 'Assert-PluginListProcessTree', 'Assert-PluginListEvidence')) {
    $matches = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true))
    Assert-True ($matches.Count -eq 1) "Expected one production function: $name"
    . ([scriptblock]::Create($matches[0].Extent.Text))
}

# 合成进程树需要真实文件来验证映像路径与 SHA，只创建临时文件，不启动进程。
$script:CandidateRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-list-process-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($script:CandidateRoot)
$script:CandidatePath = Join-Path $script:CandidateRoot 'kerminal-desktop.exe'
$script:HostPath = Join-Path $script:CandidateRoot 'kerminal-plugin-host.exe'
[IO.File]::WriteAllText($script:CandidatePath, 'synthetic desktop image')
[IO.File]::WriteAllText($script:HostPath, 'synthetic plugin host image')
$script:CandidateSha256 = (Get-FileHash -LiteralPath $script:CandidatePath -Algorithm SHA256).Hash.ToLowerInvariant()
$script:HostSha256 = (Get-FileHash -LiteralPath $script:HostPath -Algorithm SHA256).Hash.ToLowerInvariant()
$script:DesktopName = 'KerminalRail-settings-test'
$script:TestTheme = 'light'
$script:InputMode = 'production VirtualListScrollHandle.scroll_to_item'
$script:PluginId = 'test.list-performance'
$script:PluginCopyPath = Join-Path $script:CandidateRoot 'output/plugin'
$script:ExpectedHostSha256 = $script:HostSha256
$script:ImmutableMode = 'all acceptance-copy files set read-only before startup; full-tree digest recomputed after sampling'
$script:ThemeScope = 'darkMode read from the live GPUI Theme at sampling start; system mode is not forced to a specific appearance'
$script:Source = [pscustomobject]@{
    tabId = 'fixture-tab'; activeTabId = 'fixture-tab'; contentId = 'test.list-performance/records'
    plugin = 'test.list-performance'; view = 'records'; runtime = 'script'
    externalPlugin = $true; activeBindingVerified = $true
}

function New-ListCollection {
    # 集合错误字段必须存在且为 null；缺失由门禁按失败关闭处理。
    # @author kongweiguang
    param($Total, $Start, $End, $Rows, $Requests = 2, $Successes = 1, $Errors = 0)
    return [pscustomobject]@{
        total = $Total; visible_start = $Start; visible_end = $End; visible_rows = @($Rows); error = $null
        fetch_requests = $Requests; fetch_successes = $Successes; fetch_errors = $Errors
    }
}

function New-ListSnapshot {
    # 每份列表快照携带固定外部插件来源与可覆盖的 renderer 错误，便于正反用例共享形状。
    # @author kongweiguang
    param($Collection, $ErrorMessage = $null)
    return [pscustomobject]@{
        instance = 'fixture-instance'; generation = 1; status = 'Ready'; visible = $true
        error = $ErrorMessage; plugin = 'test.list-performance'; view = 'records'; runtime = 'script'; external = $true
        collection = $Collection
    }
}

function New-ListProcessTree {
    # 默认样本包含候选桌面与同一目录下的脚本宿主；反向用例可去掉宿主或替换路径/摘要。
    # @author kongweiguang
    param([bool]$IncludeHost = $true, [string]$HostPath = $script:HostPath, [string]$HostSha256 = $script:HostSha256, [int]$RootProcessId = 100)
    $processes = @([pscustomobject]@{ pid = $RootProcessId; parentPid = 1; name = 'kerminal-desktop.exe'; executablePath = $script:CandidatePath; imageSha256 = $script:CandidateSha256; created = $null })
    if ($IncludeHost) {
        $processes += [pscustomobject]@{ pid = $RootProcessId + 1; parentPid = $RootProcessId; name = 'kerminal-plugin-host.exe'; executablePath = $HostPath; imageSha256 = $HostSha256; commandLine = "`"$HostPath`" `"$script:PluginCopyPath`""; created = $null }
    }
    return @([pscustomobject]@{ elapsedMs = 100; unixMs = 1000000; processes = $processes })
}

function New-ListFixture {
    # 构造与Rust观测同形的合成数据；只覆盖纯门禁，不声称真实窗口或性能结果。
    # @author kongweiguang
    $firstList = New-ListSnapshot (New-ListCollection 100000 0 40 @([pscustomobject]@{ index = 0; key = 'record-0' }))
    $firstScreen = [pscustomobject]@{ workspaceReady = $true; settingsReady = $true; list = $firstList }
    $warmupObservations = @(
        [pscustomobject]@{
            phase = 'warmup'; step = 0; index = 50000; inputMode = $script:InputMode
            baselineVisibleStart = 0; baselineVisibleEnd = 40
            visibleStart = 50000; visibleEnd = 50040
            list = New-ListSnapshot (New-ListCollection 100000 50000 50040 @([pscustomobject]@{ index = 50000; key = 'record-50000' }))
        },
        [pscustomobject]@{
            phase = 'warmup'; step = 1; index = 20000; inputMode = $script:InputMode
            baselineVisibleStart = 50000; baselineVisibleEnd = 50040
            visibleStart = 19980; visibleEnd = 20020
            list = New-ListSnapshot (New-ListCollection 100000 19980 20020 @([pscustomobject]@{ index = 20000; key = 'record-20000' }))
        }
    )
    $observations = [Collections.Generic.List[object]]::new()
    $baselineStart = 19980.0
    $baselineEnd = 20020.0
    $requested = 0.0
    for ($i = 0; $i -lt 500; $i++) {
        $span = $baselineEnd - $baselineStart
        $target = $baselineEnd + $span
        $visibleStart = $target
        $visibleEnd = $target + 40
        $requestedNs = $requested
        $observedNs = $requested + 500000
        $observations.Add([pscustomobject]@{
                phase = 'continuous'; step = $i; index = $target; inputMode = $script:InputMode
                baselineVisibleStart = $baselineStart; baselineVisibleEnd = $baselineEnd
                visibleStart = $visibleStart; visibleEnd = $visibleEnd
                requestedNs = $requestedNs; observedNs = $observedNs
                list = New-ListSnapshot (New-ListCollection 100000 $visibleStart $visibleEnd @([pscustomobject]@{ index = $target; key = "record-$target" }))
            })
        $baselineStart = $visibleStart
        $baselineEnd = $visibleEnd
        $requested += 1000000
    }
    $expectedRoundtrip = @(0, 49999, 99999, 49999, 0, 49999, 0, 99999, 0, 49999, 0, 99999, 49999, 0, 49999)
    for ($i = 0; $i -lt $expectedRoundtrip.Count; $i++) {
        $target = $expectedRoundtrip[$i]
        $visibleStart = [math]::Max(0, $target - 20)
        $visibleEnd = [math]::Min(100000, $target + 20)
        $requestedNs = $requested
        $observedNs = $requested + 500000
        $observations.Add([pscustomobject]@{
                phase = 'roundtrip'; step = $i; index = $target; inputMode = $script:InputMode
                baselineVisibleStart = $baselineStart; baselineVisibleEnd = $baselineEnd
                visibleStart = $visibleStart; visibleEnd = $visibleEnd
                requestedNs = $requestedNs; observedNs = $observedNs
                list = New-ListSnapshot (New-ListCollection 100000 $visibleStart $visibleEnd @([pscustomobject]@{ index = $target; key = "record-$target" }))
            })
        $baselineStart = $visibleStart
        $baselineEnd = $visibleEnd
        $requested += 1000000
    }
    $report = [pscustomobject]@{
        schemaVersion = 3; mode = 'private-desktop-native-list'; totalRows = 100000
        continuousTargets = 500; roundtripTargets = 15; theme = $script:TestTheme; processId = 100
        minimumObservedSpan = 9
        fetchWindow = [pscustomobject]@{
            start = [pscustomobject]@{ requests = 1; successes = 1; errors = 0 }
            end = [pscustomobject]@{ requests = 520; successes = 516; errors = 0 }
        }
        shutdownComplete = $true; physicalInputTested = $false; inputMode = $script:InputMode
        rawFrames = 'raw-frames.json'; rowObservations = 'row-observations.json'
        pluginDigestRule = (Get-PluginDigestRule)
        observationMeaning = 'Each target row was constructed in a visible range that changed; OS window visibility was sampled once at sampling start and list.visible is renderer content visibility, not OS window visibility; this is not a frame count, system compositing proof, physical display timing, or input-to-display latency.'
        samplingStartedUnixMs = 1000000; samplingFinishedUnixMs = 1060000; samplingElapsedMs = 60000
        warmupTargets = @(50000, 20000); warmupObservations = $warmupObservations; firstScreen = $firstScreen
        source = [pscustomobject]@{
            tabId = 'fixture-tab'; activeTabId = 'fixture-tab'; contentId = 'test.list-performance/records'
            plugin = 'test.list-performance'; view = 'records'; runtime = 'script'
            externalPlugin = $true; activeBindingVerified = $true
        }
        metadata = [pscustomobject]@{
            viewport = @(1024, 720); scaleFactor = 1.25; fontFamily = 'Test'; fontSize = 14.5
            visible = $true; active = $true; debugAssertions = $false
            darkMode = $false; themeScope = $script:ThemeScope
            targetOs = (Get-ListTargetOs); targetArch = (Get-ListTargetArch)
            visibleScope = 'OS window visibility sampled once at sampling start; list.visible is renderer content visibility, not OS window visibility'
            visibleSampledAt = 'sampling-start'
            activeScope = 'OS window active state sampled once at sampling start in a private desktop; not proof the window remained focused or received user input'
            nativeIsolation = [pscustomobject]@{ platformSupported = $true; isolationProven = $true; uiThreadDesktopName = $script:DesktopName; inputDesktopName = 'Default'; processId = 100 }
        }
    }
    $frames = @()
    $halfNs = 60000 * 1000000 / 2
    for ($i = 0; $i -lt 60; $i++) {
        $first = $i * 100000000
        $second = $halfNs + $i * 100000000
        $frames += [pscustomobject]@{ kind = 'draw'; startNs = $first; endNs = $first + 100 }
        $frames += [pscustomobject]@{ kind = 'present'; startNs = $first + 50; endNs = $first + 150 }
        $frames += [pscustomobject]@{ kind = 'draw'; startNs = $second; endNs = $second + 100 }
        $frames += [pscustomobject]@{ kind = 'present'; startNs = $second + 50; endNs = $second + 150 }
    }
    $samples = @(
        [pscustomobject]@{ elapsedMs = 100; workingSetBytes = 1; privateBytes = 1 },
        [pscustomobject]@{ elapsedMs = 200; workingSetBytes = 2; privateBytes = 2 }
    )
    return [pscustomobject]@{ Report = $report; Observations = $observations; Frames = $frames; Samples = $samples; ProcessTree = (New-ListProcessTree) }
}

function Assert-ListFixture {
    # 统一提供本轮主题、桌面、候选路径和摘要，反向用例只需改夹具字段。
    # @author kongweiguang
    param($Fixture, [string]$Theme = $script:TestTheme)
    return Assert-PluginListEvidence $Fixture.Report $Fixture.Observations $Fixture.Frames $Fixture.Samples $Fixture.ProcessTree $Theme $script:DesktopName $script:CandidatePath $script:CandidateSha256 $script:ExpectedHostSha256 $script:PluginCopyPath $script:PluginId
}

$valid = New-ListFixture
$summary = Assert-ListFixture $valid
Assert-True ($summary.Observations -eq 515 -and $summary.Continuous -eq 500 -and $summary.Roundtrip -eq 15) 'Valid list evidence must cover all declared targets.'
Assert-True ($summary.Instance -ceq 'fixture-instance' -and $summary.Generation -eq 1 -and !$summary.FrameCountClaimed -and !$summary.PhysicalDisplayMeasured) 'Valid evidence must keep instance and reporting scope explicit.'
Assert-True ($summary.TargetSequence.StartsWith('continuous:') -and $summary.TargetSequence.Contains('roundtrip:0')) 'Observed target sequence must be recorded.'

$fixture = New-ListFixture
$fixture.Observations = @($fixture.Observations | Select-Object -First 514)
Assert-Throws { Assert-ListFixture $fixture } 'observations are incomplete'

$fixture = New-ListFixture
$fixture.Observations[10].phase = 'roundtrip'
Assert-Throws { Assert-ListFixture $fixture } 'phase is invalid'

$fixture = New-ListFixture
$fixture.Observations[10].index = 1024
Assert-Throws { Assert-ListFixture $fixture } 'one-page forward step'

$fixture = New-ListFixture
$fixture.Observations[10].visibleStart = $fixture.Observations[10].baselineVisibleStart
$fixture.Observations[10].visibleEnd = $fixture.Observations[10].baselineVisibleEnd
$fixture.Observations[10].list.collection.visible_start = $fixture.Observations[10].baselineVisibleStart
$fixture.Observations[10].list.collection.visible_end = $fixture.Observations[10].baselineVisibleEnd
Assert-Throws { Assert-ListFixture $fixture } 'without a new visible range'

$fixture = New-ListFixture
$fixture.Observations[500].baselineVisibleStart = 0
$fixture.Observations[500].baselineVisibleEnd = 20
Assert-Throws { Assert-ListFixture $fixture } 'baseline is not the visible range observed by the previous step'

$fixture = New-ListFixture
$fixture.Observations[10].visibleStart = $fixture.Observations[10].index + 1
$fixture.Observations[10].visibleEnd = $fixture.Observations[10].index + 41
$fixture.Observations[10].list.collection.visible_start = $fixture.Observations[10].visibleStart
$fixture.Observations[10].list.collection.visible_end = $fixture.Observations[10].visibleEnd
Assert-Throws { Assert-ListFixture $fixture } 'does not contain the target row'

$fixture = New-ListFixture
$fixture.Observations[10].list.collection.visible_rows = @()
Assert-Throws { Assert-ListFixture $fixture } 'not constructed in the observed range'

$fixture = New-ListFixture
$fixture.Observations[10].list.collection.visible_rows[0].key = 'record-wrong'
Assert-Throws { Assert-ListFixture $fixture } 'not constructed in the observed range'

$fixture = New-ListFixture
$fixture.Observations[10].list.instance = 'other-instance'
Assert-Throws { Assert-ListFixture $fixture } 'same open instance and generation'

$fixture = New-ListFixture
$fixture.Observations[10].list.generation = 2
Assert-Throws { Assert-ListFixture $fixture } 'same open instance and generation'

$fixture = New-ListFixture
$fixture.Observations[501].index = 0
Assert-Throws { Assert-ListFixture $fixture } 'roundtrip target sequence'

$fixture = New-ListFixture
$fixture.Observations[10].index = '100'
Assert-Throws { Assert-ListFixture $fixture } 'Invalid numeric field'

$fixture = New-ListFixture
$fixture.Observations[10].PSObject.Properties.Remove('list')
Assert-Throws { Assert-ListFixture $fixture } 'Missing object field'

$fixture = New-ListFixture
$fixture.Observations[10].observedNs = 1
$fixture.Observations[10].requestedNs = 2
Assert-Throws { Assert-ListFixture $fixture } 'timing or order'

$fixture = New-ListFixture
$fixture.Report.metadata.nativeIsolation.isolationProven = $false
Assert-Throws { Assert-ListFixture $fixture } 'native isolation'

$fixture = New-ListFixture
$fixture.Report.metadata.visible = $false
Assert-Throws { Assert-ListFixture $fixture } 'not visible at sampling start'

$fixture = New-ListFixture
$fixture.Report.metadata.visibleScope = 'content visible means OS visible'
Assert-Throws { Assert-ListFixture $fixture } 'window visibility scope is not explicit'

$fixture = New-ListFixture
$fixture.Report.physicalInputTested = $true
Assert-Throws { Assert-ListFixture $fixture } 'shutdown or input scope'

$fixture = New-ListFixture
$fixture.Frames = @($fixture.Frames | Select-Object -First 19)
Assert-Throws { Assert-ListFixture $fixture } 'fewer than one hundred real draw and present'

$fixture = New-ListFixture
$fixture.Samples = @($fixture.Samples[0])
Assert-Throws { Assert-ListFixture $fixture } 'process samples are incomplete'

$fixture = New-ListFixture
$fixture.Report.warmupTargets = @(50000, $fixture.Observations[0].index)
Assert-Throws { Assert-ListFixture $fixture } 'warmup targets'

$fixture = New-ListFixture
$fixture.Report.schemaVersion = 1
Assert-Throws { Assert-ListFixture $fixture } 'schema is invalid'

# 首屏必须是真正从 0 行开始的可视范围，content visible 也不能省略。
$fixture = New-ListFixture
$fixture.Report.firstScreen.list.collection.visible_start = 1
Assert-Throws { Assert-ListFixture $fixture } 'first screen visible range does not anchor row 0'
$fixture = New-ListFixture
$fixture.Report.firstScreen.list.collection.visible_start = 0
$fixture.Report.firstScreen.list.collection.visible_end = 0
Assert-Throws { Assert-ListFixture $fixture } 'first screen visible range does not anchor row 0'
$fixture = New-ListFixture
$fixture.Report.firstScreen.list.visible = $false
Assert-Throws { Assert-ListFixture $fixture } 'not ready and visible: first screen'

# 预暖不再只是目标数组：必须逐条真实观测并接入同一条范围链，首条锚定首屏范围。
$fixture = New-ListFixture
$fixture.Report.warmupObservations[0].baselineVisibleEnd = 41
Assert-Throws { Assert-ListFixture $fixture } 'warmup baseline is not the visible range observed by the previous step'
$fixture = New-ListFixture
$fixture.Report.warmupObservations[1].baselineVisibleStart = 0
Assert-Throws { Assert-ListFixture $fixture } 'warmup baseline is not the visible range observed by the previous step'
$fixture = New-ListFixture
$fixture.Report.warmupObservations[1].list.collection.visible_rows = @()
Assert-Throws { Assert-ListFixture $fixture } 'warmup target business row was not constructed'
$fixture = New-ListFixture
$fixture.Report.warmupObservations[1].list.status = 'Failed("warmup boom")'
Assert-Throws { Assert-ListFixture $fixture } 'Failed("warmup boom")'
$fixture = New-ListFixture
$fixture.Report.warmupObservations = 'not-an-array'
Assert-Throws { Assert-ListFixture $fixture } 'Invalid array field'

# 每步 baseline 必须等于上一观测的 visible；任何跳步或缺失都以专用诊断失败。
$fixture = New-ListFixture
$fixture.Observations[10].baselineVisibleStart = $fixture.Observations[10].baselineVisibleStart + 1
Assert-Throws { Assert-ListFixture $fixture } 'baseline is not the visible range observed by the previous step'
$fixture = New-ListFixture
$fixture.Observations[10].PSObject.Properties.Remove('baselineVisibleEnd')
Assert-Throws { Assert-ListFixture $fixture } 'Missing numeric field'

# renderer 的 Failed(...) 诊断必须按前缀识别并保留内容，不能被当成普通未就绪。
$fixture = New-ListFixture
$fixture.Observations[10].list.status = 'Failed("rpc timeout")'
Assert-Throws { Assert-ListFixture $fixture } 'Failed("rpc timeout")'

# 嵌套对象只接受 PSCustomObject；字符串、数组和哈希表不能冒充报告对象。
$fixture = New-ListFixture
$fixture.Report.metadata = 'not-an-object'
Assert-Throws { Assert-ListFixture $fixture } 'Invalid object field'
$fixture = New-ListFixture
$fixture.Report.metadata.nativeIsolation = @([pscustomobject]@{ isolationProven = $true })
Assert-Throws { Assert-ListFixture $fixture } 'Invalid object field'
$fixture = New-ListFixture
$fixture.Report.warmupTargets = 50000
Assert-Throws { Assert-ListFixture $fixture } 'Invalid array field'

# 集合或 renderer 上的异步错误必须立即失败并保留原文，不能因状态仍为 Ready 而继续采样。
$fixture = New-ListFixture
$fixture.Observations[10].list.collection.error = 'rpc timeout'
Assert-Throws { Assert-ListFixture $fixture } 'rpc timeout'
$fixture = New-ListFixture
$fixture.Observations[10].list.error = 'visibility notify failed'
Assert-Throws { Assert-ListFixture $fixture } 'visibility notify failed'
$fixture = New-ListFixture
$fixture.Report.warmupObservations[0].list.collection.error = 'warmup rpc timeout'
Assert-Throws { Assert-ListFixture $fixture } 'warmup rpc timeout'

# 来源伪造：宿主列表 fixture、其它插件、错误活动标签和错误运行时都必须被拒绝。
$fixture = New-ListFixture
$fixture.Report.source.contentId = 'acceptance.host-list'
$fixture.Report.source.plugin = 'fixture.host-list'
$fixture.Report.source.runtime = 'none'
Assert-Throws { Assert-ListFixture $fixture } 'Host-list fixture'
$fixture = New-ListFixture
$fixture.Observations[10].list.plugin = 'fixture.host-list'
Assert-Throws { Assert-ListFixture $fixture } 'source identity'
$fixture = New-ListFixture
$fixture.Report.source.activeTabId = 'other-tab'
Assert-Throws { Assert-ListFixture $fixture } 'active tab'
$fixture = New-ListFixture
$fixture.Observations[10].list.runtime = 'none'
Assert-Throws { Assert-ListFixture $fixture } 'source identity'

# 每条观测的 step 与 inputMode 必须和阶段/生产滚动通道一致。
$fixture = New-ListFixture
$fixture.Observations[10].step = 99
Assert-Throws { Assert-ListFixture $fixture } 'observation step'
$fixture = New-ListFixture
$fixture.Observations[10].inputMode = 'synthetic scroll'
Assert-Throws { Assert-ListFixture $fixture } 'observation input mode'
$fixture = New-ListFixture
$fixture.Report.warmupObservations[0].inputMode = 'synthetic scroll'
Assert-Throws { Assert-ListFixture $fixture } 'warmup input mode'

# 连续阶段必须真实前向：回退到请求前范围或跨度漂移都必须失败。
# 目标行可落在新视口起点之后，真实虚拟列表不要求目标靠近起点；该过严规则已移除。
$fixture = New-ListFixture
$fixture.Observations[10].visibleStart = $fixture.Observations[10].baselineVisibleStart
$fixture.Observations[10].visibleEnd = $fixture.Observations[10].index + 10
$fixture.Observations[10].list.collection.visible_start = $fixture.Observations[10].visibleStart
$fixture.Observations[10].list.collection.visible_end = $fixture.Observations[10].visibleEnd
Assert-Throws { Assert-ListFixture $fixture } 'did not advance past the baseline range'
$fixture = New-ListFixture
$fixture.Observations[10].visibleStart = $fixture.Observations[10].index
$fixture.Observations[10].visibleEnd = $fixture.Observations[10].index + 10
$fixture.Observations[10].list.collection.visible_start = $fixture.Observations[10].visibleStart
$fixture.Observations[10].list.collection.visible_end = $fixture.Observations[10].visibleEnd
Assert-Throws { Assert-ListFixture $fixture } 'span drifted'

# 私有桌面、主题、构建模式、平台和几何必须与本轮实际环境一致。
$fixture = New-ListFixture
$fixture.Report.metadata.nativeIsolation.platformSupported = $false
Assert-Throws { Assert-ListFixture $fixture } 'platform is not supported'
$fixture = New-ListFixture
$fixture.Report.metadata.nativeIsolation.uiThreadDesktopName = 'KerminalRail-settings-other'
Assert-Throws { Assert-ListFixture $fixture } 'UI thread'
$fixture = New-ListFixture
$fixture.Report.metadata.nativeIsolation.inputDesktopName = $script:DesktopName
Assert-Throws { Assert-ListFixture $fixture } 'still the input desktop'
$fixture = New-ListFixture
$fixture.Report.metadata.nativeIsolation.processId = 101
Assert-Throws { Assert-ListFixture $fixture } 'isolation process'
$fixture = New-ListFixture
$fixture.Report.theme = 'dark'
Assert-Throws { Assert-ListFixture $fixture } 'report theme'
$fixture = New-ListFixture
$fixture.Report.metadata.debugAssertions = $true
Assert-Throws { Assert-ListFixture $fixture } 'debug assertions'
$fixture = New-ListFixture
$fixture.Report.metadata.targetOs = 'linux'
Assert-Throws { Assert-ListFixture $fixture } 'target OS'
$fixture = New-ListFixture
$fixture.Report.metadata.viewport = @(799, 600)
Assert-Throws { Assert-ListFixture $fixture } 'smaller than 800x600'
$fixture = New-ListFixture
$fixture.Report.metadata.activeScope = 'window remained focused for the whole run'
Assert-Throws { Assert-ListFixture $fixture } 'active scope'
# active 只记录采样起点，不要求为 true；私有桌面中的后台窗口可以为 false。
$fixture = New-ListFixture
$fixture.Report.metadata.active = $false
$summary = Assert-ListFixture $fixture
Assert-True (!$summary.ActiveAtSamplingStart) 'Active state must be recorded without being forced true.'

# 帧必须落在采样时间窗内，不能把窗外的旧样本算进本轮。
$fixture = New-ListFixture
$fixture.Frames[0].endNs = ($fixture.Report.samplingElapsedMs + 2) * 1000000
Assert-Throws { Assert-ListFixture $fixture } 'outside the sampling window'

# 宿主身份：缺失宿主、候选目录外路径、摘要不符或候选映像路径被替换都必须失败。
$fixture = New-ListFixture
$fixture.ProcessTree = New-ListProcessTree -IncludeHost $false
Assert-Throws { Assert-ListFixture $fixture } 'kerminal-plugin-host was not observed'
$script:OutsideRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-list-outside-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($script:OutsideRoot)
$outsideHost = Join-Path $script:OutsideRoot 'kerminal-plugin-host.exe'
[IO.File]::WriteAllBytes($outsideHost, [IO.File]::ReadAllBytes($script:HostPath))
$fixture = New-ListFixture
$fixture.ProcessTree = New-ListProcessTree -HostPath $outsideHost -HostSha256 ((Get-FileHash -LiteralPath $outsideHost -Algorithm SHA256).Hash.ToLowerInvariant())
Assert-Throws { Assert-ListFixture $fixture } 'outside the candidate directory'
$fixture = New-ListFixture
$fixture.ProcessTree = New-ListProcessTree -HostSha256 ([string]::new([char]'0', 64))
Assert-Throws { Assert-ListFixture $fixture } 'expected build identity'
$fixture = New-ListFixture
$fixture.ProcessTree[0].processes[0].executablePath = $script:HostPath
Assert-Throws { Assert-ListFixture $fixture } 'Candidate desktop image path does not match'

# 插件摘要必须由 PS 按共享规则从实际目录独立重算；与 Rust 单元测试共用同一已知向量。
$digestRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-plugin-digest-' + [Guid]::NewGuid().ToString('N'))
try {
    [void][IO.Directory]::CreateDirectory((Join-Path $digestRoot 'views'))
    [IO.File]::WriteAllText((Join-Path $digestRoot 'a.js'), 'abc')
    [IO.File]::WriteAllText((Join-Path $digestRoot 'views/v.json'), '{}')
    Assert-True ((Get-PluginContentDigest $digestRoot) -ceq 'b58ce47fdb58840d6acd856e0607cadbb6e5cb26d4542cb5a18de43c5c2cdd5f') 'Plugin digest must match the shared Rust known vector.'
    [IO.File]::WriteAllText((Join-Path $digestRoot 'main.js'), '')
    Assert-True ((Get-PluginContentDigest $digestRoot) -ceq '3a5353342fad7f85b81165915f1fd68216a1a3d833fda0f6cf08564a40064fde') 'Empty runtime entry file must still participate in the plugin digest.'
    [IO.File]::WriteAllText((Join-Path $digestRoot 'a.js'), 'abd')
    Assert-True ((Get-PluginContentDigest $digestRoot) -cne '3a5353342fad7f85b81165915f1fd68216a1a3d833fda0f6cf08564a40064fde') 'Changing executed script bytes must change the plugin digest.'
} finally {
    Remove-Item -LiteralPath $digestRoot -Recurse -Force -ErrorAction SilentlyContinue
}

# 清理函数必须继续执行所有步骤并聚合错误，而不是在首次失败处中断或覆盖原始错误。
$failing = [FailingCleanupProcess]::new()
$cleanupErrors = Complete-OwnedProcessCleanup $failing
$cleanupText = $cleanupErrors -join '; '
Assert-True ($cleanupErrors.Count -eq 4 -and $failing.Terminated -and $failing.DisposeAttempts -eq 1 -and $failing.SnapshotCalls -eq 2) 'Cleanup must attempt every step after failures.'
Assert-True ($cleanupText.Contains('GetJobSnapshot failed') -and $cleanupText.Contains('WaitForExit failed') -and $cleanupText.Contains('Dispose failed')) 'Cleanup must aggregate later failures.'

# 首个采样错误必须保留，并附带后续清理诊断；没有主错误时才单独报告清理失败。
$primary = $null
try { throw 'primary sampling failed' } catch { $primary = $_ }
$combined = Get-CleanupFailureMessage $primary @('GetJobSnapshot failed: snapshot unavailable', 'Dispose failed: dispose unavailable')
Assert-True ($combined.Contains('primary sampling failed') -and $combined.Contains('GetJobSnapshot failed') -and $combined.Contains('Dispose failed')) 'Primary failure must survive cleanup diagnostics.'
$primaryOnly = Get-CleanupFailureMessage $primary @()
Assert-True ($primaryOnly -eq $primary -and $primaryOnly.Exception.Message -ceq 'primary sampling failed') 'Primary failure must be rethrown unchanged when cleanup succeeds.'
$cleanupOnly = Get-CleanupFailureMessage $null @('Dispose failed: dispose unavailable')
Assert-True ($cleanupOnly -ceq 'Owned process cleanup failed: Dispose failed: dispose unavailable') 'Cleanup-only failure must keep its diagnostic.'
Assert-True ($null -eq (Get-CleanupFailureMessage $null @())) 'Successful cleanup must not produce an error.'

# 驱动实现必须保留动态步距、请求前后范围、预暖观测、摘要规则和同一实例/代次契约，脚本门禁不能独立漂移。
$repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$acceptance = Get-Content -Raw -LiteralPath (Join-Path $repository 'crates/kerminal-desktop/src/plugin_list_performance_acceptance.rs')
Assert-True ($acceptance.Contains('const CONTINUOUS_TARGETS: usize = 500') -and $acceptance.Contains('const ROUNDTRIP_TARGETS: [usize; 15]')) 'Acceptance driver must declare the counted target stages.'
Assert-True ($acceptance.Contains('baselineVisibleStart') -and $acceptance.Contains('target_observed') -and $acceptance.Contains('same_instance') -and $acceptance.Contains('samplingFinishedUnixMs')) 'Acceptance driver must record new ranges, real rows, identity and sampling window.'
Assert-True ($acceptance.Contains((Get-PluginDigestRule)) -and $acceptance.Contains('warmupObservations') -and $acceptance.Contains('require_contiguous_baseline') -and $acceptance.Contains('MIN_FRAME_SAMPLES: usize = 100')) 'Acceptance driver must share the digest rule, warmup chain and frame floor.'
Assert-True ($acceptance.Contains('OS window visibility sampled once at sampling start; list.visible is renderer content visibility, not OS window visibility')) 'Acceptance driver must declare the OS visibility sampling boundary.'
Assert-True ($acceptance.Contains('inspect_source') -and $acceptance.Contains('validate_continuous_forward') -and $acceptance.Contains('total == 0 && rendered.is_empty()') -and $acceptance.Contains('列表集合分页错误')) 'Acceptance driver must bind source, zero-total waiting, collection errors and continuous forward checks.'
Assert-True ($acceptance.Contains('copy_plugin_tree') -and $acceptance.Contains('pluginCopyPath') -and $acceptance.Contains('pluginCopyStable') -and $acceptance.Contains('INPUT_MODE')) 'Acceptance driver must execute the output-root copy and record source/input evidence.'
Assert-True ($acceptance.Contains('mark_copy_read_only') -and $acceptance.Contains('immutableMode') -and $acceptance.Contains('fetchWindow') -and $acceptance.Contains('minimum_observed_span') -and $acceptance.Contains('darkMode')) 'Acceptance driver must enforce immutable copy, fetch counters, span floor and observed theme.'

# 生产门禁必须与驱动同源声明；脚本不能只校验列表形状而漏掉副本摘要、进程树和平台门禁。
$gateScript = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'plugin-list-performance.ps1')
Assert-True ($gateScript.Contains('#Requires -Version 7.0') -and $gateScript.Contains('pluginCopyPath') -and $gateScript.Contains('pluginCopyStable') -and $gateScript.Contains('Assert-PluginListProcessTree') -and $gateScript.Contains('Assert-PluginListPluginCopy') -and $gateScript.Contains('ExpectedHostSha256') -and $gateScript.Contains('Get-ListTargetArch')) 'Acceptance runner must enforce the copy, process-tree and platform gates.'

# 宿主期望摘要由候选构建 receipt 提供，采样与文件摘要都必须等于它，不能只自洽。
$savedExpectedHostSha = $script:ExpectedHostSha256
$fixture = New-ListFixture
$script:ExpectedHostSha256 = [string]::new([char]'0', 64)
Assert-Throws { Assert-ListFixture $fixture } 'expected build identity'
$script:ExpectedHostSha256 = $savedExpectedHostSha
$fixture = New-ListFixture
$fixture.ProcessTree[0].processes[1].PSObject.Properties.Remove('commandLine')
Assert-Throws { Assert-ListFixture $fixture } 'Missing string field'
$fixture = New-ListFixture
$fixture.ProcessTree[0].processes[1].commandLine = 'other-host --flag'
Assert-Throws { Assert-ListFixture $fixture } 'does not reference this run'
$fixture = New-ListFixture
$fixture.ProcessTree = New-ListProcessTree -IncludeHost $false
Assert-Throws { Assert-ListFixture $fixture } 'kerminal-plugin-host was not observed'

# 帧必须覆盖采样窗前后两半，集中在一半的帧不能证明整窗活动。
$fixture = New-ListFixture
$fixture.Frames = @($fixture.Frames | Where-Object { $_.startNs -lt 34500000000 })
Assert-Throws { Assert-ListFixture $fixture } 'missing from one half of the sampling window'

# 实际暗色值与请求主题一致；system 只要求记录，不强制具体明暗。
$fixture = New-ListFixture
$fixture.Report.metadata.darkMode = $true
Assert-Throws { Assert-ListFixture $fixture } 'light theme run observed dark UI'
$fixture = New-ListFixture
$fixture.Report.theme = 'system'
$fixture.Report.metadata.darkMode = $true
$summary = Assert-ListFixture $fixture 'system'
Assert-True ($summary.DarkMode) 'System theme run must record the observed dark mode without forcing it.'
$fixture = New-ListFixture
$fixture.Report.theme = 'dark'
$fixture.Report.metadata.darkMode = $false
Assert-Throws { Assert-ListFixture $fixture 'dark' } 'dark theme run observed light UI'

# 两行残片不能通过完整视口跨度下限。
$fixture = New-ListFixture
$fixture.Observations[10].visibleStart = $fixture.Observations[10].index
$fixture.Observations[10].visibleEnd = $fixture.Observations[10].index + 2
$fixture.Observations[10].list.collection.visible_start = $fixture.Observations[10].visibleStart
$fixture.Observations[10].list.collection.visible_end = $fixture.Observations[10].visibleEnd
Assert-Throws { Assert-ListFixture $fixture } 'smaller than the viewport-derived minimum'

# 分页计数：成功必须净增长、累计错误必须为零、成功不能超过请求。
$fixture = New-ListFixture
$fixture.Report.fetchWindow.start.errors = 1
Assert-Throws { Assert-ListFixture $fixture } 'pagination errors during sampling'
$fixture = New-ListFixture
$fixture.Report.fetchWindow.end.successes = $fixture.Report.fetchWindow.start.successes
Assert-Throws { Assert-ListFixture $fixture } 'did not add new successful fetches'
$fixture = New-ListFixture
$fixture.Observations[10].list.collection.fetch_errors = 1
Assert-Throws { Assert-ListFixture $fixture } 'recorded 1 pagination errors'
$fixture = New-ListFixture
$fixture.Observations[10].list.collection.fetch_requests = 2
$fixture.Observations[10].list.collection.fetch_successes = 10
Assert-Throws { Assert-ListFixture $fixture } 'success counter exceeds requests'
$fixture = New-ListFixture
$fixture.Report.minimumObservedSpan = 8
Assert-Throws { Assert-ListFixture $fixture } 'minimum observed span does not match'

# 副本必须位于输出根、保持只读，并从同一路径重算全文件摘要；同尺寸换入必须失败。
$script:CopyOutput = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-list-copy-' + [Guid]::NewGuid().ToString('N'))
$copyPlugin = Join-Path $script:CopyOutput 'plugin'
[void][IO.Directory]::CreateDirectory((Join-Path $copyPlugin 'views'))
[IO.File]::WriteAllText((Join-Path $copyPlugin 'main.js'), 'render()')
[IO.File]::WriteAllText((Join-Path $copyPlugin 'views/records.json'), '{}')
foreach ($file in @(Get-ChildItem -LiteralPath $copyPlugin -Recurse -File -Force)) { $file.IsReadOnly = $true }
$copyDigest = Get-PluginContentDigest $copyPlugin
$copyReport = [pscustomobject]@{
    pluginCopyPath = $copyPlugin; pluginSourcePath = $script:CandidateRoot
    pluginSha256 = $copyDigest; pluginCopyStable = $true; immutableMode = $script:ImmutableMode
}
$copyEvidence = Assert-PluginListPluginCopy $copyReport $script:CopyOutput $script:CandidateRoot
Assert-True ($copyEvidence.Digest -ceq $copyDigest) 'Read-only acceptance copy must pass the copy gate.'
$mainFile = Join-Path $copyPlugin 'main.js'
$mainFileInfo = [IO.FileInfo]::new($mainFile)
$mainFileInfo.IsReadOnly = $false
Assert-Throws { Assert-PluginListPluginCopy $copyReport $script:CopyOutput $script:CandidateRoot } 'is writable'
# 同尺寸替换后恢复只读位，摘要仍必须失败。
[IO.File]::WriteAllText($mainFile, 'render[]')
Set-ItemProperty -LiteralPath $mainFile -Name IsReadOnly -Value $true
Assert-True ((Get-Item -LiteralPath $mainFile).IsReadOnly) 'Synthetic same-size replacement must be restored to read-only.'
Assert-Throws { Assert-PluginListPluginCopy $copyReport $script:CopyOutput $script:CandidateRoot } 'digest does not match'
$copyReport.pluginSha256 = [string]::new([char]'0', 64)
Assert-Throws { Assert-PluginListPluginCopy $copyReport $script:CopyOutput $script:CandidateRoot } 'digest does not match'
$copyReport.pluginCopyPath = Join-Path $script:CandidateRoot 'plugin'
Assert-Throws { Assert-PluginListPluginCopy $copyReport $script:CopyOutput $script:CandidateRoot } 'not the acceptance copy'
$copyReport.pluginCopyPath = $copyPlugin
$copyReport.immutableMode = 'writable'
Assert-Throws { Assert-PluginListPluginCopy $copyReport $script:CopyOutput $script:CandidateRoot } 'immutable mode'

# 清理合成进程树文件；门禁结论已在上方完成，清理失败不影响断言结果。
Remove-Item -LiteralPath $script:CandidateRoot -Recurse -Force -ErrorAction SilentlyContinue
if ($null -ne $script:OutsideRoot) { Remove-Item -LiteralPath $script:OutsideRoot -Recurse -Force -ErrorAction SilentlyContinue }
if ($null -ne $script:CopyOutput) {
    foreach ($file in @(Get-ChildItem -LiteralPath $script:CopyOutput -Recurse -File -Force -ErrorAction SilentlyContinue)) { $file.IsReadOnly = $false }
    Remove-Item -LiteralPath $script:CopyOutput -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output 'plugin-list-performance synthetic tests passed'
