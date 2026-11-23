# @author kongweiguang
param(
    [Parameter(Mandatory = $true)][string]$ExecutablePath,
    [ValidateRange(800, 3840)][int]$Width = 1440,
    [ValidateRange(600, 2160)][int]$Height = 900,
    [ValidateRange(30, 600)][int]$TimeoutSeconds = 360,
    # 需要工作区文件与传输闭环时显式扩大同一隔离 fixture；默认跟随专项范围不变。
    [switch]$IncludeTransferChecks,
    [switch]$ZmodemOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($IncludeTransferChecks -and $ZmodemOnly) { throw 'Choose either the SFTP transfer suite or the ZMODEM smoke.' }

function Convert-ToWslPath {
    # WSL 固定输出 UTF-8，不能沿用 Windows 控制台代码页，否则中文仓库路径会损坏；独立管道也避免改动调用者编码。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path)
    $startInfo = [Diagnostics.ProcessStartInfo]::new('wsl.exe')
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $startInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($argument in @('--user', 'root', '--exec', 'wslpath', '-a', $Path)) {
        $startInfo.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) { throw 'Cannot start WSL path conversion.' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $converted = $stdout.GetAwaiter().GetResult()
        $errorOutput = $stderr.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($converted)) {
            throw "Cannot convert the isolated fixture path for WSL: $errorOutput"
        }
        return $converted.Trim()
    } finally {
        $process.Dispose()
    }
}

function Set-ProcessEnvironmentValue {
    # PowerShell 会把传给 .NET string 参数的 $null 绑定为空串；新运行时保留空值，必须显式 null 才能关闭按存在性判定的专项。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Name, [AllowNull()][object]$Value)
    if ($null -eq $Value) {
        [Environment]::SetEnvironmentVariable($Name, [NullString]::Value, 'Process')
    } else {
        [Environment]::SetEnvironmentVariable($Name, [string]$Value, 'Process')
    }
}

function Get-ProcessEnvironmentSnapshot {
    # Windows 环境变量可能不存在、为空或非空；只读值会丢失存在性，导致清理时污染调用方。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string[]]$Names)
    $environment = [Environment]::GetEnvironmentVariables('Process')
    $snapshot = @{}
    foreach ($name in $Names) {
        $snapshot[$name] = @{ Present = $environment.Contains($name); Value = $environment[$name] }
    }
    return $snapshot
}

function Get-PrivateDesktopLauncherSource {
    # 只读取现有 launcher 的常量源码并替换一个入口，沿用其句柄白名单和私有桌面约束。
    # @author kongweiguang
    $tokens = $null
    $errors = $null
    $sourcePath = Join-Path $PSScriptRoot 'settings-parity.ps1'
    $syntax = [System.Management.Automation.Language.Parser]::ParseFile($sourcePath, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw 'Cannot parse the private desktop launcher source.' }
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
    $new = '+ " --right-rail-acceptance --sftp-functional-only --fixture-manifest " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_SFTP_FOLLOW_MANIFEST")) + " --output-dir " + QuoteArgument(outputDirectory)'
    $first = $source.IndexOf($old, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $source.IndexOf($old, $first + $old.Length, [StringComparison]::Ordinal) -ge 0) {
        throw 'The existing launcher command has changed; refusing an unverified replacement.'
    }
    return $source.Replace($old, $new).Replace('namespace Kerminal.SettingsParity', 'namespace Kerminal.SftpFollowAcceptance')
}

function Stop-OwnedNativeProcess {
    # 只清理本次私有桌面返回的进程树，并以原始进程句柄确认退出。
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
        if (-not $NativeProcess.WaitForExit(10000)) {
            throw 'The owned acceptance process did not exit.'
        }
    }
}

function Write-Utf8 {
    # 验收输入统一无 BOM，便于 Rust TOML/JSON 读取并避免依赖控制台编码。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Content)
    [IO.File]::WriteAllText($Path, $Content, [Text.UTF8Encoding]::new($false))
}

function Write-FixtureHostConfigurations {
    # 两个不同 HostId 共用本轮独立 loopback sshd；原生用例从同一远端根向其独立子目录双向复制，不冒称两台服务器。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$ConfigDirectory,
        [Parameter(Mandatory = $true)][int]$Port,
        [Parameter(Mandatory = $true)][string]$RemoteDirectory,
        [Parameter(Mandatory = $true)][string]$KeyPath,
        [Parameter(Mandatory = $true)][string]$LocalPath,
        [switch]$IncludeTransferChecks
    )
    $hosts = @([pscustomobject]@{ Id = 'right-rail-fixture'; Name = 'SFTP follow fixture'; SortOrder = 10 })
    if ($IncludeTransferChecks) {
        $hosts += [pscustomobject]@{ Id = 'transfer-source-fixture'; Name = 'SFTP transfer source fixture'; SortOrder = 20 }
    }
    foreach ($hostDefinition in $hosts) {
        $hostToml = @"
# @author kongweiguang
schema_version = 3
protocol = "ssh"
id = "$($hostDefinition.Id)"
name = "$($hostDefinition.Name)"
host = "127.0.0.1"
port = $Port
username = "root"
auth_type = "key"
credential_ref = "$KeyPath"
tags = ["fixture"]
sort_order = $($hostDefinition.SortOrder)
created_at = "1"
updated_at = "1"

[ssh_options.terminal]
connect_timeout_seconds = 30
keepalive_seconds = 60
working_directory = "$RemoteDirectory"

[ssh_options.sftp]
enabled = true
local_start_directory = "$LocalPath"
remote_start_directory = "$RemoteDirectory"
preserve_mtime = true
follow_source_symlinks = false
concurrency = 2
"@
        $hostPath = Join-Path $ConfigDirectory ("hosts\{0}.toml" -f $hostDefinition.Id)
        Write-Utf8 -Path $hostPath -Content $hostToml
        if (-not (Test-Path -LiteralPath $hostPath -PathType Leaf) -or
            [IO.File]::ReadAllText($hostPath, [Text.Encoding]::UTF8) -cne $hostToml) {
            throw "The isolated fixture host configuration is missing or differs after writing: $($hostDefinition.Id)"
        }
    }
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$executable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw "Executable not found: $executable" }
$executableHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$fixtureParent = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target\diagnostics\sftp-follow-ssh-fixtures'))
[void][IO.Directory]::CreateDirectory($fixtureParent)
$fixtureRoot = Join-Path $fixtureParent ([Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $fixtureRoot) { throw 'Refusing to reuse an existing fixture directory.' }
[void][IO.Directory]::CreateDirectory($fixtureRoot)
$fixtureProcess = $null
$nativeProcess = $null
$runFailure = $null
$cleanupFailures = [Collections.Generic.List[string]]::new()
$environmentNames = @('KERMINAL_RIGHT_RAIL_FOLLOW_ONLY', 'KERMINAL_RIGHT_RAIL_REFRESH_ONLY', 'KERMINAL_RIGHT_RAIL_ZMODEM_ONLY', 'KERMINAL_SFTP_FOLLOW_MANIFEST', 'KERMINAL_CONFIG_ROOT')
$previousEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames

try {
    $fixtureScript = Convert-ToWslPath -Path (Join-Path $PSScriptRoot 'sftp-follow-native-ssh-fixture.py')
    $linuxRoot = Convert-ToWslPath -Path $fixtureRoot
    if ($fixtureScript.Contains('"') -or $linuxRoot.Contains('"')) { throw 'Unsupported quote in fixture path.' }
    $fixtureProcess = Start-Process -FilePath 'wsl.exe' -WindowStyle Hidden -PassThru `
        -ArgumentList @('--user', 'root', '--exec', 'python3', ('"{0}"' -f $fixtureScript),
            ('"{0}"' -f $linuxRoot), [string]($TimeoutSeconds + 120)) `
        -RedirectStandardOutput (Join-Path $fixtureRoot 'fixture-stdout.log') `
        -RedirectStandardError (Join-Path $fixtureRoot 'fixture-stderr.log')
    $readyPath = Join-Path $fixtureRoot 'ready.json'
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath $readyPath -PathType Leaf)) {
        $fixtureProcess.Refresh()
        if ($fixtureProcess.HasExited) { throw "SSH fixture exited before readiness: $fixtureRoot" }
        if ([DateTime]::UtcNow -ge $deadline) { throw "SSH fixture readiness timed out: $fixtureRoot" }
        Start-Sleep -Milliseconds 100
    }
    $ready = Get-Content -LiteralPath $readyPath -Raw | ConvertFrom-Json
    if ([int]$ready.port -lt 1 -or [int]$ready.port -gt 65535 -or $ready.username -cne 'root' -or
        $ready.remoteDirectory -cnotmatch '^/run/kerminal-sftp-follow-[A-Za-z0-9_-]+/follow$') {
        throw 'SSH fixture readiness metadata is invalid.'
    }
    $config = Join-Path $fixtureRoot 'fixture-config'
    $localFiles = Join-Path $fixtureRoot 'local-files'
    [void][IO.Directory]::CreateDirectory((Join-Path $config 'hosts'))
    [void][IO.Directory]::CreateDirectory($localFiles)
    Copy-Item -LiteralPath (Join-Path $fixtureRoot 'known_hosts') -Destination (Join-Path $config 'known_hosts')
    $keyPath = (Join-Path $fixtureRoot 'client-key').Replace('\', '/')
    $localPath = $localFiles.Replace('\', '/')
    if ($keyPath.Contains('"') -or $localPath.Contains('"')) { throw 'Unsupported quote in fixture path.' }
    Write-FixtureHostConfigurations -ConfigDirectory $config -Port ([int]$ready.port) `
        -RemoteDirectory ([string]$ready.remoteDirectory) -KeyPath $keyPath -LocalPath $localPath `
        -IncludeTransferChecks:$IncludeTransferChecks
    $manifestPath = Join-Path $fixtureRoot 'manifest.json'
    $manifest = [ordered]@{
        state = 'ready'
        gpuiConfigRoot = $config
        remoteSftpDirectory = [string]$ready.remoteDirectory
        localFilesRoot = $localFiles
        containerName = 'kerminal-rr-follow-fixture'
        containerTargetPort = 1
    }
    Write-Utf8 -Path $manifestPath -Content ((ConvertTo-Json -InputObject $manifest) + [Environment]::NewLine)
    Set-ProcessEnvironmentValue -Name 'KERMINAL_RIGHT_RAIL_ZMODEM_ONLY' -Value $(if ($ZmodemOnly) { '1' } else { $null })
    Set-ProcessEnvironmentValue -Name 'KERMINAL_RIGHT_RAIL_FOLLOW_ONLY' -Value $(if ($IncludeTransferChecks -or $ZmodemOnly) { $null } else { '1' })
    if ($IncludeTransferChecks) {
        Set-ProcessEnvironmentValue -Name 'KERMINAL_RIGHT_RAIL_REFRESH_ONLY' -Value $null
        $childEnvironment = [Environment]::GetEnvironmentVariables('Process')
        foreach ($name in @('KERMINAL_RIGHT_RAIL_FOLLOW_ONLY', 'KERMINAL_RIGHT_RAIL_REFRESH_ONLY')) {
            if ($childEnvironment.Contains($name)) { throw "The full acceptance child environment still contains $name." }
        }
    }
    Set-ProcessEnvironmentValue -Name 'KERMINAL_SFTP_FOLLOW_MANIFEST' -Value $manifestPath
    # 该根与 fixture-config 和 output 为兄弟目录，既隔离继承配置，也不绕过验收程序的重叠路径 guard。
    Set-ProcessEnvironmentValue -Name 'KERMINAL_CONFIG_ROOT' -Value (Join-Path $fixtureRoot 'environment-config')

    if (-not ('Kerminal.SftpFollowAcceptance.NativeDesktopLauncher' -as [type])) {
        Add-Type -TypeDefinition (Get-PrivateDesktopLauncherSource) -Language CSharp
    }
    $output = Join-Path $fixtureRoot 'output'
    $desktopName = 'KerminalRail-settings-{0}' -f [Guid]::NewGuid().ToString('N')
    $nativeProcess = [Kerminal.SftpFollowAcceptance.NativeDesktopLauncher]::Start(
        $executable, $repositoryRoot, $output, (Join-Path $fixtureRoot 'app-stdout.log'),
        (Join-Path $fixtureRoot 'app-stderr.log'), $desktopName, [uint32]$Width, [uint32]$Height)
    if (-not $nativeProcess.WaitForExit([uint32]($TimeoutSeconds * 1000))) {
        Stop-OwnedNativeProcess -NativeProcess $nativeProcess
        throw "Native acceptance timed out: $fixtureRoot"
    }
    $exitCode = $nativeProcess.GetExitCode()
    $processId = $nativeProcess.ProcessId
    $nativeProcess.Dispose()
    if (-not $nativeProcess.DesktopClosed) { throw 'The private desktop was not closed.' }
    $nativeProcess = $null
    if ($exitCode -ne 0) { throw "Native acceptance failed with exit code $exitCode : $fixtureRoot" }
    if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $executableHash) {
        throw 'The acceptance executable changed during the run.'
    }
    $report = Get-Content -LiteralPath (Join-Path $output 'report.json') -Raw | ConvertFrom-Json
    if ($report.captureMode -cne 'native_sftp_functional_actions_without_theme_capture' -or
        -not $report.nativeIsolation.isolationProven -or $report.nativeIsolation.processId -ne $processId -or
        $report.nativeIsolation.uiThreadDesktopName -cne $desktopName -or
        $report.nativeIsolation.inputDesktopName -ceq $desktopName) {
        throw 'The report does not prove the expected isolated native process and desktop.'
    }
    $expected = if ($ZmodemOnly) {
        @('zmodem-rz-prompt-light', 'zmodem-rz-prompt-dark', 'zmodem-rz-prompt', 'zmodem-rz-cancel', 'zmodem-rz-upload', 'zmodem-sz-prompt', 'zmodem-sz-download', 'zmodem-reconnect-prompt', 'zmodem-reconnect')
    } elseif ($IncludeTransferChecks) {
        @('sftp-file-tabs', 'sftp-file-dirty-close', 'sftp-file-close-cancelled',
          'sftp-file-saved', 'sftp-file-save-conflict', 'sftp-file-reloaded', 'sftp-directory-successor', 'sftp-terminal-successor',
          'sftp-file-workspace-theme-light', 'sftp-file-workspace-theme-dark', 'sftp-file-workspace-theme-system',
          'sftp-workspace-dual-host', 'sftp-workspace-host-tabs',
          'sftp-workspace-theme-light', 'sftp-workspace-theme-dark', 'sftp-workspace-theme-system',
          'sftp-workspace-copy', 'sftp-workspace-drag-file', 'sftp-workspace-drag-directory', 'sftp-workspace-tab-persist')
    } else {
        @('sftp-follow-disabled', 'sftp-follow-enabled', 'sftp-follow-unicode',
          'sftp-follow-rapid', 'sftp-follow-reenabled', 'sftp-follow-keyboard', 'sftp-follow-light', 'sftp-follow-dark', 'sftp-follow-system')
    }
    foreach ($operation in $expected) {
        $operationRecords = @($report.functional | Where-Object { $_.operation -ceq $operation })
        if ($operationRecords.Count -eq 0) { throw "Native acceptance report is missing: $operation" }
        if ($operationRecords.Count -gt 1) { throw "Native acceptance report has duplicate records: $operation" }
    }
    Write-Utf8 -Path (Join-Path $fixtureRoot 'process-result.json') -Content ((ConvertTo-Json -InputObject ([ordered]@{
        processId = $processId; exitCode = $exitCode; desktopName = $desktopName
        privateDesktopClosed = $true; executableSha256 = $executableHash
    })) + [Environment]::NewLine)
} catch {
    $runFailure = $_
} finally {
    foreach ($name in $environmentNames) {
        try {
            $previous = $previousEnvironment[$name]
            Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
        } catch { $cleanupFailures.Add("Environment restoration failed for ${name}: $($_.Exception.Message)") }
    }
    if ($null -ne $nativeProcess) {
        try {
            if (-not $nativeProcess.WaitForExit(0)) { Stop-OwnedNativeProcess -NativeProcess $nativeProcess }
            $nativeProcess.Dispose()
            if (-not $nativeProcess.DesktopClosed) { throw 'The private desktop did not close.' }
        } catch { $cleanupFailures.Add($_.Exception.Message) }
    }
    if ($null -ne $fixtureProcess) {
        try {
            Write-Utf8 -Path (Join-Path $fixtureRoot 'stop.fixture') -Content 'stop'
            if (-not $fixtureProcess.WaitForExit(15000)) {
                throw 'The owned SSH fixture did not exit; its watchdog remains active.'
            }
            $closed = Get-Content -LiteralPath (Join-Path $fixtureRoot 'closed.json') -Raw | ConvertFrom-Json
            if ($fixtureProcess.ExitCode -ne 0 -or -not $closed.listenerClosed -or -not $closed.remoteDirectoryRemoved) {
                throw 'The SSH listener or remote directory cleanup was not confirmed.'
            }
        } catch { $cleanupFailures.Add($_.Exception.Message) }
    }
    try {
        $temporaryKey = Join-Path $fixtureRoot 'client-key'
        if (Test-Path -LiteralPath $temporaryKey -PathType Leaf) {
            $item = Get-Item -LiteralPath $temporaryKey -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Fixture key is a reparse point.' }
            Remove-Item -LiteralPath $temporaryKey -Force
            if (Test-Path -LiteralPath $temporaryKey) { throw 'Fixture key removal was not confirmed.' }
        }
    } catch { $cleanupFailures.Add($_.Exception.Message) }
}
if ($null -ne $runFailure) {
    if ($cleanupFailures.Count -gt 0) { throw "Acceptance failed: $runFailure; cleanup: $($cleanupFailures -join '; ')" }
    throw $runFailure
}
if ($cleanupFailures.Count -gt 0) { throw "Acceptance cleanup failed: $($cleanupFailures -join '; ')" }
Write-Output "SFTP follow native acceptance passed. Evidence: $fixtureRoot"
