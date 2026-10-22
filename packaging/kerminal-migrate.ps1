# @author kongweiguang
[CmdletBinding()]
param(
    [string]$Root,
    [switch]$DryRun,
    [switch]$Apply
)

# 设计约束：包装器以隐藏子进程和管道运行，不依赖交互控制台代码页；JSON 与诊断统一使用无 BOM UTF-8。
$consoleUtf8NoBom = [System.Text.UTF8Encoding]::new($false)
$consoleStdout = [System.IO.StreamWriter]::new(
    [Console]::OpenStandardOutput(),
    $consoleUtf8NoBom
)
$consoleStdout.AutoFlush = $true
$consoleStderr = [System.IO.StreamWriter]::new(
    [Console]::OpenStandardError(),
    $consoleUtf8NoBom
)
$consoleStderr.AutoFlush = $true
[Console]::SetOut($consoleStdout)
[Console]::SetError($consoleStderr)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-WindowsProcessArgument {
    <#
    用 Windows CRT 参数规则引用每个参数，避免带空格、中文、引号或尾部反斜线的根目录被拆分。
    不构造或执行命令字符串；helper 的 FileName 始终是本脚本同目录的固定文件。

    @author kongweiguang
    #>
    param([AllowEmptyString()][string]$Value)

    $builder = [System.Text.StringBuilder]::new()
    [void]$builder.Append([char]34)
    $backslashCount = 0

    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq [char]92) {
            $backslashCount++
            continue
        }

        if ($character -eq [char]34) {
            if ($backslashCount -gt 0) {
                [void]$builder.Append([char]92, (2 * $backslashCount) + 1)
            } else {
                [void]$builder.Append([char]92)
            }
            [void]$builder.Append([char]34)
            $backslashCount = 0
            continue
        }

        if ($backslashCount -gt 0) {
            [void]$builder.Append([char]92, $backslashCount)
            $backslashCount = 0
        }
        [void]$builder.Append($character)
    }

    if ($backslashCount -gt 0) {
        [void]$builder.Append([char]92, 2 * $backslashCount)
    }
    [void]$builder.Append([char]34)
    return $builder.ToString()
}

if ($DryRun.IsPresent -eq $Apply.IsPresent) {
    [Console]::Error.WriteLine('Choose exactly one mode: -DryRun or -Apply.')
    exit 2
}

if ($PSBoundParameters.ContainsKey('Root') -and [string]::IsNullOrWhiteSpace($Root)) {
    [Console]::Error.WriteLine('-Root cannot be empty.')
    exit 2
}

if (-not $PSBoundParameters.ContainsKey('Root')) {
    $userProfile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
    if ([string]::IsNullOrWhiteSpace($userProfile)) {
        [Console]::Error.WriteLine('The current user profile directory is unavailable.')
        exit 2
    }
    $Root = Join-Path -Path $userProfile -ChildPath '.kerminal'
}

$helperPath = Join-Path -Path $PSScriptRoot -ChildPath 'kerminal-migrate.exe'
if (-not (Test-Path -LiteralPath $helperPath -PathType Leaf)) {
    [Console]::Error.WriteLine("Migration helper is missing beside this script: $helperPath")
    exit 127
}
$helperItem = Get-Item -LiteralPath $helperPath -Force
if ($helperItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
    [Console]::Error.WriteLine('Migration helper cannot be a reparse point.')
    exit 126
}

$modeArgument = if ($DryRun.IsPresent) { '--dry-run' } else { '--apply' }
$processArguments = @('--root', $Root, $modeArgument)
$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $helperPath
$startInfo.Arguments = (($processArguments | ForEach-Object {
            ConvertTo-WindowsProcessArgument -Value $_
        }) -join ' ')
$startInfo.WorkingDirectory = (Get-Location).Path
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $true
$startInfo.WindowStyle = [System.Diagnostics.ProcessWindowStyle]::Hidden
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.StandardOutputEncoding = [System.Text.UTF8Encoding]::new($false)
$startInfo.StandardErrorEncoding = [System.Text.UTF8Encoding]::new($false)

$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $startInfo
try {
    if (-not $process.Start()) {
        [Console]::Error.WriteLine('Could not start the migration helper.')
        exit 1
    }
} catch {
    [Console]::Error.WriteLine("Could not start the migration helper: $($_.Exception.Message)")
    $process.Dispose()
    exit 1
}

$stdoutTask = $process.StandardOutput.ReadToEndAsync()
$stderrTask = $process.StandardError.ReadToEndAsync()
$process.WaitForExit()
[Console]::Out.Write($stdoutTask.GetAwaiter().GetResult())
[Console]::Error.Write($stderrTask.GetAwaiter().GetResult())
$exitCode = $process.ExitCode
$process.Dispose()
exit $exitCode
