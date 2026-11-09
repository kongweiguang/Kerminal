# @author kongweiguang
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ExecutablePath,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [ValidateRange(60, 600)][int]$TimeoutSeconds = 90
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-NativeListDesktopLauncherSource {
    # 复用已审查的私有桌面和进程句柄管理；只替换固定入口及隔离配置根。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$SettingsParityPath)
    $tokens = $null
    $errors = $null
    $syntax = [System.Management.Automation.Language.Parser]::ParseFile(
        $SettingsParityPath, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw '无法解析私有桌面 launcher 源文件。' }
    $assignments = @($syntax.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            $node.Left.Extent.Text -ceq '$script:NativeDesktopLauncherSource'
    }, $true))
    if ($assignments.Count -ne 1 -or
        $assignments[0].Right.Expression -isnot [System.Management.Automation.Language.StringConstantExpressionAst]) {
        throw '私有桌面 launcher 源缺失或不唯一。'
    }
    $source = $assignments[0].Right.Expression.Value
    $oldEnvironment = 'variables["GIT_CONFIG_GLOBAL"] = System.IO.Path.Combine(outputDirectory, "config", ".acceptance-sync", "global.gitconfig");'
    $replacements = @(
        @{
            Old = '+ " --settings-acceptance --output-dir " + QuoteArgument(outputDirectory)'
            New = '+ " --native-list-probe --output " + QuoteArgument(System.IO.Path.Combine(outputDirectory, "report.json"))'
        },
        @{
            Old = '+ " --width " + width.ToString(CultureInfo.InvariantCulture)'
            New = ''
        },
        @{
            Old = '+ " --height " + height.ToString(CultureInfo.InvariantCulture)'
            New = ''
        },
        @{
            Old = $oldEnvironment
            New = ('variables["KERMINAL_CONFIG_ROOT"] = System.IO.Path.Combine(outputDirectory, "config");' +
                [Environment]::NewLine + '            ' + $oldEnvironment)
        },
        @{
            Old = 'namespace Kerminal.SettingsParity'
            New = 'namespace Kerminal.NativeListProbe'
        }
    )
    foreach ($replacement in $replacements) {
        $old = [string]$replacement.Old
        $first = $source.IndexOf($old, [StringComparison]::Ordinal)
        if ($first -lt 0 -or $source.IndexOf($old, $first + $old.Length, [StringComparison]::Ordinal) -ge 0) {
            throw '私有桌面 launcher 结构已变化，拒绝未经核对的替换。'
        }
        $source = $source.Replace($old, [string]$replacement.New)
    }
    return $source
}

function Stop-OwnedNativeListProbe {
    # 超时时仅结束本次持有的进程树；精确句柄仍负责等待和最后兜底。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)]$NativeProcess)
    if ($NativeProcess.WaitForExit(0)) { return }
    $process = $null
    try {
        try { $process = [Diagnostics.Process]::GetProcessById($NativeProcess.ProcessId) }
        catch [ArgumentException] { $process = $null }
        if ($null -ne $process) {
            # 先取得进程句柄，再确认原始句柄仍存活，防止 PID 被复用后误终止其他进程。
            [void]$process.Handle
            if (-not $NativeProcess.WaitForExit(0) -and -not $process.HasExited) {
                $process.Kill($true)
            }
        }
    }
    finally {
        if ($null -ne $process) { $process.Dispose() }
    }
    if (-not $NativeProcess.WaitForExit(10000)) {
        $NativeProcess.Terminate(1)
        if (-not $NativeProcess.WaitForExit(10000)) { throw '原生列表探针终止后仍未退出。' }
    }
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$executable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw "找不到候选 executable：$executable" }
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $output) { throw "验收输出目录已存在：$output" }
[void](New-Item -ItemType Directory -Path $output -ErrorAction Stop)
$beforeHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$stdoutPath = Join-Path $output 'stdout.log'
$stderrPath = Join-Path $output 'stderr.log'
$reportPath = Join-Path $output 'report.json'
$desktopName = 'KerminalRail-settings-list-{0}' -f [Guid]::NewGuid().ToString('N')
$nativeProcess = $null
try {
    if (-not ('Kerminal.NativeListProbe.NativeDesktopLauncher' -as [type])) {
        $launcherSource = Get-NativeListDesktopLauncherSource -SettingsParityPath (Join-Path $PSScriptRoot 'settings-parity.ps1')
        Add-Type -TypeDefinition $launcherSource -Language CSharp
    }
    # 宽高是现有 launcher 签名的兼容参数；探针报告记录窗口实际尺寸与 DPI。
    $nativeProcess = [Kerminal.NativeListProbe.NativeDesktopLauncher]::Start(
        $executable, $repositoryRoot, $output, $stdoutPath, $stderrPath,
        $desktopName, [uint32]1000, [uint32]720)
    $timedOut = -not $nativeProcess.WaitForExit([uint32]($TimeoutSeconds * 1000))
    if ($timedOut) { Stop-OwnedNativeListProbe -NativeProcess $nativeProcess }
    $exitCode = $nativeProcess.GetExitCode()
    $processId = $nativeProcess.ProcessId
    $nativeProcess.Dispose()
    $desktopClosed = $nativeProcess.DesktopClosed
    $nativeProcess = $null

    $afterHash = $null
    $hashError = $null
    try { $afterHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() }
    catch { $hashError = $_.Exception.Message }
    $processResult = [ordered]@{
        process_id = $processId
        exit_code = $exitCode
        timed_out = $timedOut
        desktop_name = $desktopName
        desktop_closed = $desktopClosed
        candidate_sha256_before = $beforeHash
        candidate_sha256_after = $afterHash
        candidate_hash_error = $hashError
        isolated_config_root = (Join-Path $output 'config')
        report = $reportPath
        stdout = $stdoutPath
        stderr = $stderrPath
    }
    $processResult | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $output 'process-result.json') -Encoding utf8NoBOM
    if (-not $desktopClosed) { throw "私有桌面没有关闭；输出目录：$output" }
    if ($null -ne $hashError -or $beforeHash -cne $afterHash) {
        throw "候选 executable 在验收后无法确认或已发生变化；输出目录：$output"
    }
    if ($timedOut) { throw "原生列表探针超时；保留报告（如有）及 stdout/stderr：$output" }
    if ($exitCode -ne 0) {
        throw "原生列表探针退出码为 $exitCode；保留报告（如有）及 stdout/stderr：$output"
    }
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { throw "原生列表探针报告缺失：$reportPath" }
    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    if ($report.schema -ne 1 -or $report.verdict -cne 'passed' -or $report.logical_rows -ne 100000 -or
        $report.build_and_hardware.executable_sha256 -cne $beforeHash -or
        @($report.raw_frames).Count -lt 120) {
        throw "原生列表探针报告与候选进程或原始帧不符：$reportPath"
    }
    Write-Output "Native list probe raw report: $reportPath"
    Write-Output "Process and private desktop evidence: $(Join-Path $output 'process-result.json')"
}
finally {
    if ($null -ne $nativeProcess) {
        try { Stop-OwnedNativeListProbe -NativeProcess $nativeProcess }
        finally {
            $nativeProcess.Dispose()
            if (-not $nativeProcess.DesktopClosed) { throw '原生列表探针的私有桌面没有关闭。' }
        }
    }
}
