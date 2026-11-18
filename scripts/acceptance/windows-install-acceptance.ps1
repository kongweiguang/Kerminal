#requires -Version 7.0
# @author kongweiguang
<#
.SYNOPSIS
在一次性 GitHub-hosted Windows runner 上执行真实 NSIS 全新安装、覆盖升级和首次启动验收。

.DESCRIPTION
只用已暂存并校验摘要的安装器运行 /S 静默安装；读取真实 HKCU 卸载项和 kerminal 协议键，
先做新版全新安装并卸载，再安装 v0.3.48 后用新版覆盖升级，最后只启动新版，用短隔离目录和
合成 0.3 settings fixture 检查格式标记、迁移备份、设置读回和首帧，然后回收进程、安装目录、
注册表和隔离数据。

脚本硬性拒绝在 GitHub-hosted 之外执行，避免在本机安装或覆盖真实用户数据。下载内容不进入
可信 harness 检出；安装器必须位于检出之外的独立暂存目录。

.PARAMETER NewInstaller
已校验摘要的新版 NSIS 安装器，位于暂存目录。

.PARAMETER OldInstaller
已校验摘要的旧版 v0.3.48 NSIS 安装器，位于暂存目录。

.PARAMETER WorkRoot
本次 harness 专用的空工作目录；不得与安装器暂存目录混用。

.PARAMETER EvidenceRoot
证据根；脚本写入 install/ 子目录和 summary.md，不覆盖 provenance 记录。

.PARAMETER FixtureSettings
受信任检出的 v0.3.45 settings fixture，用于构造合成 0.3 数据根。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$NewInstaller,

    [Parameter(Mandatory = $true)]
    [string]$OldInstaller,

    [Parameter(Mandatory = $true)]
    [string]$WorkRoot,

    [Parameter(Mandatory = $true)]
    [string]$EvidenceRoot,

    [Parameter(Mandatory = $true)]
    [string]$FixtureSettings,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$ExpectedNewVersion,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$ExpectedOldVersion,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string]$ExpectedNewSha256,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string]$ExpectedOldSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# 真实安装会写 HKCU 和用户目录，只能发生在一次性托管 runner；本机执行可能覆盖真实安装。
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {
    throw '此 harness 会执行真实 NSIS 安装，只允许在一次性 GitHub-hosted Windows runner 上运行。'
}

$script:RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$script:RealLocalAppData = [Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')
$script:RealUserProfile = [Environment]::GetEnvironmentVariable('USERPROFILE', 'Process')
$script:DefaultInstallDir = Join-Path $script:RealLocalAppData 'Kerminal'
$script:ExeName = 'kerminal.exe'
$script:UninstallRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
$script:MachineUninstallRoot = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
$script:ManufacturerKey = 'HKCU:\Software\github\Kerminal'
$script:ProtocolKey = 'HKCU:\Software\Classes\kerminal'
$script:MarkerName = '.kerminal-format.toml'
$script:MigrationDirName = '.kerminal-migration'
$script:ExpectedNewHash = $ExpectedNewSha256.ToLowerInvariant()
$script:ExpectedOldHash = $ExpectedOldSha256.ToLowerInvariant()
$script:EvidenceRoot = [IO.Path]::GetFullPath($EvidenceRoot)
$script:InstallEvidence = Join-Path $script:EvidenceRoot 'install'
$script:CaseRoot = $null
$script:AppProcess = $null
$script:InstalledDirs = [Collections.Generic.List[string]]::new()
$script:CleanupErrors = [Collections.Generic.List[string]]::new()
$script:SavedConfigRoot = [Environment]::GetEnvironmentVariable('KERMINAL_CONFIG_ROOT', 'Process')
# 空字符串与缺失不同：把它当成“未设置”才能避免 finally 再次写回无效覆盖。
if ([string]::IsNullOrEmpty($script:SavedConfigRoot)) { $script:SavedConfigRoot = $null }
$script:Failure = $null

function Write-JsonFile {
    <#
    JSON 统一无 BOM UTF-8；避免 Windows 默认编码破坏后续工具和人工复核。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][object]$Value
    )

    # InputObject 保证空数组也写成 []，失败证据里不会留下无法解析的空 JSON 文件。
    $json = ConvertTo-Json -InputObject $Value -Depth 30
    [IO.File]::WriteAllText($Path, $json + "`n", [Text.UTF8Encoding]::new($false))
}

function Assert-True {
    <#
    把验收条件写成显式失败；任何不满足都立即停止并交给统一清理，不能继续安装或启动。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Assert-NoCleanupErrors {
    <#
    清理错误不能吞掉；显式步骤用它把残留注册表、进程或目录升级为失败。
    @author kongweiguang
    #>
    if ($script:CleanupErrors.Count -gt 0) {
        throw ('清理未完成：' + ($script:CleanupErrors -join '; '))
    }
}

function New-Directory {
    <#
    只创建缺失目录；已存在时保留内容，让调用方决定是否允许复用证据目录。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        [void](New-Item -ItemType Directory -Path $Path -Force)
    }
}

function New-EmptyDirectory {
    <#
    下载、暂存和 harness 工作目录必须来自本次运行；非空目录拒绝复用，避免旧文件混入。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path)

    $full = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $full) {
        $items = @(Get-ChildItem -LiteralPath $full -Force)
        Assert-True ($items.Count -eq 0) "目录必须为空，拒绝复用：$full"
    } else {
        [void](New-Item -ItemType Directory -Path $full -Force)
    }
    return $full
}

function Test-PathUnder {
    <#
    用规范化绝对路径判断嵌套关系，保证安装器不来自可信检出或 harness 工作目录。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root
    )

    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $candidate = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
    $container = [IO.Path]::GetFullPath($Root).TrimEnd($separators)
    return $candidate.StartsWith(
        $container + [IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase
    )
}

function Test-PathEqual {
    <#
    Windows 路径按大小写不敏感比较；先规范化再比较，避免相对/绝对或尾部分隔符造成假失败。
    @author kongweiguang
    #>
    param([string]$Path, [string]$Expected)

    if ([string]::IsNullOrWhiteSpace($Path) -or [string]::IsNullOrWhiteSpace($Expected)) {
        return $false
    }
    try {
        $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
        $left = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
        $right = [IO.Path]::GetFullPath($Expected).TrimEnd($separators)
        return [string]::Equals($left, $right, [StringComparison]::OrdinalIgnoreCase)
    } catch {
        return $false
    }
}

function Get-Sha256Hex {
    <#
    只按文件字节判断主程序和安装器身份，不信任文件名或界面版本。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Limit-Text {
    <#
    安装器日志只保留有界前缀，避免失败证据被无界输出撑爆 artifact。
    @author kongweiguang
    #>
    param([string]$Text, [int]$MaxLength = 65536)

    if ([string]::IsNullOrEmpty($Text)) { return '' }
    if ($Text.Length -le $MaxLength) { return $Text }
    return $Text.Substring(0, $MaxLength) + "`n...[truncated]"
}

function Get-CompletedText {
    <#
    进程退出后读取异步管道；只用于已结束或已终止的进程，避免同步读死锁。
    @author kongweiguang
    #>
    param($Task)

    if ($null -eq $Task) { return '' }
    try { return [string]$Task.GetAwaiter().GetResult() } catch { return '' }
}

function Get-PropertyString {
    <#
    StrictMode 下缺失属性直接访问会抛错；统一返回 null 让调用方按业务含义分类。
    @author kongweiguang
    #>
    param($Object, [string]$Name)

    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return [string]$property.Value
}

function ConvertFrom-QuotedValue {
    <#
    NSIS 把 InstallLocation 写成带引号字符串；只去掉最外层引号，保留路径内部字符。
    @author kongweiguang
    #>
    param([string]$Value)

    if ($null -eq $Value) { return $null }
    $trimmed = $Value.Trim()
    if ($trimmed.Length -ge 2 -and $trimmed.StartsWith('"') -and $trimmed.EndsWith('"')) {
        return $trimmed.Substring(1, $trimmed.Length - 2)
    }
    return $trimmed
}

function Get-ExecutablePathFromCommand {
    <#
    注册表命令可能带引号、参数或图标索引；只取真实可执行文件路径用于归属判断。
    @author kongweiguang
    #>
    param([string]$Command)

    if ([string]::IsNullOrWhiteSpace($Command)) { return $null }
    $trimmed = $Command.Trim()
    if ($trimmed.StartsWith('"')) {
        $end = $trimmed.IndexOf([char]34, 1)
        if ($end -lt 2) { return $null }
        return $trimmed.Substring(1, $end - 1)
    }
    return ($trimmed -split '\s+', 2)[0]
}

function Get-DefaultRegistryValue {
    <#
    读取真实注册表默认值；不存在时返回 null，不能用空字符串冒充已写入。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $key = Get-Item -LiteralPath $Path
    $value = $key.GetValue('')
    if ($null -eq $value) { return $null }
    return [string]$value
}

function Get-RegistryValueNames {
    <#
    空字符串值仍可能是已存在的值项；取值名列表判断 URL Protocol 是否真的注册。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) { return @() }
    $key = Get-Item -LiteralPath $Path
    return @($key.GetValueNames())
}

function Get-InstalledKerminal {
    <#
    只按 DisplayName=Kerminal 从真实 HKCU/HKLM 枚举卸载项；不假设旧版和新版使用同一个键名。
    @author kongweiguang
    #>
    param([ValidateSet('HKCU', 'HKLM')][string]$Hive = 'HKCU')

    $root = if ($Hive -eq 'HKCU') { $script:UninstallRoot } else { $script:MachineUninstallRoot }
    if (-not (Test-Path -LiteralPath $root)) { return @() }

    $results = [Collections.Generic.List[object]]::new()
    foreach ($key in @(Get-ChildItem -LiteralPath $root -ErrorAction SilentlyContinue)) {
        $properties = Get-ItemProperty -LiteralPath $key.PSPath -ErrorAction SilentlyContinue
        if ($null -eq $properties) { continue }
        $displayName = Get-PropertyString -Object $properties -Name 'DisplayName'
        if ($displayName -cne 'Kerminal') { continue }

        $installLocation = Get-PropertyString -Object $properties -Name 'InstallLocation'
        $uninstallString = Get-PropertyString -Object $properties -Name 'UninstallString'
        $displayIcon = Get-PropertyString -Object $properties -Name 'DisplayIcon'
        $results.Add([pscustomobject]@{
                Hive              = $Hive
                KeyPath           = [string]$key.PSPath
                DisplayName       = $displayName
                DisplayVersion    = Get-PropertyString -Object $properties -Name 'DisplayVersion'
                InstallLocation   = ConvertFrom-QuotedValue -Value $installLocation
                UninstallString   = Get-ExecutablePathFromCommand -Command $uninstallString
                DisplayIcon       = Get-ExecutablePathFromCommand -Command $displayIcon
                Publisher         = Get-PropertyString -Object $properties -Name 'Publisher'
                RawInstallLocation = $installLocation
                RawUninstallString = $uninstallString
                RawDisplayIcon     = $displayIcon
            })
    }
    return @($results.ToArray())
}

function Get-KerminalProcessesForPath {
    <#
    只按安装目录归属查找主程序和插件子进程，避免误杀其他安装或其他用户会话中的 Kerminal。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$ExecutablePath)

    $installDirectory = [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($ExecutablePath))
    $found = [Collections.Generic.List[object]]::new()
    foreach ($name in @('kerminal.exe', 'kerminal-plugin.exe')) {
        try {
            $processes = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = '$name'" -ErrorAction Stop)
            foreach ($process in $processes) {
                $path = Get-PropertyString -Object $process -Name 'ExecutablePath'
                if ([string]::IsNullOrWhiteSpace($path)) { continue }
                if (Test-PathEqual -Path ([IO.Path]::GetDirectoryName($path)) -Expected $installDirectory) {
                    $found.Add($process)
                }
            }
        } catch {
            foreach ($process in @(Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($name)) -ErrorAction SilentlyContinue)) {
                try {
                    if (Test-PathEqual -Path ([IO.Path]::GetDirectoryName($process.Path)) -Expected $installDirectory) {
                        $found.Add($process)
                    }
                } catch { }
            }
        }
    }
    return @($found.ToArray())
}

function Stop-KerminalProcessesForPath {
    <#
    安装器和卸载器都要求应用退出；只终止本安装目录下的进程，并等待句柄释放后再动文件。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$ExecutablePath)

    $stopped = 0
    foreach ($process in @(Get-KerminalProcessesForPath -ExecutablePath $ExecutablePath)) {
        $processId = if ($null -ne $process.PSObject.Properties['ProcessId']) { [int]$process.ProcessId } else { [int]$process.Id }
        try {
            # 枚举到终止之间进程可能已自行退出；已不存在时不算回收失败。
            if ($null -eq (Get-Process -Id $processId -ErrorAction SilentlyContinue)) { continue }
            Stop-Process -Id $processId -Force -ErrorAction Stop
            Wait-Process -Id $processId -Timeout 10 -ErrorAction SilentlyContinue
            $stopped++
        } catch {
            $script:CleanupErrors.Add("终止进程 $processId 失败：$($_.Exception.Message)")
        }
    }
    return $stopped
}

function Wait-For {
    <#
    注册表写入、首帧和迁移提交都是异步可观测状态；统一短轮询并在超时报告最后错误。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Description,
        [Parameter(Mandatory = $true)][int]$TimeoutSeconds,
        [Parameter(Mandatory = $true)][scriptblock]$Condition
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $lastError = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            if (& $Condition) { return }
        } catch {
            $lastError = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 250
    }
    $suffix = if ($lastError) { " 最后错误：$lastError" } else { '' }
    throw "等待超时（${TimeoutSeconds}s）：$Description$suffix"
}

function Assert-InstallerFileVersion {
    <#
    NSIS 版本资源是安装器身份的一部分；摘要之外再核对 ProductVersion/FileVersion。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $info = (Get-Item -LiteralPath $Path).VersionInfo
    $pattern = '^' + [regex]::Escape($Version) + '(\.[0-9]+)*$'
    $matches = @(@([string]$info.ProductVersion, [string]$info.FileVersion) | Where-Object { $_ -match $pattern })
    Assert-True ($matches.Count -ge 1) "$Label 版本不是 $Version：ProductVersion=$($info.ProductVersion) FileVersion=$($info.FileVersion)"
}

function Invoke-Installer {
    <#
    只用 /S（NSIS 静默）和当前用户默认安装模式；不传 /D，以实际走默认路径和真实 HKCU 注册表。
    安装器从独立暂存目录运行，工作目录也固定在暂存目录，绝不从下载目录启动。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label,
        [int]$TimeoutSeconds = 300,
        [string]$WorkingDirectory
    )

    $safeLabel = $Label -replace '[^A-Za-z0-9._-]', '-'
    $stdoutPath = Join-Path $script:InstallEvidence "installer-$safeLabel.stdout.log"
    $stderrPath = Join-Path $script:InstallEvidence "installer-$safeLabel.stderr.log"
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Path
    $startInfo.Arguments = '/S'
    # 安装器固定从暂存目录启动；卸载器改在 TEMP 运行，避免卸载器自身的工作目录锁住待删除的安装目录。
    $startInfo.WorkingDirectory = if (-not [string]::IsNullOrWhiteSpace($WorkingDirectory)) {
        $WorkingDirectory
    } else {
        Split-Path -Parent $Path
    }
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $startInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $stdoutTask = $null
    $stderrTask = $null
    try {
        if (-not $process.Start()) { throw "无法启动安装器：$Path" }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            try { $process.Kill($true) } catch { }
            [void]$process.WaitForExit(10000)
            throw "$Label 安装器超时（${TimeoutSeconds}s）：$Path"
        }
        $stdout = Limit-Text (Get-CompletedText $stdoutTask)
        $stderr = Limit-Text (Get-CompletedText $stderrTask)
        [IO.File]::WriteAllText($stdoutPath, $stdout, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText($stderrPath, $stderr, [Text.UTF8Encoding]::new($false))
        if ($process.ExitCode -ne 0) {
            throw "$Label 安装器退出码 $($process.ExitCode)。stderr: $stderr"
        }
    } finally {
        $process.Dispose()
    }
}

function Assert-ProtocolRegistration {
    <#
    协议必须指向本次安装目录的 kerminal.exe，且保留 URL Protocol 和 %1 参数；
    只比较真实 HKCU 值，避免把仅存在键名当成已注册。
    上游 NSIS 模板把描述写成字面量 `URL:${BUNDLEID} protocol`，这是文案缺陷而不是指向错误；
    这里只要求描述非空并把实际值交给报告，功能指向仍严格校验。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$InstallDir)

    $expectedExe = [IO.Path]::GetFullPath((Join-Path $InstallDir $script:ExeName))
    Assert-True (Test-Path -LiteralPath $script:ProtocolKey) "协议键不存在：$script:ProtocolKey"
    $names = @(Get-RegistryValueNames -Path $script:ProtocolKey)
    Assert-True ($names -contains 'URL Protocol') '协议键缺少 URL Protocol 值'
    $description = Get-DefaultRegistryValue -Path $script:ProtocolKey
    Assert-True (-not [string]::IsNullOrWhiteSpace($description)) '协议描述为空'

    $icon = Get-DefaultRegistryValue -Path (Join-Path $script:ProtocolKey 'DefaultIcon')
    Assert-True (-not [string]::IsNullOrWhiteSpace($icon)) 'DefaultIcon 为空'
    $iconExe = Get-ExecutablePathFromCommand -Command $icon
    Assert-True (Test-PathEqual -Path $iconExe -Expected $expectedExe) "DefaultIcon 未指向安装目录：$icon"

    $command = Get-DefaultRegistryValue -Path (Join-Path $script:ProtocolKey 'shell\open\command')
    Assert-True (-not [string]::IsNullOrWhiteSpace($command)) '协议命令为空'
    $commandExe = Get-ExecutablePathFromCommand -Command $command
    Assert-True (Test-PathEqual -Path $commandExe -Expected $expectedExe) "协议命令未指向安装目录：$command"
    Assert-True ($command.TrimStart().StartsWith('"')) "协议命令未引用可执行文件路径：$command"
    Assert-True ($command.TrimEnd().EndsWith('"%1"')) "协议命令缺少 %1 参数：$command"

    return [ordered]@{
        description = $description
        icon        = $icon
        command     = $command
    }
}

function Assert-Installation {
    <#
    统一核对真实 HKCU 版本/路径/卸载器、主程序版本和协议；新版额外核对迁移资源与厂商路径键。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$ExpectedVersion,
        [Parameter(Mandatory = $true)][string]$ExpectedPath,
        [switch]$RequireProtocol,
        [switch]$RequireMigrationResources,
        [switch]$VerifyManufacturerKey
    )

    $entries = @(Get-InstalledKerminal -Hive HKCU)
    Assert-True ($entries.Count -eq 1) "期望恰好一个 HKCU Kerminal 卸载项，实际 $($entries.Count) 个：$(($entries | ForEach-Object { $_.KeyPath }) -join ', ')"
    $entry = $entries[0]
    Assert-True ([string]$entry.DisplayVersion -eq $ExpectedVersion) "DisplayVersion 不是 $ExpectedVersion：$($entry.DisplayVersion)"
    Assert-True (-not [string]::IsNullOrWhiteSpace($entry.InstallLocation)) 'InstallLocation 为空'
    Assert-True (Test-PathEqual -Path $entry.InstallLocation -Expected $ExpectedPath) "InstallLocation 不是 $ExpectedPath：$($entry.InstallLocation)"

    $installDir = [IO.Path]::GetFullPath($ExpectedPath)
    $exe = Join-Path $installDir $script:ExeName
    Assert-True (Test-Path -LiteralPath $exe -PathType Leaf) "主程序不存在：$exe"
    # Rust 主程序只嵌入图标资源、没有 VERSIONINFO，空版本是当前产品的正常状态；
    # 只有存在版本资源时才对版本号加锁，版本身份主要由已验摘要的安装器和 HKCU DisplayVersion 证明。
    $info = (Get-Item -LiteralPath $exe).VersionInfo
    $productVersion = [string]$info.ProductVersion
    $fileVersion = [string]$info.FileVersion
    if (-not [string]::IsNullOrWhiteSpace($productVersion) -or -not [string]::IsNullOrWhiteSpace($fileVersion)) {
        $versionPattern = '^' + [regex]::Escape($ExpectedVersion) + '(\.[0-9]+)*$'
        Assert-True ($productVersion -match $versionPattern -or $fileVersion -match $versionPattern) "主程序版本不是 $ExpectedVersion：ProductVersion=$productVersion FileVersion=$fileVersion"
    }

    $uninstaller = Join-Path $installDir 'uninstall.exe'
    Assert-True (Test-Path -LiteralPath $uninstaller -PathType Leaf) "卸载器不存在：$uninstaller"
    Assert-True (Test-PathEqual -Path $entry.UninstallString -Expected $uninstaller) "UninstallString 不是 $uninstaller：$($entry.RawUninstallString)"
    Assert-True (Test-PathEqual -Path $entry.DisplayIcon -Expected $exe) "DisplayIcon 未指向主程序：$($entry.RawDisplayIcon)"

    if ($RequireMigrationResources) {
        foreach ($relative in @(
                'migration\kerminal-migrate.exe',
                'migration\kerminal-migrate.ps1',
                'migration\kerminal-migrate.sh',
                'licenses\LICENSE',
                'licenses\THIRD_PARTY_NOTICES.md',
                'licenses\THIRD_PARTY_DEPENDENCIES.html',
                'licenses\GPUI_KIT_LUCIDE_LICENSE.txt'
            )) {
            Assert-True (Test-Path -LiteralPath (Join-Path $installDir $relative) -PathType Leaf) "安装缺少资源：$relative"
        }
    }

    if ($RequireProtocol) {
        $protocol = Assert-ProtocolRegistration -InstallDir $installDir
        # 把实际协议文案和命令保留在报告里；描述受上游模板字面量影响，不能只凭 summary 的“已验证”概括。
        Add-Member -InputObject $entry -NotePropertyName ProtocolDescription -NotePropertyValue $protocol.description -Force
        Add-Member -InputObject $entry -NotePropertyName ProtocolCommand -NotePropertyValue $protocol.command -Force
    }

    if ($VerifyManufacturerKey) {
        $manufacturer = Get-DefaultRegistryValue -Path $script:ManufacturerKey
        Assert-True (Test-PathEqual -Path $manufacturer -Expected $installDir) "厂商安装目录键不是 $installDir：$manufacturer"
    }

    $machineEntries = @(Get-InstalledKerminal -Hive HKLM)
    # 严格模式下不能对空数组做成员枚举；正常的 currentUser 安装没有 HKLM 卸载项。
    Assert-True ($machineEntries.Count -eq 0) "currentUser 安装不应写 HKLM 卸载项：$(($machineEntries | ForEach-Object { $_.KeyPath }) -join ', ')"

    return $entry
}

function Assert-CleanRunner {
    <#
    全新安装和卸载后都要求真实 HKCU/HKLM、默认目录、协议、固定快捷方式和正式数据根保持干净；
    正式数据根存在时拒绝继续，避免安装预检或迁移触碰真实用户数据。
    @author kongweiguang
    #>
    param([switch]$RequireFormalRootAbsent)

    foreach ($hive in @('HKCU', 'HKLM')) {
        $entries = @(Get-InstalledKerminal -Hive $hive)
        # 空列表正是预检成功条件；只在失败时构建属性诊断，避免严格模式拒绝空列表。
        if ($entries.Count -gt 0) {
            throw "${hive} 仍有 Kerminal 卸载项：$($entries.KeyPath -join ', ')"
        }
    }
    Assert-True (-not (Test-Path -LiteralPath $script:DefaultInstallDir)) "默认安装目录仍存在：$script:DefaultInstallDir"
    Assert-True (-not (Test-Path -LiteralPath $script:ProtocolKey)) "协议键仍存在：$script:ProtocolKey"
    Assert-True (-not (Test-Path -LiteralPath $script:ManufacturerKey)) "厂商键仍存在：$script:ManufacturerKey"
    # 一个干净 runner 不应预置这三个固定快捷方式；安装/卸载后也必须回到不存在状态。
    foreach ($shortcut in @(Get-KerminalShortcutPaths)) {
        Assert-True (-not (Test-Path -LiteralPath $shortcut)) "快捷方式仍存在：$shortcut"
    }
    Assert-True (@(Get-KerminalProcessesForPath -ExecutablePath (Join-Path $script:DefaultInstallDir $script:ExeName)).Count -eq 0) '仍有 Kerminal 进程未回收'

    if ($RequireFormalRootAbsent) {
        $formalRoot = Join-Path $script:RealUserProfile '.kerminal'
        Assert-True (-not (Test-Path -LiteralPath $formalRoot)) "正式数据根存在，拒绝在真实数据上继续：$formalRoot"
    }
}

function Remove-DirectoryWithRetry {
    <#
    Windows 进程退出后句柄可能短暂滞留；有限重试避免把正常的回收延迟当成残留。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path, [int]$Attempts = 8)

    if (-not (Test-Path -LiteralPath $Path)) { return }
    for ($attempt = 0; $attempt -lt $Attempts; $attempt++) {
        try {
            Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop
            return
        } catch {
            Start-Sleep -Milliseconds 500
        }
    }
    Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop
}

function Get-KerminalShortcutPaths {
    <#
    安装器固定写桌面和开始菜单三处快捷方式；预检、清理和回读共用一张表，避免路径漂移。
    @author kongweiguang
    #>
    $programs = [Environment]::GetFolderPath('Programs')
    return @(
        (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Kerminal.lnk'),
        (Join-Path $programs 'Kerminal.lnk'),
        (Join-Path (Join-Path $programs 'Kerminal') 'Kerminal.lnk')
    )
}

function Get-ShortcutTargetPath {
    <#
    只读取 .lnk 的真实目标用于归属判断；缺失、损坏或无法解析时返回 null，绝不凭文件名删除。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $shell = $null
    try {
        $shell = New-Object -ComObject WScript.Shell
        $target = [string]$shell.CreateShortcut($Path).TargetPath
        if ([string]::IsNullOrWhiteSpace($target)) { return $null }
        return $target
    } catch {
        return $null
    } finally {
        if ($null -ne $shell) { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($shell) }
    }
}

function Remove-Installation {
    <#
    卸载器之外再做按归属的防御性清理：只有注册表值仍指向本次安装目录时才删除；
    同时回收进程、快捷方式和残留目录，确保下一次场景从干净 HKCU 开始。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ([string]::IsNullOrWhiteSpace($InstallDir)) { return }
    $installDirFull = [IO.Path]::GetFullPath($InstallDir)
    $exe = Join-Path $installDirFull $script:ExeName
    $uninstaller = Join-Path $installDirFull 'uninstall.exe'

    [void](Stop-KerminalProcessesForPath -ExecutablePath $exe)
    if (Test-Path -LiteralPath $uninstaller -PathType Leaf) {
        try {
            Invoke-Installer -Path $uninstaller -Label $Label -TimeoutSeconds 180 -WorkingDirectory $env:TEMP
        } catch {
            $script:CleanupErrors.Add("$Label 卸载器：$($_.Exception.Message)")
        }
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (-not (Test-Path -LiteralPath $exe) -and @(Get-InstalledKerminal -Hive HKCU).Count -eq 0) { break }
        Start-Sleep -Milliseconds 500
    }

    foreach ($entry in @(Get-InstalledKerminal -Hive HKCU)) {
        if (Test-PathEqual -Path $entry.InstallLocation -Expected $installDirFull) {
            try {
                Remove-Item -LiteralPath $entry.KeyPath -Recurse -Force -ErrorAction Stop
            } catch {
                $script:CleanupErrors.Add("$Label 删除卸载项失败：$($_.Exception.Message)")
            }
        }
    }

    $manufacturer = Get-DefaultRegistryValue -Path $script:ManufacturerKey
    if (Test-PathEqual -Path $manufacturer -Expected $installDirFull) {
        try {
            Remove-Item -LiteralPath $script:ManufacturerKey -Recurse -Force -ErrorAction Stop
        } catch {
            $script:CleanupErrors.Add("$Label 删除厂商键失败：$($_.Exception.Message)")
        }
    }

    $command = Get-DefaultRegistryValue -Path (Join-Path $script:ProtocolKey 'shell\open\command')
    if (Test-PathEqual -Path (Get-ExecutablePathFromCommand -Command $command) -Expected $exe) {
        try {
            Remove-Item -LiteralPath $script:ProtocolKey -Recurse -Force -ErrorAction Stop
        } catch {
            $script:CleanupErrors.Add("$Label 删除协议键失败：$($_.Exception.Message)")
        }
    }

    foreach ($shortcut in @(Get-KerminalShortcutPaths)) {
        if (-not (Test-Path -LiteralPath $shortcut)) { continue }
        $target = Get-ShortcutTargetPath -Path $shortcut
        # 只删除指向本次安装目录的快捷方式；clean runner 预检已保证同名项不是用户已有环境。
        if ([string]::IsNullOrWhiteSpace($target) -or -not (Test-PathUnder -Path $target -Root $installDirFull)) {
            $script:CleanupErrors.Add("$Label 快捷方式目标不属于安装目录，未删除：$shortcut -> $target")
            continue
        }
        try {
            Remove-Item -LiteralPath $shortcut -Force -ErrorAction Stop
        } catch {
            $script:CleanupErrors.Add("$Label 删除快捷方式失败：$($_.Exception.Message)")
            continue
        }
        # 删除后回读；只要文件仍在就说明删除未生效，不能用目标归属掩盖残留。
        if (Test-Path -LiteralPath $shortcut) {
            $remainingTarget = Get-ShortcutTargetPath -Path $shortcut
            $script:CleanupErrors.Add("$Label 快捷方式删除后仍存在：$shortcut -> $remainingTarget")
        }
    }

    if (Test-Path -LiteralPath $installDirFull) {
        try {
            Remove-DirectoryWithRetry -Path $installDirFull
        } catch {
            $script:CleanupErrors.Add("$Label 删除安装目录失败：$($_.Exception.Message)")
        }
    }

    [void](Stop-KerminalProcessesForPath -ExecutablePath $exe)

    if (Test-Path -LiteralPath $exe) { $script:CleanupErrors.Add("$Label 后主程序仍存在：$exe") }
    if (Test-Path -LiteralPath $installDirFull) { $script:CleanupErrors.Add("$Label 后安装目录仍存在：$installDirFull") }
    $remaining = @(Get-InstalledKerminal -Hive HKCU | Where-Object { Test-PathEqual -Path $_.InstallLocation -Expected $installDirFull })
    if ($remaining.Count -ne 0) { $script:CleanupErrors.Add("$Label 后仍有指向安装目录的卸载项") }
    $remainingProtocol = Get-ExecutablePathFromCommand -Command (Get-DefaultRegistryValue -Path (Join-Path $script:ProtocolKey 'shell\open\command'))
    if (Test-PathEqual -Path $remainingProtocol -Expected $exe) { $script:CleanupErrors.Add("$Label 后协议仍指向安装目录") }
}

function Find-MigrationBackupSettings {
    <#
    迁移备份目录名是 UUID；只按受控相对路径 before/managed/settings.toml 查找，不猜测其他结构。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$DataRoot)

    $migrationRoot = Join-Path $DataRoot $script:MigrationDirName
    if (-not (Test-Path -LiteralPath $migrationRoot)) { return $null }
    foreach ($directory in @(Get-ChildItem -LiteralPath $migrationRoot -Directory -ErrorAction SilentlyContinue)) {
        $candidate = Join-Path $directory.FullName 'before\managed\settings.toml'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
    }
    return $null
}

function Test-FormatMarker {
    <#
    marker 是 0.4 事务最后提交项；只接受 data_format=4 且 migration_contract=4。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$DataRoot)

    $path = Join-Path $DataRoot $script:MarkerName
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
    $text = Get-Content -Raw -LiteralPath $path
    return (
        $text -match '(?m)^\s*data_format\s*=\s*4\s*$' -and
        $text -match '(?m)^\s*migration_contract\s*=\s*4\s*$'
    )
}

function Test-MigrationBackup {
    <#
    同时核对 active.json 已提交、settings.toml 的 before 摘要和真实备份字节；三者一致才证明可恢复。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$DataRoot,
        [Parameter(Mandatory = $true)][string]$OriginalHash
    )

    $migrationRoot = Join-Path $DataRoot $script:MigrationDirName
    $activePath = Join-Path $migrationRoot 'active.json'
    if (-not (Test-Path -LiteralPath $activePath -PathType Leaf)) { return $false }
    try {
        $active = Get-Content -Raw -LiteralPath $activePath | ConvertFrom-Json -Depth 20
    } catch {
        return $false
    }
    if ([string]$active.state -ne 'committed') { return $false }
    $files = @($active.files | Where-Object { [string]$_.relative_path -eq 'settings.toml' })
    if ($files.Count -ne 1) { return $false }
    if ([string]$files[0].before_sha256 -ine $OriginalHash) { return $false }
    $backup = Find-MigrationBackupSettings -DataRoot $DataRoot
    if ($null -eq $backup) { return $false }
    return ((Get-Sha256Hex -Path $backup) -eq $OriginalHash)
}

function Test-SettingsReadback {
    <#
    迁移后设置仍保留用户可区分的非默认值，才说明读取的是合成旧配置而不是重置为默认。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$DataRoot)

    $path = Join-Path $DataRoot 'settings.toml'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
    $text = Get-Content -Raw -LiteralPath $path
    foreach ($pattern in @(
            '(?m)^\s*schema_version\s*=\s*1\s*$',
            '(?m)^\s*themeMode\s*=\s*"dark"\s*(#.*)?$',
            '(?m)^\s*interfaceDensity\s*=\s*"compact"\s*(#.*)?$',
            '(?m)^\s*fontSize\s*=\s*16\s*(#.*)?$',
            '(?m)^\s*fontFamily\s*=\s*"Cascadia Code"\s*(#.*)?$'
        )) {
        if ($text -notmatch $pattern) { return $false }
    }
    return $true
}

function Test-FirstFrame {
    <#
    plugins/state.json 的 first_frame 是新版真正进入首帧的持久证据；没有它不把进程存活当启动成功。
    @author kongweiguang
    #>
    param([Parameter(Mandatory = $true)][string]$DataRoot)

    $path = Join-Path $DataRoot 'plugins\state.json'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
    try {
        $state = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json -Depth 20
    } catch {
        return $false
    }
    $boot = $state.PSObject.Properties['boot']
    if ($null -eq $boot -or $null -eq $boot.Value) { return $false }
    return (
        [string]$boot.Value.phase -eq 'first_frame' -and
        -not [string]::IsNullOrWhiteSpace([string]$boot.Value.id)
    )
}

function Copy-EvidenceFile {
    <#
    证据只复制存在的小文件；失败路径也不因为缺少某个文件而掩盖原始错误。
    @author kongweiguang
    #>
    param([string]$Source, [string]$Destination)

    if (Test-Path -LiteralPath $Source -PathType Leaf) {
        Copy-Item -LiteralPath $Source -Destination $Destination -Force
    }
}

function Copy-AppEvidence {
    <#
    在删除隔离根前保存 marker、迁移 journal、备份、设置、日志和首帧状态；同时覆盖成功和失败路径。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$DataRoot,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$FixtureSettings,
        [string]$Stdout = '',
        [string]$Stderr = '',
        [string]$ErrorText = ''
    )

    New-Directory -Path $Destination
    Copy-EvidenceFile -Source (Join-Path $DataRoot $script:MarkerName) -Destination (Join-Path $Destination 'marker.toml')
    Copy-EvidenceFile -Source (Join-Path $DataRoot 'settings.toml') -Destination (Join-Path $Destination 'settings-after.toml')
    Copy-EvidenceFile -Source (Join-Path $DataRoot (Join-Path $script:MigrationDirName 'active.json')) -Destination (Join-Path $Destination 'migration-active.json')
    Copy-EvidenceFile -Source (Join-Path $DataRoot 'plugins\state.json') -Destination (Join-Path $Destination 'plugins-state.json')
    Copy-EvidenceFile -Source $FixtureSettings -Destination (Join-Path $Destination 'settings-before.toml')

    $backup = Find-MigrationBackupSettings -DataRoot $DataRoot
    if ($null -ne $backup) {
        Copy-Item -LiteralPath $backup -Destination (Join-Path $Destination 'settings-before-backup.toml') -Force
    }

    $migrationRoot = Join-Path $DataRoot $script:MigrationDirName
    $migrationTree = @()
    if (Test-Path -LiteralPath $migrationRoot) {
        $migrationTree = @(Get-ChildItem -LiteralPath $migrationRoot -Recurse -Force -ErrorAction SilentlyContinue | ForEach-Object {
                [pscustomobject]@{
                    relativePath = [IO.Path]::GetRelativePath($DataRoot, $_.FullName)
                    length       = if ($_.PSIsContainer) { $null } else { $_.Length }
                }
            })
    }
    Write-JsonFile -Path (Join-Path $Destination 'migration-tree.json') -Value $migrationTree
    [IO.File]::WriteAllText((Join-Path $Destination 'stdout.log'), $Stdout, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $Destination 'stderr.log'), $Stderr, [Text.UTF8Encoding]::new($false))
    if (-not [string]::IsNullOrWhiteSpace($ErrorText)) {
        [IO.File]::WriteAllText((Join-Path $Destination 'error.txt'), $ErrorText, [Text.UTF8Encoding]::new($false))
    }

    $logRoot = Join-Path $DataRoot 'logs'
    if (Test-Path -LiteralPath $logRoot) {
        $logText = @(Get-ChildItem -LiteralPath $logRoot -Filter '*.log' -File -ErrorAction SilentlyContinue | ForEach-Object {
                "===== $($_.Name) ====="
                Get-Content -LiteralPath $_.FullName -Tail 200 -ErrorAction SilentlyContinue
            }) -join "`n"
        [IO.File]::WriteAllText((Join-Path $Destination 'kerminal-log-tail.txt'), $logText, [Text.UTF8Encoding]::new($false))
    }
}

function Start-AcceptanceApp {
    <#
    只启动新版：用 LOCALAPPDATA 下的短隔离根和合成 0.3 settings，验证 marker、真实备份、
    设置读回和首帧；关闭或强制回收后确认没有同目录子进程，并删除隔离根。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string]$FixtureSettings,
        [Parameter(Mandatory = $true)][string]$FixtureHash
    )

    $caseRoot = Join-Path $script:RealLocalAppData ('kla-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    Assert-True ($caseRoot.Length -le 100) "隔离数据根过长，可能触发路径限制：$caseRoot"
    $dataRoot = Join-Path $caseRoot 'data'
    $localRoot = Join-Path $caseRoot 'local'
    $roamingRoot = Join-Path $caseRoot 'roaming'
    $tempRoot = Join-Path $caseRoot 'temp'
    foreach ($directory in @($dataRoot, $localRoot, $roamingRoot, $tempRoot)) {
        New-Directory -Path $directory
    }
    $script:CaseRoot = $caseRoot
    Copy-Item -LiteralPath $FixtureSettings -Destination (Join-Path $dataRoot 'settings.toml') -Force

    $formalRoot = Join-Path $script:RealUserProfile '.kerminal'
    $betaRoot = Join-Path $script:RealUserProfile '.kerminal-gpui'
    $formalExisted = Test-Path -LiteralPath $formalRoot
    $betaExisted = Test-Path -LiteralPath $betaRoot
    $appEvidence = Join-Path $script:InstallEvidence 'app'

    $saved = @{}
    foreach ($name in @('KERMINAL_CONFIG_ROOT', 'LOCALAPPDATA', 'APPDATA', 'TEMP', 'TMP')) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    $process = $null
    $stdoutTask = $null
    $stderrTask = $null
    $forced = $false
    $markerValid = $false
    $backupValid = $false
    $settingsValid = $false
    $firstFrame = $false
    try {
        try {
            [Environment]::SetEnvironmentVariable('KERMINAL_CONFIG_ROOT', $dataRoot, 'Process')
            [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $localRoot, 'Process')
            [Environment]::SetEnvironmentVariable('APPDATA', $roamingRoot, 'Process')
            [Environment]::SetEnvironmentVariable('TEMP', $tempRoot, 'Process')
            [Environment]::SetEnvironmentVariable('TMP', $tempRoot, 'Process')

            $startInfo = [Diagnostics.ProcessStartInfo]::new()
            $startInfo.FileName = $Executable
            $startInfo.Arguments = ''
            $startInfo.WorkingDirectory = $InstallDir
            $startInfo.UseShellExecute = $false
            $startInfo.CreateNoWindow = $false
            $startInfo.RedirectStandardOutput = $true
            $startInfo.RedirectStandardError = $true
            $startInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
            $startInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
            $process = [Diagnostics.Process]::new()
            $process.StartInfo = $startInfo
            if (-not $process.Start()) { throw "无法启动新版应用：$Executable" }
            $script:AppProcess = $process
            $stdoutTask = $process.StandardOutput.ReadToEndAsync()
            $stderrTask = $process.StandardError.ReadToEndAsync()
        } finally {
            foreach ($name in $saved.Keys) {
                $value = $saved[$name]
                if ($null -eq $value) {
                    # 恢复缺失变量时必须删除；$null 赋值在 Windows 上会留下空字符串并继续传给子进程。
                    Remove-Item "Env:$name" -ErrorAction SilentlyContinue
                } else {
                    [Environment]::SetEnvironmentVariable($name, $value, 'Process')
                }
            }
        }

        $deadline = [DateTime]::UtcNow.AddSeconds(180)
        $lastError = $null
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($process.HasExited) {
                $stderr = Get-CompletedText $stderrTask
                throw "Kerminal 在验收完成前退出（exit=$($process.ExitCode)）。stderr: $stderr"
            }
            try {
                $markerValid = Test-FormatMarker -DataRoot $dataRoot
                $backupValid = Test-MigrationBackup -DataRoot $dataRoot -OriginalHash $FixtureHash
                $settingsValid = Test-SettingsReadback -DataRoot $dataRoot
                $firstFrame = Test-FirstFrame -DataRoot $dataRoot
            } catch {
                $lastError = $_.Exception.Message
            }
            if ($markerValid -and $backupValid -and $settingsValid -and $firstFrame) { break }
            Start-Sleep -Milliseconds 250
        }
        if (-not ($markerValid -and $backupValid -and $settingsValid -and $firstFrame)) {
            throw "等待新版启动/迁移超时：marker=$markerValid backup=$backupValid settings=$settingsValid firstFrame=$firstFrame lastError=$lastError"
        }

        # 首帧后保留短暂宽限期：应用自行提前退出说明启动验证没有稳定完成，即使退出码为 0 也据实失败。
        Start-Sleep -Milliseconds 1000
        if ($process.HasExited) {
            throw "Kerminal 首帧后自行提前退出（exit=$($process.ExitCode)），不能判定为正常启动。stderr: $(Get-CompletedText $stderrTask)"
        }

        $exitMode = 'running'
        $closed = $false
        try { $closed = $process.CloseMainWindow() } catch { $closed = $false }
        if ($closed) {
            if ($process.WaitForExit(20000)) { $exitMode = 'close-message' }
        }
        if (-not $process.HasExited) {
            # 关闭消息超时或没有主窗口时才强制回收；显式标记 forced-timeout，绝不冒充正常关闭。
            $forced = $true
            $exitMode = 'forced-timeout'
            try { $process.Kill($true) } catch { }
            [void]$process.WaitForExit(15000)
        }
        if (-not $process.HasExited) { throw "Kerminal 进程未能回收：PID $($process.Id)" }
        if ($exitMode -eq 'running') {
            if (-not $closed) {
                # 没有可用的关闭通道就自行退出，仍属自发退出，不能按正常关闭记录。
                throw "Kerminal 在关闭请求前后自行退出（exit=$($process.ExitCode)），不能判定为正常启动。stderr: $(Get-CompletedText $stderrTask)"
            }
            # 关闭请求已送达，进程在等待边界后完成退出；同样属于关闭消息路径。
            $exitMode = 'close-message'
        }
        $exitCode = $process.ExitCode
        if ($exitMode -eq 'close-message' -and $exitCode -ne 0) {
            throw "Kerminal 响应关闭消息后非零退出（exit=$exitCode）。stderr: $(Get-CompletedText $stderrTask)"
        }
        $stdout = Get-CompletedText $stdoutTask
        $stderr = Get-CompletedText $stderrTask

        foreach ($item in @(Get-KerminalProcessesForPath -ExecutablePath $Executable)) {
            $processId = if ($null -ne $item.PSObject.Properties['ProcessId']) { [int]$item.ProcessId } else { [int]$item.Id }
            Stop-Process -Id $processId -Force -ErrorAction SilentlyContinue
            Wait-Process -Id $processId -Timeout 10 -ErrorAction SilentlyContinue
        }
        Assert-True (@(Get-KerminalProcessesForPath -ExecutablePath $Executable).Count -eq 0) '新版应用或其同目录子进程未被回收'

        Assert-True ((Test-Path -LiteralPath $formalRoot) -eq $formalExisted) "隔离启动改动了正式数据根存在状态：$formalRoot"
        Assert-True ((Test-Path -LiteralPath $betaRoot) -eq $betaExisted) "隔离启动改动了 Beta 数据根存在状态：$betaRoot"

        Copy-AppEvidence -DataRoot $dataRoot -Destination $appEvidence -FixtureSettings $FixtureSettings -Stdout $stdout -Stderr $stderr
        $backupSettings = Find-MigrationBackupSettings -DataRoot $dataRoot
        $result = [ordered]@{
            status                 = 'passed'
            firstFrameObserved     = $firstFrame
            formatMarker           = $markerValid
            migrationBackup        = $backupValid
            settingsReadback       = $settingsValid
            migrationBackupPath    = if ($null -ne $backupSettings) { [IO.Path]::GetRelativePath($dataRoot, $backupSettings) } else { $null }
            originalSettingsSha256 = $FixtureHash
            migratedSettingsSha256 = Get-Sha256Hex -Path (Join-Path $dataRoot 'settings.toml')
            processId              = $process.Id
            exitCode               = $exitCode
            exitMode               = $exitMode
            forcedTermination      = $forced
            stdout                 = 'app/stdout.log'
            stderr                 = 'app/stderr.log'
            dataRoot               = $dataRoot
        }

        Remove-DirectoryWithRetry -Path $caseRoot
        $script:CaseRoot = $null
        return $result
    } catch {
        # Process.Start 可能在建进程前失败，此时 HasExited 本身不可用；按可查询状态安全回收。
        if ($null -ne $process) {
            try {
                if (-not $process.HasExited) {
                    try { $process.Kill($true) } catch { }
                    [void]$process.WaitForExit(15000)
                }
            } catch { }
        }
        try {
            Copy-AppEvidence -DataRoot $dataRoot -Destination $appEvidence -FixtureSettings $FixtureSettings `
                -Stdout (Get-CompletedText $stdoutTask) -Stderr (Get-CompletedText $stderrTask) -ErrorText $_.Exception.Message
        } catch { }
        throw
    }
}

function Invoke-Step {
    <#
    每个安装/启动场景独立记录耗时和失败原因，顶层仍统一清理；报告不依赖控制台文本。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][scriptblock]$Action
    )

    $timer = [Diagnostics.Stopwatch]::StartNew()
    try {
        $result = & $Action
        $script:Report.steps += [ordered]@{
            name      = $Name
            status    = 'passed'
            elapsedMs = [math]::Round($timer.Elapsed.TotalMilliseconds, 1)
        }
        return $result
    } catch {
        $script:Report.steps += [ordered]@{
            name      = $Name
            status    = 'failed'
            elapsedMs = [math]::Round($timer.Elapsed.TotalMilliseconds, 1)
            error     = $_.Exception.Message
        }
        throw
    }
}

function Save-Report {
    <#
    报告和摘要最后写，确保即使失败也留下安装路径、注册表、隔离根和清理错误。
    @author kongweiguang
    #>
    $script:Report.endedAt = (Get-Date).ToUniversalTime().ToString('o')
    $script:Report.cleanup = [ordered]@{
        errors        = @($script:CleanupErrors)
        caseRoot      = $script:CaseRoot
        installedDirs = @($script:InstalledDirs | Select-Object -Unique)
    }
    Write-JsonFile -Path (Join-Path $script:InstallEvidence 'report.json') -Value $script:Report

    $lines = [Collections.Generic.List[string]]::new()
    $lines.Add('# Windows install acceptance')
    $lines.Add('')
    $lines.Add("- Result: **$($script:Report.status)**")
    $lines.Add("- New installer: $($script:Report.installers.new.version) SHA-256 ``$($script:Report.installers.new.sha256)``")
    $lines.Add("- Old installer: $($script:Report.installers.old.version) SHA-256 ``$($script:Report.installers.old.sha256)``")
    if ($null -ne $script:Report.scenarios.fresh) {
        $lines.Add("- Fresh install: ``$($script:Report.scenarios.fresh.InstallLocation)`` version $($script:Report.scenarios.fresh.DisplayVersion); protocol and HKCU verified")
        # 描述文案由安装器模板决定；原样展示便于区分功能指向通过和上游文案缺陷。
        if ($null -ne $script:Report.scenarios.fresh.PSObject.Properties['ProtocolDescription']) {
            $lines.Add("- Protocol description: ``$($script:Report.scenarios.fresh.ProtocolDescription)``")
        }
    }
    if ($null -ne $script:Report.scenarios.old) {
        $lines.Add("- Old install: ``$($script:Report.scenarios.old.path)`` version $($script:Report.scenarios.old.version); old app not launched")
    }
    if ($null -ne $script:Report.scenarios.upgrade) {
        $lines.Add("- Upgrade: version $($script:Report.scenarios.upgrade.version) kept path ``$($script:Report.scenarios.upgrade.path)``; protocol points to it")
    }
    if ($null -ne $script:Report.appLaunch) {
        $app = $script:Report.appLaunch
        $lines.Add("- New app launch: firstFrame=$($app.firstFrameObserved) marker=$($app.formatMarker) backup=$($app.migrationBackup) settingsReadback=$($app.settingsReadback)")
        # 明确区分关闭消息正常退出与超时强制回收，避免把强杀记录成正常关闭。
        $lines.Add("- New app shutdown: mode=$($app.exitMode) exit=$($app.exitCode) forced=$($app.forcedTermination)")
        $lines.Add("- Migrated settings SHA-256: ``$($app.migratedSettingsSha256)``; original ``$($app.originalSettingsSha256)``")
    }
    if ($script:CleanupErrors.Count -eq 0) {
        $lines.Add('- Cleanup: HKCU uninstall entry, protocol key, install files, shortcuts and isolated data root removed; no owned processes remain')
    } else {
        $lines.Add("- Cleanup errors: $($script:CleanupErrors.Count); see install/report.json")
    }
    if ($script:Report.errors.Count -gt 0) {
        $lines.Add("- Errors: $($script:Report.errors -join '; ')")
    }
    $markdown = ($lines -join "`n") + "`n"
    [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot 'summary.md'), $markdown, [Text.UTF8Encoding]::new($false))
    if ($env:GITHUB_STEP_SUMMARY) {
        Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $markdown
    }
}

# 安装器只能来自已验摘要的独立暂存目录；可信 harness 检出和 harness 工作目录都不再混入下载内容。
$workRoot = New-EmptyDirectory -Path $WorkRoot
New-Directory -Path $script:EvidenceRoot
$script:InstallEvidence = New-EmptyDirectory -Path $script:InstallEvidence

Assert-True (Test-Path -LiteralPath $NewInstaller -PathType Leaf) "新安装器不存在：$NewInstaller"
Assert-True (Test-Path -LiteralPath $OldInstaller -PathType Leaf) "旧安装器不存在：$OldInstaller"
Assert-True (-not (Test-PathUnder -Path $NewInstaller -Root $script:RepoRoot)) '新安装器不能位于可信 harness 检出内'
Assert-True (-not (Test-PathUnder -Path $OldInstaller -Root $script:RepoRoot)) '旧安装器不能位于可信 harness 检出内'
Assert-True (-not (Test-PathUnder -Path $NewInstaller -Root $workRoot)) '新安装器不能位于 harness 工作目录内'
Assert-True (-not (Test-PathUnder -Path $OldInstaller -Root $workRoot)) '旧安装器不能位于 harness 工作目录内'
Assert-True (-not [string]::Equals([IO.Path]::GetFullPath($NewInstaller), [IO.Path]::GetFullPath($OldInstaller), [StringComparison]::OrdinalIgnoreCase)) '新旧安装器不能是同一文件'

$newHash = Get-Sha256Hex -Path $NewInstaller
$oldHash = Get-Sha256Hex -Path $OldInstaller
Assert-True ($newHash -eq $script:ExpectedNewHash) "新安装器 SHA-256 不匹配：$newHash"
Assert-True ($oldHash -eq $script:ExpectedOldHash) "旧安装器 SHA-256 不匹配：$oldHash"
Assert-InstallerFileVersion -Path $NewInstaller -Version $ExpectedNewVersion -Label '新安装器'
Assert-InstallerFileVersion -Path $OldInstaller -Version $ExpectedOldVersion -Label '旧安装器'

Assert-True (Test-Path -LiteralPath $FixtureSettings -PathType Leaf) "settings fixture 不存在：$FixtureSettings"
Assert-True (Test-PathUnder -Path $FixtureSettings -Root $script:RepoRoot) 'settings fixture 必须来自受信任检出'
$fixtureHash = Get-Sha256Hex -Path $FixtureSettings

$script:Report = [ordered]@{
    schema      = 1
    status      = 'running'
    startedAt   = (Get-Date).ToUniversalTime().ToString('o')
    endedAt     = $null
    environment = [ordered]@{
        repository  = $env:GITHUB_REPOSITORY
        runId       = $env:GITHUB_RUN_ID
        runAttempt  = $env:GITHUB_RUN_ATTEMPT
        runnerName  = $env:RUNNER_NAME
        runnerImage = $env:ImageOS
        os          = [Environment]::OSVersion.VersionString
        powershell  = $PSVersionTable.PSVersion.ToString()
    }
    installers  = [ordered]@{
        new = [ordered]@{ path = $NewInstaller; version = $ExpectedNewVersion; sha256 = $newHash }
        old = [ordered]@{ path = $OldInstaller; version = $ExpectedOldVersion; sha256 = $oldHash }
    }
    scenarios   = [ordered]@{
        fresh   = $null
        old     = $null
        upgrade = $null
    }
    appLaunch   = $null
    cleanup     = $null
    errors      = @()
    steps       = @()
}

try {
    # PowerShell 把 $null 传给 .NET string 参数会写成空值；空值仍会被安装器预检当作无效覆盖并中止。
    # 必须真正移除变量，让安装器读取正式默认数据根。
    Remove-Item Env:KERMINAL_CONFIG_ROOT -ErrorAction SilentlyContinue

    Invoke-Step -Name 'preflight' -Action {
        Assert-CleanRunner -RequireFormalRootAbsent
        # 空字符串也会进入子进程环境；确认变量确实不存在，避免安装器预检把空覆盖读成无效根。
        Assert-True ($null -eq [Environment]::GetEnvironmentVariable('KERMINAL_CONFIG_ROOT', 'Process')) 'KERMINAL_CONFIG_ROOT 必须真正移除，空值会让安装器预检失败'
        [ordered]@{ clean = $true; defaultInstallDir = $script:DefaultInstallDir }
    } | Out-Null

    $script:Report.scenarios.fresh = Invoke-Step -Name 'fresh-install' -Action {
        $script:InstalledDirs.Add($script:DefaultInstallDir) | Out-Null
        Invoke-Installer -Path $NewInstaller -Label 'fresh-install' -TimeoutSeconds 300
        Wait-For -Description '新版 HKCU 卸载项和 0.4.0 版本' -TimeoutSeconds 90 -Condition {
            $entries = @(Get-InstalledKerminal -Hive HKCU)
            return ($entries.Count -eq 1 -and [string]$entries[0].DisplayVersion -eq $ExpectedNewVersion)
        }
        $entry = Assert-Installation -ExpectedVersion $ExpectedNewVersion -ExpectedPath $script:DefaultInstallDir -RequireProtocol -RequireMigrationResources -VerifyManufacturerKey
        return $entry
    }

    Invoke-Step -Name 'fresh-uninstall' -Action {
        Remove-Installation -InstallDir $script:DefaultInstallDir -Label 'fresh-uninstall'
        Assert-NoCleanupErrors
        Assert-CleanRunner -RequireFormalRootAbsent
        [ordered]@{ removed = $true }
    } | Out-Null

    $script:Report.scenarios.old = Invoke-Step -Name 'old-install' -Action {
        $script:InstalledDirs.Add($script:DefaultInstallDir) | Out-Null
        Invoke-Installer -Path $OldInstaller -Label 'old-install' -TimeoutSeconds 300
        Wait-For -Description '旧版 HKCU 卸载项和 0.3.48 版本' -TimeoutSeconds 90 -Condition {
            $entries = @(Get-InstalledKerminal -Hive HKCU)
            return ($entries.Count -eq 1 -and [string]$entries[0].DisplayVersion -eq $ExpectedOldVersion)
        }
        $entries = @(Get-InstalledKerminal -Hive HKCU)
        $oldPath = [string]$entries[0].InstallLocation
        Assert-True (-not [string]::IsNullOrWhiteSpace($oldPath)) '旧版安装器未写 InstallLocation'
        Assert-True (Test-PathEqual -Path $oldPath -Expected $script:DefaultInstallDir) "旧版安装路径 $oldPath 与当前用户默认路径 $script:DefaultInstallDir 不一致"
        $entry = Assert-Installation -ExpectedVersion $ExpectedOldVersion -ExpectedPath $oldPath -RequireProtocol:$false -RequireMigrationResources:$false -VerifyManufacturerKey:$false
        # 旧版应用不应被启动；若旧安装器仍自动拉起，只终止本安装目录的进程，绝不执行旧应用验收。
        $stoppedCount = [int](Stop-KerminalProcessesForPath -ExecutablePath (Join-Path $oldPath $script:ExeName))
        return [ordered]@{
            path             = $oldPath
            version          = $entry.DisplayVersion
            exeSha256        = Get-Sha256Hex -Path (Join-Path $oldPath $script:ExeName)
            stoppedProcesses = $stoppedCount
        }
    }

    $script:Report.scenarios.upgrade = Invoke-Step -Name 'upgrade-install' -Action {
        $oldPath = [string]$script:Report.scenarios.old.path
        Wait-For -Description '升级前旧版进程全部退出' -TimeoutSeconds 60 -Condition {
            return (@(Get-KerminalProcessesForPath -ExecutablePath (Join-Path $oldPath $script:ExeName)).Count -eq 0)
        }
        Invoke-Installer -Path $NewInstaller -Label 'upgrade-install' -TimeoutSeconds 300
        Wait-For -Description '覆盖升级后 HKCU 版本变为 0.4.0' -TimeoutSeconds 90 -Condition {
            $entries = @(Get-InstalledKerminal -Hive HKCU)
            return ($entries.Count -eq 1 -and [string]$entries[0].DisplayVersion -eq $ExpectedNewVersion)
        }
        $entry = Assert-Installation -ExpectedVersion $ExpectedNewVersion -ExpectedPath $oldPath -RequireProtocol -RequireMigrationResources -VerifyManufacturerKey
        $newExeHash = Get-Sha256Hex -Path (Join-Path $oldPath $script:ExeName)
        Assert-True ($newExeHash -ne [string]$script:Report.scenarios.old.exeSha256) '覆盖升级后主程序摘要未变化'
        return [ordered]@{
            path                = $oldPath
            version             = $entry.DisplayVersion
            exeSha256           = $newExeHash
            protocol            = Get-DefaultRegistryValue -Path (Join-Path $script:ProtocolKey 'shell\open\command')
            protocolDescription = $entry.ProtocolDescription
            oldVersion          = $script:Report.scenarios.old.version
        }
    }

    $script:Report.appLaunch = Invoke-Step -Name 'new-app-launch' -Action {
        $upgradePath = [string]$script:Report.scenarios.upgrade.path
        return (Start-AcceptanceApp -InstallDir $upgradePath -Executable (Join-Path $upgradePath $script:ExeName) -FixtureSettings $FixtureSettings -FixtureHash $fixtureHash)
    }

    Invoke-Step -Name 'upgrade-uninstall' -Action {
        $upgradePath = [string]$script:Report.scenarios.upgrade.path
        Remove-Installation -InstallDir $upgradePath -Label 'upgrade-uninstall'
        Assert-NoCleanupErrors
        Assert-CleanRunner -RequireFormalRootAbsent
        [ordered]@{ removed = $true }
    } | Out-Null

    $script:Report.status = 'passed'
} catch {
    $script:Failure = $_
    $script:Report.status = 'failed'
    $script:Report.errors += $_.Exception.Message
} finally {
    if ($null -ne $script:AppProcess -and -not $script:AppProcess.HasExited) {
        try { $script:AppProcess.Kill($true) } catch { }
        [void]$script:AppProcess.WaitForExit(15000)
    }
    foreach ($installDir in @($script:InstalledDirs | Select-Object -Unique)) {
        try {
            Remove-Installation -InstallDir $installDir -Label 'final-cleanup'
        } catch {
            $script:CleanupErrors.Add("最终清理 $installDir 失败：$($_.Exception.Message)")
        }
    }
    if ($null -ne $script:CaseRoot) {
        try {
            Remove-DirectoryWithRetry -Path $script:CaseRoot
            $script:CaseRoot = $null
        } catch {
            $script:CleanupErrors.Add("删除隔离数据根失败：$($_.Exception.Message)")
        }
    }
    if ($null -ne $script:SavedConfigRoot) {
        [Environment]::SetEnvironmentVariable('KERMINAL_CONFIG_ROOT', $script:SavedConfigRoot, 'Process')
    }
    try {
        Save-Report
    } catch {
        $script:CleanupErrors.Add("写报告失败：$($_.Exception.Message)")
    }
}

if ($null -ne $script:Failure) {
    throw $script:Failure
}
if ($script:CleanupErrors.Count -gt 0) {
    throw ('验收清理未完成：' + ($script:CleanupErrors -join '; '))
}
