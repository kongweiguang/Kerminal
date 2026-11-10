# @author kongweiguang
# 仅解析正式脚本并使用内存替身；不创建验收报告、启动窗口或访问真实进程。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# 与正式脚本同源的门禁常量；测试从内存验证短 burst 和低尝试量会被拒绝。
$FloodMinimumSendMs = 12000
$FloodMinimumAttempts = 12000

function Assert-True {
    # 单测只接受明确断言，避免转换错误掩盖失败。
    # @author kongweiguang
    param([bool]$Condition, [string]$Message)
    if (!$Condition) { throw $Message }
}

function Assert-Throws {
    # 检查预期失败原因，避免其他异常碰巧通过回归。
    # @author kongweiguang
    param([scriptblock]$Action, [string]$Expected)
    $failure = $null
    try { & $Action } catch { $failure = $_.Exception.Message }
    Assert-True ($null -ne $failure -and $failure.Contains($Expected)) "Expected '$Expected'; actual: $failure"
}

function New-TestReport {
    # 用确定的相对单调时间证明同帧减法，而不是合成验收材料。
    # @author kongweiguang
    $frames = @(
        [pscustomobject]@{ dirtyAtNs = 100000000; drawStartNs = 101000000; drawEndNs = 102000000; presentStartNs = 103000000; presentEndNs = 104000000; windowIdsMatch = $true },
        [pscustomobject]@{ dirtyAtNs = 1003000000; drawStartNs = 1004000000; drawEndNs = 1006000000; presentStartNs = 1007000000; presentEndNs = 1009000000; windowIdsMatch = $true },
        [pscustomobject]@{ dirtyAtNs = 1010000000; drawStartNs = 1013000000; drawEndNs = 1014000000; presentStartNs = 1016000000; presentEndNs = 1018000000; windowIdsMatch = $true }
    )
    return [pscustomobject]@{
        status = 'frame_samples_collected'; actualMeasurementDurationMs = 20000; targetMeasurementDurationMs = 20000
        framePhases = [pscustomobject]@{
            journalEntriesLost = 0; journalFenceObserved = $true; journalFenceDirtyAtNs = 20100000000; journalFenceDrawStartNs = 20100000000; journalFencePresentEndNs = 20110000000; journalCrossBoundaryFrames = 0; powerTransitions = 0; presentedFrameCount = 3
            rawFrames = [pscustomobject]@{
                originUnixMs = 1000000; originClockReadDelayNs = 0; measurementEndNs = 20000000000
                measurementEndUnixMs = 1020000; endClockReadDelayNs = 0; dropped = 0; invalid = 0; samples = $frames
            }
        }
    }
}

function New-TestFlood {
    # 分开发送与确认耗时，覆盖 ACK 延迟对实际吞吐的影响。
    # @author kongweiguang
    return [pscustomobject]@{
        done = $true; completed = $true; attempted = 13000; acknowledged = 13000; failed = 0; in_flight = 0; skipped_due_to_in_flight_limit = 300; seq = 13000
        send_elapsed_ms = 12000; completion_elapsed_ms = 13000
        started_unix_ms = 1001000; send_finished_unix_ms = 1013000; finished_unix_ms = 1014000
    }
}

function New-FakeProcess {
    # 替身只改变内存状态，验证失败时仍尝试 Terminate 和 Dispose。
    # @author kongweiguang
    param([bool]$QueryFails, [bool]$JobTerminateFails, [bool]$DisposeFails)
    $process = [pscustomobject]@{ QueryFails = $QueryFails; JobTerminateFails = $JobTerminateFails; DisposeFails = $DisposeFails; JobTerminateCalled = $false; ProcessTerminateCalled = $false; DisposeCalled = $false; DesktopClosed = $false; Exited = $false }
    $process | Add-Member ScriptMethod GetJobSnapshot {
        if ($this.QueryFails) { throw 'fake query error' }
        return [pscustomobject]@{ ActiveProcesses = if ($this.Exited) { 0 } else { 1 } }
    }
    $process | Add-Member ScriptMethod TerminateJob {
        param($code)
        $this.JobTerminateCalled = $true
        if ($this.JobTerminateFails) { throw 'fake job termination error' }
        $this.Exited = $true
    }
    $process | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $this.Exited }
    $process | Add-Member ScriptMethod Terminate { param($code) $this.ProcessTerminateCalled = $true; $this.Exited = $true }
    $process | Add-Member ScriptMethod Dispose { $this.DisposeCalled = $true; if ($this.DisposeFails) { throw 'fake dispose error' }; $this.DesktopClosed = $true }
    return $process
}

$tokens = $null; $parseErrors = $null
$syntax = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'plugin-performance.ps1'), [ref]$tokens, [ref]$parseErrors)
Assert-True ($parseErrors.Count -eq 0) "Performance script syntax errors: $($parseErrors -join '; ')"
foreach ($name in @('Get-RequiredNumber', 'Assert-RawFrameIntegrity', 'Get-StressFrameSummary', 'Get-FloodThroughput', 'Assert-FloodThroughput', 'Close-PerformanceProcess')) {
    $definitions = @($syntax.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true))
    Assert-True ($definitions.Count -eq 1) "Expected one function: $name"
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}
$case = @($syntax.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq 'Invoke-PerformanceCase' }, $true))
Assert-True ($case.Count -eq 1 -and $case[0].Extent.Text.Contains('Assert-FloodThroughput $result.throughput') -and $case[0].Extent.Text.Contains('Assert-RawFrameIntegrity $frameReport') -and $case[0].Extent.Text.Contains('$cleanup.reclaimed') -and $case[0].Extent.Text.Contains('Get-PluginProcessEvidence $treeSamples $result.hostListSummary $Expected')) 'Acceptance case must enforce frame, throughput, process-load and cleanup gates.'

$report = New-TestReport; $data = New-TestFlood
$stress = Get-StressFrameSummary $report $data
Assert-True ($stress.frames.Count -eq 2) 'Stress selection must retain both complete same-frame samples.'
Assert-True ($stress.summary.invalidationToPresentP95Ms -eq 8 -and $stress.summary.invalidationToPresentMaxMs -eq 8) 'p95 must use each frame dirtyAt to its own presentEnd.'
Assert-True (!$stress.summary.physicalDisplayMeasured -and !$stress.summary.platformSubmissionSuccessVerified) 'Platform draw return is not physical display or verified submission.'
$late = New-TestReport
$late.framePhases.rawFrames.samples[2].presentEndNs = 13300000000
$lateStress = Get-StressFrameSummary $late $data
Assert-True ($lateStress.frames.Count -eq 2 -and $lateStress.summary.completedAfterSendMargin -eq 1 -and $lateStress.summary.invalidationToPresentMaxMs -eq 12290) 'A flood frame completed after sending must remain in the latency distribution.'
$report = New-TestReport; $report.framePhases.journalFenceDirtyAtNs = 12000000000
Assert-Throws { Get-StressFrameSummary $report (New-TestFlood) } 'remained incomplete'
$report = New-TestReport; $report.framePhases.journalFenceDirtyAtNs = $null
Assert-Throws { Get-StressFrameSummary $report (New-TestFlood) } 'fence has no valid dirty timestamp'
$throughput = Get-FloodThroughput $data
Assert-True ($throughput.acknowledgedPerSecond -eq 1000 -and $throughput.skippedDueToInFlightLimit -eq 300) 'Skipped opportunities must be reported separately and ACK rate must include completion time.'
Assert-FloodThroughput $throughput
$data.attempted = 12000; $data.acknowledged = 12000
$throughput = Get-FloodThroughput $data
Assert-True ($throughput.acknowledgedPerSecond -lt 1000 -and !$throughput.targetAchieved) 'Late ACKs must lower confirmed throughput.'
Assert-Throws { Assert-FloodThroughput $throughput } 'below 1000'
$data.failed = 1
Assert-Throws { Get-FloodThroughput $data } 'did not complete'
$data = New-TestFlood; $data.done = 'True'
Assert-Throws { Get-FloodThroughput $data } 'completion is missing or invalid'
# 高瞬时速率不能掩盖短 burst；发送窗和尝试次数都必须达到 fixture 的持续负载语义。
$data = New-TestFlood
$data.send_elapsed_ms = 5000; $data.completion_elapsed_ms = 6000
$data.send_finished_unix_ms = $data.started_unix_ms + 5000; $data.finished_unix_ms = $data.started_unix_ms + 6000
Assert-Throws { Get-FloodThroughput $data } 'rejects short bursts'
$data = New-TestFlood; $data.attempted = 11000; $data.acknowledged = 11000; $data.seq = 11000
Assert-Throws { Get-FloodThroughput $data } 'at least 12000'
$fixtureSource = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot '../../tests/fixtures/plugins/performance/main.js')
Assert-True ($fixtureSource.Contains('sendElapsed >= 12000') -and $fixtureSource.Contains('UPDATES_PER_TICK = 24') -and $fixtureSource.Split([string[]]@('sendElapsed >= 12000'), [StringSplitOptions]::None).Count -eq 2) 'The fixture must keep the 12-second send window and update-rate constants that the flood gates rely on.'

$report = New-TestReport; $report.status = 'insufficient_frame_samples'
Assert-Throws { Assert-RawFrameIntegrity $report } 'histogram has insufficient samples'
$crossing = New-TestReport
$crossing.framePhases.rawFrames.samples += [pscustomobject]@{ dirtyAtNs = 12000000000; drawStartNs = 19000000000; drawEndNs = 20000000000; presentStartNs = 20000000000; presentEndNs = 21000000000; windowIdsMatch = $true }
$crossing.framePhases.presentedFrameCount = 4
$crossing.framePhases.journalCrossBoundaryFrames = 1
$crossing.framePhases.journalFenceDirtyAtNs = 22000000000
$crossing.framePhases.journalFenceDrawStartNs = 22000000000
$crossing.framePhases.journalFencePresentEndNs = 23000000000
Assert-Throws { Get-StressFrameSummary $crossing (New-TestFlood) } 'crossed the measurement boundary'
$report = New-TestReport; $report.framePhases.journalEntriesLost = 1
Assert-Throws { Assert-RawFrameIntegrity $report } 'lost or invalid'
$report = New-TestReport; $report.framePhases.rawFrames.dropped = 1
Assert-Throws { Assert-RawFrameIntegrity $report } 'lost or invalid'
$report = New-TestReport; $report.framePhases.rawFrames.invalid = 1
Assert-Throws { Assert-RawFrameIntegrity $report } 'lost or invalid'
$report = New-TestReport; $report.framePhases.presentedFrameCount = 4
Assert-Throws { Assert-RawFrameIntegrity $report } 'does not match'
$report = New-TestReport; $report.framePhases.journalFenceObserved = $false
Assert-Throws { Assert-RawFrameIntegrity $report } 'publication fence'
$report = New-TestReport; $report.framePhases.journalFenceDrawStartNs = 19900000000
Assert-Throws { Assert-RawFrameIntegrity $report } 'fence timestamps do not follow'
$report = New-TestReport; $report.framePhases.journalFenceDirtyAtNs = 20200000000
Assert-Throws { Assert-RawFrameIntegrity $report } 'fence dirty timestamp is after'
$report = New-TestReport; $report.framePhases.powerTransitions = 1
Assert-Throws { Assert-RawFrameIntegrity $report } 'Power transition'
$report = New-TestReport; $report.framePhases.rawFrames.samples[1].dirtyAtNs = $null
Assert-Throws { Get-StressFrameSummary $report (New-TestFlood) } 'no dirty timestamp'
$report = New-TestReport; $report.framePhases.rawFrames.samples[1].PSObject.Properties.Remove('drawEndNs')
Assert-Throws { Assert-RawFrameIntegrity $report } 'Missing numeric field: drawEndNs'
$report = New-TestReport; $report.framePhases.rawFrames.samples[1].drawEndNs = [double]::NaN
Assert-Throws { Assert-RawFrameIntegrity $report } 'Invalid numeric field: drawEndNs'
$report = New-TestReport; $report.framePhases.rawFrames.samples[1].dirtyAtNs = 1005000000
Assert-Throws { Assert-RawFrameIntegrity $report } 'after draw start'
$report = New-TestReport; $report.framePhases.rawFrames.samples[1].presentEndNs = 1005000000
Assert-Throws { Assert-RawFrameIntegrity $report } 'timestamps are invalid'
$report = New-TestReport; $report.framePhases.rawFrames.samples[1].windowIdsMatch = $false
Assert-Throws { Assert-RawFrameIntegrity $report } 'inconsistent identity'
$report = New-TestReport; $report.framePhases.rawFrames.originUnixMs = $null
Assert-Throws { Assert-RawFrameIntegrity $report } 'Invalid numeric field'
$report = New-TestReport; $report.framePhases.rawFrames.measurementEndUnixMs = 1020100
Assert-Throws { Assert-RawFrameIntegrity $report } 'Wall-clock and monotonic'
$report = New-TestReport; $report.actualMeasurementDurationMs = 19999
Assert-Throws { Assert-RawFrameIntegrity $report } 'incomplete measurement window'
$data = New-TestFlood; $data.started_unix_ms = 999999
Assert-Throws { Get-StressFrameSummary (New-TestReport) $data } 'not captured'
$data = New-TestFlood; $data.finished_unix_ms = 1020000
Assert-Throws { Get-StressFrameSummary (New-TestReport) $data } 'not captured'

$native = New-FakeProcess $true $false $false
$cleanup = Close-PerformanceProcess $native $true
Assert-True ($native.JobTerminateCalled -and $native.DisposeCalled -and !$cleanup.reclaimed -and $cleanup.errors.Count -ge 2) 'Job query failures must not skip termination or Dispose.'
$native = New-FakeProcess $false $true $false
$cleanup = Close-PerformanceProcess $native $true
Assert-True ($native.JobTerminateCalled -and $native.ProcessTerminateCalled -and $native.DisposeCalled -and !$cleanup.reclaimed -and ($cleanup.errors -join '; ').Contains('Job termination failed')) 'Job termination failure must fall back and remain visible.'
$native = New-FakeProcess $false $false $false
$native | Add-Member -Force ScriptMethod WaitForExit { param($milliseconds) throw 'fake wait error' }
$native | Add-Member -Force ScriptMethod Terminate { param($code) $this.ProcessTerminateCalled = $true; throw 'fake process termination error' }
$cleanup = Close-PerformanceProcess $native $true
Assert-True ($native.ProcessTerminateCalled -and $native.DisposeCalled -and !$cleanup.reclaimed -and ($cleanup.errors -join '; ').Contains('Process termination failed')) 'Wait and termination errors must not suppress Dispose or be reported as reclaimed.'
$native = New-FakeProcess $false $false $true
$cleanup = Close-PerformanceProcess $native $true
Assert-True ($native.DisposeCalled -and !$cleanup.reclaimed -and ($cleanup.errors -join '; ').Contains('Dispose failed')) 'Dispose failure must not be reported as reclaimed.'
$native = New-FakeProcess $false $false $false
$cleanup = Close-PerformanceProcess $native $true
Assert-True ($native.DisposeCalled -and $cleanup.reclaimed -and $cleanup.errors.Count -eq 0) 'Confirmed termination and Dispose should be reported honestly.'
function Invoke-SyntheticAssessment {
    # 在内存中执行正式洪泛门禁，替换写盘操作以验证失败不会生成成功结果。
    # @author kongweiguang
    param([scriptblock]$Block, $FrameReport, $Flood)
    $Expected = 3
    $case = 'C:\synthetic-performance-no-write'
    $result = [ordered]@{ throughput = $null; stressFrameSummary = $null; hostListSummary = [pscustomobject]@{ finishedUnixMs = 1000900 } }
    $workload = [pscustomobject]@{ data = [pscustomobject]@{ renderer = [pscustomobject]@{ data = $Flood } } }
    function Set-Content {
        # 捕获管道结果而不触碰任何文件，专用于该内存内回归。
        # @author kongweiguang
        param([string]$Encoding, [Parameter(Position = 0)][string]$Path, [Parameter(ValueFromPipeline)]$Value)
        process { }
    }
    try { . $Block; return [pscustomobject]@{ failure = $null; result = $result } }
    catch { return [pscustomobject]@{ failure = $_.Exception.Message; result = $result } }
}

$assessmentNodes = @($case[0].FindAll({ param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.Contains('$result.throughput = Get-FloodThroughput $data') }, $true))
Assert-True ($assessmentNodes.Count -eq 1) 'Expected exactly one production flood assessment block.'
$assessmentBlock = [scriptblock]::Create($assessmentNodes[0].Extent.Text)
$assessment = Invoke-SyntheticAssessment $assessmentBlock (New-TestReport) (New-TestFlood)
Assert-True ($null -eq $assessment.failure -and $assessment.result.throughput.targetAchieved -and $assessment.result.stressFrameSummary.count -eq 2) 'Valid flood and frames must reach the success path.'
$slow = New-TestFlood; $slow.attempted = 12000; $slow.acknowledged = 12000
$assessment = Invoke-SyntheticAssessment $assessmentBlock (New-TestReport) $slow
Assert-True ($null -ne $assessment.failure -and $assessment.failure.Contains('below 1000') -and !$assessment.result.throughput.targetAchieved -and $null -ne $assessment.result.stressFrameSummary) 'Sub-target ACK throughput must fail while preserving available diagnostics.'
$assessment = Invoke-SyntheticAssessment $assessmentBlock $crossing (New-TestFlood)
Assert-True ($null -ne $assessment.failure -and $assessment.failure.Contains('crossed the measurement boundary')) 'A later same-window fence must not conceal a crossing slow frame.'
$broken = New-TestReport; $broken.framePhases.journalEntriesLost = 1
$assessment = Invoke-SyntheticAssessment $assessmentBlock $broken (New-TestFlood)
Assert-True ($null -ne $assessment.failure -and $assessment.failure.Contains('lost or invalid')) 'Lost journal entries must make the production assessment fail.'
Write-Output 'plugin-performance synthetic tests passed'
