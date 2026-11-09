# @author kongweiguang
param(
    [Parameter(Mandatory = $true)][string]$ExecutablePath,
    [ValidateRange(800, 3840)][int]$Width = 1440,
    [ValidateRange(600, 2160)][int]$Height = 900,
    [ValidateRange(30, 600)][int]$TimeoutSeconds = 180
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-PluginDesktopLauncherSource {
    # 复用已审查的私有桌面进程管理，仅替换应用入口和类型命名空间。
    # @author kongweiguang
    $tokens = $null
    $errors = $null
    $path = Join-Path $PSScriptRoot 'settings-parity.ps1'
    $syntax = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw 'Cannot parse the private desktop launcher.' }
    $assignments = @($syntax.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            $node.Left.Extent.Text -ceq '$script:NativeDesktopLauncherSource'
    }, $true))
    if ($assignments.Count -ne 1 -or
        $assignments[0].Right.Expression -isnot [System.Management.Automation.Language.StringConstantExpressionAst]) {
        throw 'The private desktop launcher source is missing or ambiguous.'
    }
    $source = $assignments[0].Right.Expression.Value
    $old = '+ " --settings-acceptance --output-dir " + QuoteArgument(outputDirectory)'
    $new = '+ " --plugin-acceptance --phase " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_PLUGIN_ACCEPTANCE_PHASE")) + " --output-dir " + QuoteArgument(outputDirectory)'
    $first = $source.IndexOf($old, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $source.IndexOf($old, $first + $old.Length, [StringComparison]::Ordinal) -ge 0) {
        throw 'The launcher command changed; refusing an unverified replacement.'
    }
    return $source.Replace($old, $new).Replace('namespace Kerminal.SettingsParity', 'namespace Kerminal.PluginAcceptance')
}

function Stop-OwnedPluginProcess {
    # 仅终止本轮持有的进程，并等待私有桌面资源归还。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)]$NativeProcess)
    $process = $null
    try { $process = [Diagnostics.Process]::GetProcessById($NativeProcess.ProcessId) }
    catch [ArgumentException] { $process = $null }
    if ($null -ne $process) {
        try { if (-not $process.HasExited) { $process.Kill($true) } }
        finally { $process.Dispose() }
    }
    if (-not $NativeProcess.WaitForExit(10000)) {
        $NativeProcess.Terminate(1)
        if (-not $NativeProcess.WaitForExit(10000)) { throw 'The owned acceptance process did not exit.' }
    }
}

function Assert-PluginCaptureSize {
    # 逐张核对 PNG 的实际像素尺寸，避免逻辑报告正确而截图仍来自启动最大化窗口。
    # @author kongweiguang
    param([string]$Path, [uint32]$ExpectedWidth, [uint32]$ExpectedHeight)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 24 -or [Convert]::ToHexString([byte[]]$bytes[0..7]) -cne '89504E470D0A1A0A') {
        throw "Invalid PNG capture: $Path"
    }
    $actualWidth = [uint32]$bytes[16] * 16777216 + [uint32]$bytes[17] * 65536 + [uint32]$bytes[18] * 256 + [uint32]$bytes[19]
    $actualHeight = [uint32]$bytes[20] * 16777216 + [uint32]$bytes[21] * 65536 + [uint32]$bytes[22] * 256 + [uint32]$bytes[23]
    if ($actualWidth -ne $ExpectedWidth -or $actualHeight -ne $ExpectedHeight) {
        throw "Capture size mismatch: ${Path}: expected=${ExpectedWidth}x${ExpectedHeight}, actual=${actualWidth}x${actualHeight}"
    }
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$executable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw "Executable not found: $executable" }
$candidateHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$base = Join-Path $repositoryRoot 'target\acceptance\plugin'
[void][IO.Directory]::CreateDirectory($base)
$runId = [Guid]::NewGuid().ToString('N')
$output = Join-Path $base $runId
$previousPhase = [Environment]::GetEnvironmentVariable('KERMINAL_PLUGIN_ACCEPTANCE_PHASE', 'Process')
$nativeProcess = $null
$processIds = @()
$prepareHostId = $null
try {
    if (-not ('Kerminal.PluginAcceptance.NativeDesktopLauncher' -as [type])) {
        Add-Type -TypeDefinition (Get-PluginDesktopLauncherSource) -Language CSharp
    }
    foreach ($phase in @('prepare', 'verify')) {
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_ACCEPTANCE_PHASE', $phase, 'Process')
        $desktopName = 'KerminalRail-settings-{0}' -f [Guid]::NewGuid().ToString('N')
        $nativeProcess = [Kerminal.PluginAcceptance.NativeDesktopLauncher]::Start(
            $executable, $repositoryRoot, $output, (Join-Path $base "$runId-$phase-stdout.log"),
            (Join-Path $base "$runId-$phase-stderr.log"), $desktopName, [uint32]$Width, [uint32]$Height)
        if (-not $nativeProcess.WaitForExit([uint32]($TimeoutSeconds * 1000))) {
            Stop-OwnedPluginProcess -NativeProcess $nativeProcess
            throw "Plugin acceptance $phase timed out: $output"
        }
        $exitCode = $nativeProcess.GetExitCode()
        $processId = $nativeProcess.ProcessId
        $nativeProcess.Dispose()
        if (-not $nativeProcess.DesktopClosed) { throw 'The private desktop was not closed.' }
        $nativeProcess = $null
        if ($exitCode -ne 0) { throw "Plugin acceptance $phase failed with exit code $exitCode : $output" }
        if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidateHash) {
            throw 'The candidate executable changed during acceptance.'
        }
        $report = Get-Content -LiteralPath (Join-Path $output "$phase-report.json") -Raw | ConvertFrom-Json
        if ($report.requestedViewport[0] -ne $Width -or $report.requestedViewport[1] -ne $Height -or
            [Math]::Abs($report.viewport[0] - $Width) -gt 0.5 -or
            [Math]::Abs($report.viewport[1] - $Height) -gt 0.5 -or $report.scaleFactor -le 0) {
            throw 'The accepted viewport did not match the requested window size.'
        }
        $captureWidth = [uint32][Math]::Round($Width * $report.scaleFactor)
        $captureHeight = [uint32][Math]::Round($Height * $report.scaleFactor)
        foreach ($captureFile in Get-ChildItem -LiteralPath $output -Filter '*.png' -File) {
            Assert-PluginCaptureSize -Path $captureFile.FullName -ExpectedWidth $captureWidth -ExpectedHeight $captureHeight
        }
        if ($report.phase -cne $phase -or $report.processId -ne $processId -or
            $report.candidateSha256 -cne $candidateHash -or -not $report.nativeIsolation.isolationProven -or
            $report.nativeIsolation.uiThreadDesktopName -cne $desktopName -or
            $report.nativeIsolation.processId -ne $processId -or
            [string]::IsNullOrWhiteSpace($report.hostId) -or
            $report.hostConfigurationText -cne 'host-edited-plugin-value' -or
            -not (Test-Path -LiteralPath (Join-Path $output $report.capture) -PathType Leaf)) {
            throw "Plugin acceptance $phase evidence did not match the launched candidate and private desktop."
        }
        if ($phase -ceq 'prepare') {
            if (-not $report.hostFormCreated -or -not $report.hostFormEdited) {
                throw 'The preparation report did not verify host creation and editing through the UI.'
            }
            $prepareHostId = $report.hostId
        } elseif ($report.hostId -cne $prepareHostId) {
            throw 'The restarted process read a different plugin host.'
        }
        $processIds += $processId
    }
    if ($processIds[0] -eq $processIds[1]) { throw 'Restart verification did not use a new process.' }
    Write-Output "Plugin acceptance evidence: $output"
}
finally {
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_ACCEPTANCE_PHASE', $previousPhase, 'Process')
    if ($null -ne $nativeProcess) {
        try { Stop-OwnedPluginProcess -NativeProcess $nativeProcess }
        finally { $nativeProcess.Dispose() }
    }
}
