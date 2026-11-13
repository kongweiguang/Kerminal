# @author kongweiguang
param(
    [Parameter(Mandatory = $true)][string]$ExecutablePath,
    [string]$PluginPath,
    [string]$OutputDirectory,
    [ValidateSet('light', 'dark', 'system', 'all')][string]$Theme = 'all',
    [ValidateRange(800, 3840)][int]$Width = 1440,
    [ValidateRange(600, 2160)][int]$Height = 900,
    [ValidateRange(30, 600)][int]$TimeoutSeconds = 180
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-ExternalPluginDesktopLauncherSource {
    # 只复用已验证的私有桌面生命周期；唯一入口替换失败时停止，防止误跑其它验收。
    # @author kongweiguang
    $tokens = $null
    $errors = $null
    $syntax = [System.Management.Automation.Language.Parser]::ParseFile(
        (Join-Path $PSScriptRoot 'settings-parity.ps1'), [ref]$tokens, [ref]$errors)
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
    $new = '+ " --external-plugin-acceptance --phase " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_EXTERNAL_ACCEPTANCE_PHASE")) + " --dev-plugin " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_EXTERNAL_ACCEPTANCE_PLUGIN")) + " --theme " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_EXTERNAL_ACCEPTANCE_THEME")) + " --output-dir " + QuoteArgument(outputDirectory)'
    $first = $source.IndexOf($old, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $source.IndexOf($old, $first + $old.Length, [StringComparison]::Ordinal) -ge 0) {
        throw 'The launcher command changed; refusing an unverified replacement.'
    }
    $namespace = 'namespace Kerminal.SettingsParity'
    if ($source.Split($namespace).Count -ne 2) { throw 'The launcher namespace changed.' }
    foreach ($dimension in @('width', 'height')) {
        $oldDimension = '+ " --' + $dimension + ' " + ' + $dimension + '.ToString'
        if ($source.Split($oldDimension).Count -ne 2) { throw "The launcher $dimension option changed." }
        $source = $source.Replace($oldDimension, '+ " --window-' + $dimension + ' " + ' + $dimension + '.ToString')
    }
    return $source.Replace($old, $new).Replace($namespace, 'namespace Kerminal.ExternalPluginAcceptance')
}

function Stop-OwnedExternalPluginProcess {
    # 优先沿进程树回收脚本子进程，最终只终止本轮持有的句柄，避免影响用户实例。
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

function Assert-ExternalPluginCaptureSize {
    # 从 PNG IHDR 回读实际像素，避免仅报告逻辑尺寸而截图仍为错误窗口。
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
if ([string]::IsNullOrWhiteSpace($PluginPath)) { $PluginPath = Join-Path $repositoryRoot 'sdk\examples\acme-notes' }
$plugin = [IO.Path]::GetFullPath($PluginPath)
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw "Executable not found: $executable" }
if (-not (Test-Path -LiteralPath (Join-Path $plugin 'kerminal-plugin.json') -PathType Leaf)) {
    throw "Plugin manifest not found: $plugin"
}
# 源码目录不能直接交给宿主：桌面验收要求清单声明的平台二进制已经存在。
if (Test-Path -LiteralPath (Join-Path $plugin 'Cargo.toml') -PathType Leaf) {
    $pluginCli = Join-Path (Split-Path $executable) 'kerminal-plugin.exe'
    if (-not (Test-Path -LiteralPath $pluginCli -PathType Leaf)) { throw "Plugin CLI not found for source fixture build: $pluginCli" }
    & $pluginCli build --project $plugin --platform windows-x86_64
    if ($LASTEXITCODE -ne 0) { throw "Plugin fixture build failed: $plugin" }
    $plugin = (Resolve-Path -LiteralPath (Join-Path $plugin '.kerminal/build')).Path
}
$candidateHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$runId = [Guid]::NewGuid().ToString('N')
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot "target\acceptance\external-plugin\$runId"
}
$base = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $base) { throw "Acceptance output must be a new directory: $base" }
[void][IO.Directory]::CreateDirectory($base)
$environmentNames = @('KERMINAL_EXTERNAL_ACCEPTANCE_PHASE', 'KERMINAL_EXTERNAL_ACCEPTANCE_PLUGIN', 'KERMINAL_EXTERNAL_ACCEPTANCE_THEME')
$previousEnvironment = @{}
foreach ($name in $environmentNames) { $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$nativeProcess = $null
$themes = if ($Theme -ceq 'all') { @('light', 'dark', 'system') } else { @($Theme) }
$receipts = @()
try {
    if (-not ('Kerminal.ExternalPluginAcceptance.NativeDesktopLauncher' -as [type])) {
        Add-Type -TypeDefinition (Get-ExternalPluginDesktopLauncherSource) -Language CSharp
    }
    [Environment]::SetEnvironmentVariable('KERMINAL_EXTERNAL_ACCEPTANCE_PLUGIN', $plugin, 'Process')
    foreach ($currentTheme in $themes) {
        $output = Join-Path $base $currentTheme
        $processIds = @()
        $pluginHash = $null
        [Environment]::SetEnvironmentVariable('KERMINAL_EXTERNAL_ACCEPTANCE_THEME', $currentTheme, 'Process')
        foreach ($phase in @('prepare', 'verify')) {
            [Environment]::SetEnvironmentVariable('KERMINAL_EXTERNAL_ACCEPTANCE_PHASE', $phase, 'Process')
            $desktopName = 'KerminalRail-settings-{0}' -f [Guid]::NewGuid().ToString('N')
            $nativeProcess = [Kerminal.ExternalPluginAcceptance.NativeDesktopLauncher]::Start(
                $executable, $repositoryRoot, $output, (Join-Path $base "$currentTheme-$phase-stdout.log"),
                (Join-Path $base "$currentTheme-$phase-stderr.log"), $desktopName, [uint32]$Width, [uint32]$Height)
            if (-not $nativeProcess.WaitForExit([uint32]($TimeoutSeconds * 1000))) {
                Stop-OwnedExternalPluginProcess -NativeProcess $nativeProcess
                throw "External plugin acceptance $currentTheme/$phase timed out: $output"
            }
            $exitCode = $nativeProcess.GetExitCode()
            $processId = $nativeProcess.ProcessId
            $nativeProcess.Dispose()
            if (-not $nativeProcess.DesktopClosed) { throw 'The private desktop was not closed.' }
            $nativeProcess = $null
            if ($exitCode -ne 0) { throw "External plugin acceptance $currentTheme/$phase failed with exit code $exitCode : $output" }
            if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidateHash) {
                throw 'The candidate executable changed during acceptance.'
            }
            $report = Get-Content -LiteralPath (Join-Path $output "$phase-report.json") -Raw | ConvertFrom-Json
            if ($report.phase -cne $phase -or $report.theme -cne $currentTheme -or $report.processId -ne $processId -or
                $report.candidateSha256 -cne $candidateHash -or -not $report.shutdownComplete -or
                -not $report.firstFrameMarked -or -not $report.nativeIsolation.isolationProven -or
                $report.nativeIsolation.uiThreadDesktopName -cne $desktopName -or
                $report.nativeIsolation.processId -ne $processId -or
                $report.pluginSha256 -cnotmatch '^[0-9a-f]{64}$' -or
                $report.nativeImeTested -ne $false -or $report.dpiSwitchTested -ne $false -or $report.sleepResumeTested -ne $false) {
                throw "External plugin evidence did not match the candidate, shutdown or private desktop: $currentTheme/$phase"
            }
            if ($phase -ceq 'prepare') { $pluginHash = $report.pluginSha256 }
            elseif ($report.pluginSha256 -cne $pluginHash) { throw 'The plugin changed between preparation and restart.' }
            if ($report.requestedViewport[0] -ne $Width -or $report.requestedViewport[1] -ne $Height -or
                [Math]::Abs($report.viewport[0] - $Width) -gt 0.5 -or
                [Math]::Abs($report.viewport[1] - $Height) -gt 0.5 -or $report.scaleFactor -le 0) {
                throw 'The accepted viewport did not match the requested window size.'
            }
            if (-not (Test-Path -LiteralPath (Join-Path $output $report.capture) -PathType Leaf)) {
                throw 'The final capture is missing.'
            }
            $captureWidth = [uint32][Math]::Round($Width * $report.scaleFactor)
            $captureHeight = [uint32][Math]::Round($Height * $report.scaleFactor)
            foreach ($captureFile in Get-ChildItem -LiteralPath $output -Filter "$phase-*.png" -File) {
                Assert-ExternalPluginCaptureSize -Path $captureFile.FullName -ExpectedWidth $captureWidth -ExpectedHeight $captureHeight
            }
            $processIds += $processId
            $receipts += [pscustomobject]@{ Theme = $currentTheme; Phase = $phase; ProcessId = $processId; Report = "$currentTheme/$phase-report.json" }
        }
        if ($processIds[0] -eq $processIds[1]) { throw 'Restart verification did not use a new process.' }
    }
    [pscustomobject]@{
        SchemaVersion = 1; CandidateSha256 = $candidateHash; PluginDirectory = $plugin; Runs = $receipts
        NativeImeTested = $false; DpiSwitchTested = $false; SleepResumeTested = $false
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $base 'acceptance-report.json') -Encoding utf8
    Write-Output "External plugin acceptance evidence: $base"
}
finally {
    foreach ($name in $environmentNames) { [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], 'Process') }
    if ($null -ne $nativeProcess) {
        try { Stop-OwnedExternalPluginProcess -NativeProcess $nativeProcess }
        finally { $nativeProcess.Dispose() }
    }
}
