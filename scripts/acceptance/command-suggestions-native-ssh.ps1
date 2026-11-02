# @author kongweiguang
param(
    [Parameter(Mandatory = $true)][string]$ExecutablePath,
    [ValidateRange(800, 3840)][int]$Width = 1440,
    [ValidateRange(600, 2160)][int]$Height = 900,
    [ValidateRange(30, 600)][int]$TimeoutSeconds = 360
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Convert-ToWslPath {
    # 由 WSL 自己转换绝对路径，避免盘符或中文目录被手工拼接误解。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path)
    $converted = & wsl.exe --user root --exec wslpath -a $Path
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($converted)) {
        throw 'Cannot convert the isolated fixture path for WSL.'
    }
    return ([string]$converted).Trim()
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$fixtureParent = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target\diagnostics\command-suggestions-ssh-fixtures'))
[void][IO.Directory]::CreateDirectory($fixtureParent)
$fixtureRoot = Join-Path $fixtureParent ([Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $fixtureRoot) {
    throw 'Refusing to reuse an existing SSH fixture directory.'
}
[void][IO.Directory]::CreateDirectory($fixtureRoot)
$fixtureScript = Join-Path $PSScriptRoot 'command-suggestions-native-ssh-fixture.py'
$fixtureProcess = $null
$runFailure = $null
$cleanupFailure = $null
$names = @('KERM_ACCEPT_SSH_TARGET', 'KERM_ACCEPT_SSH_USERNAME', 'KERM_ACCEPT_SSH_PRIVATE_KEY_FILE',
    'KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE', 'KERMINAL_SUGGESTIONS_SSH_FIXTURE_ROOT')
$previous = @{}
foreach ($name in $names) { $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }

try {
    $linuxScript = Convert-ToWslPath -Path $fixtureScript
    $linuxRoot = Convert-ToWslPath -Path $fixtureRoot
    if ($linuxScript.Contains('"') -or $linuxRoot.Contains('"')) {
        throw 'The WSL fixture path contains an unsupported quote.'
    }
    # 只启动本次拥有的 WSL 进程，应用窗口本身仍由 settings-parity 放入独立 Win32 desktop。
    $arguments = @('--user', 'root', '--exec', 'python3', ('"{0}"' -f $linuxScript),
        ('"{0}"' -f $linuxRoot), [string]($TimeoutSeconds + 120))
    $fixtureProcess = Start-Process -FilePath 'wsl.exe' -ArgumentList $arguments -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $fixtureRoot 'stdout.log') `
        -RedirectStandardError (Join-Path $fixtureRoot 'stderr.log')
    $readyPath = Join-Path $fixtureRoot 'ready.json'
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath $readyPath -PathType Leaf)) {
        $fixtureProcess.Refresh()
        if ($fixtureProcess.HasExited) { throw "SSH fixture exited before readiness; see $fixtureRoot." }
        if ([DateTime]::UtcNow -ge $deadline) { throw "SSH fixture readiness timed out; see $fixtureRoot." }
        Start-Sleep -Milliseconds 100
    }
    $ready = Get-Content -LiteralPath $readyPath -Raw | ConvertFrom-Json
    if ([int]$ready.port -lt 1 -or [int]$ready.port -gt 65535 -or $ready.username -cne 'root') {
        throw 'SSH fixture readiness metadata is invalid.'
    }
    [Environment]::SetEnvironmentVariable('KERM_ACCEPT_SSH_TARGET', "127.0.0.1:$($ready.port)", 'Process')
    [Environment]::SetEnvironmentVariable('KERM_ACCEPT_SSH_USERNAME', 'root', 'Process')
    [Environment]::SetEnvironmentVariable('KERM_ACCEPT_SSH_PRIVATE_KEY_FILE', (Join-Path $fixtureRoot 'client-key'), 'Process')
    [Environment]::SetEnvironmentVariable('KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE', (Join-Path $fixtureRoot 'known_hosts'), 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_SUGGESTIONS_SSH_FIXTURE_ROOT', $fixtureRoot, 'Process')
    & (Join-Path $PSScriptRoot 'settings-parity.ps1') -ExecutablePath $ExecutablePath -SkipBuild `
        -SuggestionsOnly -SshSuggestions -Width $Width -Height $Height -TimeoutSeconds $TimeoutSeconds
} catch {
    $runFailure = $_
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process') }
    try {
        if ($null -ne $fixtureProcess) {
            [IO.File]::WriteAllText((Join-Path $fixtureRoot 'stop.fixture'), 'stop')
            if (-not $fixtureProcess.WaitForExit(15000)) {
                throw "Owned SSH fixture did not stop; its watchdog remains active. See $fixtureRoot."
            }
            $closedPath = Join-Path $fixtureRoot 'closed.json'
            if ($fixtureProcess.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $closedPath -PathType Leaf) -or
                -not (Get-Content -LiteralPath $closedPath -Raw | ConvertFrom-Json).listenerClosed) {
                throw "SSH fixture did not confirm listener closure; see $fixtureRoot."
            }
        }
        $keyPath = Join-Path $fixtureRoot 'client-key'
        if (Test-Path -LiteralPath $keyPath -PathType Leaf) {
            $item = Get-Item -LiteralPath $keyPath -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'Refusing to remove a linked fixture key.'
            }
            Remove-Item -LiteralPath $keyPath -Force
            if (Test-Path -LiteralPath $keyPath) { throw 'Fixture key removal was not confirmed.' }
        }
    } catch {
        $cleanupFailure = $_
    }
}
if ($null -ne $runFailure) {
    if ($null -ne $cleanupFailure) { throw "SSH acceptance failed: $runFailure; cleanup failed: $cleanupFailure" }
    throw $runFailure
}
if ($null -ne $cleanupFailure) { throw $cleanupFailure }
Write-Output "Isolated SSH listener and temporary client key cleaned: $fixtureRoot"
