# 只验证共同列表回执门禁，不启动候选进程或写入验收材料。
# @author kongweiguang
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# 生产函数通过动态作用域读取这些预算常量；必须在任何函数调用前定义。
$MaximumDurationSeconds = 120
$FloodMinimumSendMs = 12000
$FloodMinimumAttempts = 12000
$ActivationReserveMs = 6000

function Assert-True {
    # 使用显式布尔断言，避免空值被当作成功。
    # @author kongweiguang
    param([bool]$Value, [string]$Message)
    if (!$Value) { throw $Message }
}

function Assert-Throws {
    # 同时核对失败原因，防止其他异常意外覆盖目标校验。
    # @author kongweiguang
    param([scriptblock]$Action, [string]$Expected)
    $failure = $null
    try { & $Action } catch { $failure = $_.Exception.Message }
    Assert-True ($null -ne $failure -and $failure.Contains($Expected)) "Expected '$Expected'; actual: $failure"
}

function New-ListReport {
    # 固定墙钟和单调时钟，用于验证采样窗与真实构造行门禁。
    # @author kongweiguang
    $frames = @(
        [pscustomobject]@{ dirtyAtNs=5001000000; drawStartNs=5002000000; drawEndNs=5003000000; presentStartNs=5004000000; presentEndNs=5005000000; windowIdsMatch=$true },
        [pscustomobject]@{ dirtyAtNs=6001000000; drawStartNs=6002000000; drawEndNs=6003000000; presentStartNs=6004000000; presentEndNs=6005000000; windowIdsMatch=$true }
    )
    return [pscustomobject]@{
        status='frame_samples_collected'; actualMeasurementDurationMs=60000; targetMeasurementDurationMs=60000
        framePhases=[pscustomobject]@{
            journalEntriesLost=0; journalCrossBoundaryFrames=0; journalFenceObserved=$true; journalFenceDirtyAtNs=60100000000; journalFenceDrawStartNs=60100000000; journalFencePresentEndNs=60110000000; powerTransitions=0; presentedFrameCount=2
            rawFrames=[pscustomobject]@{ originUnixMs=1000000; originClockReadDelayNs=0; measurementEndNs=60000000000; measurementEndUnixMs=1060000; endClockReadDelayNs=0; dropped=0; invalid=0; samples=$frames }
        }
    }
}

function New-HostList {
    # 构造完整目标序列与明确的业务键，只用于内存测试而非性能证明。
    # @author kongweiguang
    $expectedRoundtrip = @(0,49999,99999,49999,0,49999,0,99999,0,49999,0,99999,49999,0,49999)
    $observations = for ($i=0; $i -lt 515; $i++) {
        $index = if ($i -lt 500) { ($i * 4096) % 100000 } else { $expectedRoundtrip[($i - 500)] }
        $visibleEnd = [math]::Min(100000, $index + 40)
        $planned = [long]$i * 100000000
        $late = [long]($i % 21) * 1000000
        [pscustomobject]@{
            phase=if ($i -lt 500) {'sweep'} else {'roundtrip'}; index=$index; slot=$i; plannedNs=$planned; lateMs=($i % 21)
            baselineVisibleStart=96000; baselineVisibleEnd=96040
            visibleStart=$index; visibleEnd=$visibleEnd
            requestedNs=($planned + $late); observedNs=($planned + $late + 1000000)
            list=[pscustomobject]@{ instance='fixture-instance'; generation=1; status='Ready'; visible=$true; collection=[pscustomobject]@{total=100000; visible_start=$index; visible_end=$visibleEnd; visible_rows=@([pscustomobject]@{index=$index;key="record-$index"})} }
        }
    }
    return [pscustomobject]@{
        fixture=[pscustomobject]@{fixtureId='host-list-100000-v1';source='host-in-process';externalPlugin=$false;pluginFactoryOrIpc=$false;phase='open';instance='fixture-instance';visibility=[pscustomobject]@{visible=$true;focused=$true};fetches=@([pscustomobject]@{start=0;end=80;returned=80},[pscustomobject]@{start=99960;end=100000;returned=40})}
        fixtureFetchStart=1; sweepTargets=500; roundtripTargets=15
        scheduleIntervalMs=100; scheduleLateToleranceMs=20; scheduleSlots=515
        activation=[pscustomobject]@{elapsedMs=0;reserveMs=6000}
        themeScope='themeMode sampled once at the list interval start; not a light/dark/system sweep'
        windowStateScope='windowVisible/windowActive sampled once at the interval start on a private desktop; activity may be false'
        startedUnixMs=1005000; finishedUnixMs=1056421; elapsedMs=51421; observations=@($observations)
        metadata=[pscustomobject]@{viewport=@(1024,720);scaleFactor=1.25;fontFamily='Test';fontSize=14.5;themeMode='light';windowVisible=$true;windowActive=$true}
    }
}

$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'plugin-performance.ps1'),[ref]$tokens,[ref]$errors)
Assert-True ($errors.Count -eq 0) "Performance script syntax errors: $($errors -join '; ')"
foreach ($name in @('Get-RequiredNumber','Get-RequiredBoolean','Get-RequiredFiniteNumber','Get-PluginState','Get-TemplateVisualState','Assert-PerformanceTemplateParity','Assert-RawFrameIntegrity','Get-HostListSummary','Assert-HostListComparison','Assert-PerformanceBudget','Get-PluginProcessEvidence','Assert-ListLoadComparison','Get-StartupMemoryComparison','Get-FloodComparison')) {
    $matches=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name},$true))
    Assert-True ($matches.Count -eq 1) "Expected one production function: $name"
    . ([scriptblock]::Create($matches[0].Extent.Text))
}
$summary=Get-HostListSummary (New-HostList) (New-ListReport)
Assert-True ($summary.observations -eq 515 -and $summary.measuredFetches -eq 1 -and $summary.frameCount -eq 2 -and $summary.startOffsetMs -eq 5000 -and !$summary.physicalDisplayMeasured -and !$summary.pluginIpcMeasured) 'Valid list evidence must stay scoped to host data and platform draw.'
Assert-True ($summary.targetSequence.StartsWith('sweep:0,sweep:4096') -and $summary.metadata.themeMode -eq 'light' -and $summary.metadata.windowActive) 'Observed target sequence and theme evidence must be recorded.'
Assert-True ($summary.scheduleIntervalMs -eq 100 -and $summary.scheduleLateToleranceMs -eq 20 -and $summary.maxScheduleLateMs -eq 20 -and $summary.p95ScheduleLateMs -eq 19 -and @($summary.requestedOffsetsNs).Count -eq 515) 'Fixed sampling plan and raw request offsets must be preserved.'
Assert-True ($summary.activationElapsedMs -eq 0 -and $summary.activationReserveMs -eq 6000 -and $summary.windowVisible -and $summary.windowActive -and $summary.themeScope.Contains('not a light/dark/system sweep') -and $summary.windowStateScope.Contains('activity may be false')) 'Activation timing, window state and single-point theme scope must be carried into the comparison summary.'
$list=New-HostList; $list.observations[10].lateMs=22; $list.observations[10].requestedNs=$list.observations[10].plannedNs+22000000; $list.observations[10].observedNs=$list.observations[10].requestedNs+1000000
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'late, early'
$list=New-HostList; $list.observations[10].requestedNs=$list.observations[10].plannedNs-1000000
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'late, early'
$list=New-HostList; $list.observations[10].plannedNs=$list.observations[10].plannedNs+1
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'plan differs'
$list=New-HostList; $list.observations[10].slot=11
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'slot was skipped'
$list=New-HostList; $list.PSObject.Properties.Remove('scheduleIntervalMs')
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'Missing numeric field'
$list=New-HostList; $list.observations=@($list.observations | Select-Object -First 514)
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'incomplete'
$list=New-HostList; $list.fixture.externalPlugin=$true
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'fixture identity'
$list=New-HostList; $list.fixture.visibility.focused=$false
$unfocused=Get-HostListSummary $list (New-ListReport)
Assert-True ($unfocused.visibilityVisible -and !$unfocused.visibilityFocused) 'A private desktop may report focused=false; visible content and rendered rows must still pass.'
$list=New-HostList; $list.fixture.visibility.PSObject.Properties.Remove('focused')
$missingFocus=Get-HostListSummary $list (New-ListReport)
Assert-True ($missingFocus.visibilityVisible -and $null -eq $missingFocus.visibilityFocused) 'A missing focus field must be recorded as absent rather than treated as a failure gate.'
$list=New-HostList; $list.fixture.visibility.visible=$false
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'not visible'
$list=New-HostList; $list.observations[10].list.instance='other-instance'
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'same open instance'
$list=New-HostList; $list.metadata.windowActive=$false; $list.fixture.visibility.focused=$false
$inactive=Get-HostListSummary $list (New-ListReport)
Assert-True (!$inactive.windowActive -and $inactive.windowVisible -and $inactive.visibilityVisible -and !$inactive.visibilityFocused) 'An inactive, unfocused private desktop must still pass on visible content and rendered rows.'
$list=New-HostList; $list.metadata.PSObject.Properties.Remove('windowVisible')
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'windowVisible'
$list=New-HostList; $list.PSObject.Properties.Remove('activation')
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'activation evidence'
$list=New-HostList; $list.activation.reserveMs=5000
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'activation reserve'
$list=New-HostList; $list.PSObject.Properties.Remove('themeScope')
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'theme sampling scope'
$list=New-HostList; $list.metadata.themeMode='system'
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'theme identity'
$list=New-HostList; $list.fixture.fetches[1].end=100600
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'paging evidence'
$list=New-HostList; $list.observations[50].list.collection.visible_rows[0].key='wrong'
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'not observed'
$list=New-HostList; $list.observations[50].index=99
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'scroll order'
$list=New-HostList; $list.observations[10].baselineVisibleStart=$list.observations[10].list.collection.visible_start; $list.observations[10].baselineVisibleEnd=$list.observations[10].list.collection.visible_end
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'without a new visible range'
$list=New-HostList; $list.observations[10].list.collection.visible_start=1000; $list.observations[10].list.collection.visible_end=1040
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'does not contain'
$list=New-HostList; $list.observations[10].index=$list.observations[9].index
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'scroll order'
$list=New-HostList; $list.startedUnixMs=999999; $list.finishedUnixMs=1005149
Assert-Throws { Get-HostListSummary $list (New-ListReport) } 'not fully captured'
$report=New-ListReport; $report.framePhases.rawFrames.dropped=1
Assert-Throws { Get-HostListSummary (New-HostList) $report } 'lost or invalid'

# 运行前预算必须与桌面端同公式拒绝不足时长；正反用例都覆盖。
Assert-PerformanceBudget 65 5
Assert-PerformanceBudget 120 5
Assert-Throws { Assert-PerformanceBudget 60 5 } 'do not fit'
Assert-Throws { Assert-PerformanceBudget 65 10 } 'do not fit'
Assert-Throws { Assert-PerformanceBudget 121 5 } 'exceeds the 120 s cap'
$performanceSource = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'plugin-performance.ps1')
Assert-True ($performanceSource.Contains('$MaximumDurationSeconds = 120') -and $performanceSource.Contains('$FloodMinimumSendMs = 12000') -and $performanceSource.Contains('$FloodMinimumAttempts = 12000') -and $performanceSource.Contains('$ActivationReserveMs = 6000')) 'Acceptance constants must bind the duration cap, flood window, attempt floor and activation reserve to the shared budget.'

function New-TreeSample {
    # 进程树替身只保留门禁使用的采样时刻与进程名。
    # @author kongweiguang
    param([long]$UnixMs, [int]$PluginHosts)
    $processes = @([pscustomobject]@{ name = 'kerminal-desktop.exe' })
    for ($i = 0; $i -lt $PluginHosts; $i++) { $processes += [pscustomobject]@{ name = 'kerminal-plugin-host.exe' } }
    return [pscustomobject]@{ unixMs = $UnixMs; elapsedMs = 0; processes = $processes }
}

$listSummary = Get-HostListSummary (New-HostList) (New-ListReport)
$threeTree = @((New-TreeSample 1005100 3), (New-TreeSample 1030000 3), (New-TreeSample 1056000 3))
$evidence3 = Get-PluginProcessEvidence $threeTree $listSummary 3
Assert-True ($evidence3.applicable -and $evidence3.status -eq 'observed' -and $evidence3.pluginHostProcessesMax -eq 3 -and $evidence3.pluginHostProcessesMin -eq 3 -and $evidence3.samplesInWindow -eq 3 -and $evidence3.fieldMeaning.Contains('idle resident') -and $evidence3.fieldMeaning.Contains('not plugin RPC')) 'Three live plugin hosts must be present in every list-window sample and the field must state it is resident process count, not RPC load.'
Assert-Throws { Get-PluginProcessEvidence @((New-TreeSample 900000 3)) $listSummary 3 } 'No process tree sample'
Assert-Throws { Get-PluginProcessEvidence @((New-TreeSample 1005100 2), (New-TreeSample 1056000 3)) $listSummary 3 } 'not present across'
Assert-Throws { Get-PluginProcessEvidence @((New-TreeSample 1005100 3), (New-TreeSample 1030000 2), (New-TreeSample 1056000 3)) $listSummary 3 } 'not present across every process tree sample'
Assert-Throws { Get-PluginProcessEvidence $threeTree $listSummary 2 } 'explicit 0 or 3'
$zeroTree = @((New-TreeSample 1010000 0), (New-TreeSample 1050000 0))
$evidence0 = Get-PluginProcessEvidence $zeroTree $listSummary 0
Assert-True (!$evidence0.applicable -and $evidence0.status -eq 'not-applicable' -and $evidence0.pluginHostProcessesMax -eq 0 -and $evidence0.samplesInWindow -eq 2) 'The zero-plugin case must record explicit not-applicable load evidence with no plugin hosts.'
Assert-Throws { Get-PluginProcessEvidence @((New-TreeSample 1010000 1)) $listSummary 0 } 'Zero-plugin case ran plugin host'

$load = Assert-ListLoadComparison $evidence0 $evidence3
Assert-True ($load.comparable -and $load.zero.status -eq 'not-applicable' -and $load.three.status -eq 'observed' -and $load.fieldMeaning.Contains('not plugin RPC') -and $load.fieldMeaning.Contains('flood runs after the list window') -and $load.scope.Contains('host list sampling window only')) 'Plan-comparable and load-comparable must remain separate but both evidenced, and load must not be described as RPC concurrency.'
Assert-Throws { Assert-ListLoadComparison $evidence3 $evidence3 } 'Zero-plugin list load'
Assert-Throws { Assert-ListLoadComparison $evidence0 $evidence0 } 'Three-plugin list load'

$zeroCase = [pscustomobject]@{ firstFrameObservedMs = 1000.5; samples = @([pscustomobject]@{ workingSetBytes = 100; privateBytes = 50 }, [pscustomobject]@{ workingSetBytes = 300; privateBytes = 150 }) }
$threeCase = [pscustomobject]@{ firstFrameObservedMs = 1200.25; samples = @([pscustomobject]@{ workingSetBytes = 200; privateBytes = 60 }, [pscustomobject]@{ workingSetBytes = 400; privateBytes = 160 }, [pscustomobject]@{ workingSetBytes = 600; privateBytes = 260 }) }
$startup = Get-StartupMemoryComparison $zeroCase $threeCase
Assert-True ($startup.applicable -and !$startup.budgetEvaluated -and $startup.firstFrameObservedMs.delta -eq 199.75 -and $startup.workingSetBytes.zero.median -eq 200 -and $startup.workingSetBytes.three.median -eq 400 -and $startup.privateBytes.three.max -eq 260 -and $startup.workingSetBytes.zero.count -eq 2) 'Startup and memory must be descriptive values from the same polling methodology, not a fabricated budget verdict.'
Assert-Throws { Get-StartupMemoryComparison ([pscustomobject]@{ samples = $zeroCase.samples }) $threeCase } 'Missing numeric field'
Assert-Throws { Get-StartupMemoryComparison ([pscustomobject]@{ firstFrameObservedMs = 1000.5; samples = @() }) $threeCase } 'requires process samples'

$zeroFlood = [pscustomobject]@{ pluginWorkload = [pscustomobject]@{ data = [pscustomobject]@{ renderer = $null } }; throughput = $null; stressFrameSummary = $null }
$threeFlood = [pscustomobject]@{ pluginWorkload = [pscustomobject]@{ data = [pscustomobject]@{ renderer = [pscustomobject]@{} } }; throughput = [pscustomobject]@{ acknowledgedPerSecond = 1000 }; stressFrameSummary = [pscustomobject]@{ count = 2 } }
$flood = Get-FloodComparison $zeroFlood $threeFlood
Assert-True (!$flood.applicable -and !$flood.comparable -and $flood.zero.status -eq 'not-applicable' -and $flood.three.applicable -and $flood.three.throughput.acknowledgedPerSecond -eq 1000) 'The zero-plugin case must not fabricate a flood baseline while preserving three-plugin raw values.'
Assert-Throws { Get-FloodComparison ([pscustomobject]@{ pluginWorkload = [pscustomobject]@{ data = [pscustomobject]@{ renderer = [pscustomobject]@{} } } }) $threeFlood } 'unexpectedly contains'

$makeSummary = { Get-HostListSummary (New-HostList) (New-ListReport) }
$comparison=Assert-HostListComparison (& $makeSummary) (& $makeSummary)
Assert-True ($comparison.fixtureMatched -and $comparison.targetSequenceMatched -and $comparison.geometryMatched -and $comparison.windowStateRecorded -and $comparison.themeMatched -and $comparison.listStartOffsetDeltaMs -eq 0 -and $comparison.pacingComparable -and $comparison.samplingPlanMatched -and $comparison.maxRequestOffsetDeviationMs -eq 0 -and $comparison.p95Comparison.latency.zero -eq 4 -and $comparison.p95Comparison.latency.three -eq 4 -and $comparison.p95Comparison.latency.delta -eq 0 -and $comparison.p95Comparison.latency.ratio -eq 1 -and $comparison.p95Comparison.latency.ratioApplicable -and !$comparison.p95Comparison.budgetEvaluated -and !$comparison.physicalDisplayMeasured -and !$comparison.pluginIpcMeasured) 'Identical plan and evidence must expose same-methodology p95 values without inventing a budget.'
Assert-True ($comparison.activationReserveMs -eq 6000 -and $comparison.activationElapsedMs[0] -eq 0 -and $comparison.activationElapsedMs[1] -eq 0 -and $comparison.activationBudgetScope.Contains('over-reserve') -and $comparison.windowVisible[0] -and $comparison.windowVisible[1] -and $comparison.windowActive[0] -and $comparison.themeScope.Contains('not a light/dark/system sweep') -and @($comparison.p95Comparison.frameCount).Count -eq 2 -and $comparison.p95Comparison.windowOffsetScope.Contains('not absolute timestamps') -and $comparison.p95Comparison.frameScope.Contains('own list interval')) 'Comparison must record activation timing, window visibility/activity, single-point theme scope and per-case p95 frame windows.'
# 私有桌面可能报告非活动窗口；不能因此拒绝比较。
$three=& $makeSummary; $three.metadata.windowActive=$false
$inactiveComparison=Assert-HostListComparison (& $makeSummary) $three
Assert-True (!$inactiveComparison.windowActive[1] -and $inactiveComparison.pacingComparable) 'An inactive private-desktop window must be recorded without blocking comparison when content and render evidence hold.'
# 3 插件激活造成的不可避免起点差可以进入门禁；超过激活预留则明确报预算失败。
$zero=& $makeSummary; $three=& $makeSummary
$three.activationElapsedMs=5000; $three.startOffsetMs=$zero.startOffsetMs+5000
$activationAdjusted=Assert-HostListComparison $zero $three
Assert-True ($activationAdjusted.listStartOffsetDeltaMs -eq 5000 -and $activationAdjusted.listStartOffsetGateMs -eq 7000 -and $activationAdjusted.activationDeltaMs -eq 5000) 'Activation time must widen the start-offset gate while keeping the 515-slot plan and duration identical.'
$three=& $makeSummary; $three.activationElapsedMs=6001
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } '激活超预算'
$three=& $makeSummary; $three.activationElapsedMs=5000; $three.startOffsetMs=12001
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'relative sampling windows'
$three=& $makeSummary; $three.activationReserveMs=5000
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'activation reserve differs'
$three=& $makeSummary; $three.metadata.themeMode='dark'
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'theme'
$three=& $makeSummary; $three.metadata.viewport[0]=1280
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'viewport'
$three=& $makeSummary; $three.targetSequence='changed'
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'target sequence'
$three=& $makeSummary; $three.startOffsetMs=9000
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'relative sampling windows'
$three=& $makeSummary; $three.elapsedMs=54000
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'relative sampling windows'
$three=& $makeSummary; $three.scheduleIntervalMs=101
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'sampling plan differs'
$three=& $makeSummary; $three.scheduleLateToleranceMs=19
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'sampling plan differs'
$three=& $makeSummary; $three.requestedOffsetsNs[10]=$three.requestedOffsetsNs[10]+100000000
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'outside the recorded plan'
$three=& $makeSummary; $three.PSObject.Properties.Remove('requestedOffsetsNs')
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'raw sampling plan'
$three=& $makeSummary; $three.frameEndUnixMs=$three.frameOriginUnixMs
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'frame window'
$three=& $makeSummary; $three.targetSequence=''
Assert-Throws { Assert-HostListComparison (& $makeSummary) $three } 'target sequence'
$zero=& $makeSummary; $three=& $makeSummary
$zero.invalidationToPresentP95Ms=0; $zero.drawP95Ms=0
$comparison=Assert-HostListComparison $zero $three
Assert-True (!$comparison.p95Comparison.latency.ratioApplicable -and $null -eq $comparison.p95Comparison.latency.ratio -and $comparison.p95Comparison.latency.delta -eq 4 -and !$comparison.p95Comparison.draw.ratioApplicable -and $comparison.p95Comparison.draw.delta -eq 1) 'A zero p95 denominator must omit the ratio instead of fabricating one.'
$zero=& $makeSummary; $three=& $makeSummary; $three.invalidationToPresentP95Ms=8
$comparison=Assert-HostListComparison $zero $three
Assert-True ($comparison.p95Comparison.latency.delta -eq 4 -and $comparison.p95Comparison.latency.ratio -eq 2) 'Comparable p95 values must retain originals, delta and ratio.'

$probeRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-performance-state-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $probeRoot 'plugins') -Force | Out-Null
try {
    Assert-True ($null -eq (Get-PluginState $probeRoot 0)) 'Missing state for an empty case must stay explicit instead of being treated as enabled plugins.'
    Assert-Throws { Get-PluginState $probeRoot 3 } 'Missing plugin state'
    [ordered]@{ plugins=[ordered]@{}; pending=@() } | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $probeRoot 'plugins/state.json')
    Assert-True ($null -ne (Get-PluginState $probeRoot 0)) 'An explicitly written empty plugin state must remain readable.'
} finally {
    Remove-Item -LiteralPath $probeRoot -Recurse -Force
}

$templateBase = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-performance-template-' + [guid]::NewGuid().ToString('N'))
$templateEmpty = Join-Path $templateBase 'empty'
$templateThree = Join-Path $templateBase 'three'
New-Item -ItemType Directory -Path $templateEmpty, $templateThree -Force | Out-Null
try {
    $settings = @'
schema_version = 1
themeMode = "light"
interfaceDensity = "compact"

[appearance]
colorScheme = "kerminal"

[terminal]
fontFamily = "Cascadia Code"
fontSize = 15
'@
    $windowState = '{"schema_version":1,"revision":1,"state":{"normal_bounds":{"x":10,"y":20,"width":1440,"height":900},"maximized":false,"fullscreen":false}}'
    $writeTemplates = {
        param([string]$EmptySettings, [string]$ThreeSettings, [string]$ThreeWindow)
        $EmptySettings | Set-Content -Encoding utf8 (Join-Path $templateEmpty 'settings.toml')
        $ThreeSettings | Set-Content -Encoding utf8 (Join-Path $templateThree 'settings.toml')
        $windowState | Set-Content -Encoding utf8 (Join-Path $templateEmpty 'window-state.json')
        $ThreeWindow | Set-Content -Encoding utf8 (Join-Path $templateThree 'window-state.json')
    }
    & $writeTemplates $settings $settings $windowState
    $parity = Assert-PerformanceTemplateParity $templateEmpty $templateThree
    Assert-True ($parity.matched -and $parity.themeMode -eq 'light' -and $parity.colorScheme -eq 'kerminal' -and $parity.normalBounds -eq '10,20,1440,900' -and $parity.scope.Contains('terminal font is not compared') -and $parity.scope.Contains('background/workbench-layout keys are outside') -and $null -eq $parity.PSObject.Properties['fontFamily']) 'Matching geometry/theme/density/color scheme must pass; terminal font must stay outside the preflight and the scope must not claim full visual equality.'
    # 终端字体不影响宿主列表的 UI theme 字体，只改 terminal 字体仍必须通过。
    $terminalOnly = $settings -replace 'fontFamily = "Cascadia Code"', 'fontFamily = "Consolas"'
    & $writeTemplates $settings $terminalOnly $windowState
    $terminalParity = Assert-PerformanceTemplateParity $templateEmpty $templateThree
    Assert-True ($terminalParity.matched) 'A terminal-font-only difference must not block the host-list comparison.'
    # 应用默认 themeMode 是 Dark：缺失与显式 dark 等价，缺失与显式 system 不等价。
    $withoutTheme = $settings -replace '(?m)^themeMode = "light"\r?\n', ''
    $dark = $settings -replace 'themeMode = "light"', 'themeMode = "dark"'
    $system = $settings -replace 'themeMode = "light"', 'themeMode = "system"'
    & $writeTemplates $withoutTheme $dark $windowState
    $defaultParity = Assert-PerformanceTemplateParity $templateEmpty $templateThree
    Assert-True ($defaultParity.matched -and $defaultParity.themeMode -eq 'dark') 'A missing themeMode must normalize to the application default Dark.'
    & $writeTemplates $withoutTheme $system $windowState
    Assert-Throws { Assert-PerformanceTemplateParity $templateEmpty $templateThree } 'differ in themeMode'
    & $writeTemplates $settings $settings $windowState
    ($settings -replace 'themeMode = "light"', 'themeMode = "dark"') | Set-Content -Encoding utf8 (Join-Path $templateThree 'settings.toml')
    Assert-Throws { Assert-PerformanceTemplateParity $templateEmpty $templateThree } 'differ in themeMode'
    & $writeTemplates $settings $settings $windowState
    ($windowState -replace '"width":1440', '"width":1280') | Set-Content -Encoding utf8 (Join-Path $templateThree 'window-state.json')
    Assert-Throws { Assert-PerformanceTemplateParity $templateEmpty $templateThree } 'differ in normalBounds'
    & $writeTemplates $settings $settings ($windowState -replace '"schema_version":1', '"schema_version":2')
    Assert-Throws { Assert-PerformanceTemplateParity $templateEmpty $templateThree } 'schema or revision'
    & $writeTemplates $settings $settings ($windowState -replace '"width":1440', '"width":100')
    Assert-Throws { Assert-PerformanceTemplateParity $templateEmpty $templateThree } 'outside the persisted window range'
    & $writeTemplates $settings $settings $windowState
    Remove-Item -LiteralPath (Join-Path $templateThree 'window-state.json')
    Assert-Throws { Assert-PerformanceTemplateParity $templateEmpty $templateThree } 'differ in windowStatePresent'
} finally {
    Remove-Item -LiteralPath $templateBase -Recurse -Force
}

$repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$main = Get-Content -Raw -LiteralPath (Join-Path $repository 'crates/kerminal-desktop/src/main.rs')
$catalogs = Get-Content -Raw -LiteralPath (Join-Path $repository 'crates/kerminal-desktop/src/plugin_catalogs.rs')
$acceptance = Get-Content -Raw -LiteralPath (Join-Path $repository 'crates/kerminal-desktop/src/plugin_performance_acceptance.rs')
$uiAcceptance = Get-Content -Raw -LiteralPath (Join-Path $repository 'crates/kerminal-ui/src/root/plugin_list_performance_acceptance.rs')
$performance = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'plugin-performance.ps1')
$caseFunctions=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq 'Invoke-PerformanceCase'},$true))
Assert-True ($caseFunctions.Count -eq 1 -and $caseFunctions[0].Extent.Text.Contains('if ($null -eq $finalState)')) 'The case must reject a missing post-run plugin state before strict-mode property access.'
Assert-True ($caseFunctions[0].Extent.Text.Contains('overlapped the common host list workload') -and $caseFunctions[0].Extent.Text.Contains('Get-PluginProcessEvidence $treeSamples $result.hostListSummary $Expected')) 'The plugin flood must remain after the common list window and load evidence must come from intersecting process-tree samples.'
Assert-True ($caseFunctions[0].Extent.Text.Contains('App-reported external plugin expectation differs') -and $caseFunctions[0].Extent.Text.Contains('Loaded external plugin count differs')) 'The case must reject app-reported plugin counts that differ from the declared 0 or 3 expectation.'
Assert-True ($caseFunctions[0].Extent.Text.Contains('comparisonScope') -and $caseFunctions[0].Extent.Text.Contains('observationMeaning')) 'The case must require the app to scope single-case evidence and describe the new visible range.'
Assert-True ($main.Contains('KERMINAL_PLUGIN_PERFORMANCE_REPORT') -and $main.Contains('acceptance-metrics 与 right-rail-acceptance')) 'Performance report must be rejected when both acceptance features are absent.'
Assert-True ($catalogs.Contains('Host list acceptance') -and $catalogs.Contains('right-rail-acceptance')) 'Host list declaration must stay behind the acceptance feature pair.'
Assert-True ($acceptance.Contains('KERMINAL_PLUGIN_PERFORMANCE_ISOLATION_ROOT') -and $acceptance.Contains('宿主列表滚动未完整落入实际帧采样窗') -and $acceptance.Contains('KERMINAL_PLUGIN_PERFORMANCE_EXPECTED_PLUGINS') -and $acceptance.Contains('实际外部插件数量或负载身份与验收期望不一致') -and $acceptance.Contains('wait_host_visibility') -and $acceptance.Contains('themeMode') -and $acceptance.Contains('comparisonScope') -and $acceptance.Contains('visible range that changed') -and $acceptance.Contains('fn visible_range') -and $acceptance.Contains('previous != (start, end)') -and $acceptance.Contains('verify_plugin_generations') -and $acceptance.Contains('pluginLiveness') -and -not $acceptance.Contains('acceptanceActivityOverride')) 'Acceptance driver must require an isolated root, exact plugin expectation, real visibility, new-range evidence and no constant override claim.'
Assert-True ($uiAcceptance.Contains('struct HostListCreation') -and $uiAcceptance.Contains('creation.reserve(&identity.instance)?') -and $uiAcceptance.Contains('creation.rollback(&identity.instance, epoch)') -and $uiAcceptance.Contains('fn confirm') -and $uiAcceptance.Contains('let reopening = phase == "closed"') -and $uiAcceptance.Contains('telemetry["fetches"] = json!([])') -and $uiAcceptance.Contains('instanceEpoch') -and $uiAcceptance.Contains('fn owns_state') -and $uiAcceptance.Contains('epoch: u64')) 'Factory creation must reserve before mount, confirm success, roll back failed mounts and reset evidence when reopening after a normal close; every runtime entry must also match the shared instance epoch.'
Assert-True ($performance.Contains("KERMINAL_PLUGIN_PERFORMANCE_ISOLATION_ROOT = `$case") -and $performance.Contains("KERMINAL_PLUGIN_PERFORMANCE_EXPECTED_PLUGINS = [string]`$Expected") -and $performance.Contains('Get-HostListSummary $workload.data.hostList $frameReport') -and $performance.Contains('Assert-HostListComparison $zero $threeResult') -and $performance.Contains('without a new visible range') -and $performance.Contains('scheduleIntervalMs') -and $performance.Contains('pacingComparable = $true') -and $performance.Contains('p95Comparison') -and $performance.Contains('ratioApplicable') -and $performance.Contains('Get-PluginProcessEvidence') -and $performance.Contains('Assert-PerformanceBudget') -and $performance.Contains('unixMs') -and $performance.Contains('loadComparable') -and $performance.Contains('startupAndMemoryComparison') -and $performance.Contains('floodComparison')) 'Performance script must pass the isolated root and plugin expectation, validate fixed schedule and process load, and expose separate plan, load, startup/memory and flood comparisons.'
# 状态机静态门禁：验收目录必须先提交 RegistryFrozen，再构建附加宿主声明；
# 否则窗口首帧标记会因仍处于 Starting 而永远冲突。
$markIndex = $acceptance.IndexOf('.mark_boot_phase(')
$frozenIndex = $acceptance.IndexOf('PluginBootPhase::RegistryFrozen')
$acceptanceCatalogBuildIndex = $acceptance.IndexOf('catalogs_for_plugins_inner(')
Assert-True ($markIndex -gt 0 -and $frozenIndex -gt $markIndex -and $acceptanceCatalogBuildIndex -gt $frozenIndex) 'Acceptance catalogs must commit RegistryFrozen before building the appended host-list declarations.'
Assert-True ($acceptance.Contains('HOST_LIST_MAX_MEASUREMENT_MS: u64 = 120_000') -and $acceptance.Contains('插件激活超预算') -and $acceptance.Contains('"windowVisible":window.is_visible()') -and $acceptance.Contains('themeScope') -and $acceptance.Contains('windowStateScope') -and $acceptance.Contains('"activation":{')) 'Acceptance driver must cap duration to raw capacity, fail over-reserve activation explicitly, and record single-point window/theme scopes with activation timing.'
# 私有桌面可能 inactive/unfocused；可见性等待不得再要求 focused 真值。
$visibilityGate = $acceptance.IndexOf('snapshot["phase"] == "open" && snapshot["visibility"]["visible"] == true')
$focusedGate = $acceptance.IndexOf('snapshot["visibility"]["focused"] == true')
Assert-True ($visibilityGate -gt 0 -and $focusedGate -lt 0) 'Host visibility waiting must require visible content, not focused or active window state.'
# 槽 0 必须在计划锚点前读取基线，否则首次 snapshot 的耗时会被误判为迟到。
$pendingBaselineIndex = $acceptance.IndexOf('pending_baseline = Some(current_visible_range')
$originIndex = $acceptance.IndexOf('let origin = Instant::now();')
Assert-True ($pendingBaselineIndex -gt 0 -and $originIndex -gt $pendingBaselineIndex) 'Slot 0 baseline must be read before the sampling origin so the first slot gets a full scheduling interval.'
Assert-True ($performance.Contains('Assert-PerformanceTemplateParity $empty $three') -and $performance.Contains('terminal font is not compared') -and $performance.Contains('fieldMeaning') -and $performance.Contains('idle resident') -and $performance.Contains('not plugin RPC') -and $performance.Contains('windowOffsetScope') -and $performance.Contains('frameScope') -and $performance.Contains('listStartOffsetGateMs') -and $performance.Contains('activationDeltaMs')) 'Performance script must preflight geometry/theme parity without terminal fonts, describe load as resident process count, and expose activation-adjusted offset and p95 window scopes.'
Write-Output 'plugin-performance host-list synthetic tests passed'
