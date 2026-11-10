# @author kongweiguang
#Requires -Version 7.0
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$ExpectedHostSha256,
    [string]$PluginPath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('light','dark','system','all')][string]$Theme='light',
    [ValidateRange(60,900)][int]$TimeoutSeconds=300
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Initialize-PluginListLauncher {
    # 只抽取既有后台启动器的声明与完整Job装配块，不执行旧探针或自行拼装平台生命周期。
    # @author kongweiguang
    $tokens=$null; $errors=$null
    $ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'webview-native.ps1'),[ref]$tokens,[ref]$errors)
    if ($errors.Count) { throw 'Cannot parse the existing WebView launcher.' }
    $parts=[Collections.Generic.List[string]]::new()
    foreach ($name in @('Get-WebViewLauncherSource','Replace-WebViewLauncherPart','Stop-OwnedProbe')) {
        $matches=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name},$true))
        if ($matches.Count -ne 1) { throw "Launcher function is missing or ambiguous: $name" }
        $parts.Add($matches[0].Extent.Text)
    }
    $jobs=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$jobSource'},$true))
    $launch=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.StartsWith("if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher'",[StringComparison]::Ordinal)},$true))
    if ($jobs.Count -ne 1 -or $launch.Count -ne 1) { throw 'The complete Job launcher cannot be identified.' }
    $parts.Add($jobs[0].Extent.Text)
    # 列表不需要WGC输入桌面：删去网页启动器专用的五处OpenDesktop改写，保留Job回收。
    $launchSource = $launch[0].Extent.Text -split "`r?`n"
    $skip = @('String.IsNullOrWhiteSpace(desktopName)', 'EntryPoint = "CreateDesktopW"', 'private static extern IntPtr CreateDesktop(', 'CreateDesktop(desktopName, IntPtr.Zero', 'startupInfo.StartupInfo.Flags = StartfUseStdHandles;')
    $parts.Add(($launchSource | Where-Object { $line=$_; -not ($line.Contains('Replace-WebViewLauncherPart $source') -and @($skip | Where-Object { $line.Contains($_) }).Count -gt 0) }) -join "`n")
    $source=$parts -join "`n"
    # 动态脚本块没有来源文件，先固定被抽取函数的依赖目录，避免其 PSScriptRoot 变为空。
    $source=$source.Replace('$PSScriptRoot',("'"+$PSScriptRoot.Replace("'","''")+"'"))
    $old='--webview-probe --output '
    $new='--plugin-list-performance-acceptance --dev-plugin " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_PLUGIN_WEBVIEW_DIRECTORY")) + " --output-dir '
    if ($source.Split($old).Count -ne 2) { throw 'The WebView launcher command changed.' }
    $source=$source.Replace($old,$new).Replace('Kerminal.WebViewProbe','Kerminal.PluginListPerformance')
    . ([scriptblock]::Create($source))
}


function Get-ListHardwareMetadata {
    # 硬件清单不等于实际GPU后端，后者保留unknown，避免软件渲染被标成硬件通过。
    # @author kongweiguang
    try {
        [pscustomobject]@{
            OS=(Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber)
            CPU=@(Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors)
            MemoryBytes=(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
            VideoControllers=@(Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion,AdapterRAM)
            ActualRendererBackend='unknown'
        }
    } catch { [pscustomobject]@{Error=$_.Exception.Message;ActualRendererBackend='unknown'} }
}

function Get-ListProcessSample {
    # PID 可能在内核退出句柄发信号前已从进程表消失；缺失采样不代表成功，退出码与报告仍由原句柄校验。
    # @author kongweiguang
    param([int]$OwnedProcessId, [long]$ElapsedMilliseconds)
    $owned=$null
    try {
        $owned=[Diagnostics.Process]::GetProcessById($OwnedProcessId)
        $owned.Refresh()
        return [pscustomobject]@{UtcMilliseconds=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();ElapsedMilliseconds=$ElapsedMilliseconds;ProcessId=$OwnedProcessId;WorkingSetBytes=$owned.WorkingSet64;PrivateMemoryBytes=$owned.PrivateMemorySize64;CpuMilliseconds=$owned.TotalProcessorTime.TotalMilliseconds}
    } catch {
        $cause=$_.Exception.GetBaseException()
        if($cause -is [ArgumentException]){return $null}
        if($cause -is [InvalidOperationException] -and $null -ne $owned -and $owned.HasExited){return $null}
        throw
    } finally {if($null -ne $owned){$owned.Dispose()}}
}

function Get-ListProcessTreeSample {
    # 只沿当前候选 PID 的父子关系采集，并保存映像路径与文件 SHA256；短命退出竞态可能
    # 让个别行缺少路径，因此采样阶段保留完整行、门禁只接受带路径的真实宿主进程。
    # @author kongweiguang
    param([int]$RootProcessId, [long]$ElapsedMilliseconds, [hashtable]$ImageHashes)
    $rows = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine, CreationDate)
    $ids = [Collections.Generic.HashSet[uint32]]::new()
    $null = $ids.Add([uint32]$RootProcessId)
    do {
        $added = $false
        foreach ($row in $rows) {
            if ($ids.Contains([uint32]$row.ParentProcessId) -and $ids.Add([uint32]$row.ProcessId)) { $added = $true }
        }
    } while ($added)
    $members = @($rows | Where-Object { $ids.Contains([uint32]$_.ProcessId) } | ForEach-Object {
        $imagePath = $_.ExecutablePath
        if ([string]::IsNullOrWhiteSpace($imagePath)) {
            try { $imagePath = (Get-Process -Id $_.ProcessId -ErrorAction Stop).Path } catch { $imagePath = $null }
        }
        $imageSha256 = $null
        if (-not [string]::IsNullOrWhiteSpace($imagePath)) {
            if (-not $ImageHashes.ContainsKey($imagePath)) {
                try { $ImageHashes[$imagePath] = (Get-FileHash -LiteralPath $imagePath -Algorithm SHA256).Hash.ToLowerInvariant() } catch { $ImageHashes[$imagePath] = $null }
            }
            $imageSha256 = $ImageHashes[$imagePath]
        }
        # 内存样本与 JSON 回读都按属性访问；OrderedDictionary 不把键暴露成属性会让证据门禁误判缺失。
        [pscustomobject][ordered]@{
            pid = [int]$_.ProcessId
            parentPid = [int]$_.ParentProcessId
            name = $_.Name
            executablePath = $imagePath
            imageSha256 = $imageSha256
            commandLine = $_.CommandLine
            created = $_.CreationDate
        }
    })
    return [pscustomobject][ordered]@{
        elapsedMs = $ElapsedMilliseconds
        unixMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        processes = $members
    }
}

function Assert-PluginListProcessTree {
    # 宿主进程必须来自候选桌面进程树，映像位于候选目录，采样与文件 SHA 均等于候选构建
    # receipt 提供的期望值，命令行还必须引用本轮输出根副本或插件身份；只凭进程名不能通过。
    # @author kongweiguang
    param($Samples, [string]$CandidatePath, [string]$CandidateSha256, [int]$RootProcessId, [string]$ExpectedHostSha256, [string]$PluginCopyPath, [string]$PluginId)
    if ($null -eq $Samples -or @($Samples).Count -lt 1) { throw 'Plugin list process tree samples are missing.' }
    $root = $null
    $hosts = [ordered]@{}
    $treeLines = [Collections.Generic.HashSet[string]]::new()
    $copyPath = ConvertTo-NormalListPath $PluginCopyPath
    foreach ($sample in @($Samples)) {
        foreach ($process in @($sample.processes)) {
            $processId = Get-RequiredNumber $process 'pid'
            $name = Get-RequiredString $process 'name'
            $imagePath = $process.PSObject.Properties['executablePath'].Value
            $imageSha = $process.PSObject.Properties['imageSha256'].Value
            $line = "$processId`t$name`t$imageSha"
            $null = $treeLines.Add($line)
            if ($processId -eq $RootProcessId) { $root = $process }
            if ($name -ieq 'kerminal-plugin-host.exe' -or $name -ieq 'kerminal-plugin-host') {
                $hostPath = Get-RequiredString $process 'executablePath'
                $hostSha = Get-RequiredString $process 'imageSha256'
                $commandLine = Get-RequiredString $process 'commandLine'
                if ($hostSha -cne $ExpectedHostSha256) { throw 'Plugin host sampled image hash does not match the expected build identity.' }
                if (-not ($commandLine.Contains($copyPath, [StringComparison]::OrdinalIgnoreCase) -or $commandLine.Contains($PluginId, [StringComparison]::OrdinalIgnoreCase))) { throw 'Plugin host command line does not reference this run plugin copy or plugin identity.' }
                $key = "$processId|$hostPath|$hostSha"
                if (-not $hosts.Contains($key)) {
                    $hosts[$key] = [pscustomobject]@{ProcessId = $processId; Name = $name; ImagePath = $hostPath; ImageSha256 = $hostSha; CommandLine = $commandLine }
                }
            }
        }
    }
    if ($null -eq $root) { throw 'Candidate desktop process was not observed in the process tree.' }
    $rootPath = ConvertTo-NormalListPath (Get-RequiredString $root 'executablePath')
    $candidatePath = ConvertTo-NormalListPath $CandidatePath
    if (-not $rootPath.Equals($candidatePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Candidate desktop image path does not match the launched executable.' }
    $rootSha = (Get-FileHash -LiteralPath $rootPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($rootSha -cne $CandidateSha256 -or (Get-RequiredString $root 'imageSha256') -cne $rootSha) { throw 'Candidate desktop image hash mismatch.' }
    if (@($hosts.Keys).Count -lt 1) { throw 'kerminal-plugin-host was not observed in the candidate process tree.' }
    $candidateDirectory = [IO.Path]::GetDirectoryName($candidatePath)
    $verified = @()
    foreach ($hostEntry in $hosts.Values) {
        $path = ConvertTo-NormalListPath $hostEntry.ImagePath
        $directory = [IO.Path]::GetDirectoryName($path)
        if (-not $directory.Equals($candidateDirectory, [StringComparison]::OrdinalIgnoreCase)) { throw "Plugin host image is outside the candidate directory: $path" }
        $sha = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($sha -cne $ExpectedHostSha256 -or $sha -cne $hostEntry.ImageSha256) { throw "Plugin host image hash does not match the expected build identity: $path" }
        $verified += [pscustomobject]@{ProcessId = $hostEntry.ProcessId; Name = $hostEntry.Name; ImagePath = $path; ImageSha256 = $sha; CommandLine = $hostEntry.CommandLine }
    }
    $treeText = ((@($treeLines) | Sort-Object -CaseSensitive) -join "`n")
    $treeSha = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($treeText))).ToLowerInvariant()
    return [pscustomobject]@{
        RootProcessId = $RootProcessId
        RootImagePath = $rootPath
        RootImageSha256 = $rootSha
        ExpectedHostSha256 = $ExpectedHostSha256
        HostProcesses = $verified
        ProcessTreeSha256 = $treeSha
        SampleCount = @($Samples).Count
    }
}

function ConvertTo-NormalListPath {
    # Rust canonicalize 在 Windows 返回 \\?\ 前缀；门禁只比较同一实际路径，不因前缀差异误报。
    # @author kongweiguang
    param([string]$Path)
    $full = [IO.Path]::GetFullPath($Path)
    if ($full.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) { return '\\' + $full.Substring(8) }
    if ($full.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) { return $full.Substring(4) }
    return $full
}

function Get-ListTargetOs {
    # 与 Rust 的 std::env::consts::OS 对齐；其它平台显式返回 non-windows。
    # @author kongweiguang
    if ($IsWindows) { return 'windows' }
    if ($IsLinux) { return 'linux' }
    if ($IsMacOS) { return 'macos' }
    return 'unknown'
}

function Get-ListTargetArch {
    # PowerShell 使用 x64/arm64 名称，而 Rust 使用 x86_64/aarch64；门禁必须比较同一语义。
    # @author kongweiguang
    $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    if ($architecture -ieq 'x64') { return 'x86_64' }
    if ($architecture -ieq 'arm64') { return 'aarch64' }
    if ($architecture -ieq 'x86') { return 'x86' }
    return $architecture.ToLowerInvariant()
}

function Complete-OwnedProcessCleanup {
    # finally 清理必须保留首个原始错误并继续后续步骤；查询 Job 或 Dispose 抛出的新异常
    # 只能作为附加诊断聚合，不能覆盖采样本身或首个清理失败。
    # @author kongweiguang
    param($Process, [int]$WaitMilliseconds = 10000)
    $errors = [Collections.Generic.List[string]]::new()
    if ($null -eq $Process) { return ,$errors }
    $active = $null
    try { $active = $Process.GetJobSnapshot().ActiveProcesses } catch { $errors.Add("GetJobSnapshot failed: $($_.Exception.Message)") }
    # 快照失败时仍尝试终止，避免只因诊断查询失败就留下仍在运行的候选 Job。
    if ($null -eq $active -or $active -gt 0) {
        try { $Process.TerminateJob(1) } catch { $errors.Add("TerminateJob failed: $($_.Exception.Message)") }
    }
    try {
        if (-not $Process.WaitForExit($WaitMilliseconds)) { $errors.Add('Owned candidate did not terminate.') }
    } catch { $errors.Add("WaitForExit failed: $($_.Exception.Message)") }
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while ($true) {
            if ($Process.GetJobSnapshot().ActiveProcesses -eq 0) { break }
            if ([DateTime]::UtcNow -ge $deadline) { $errors.Add('Owned Job still has descendants.'); break }
            Start-Sleep -Milliseconds 100
        }
    } catch { $errors.Add("GetJobSnapshot failed: $($_.Exception.Message)") }
    # Dispose 放在最后且独立捕获，确保之前所有清理动作都至少尝试过一次。
    try { $Process.Dispose() } catch { $errors.Add("Dispose failed: $($_.Exception.Message)") }
    return ,$errors
}

function Get-CleanupFailureMessage {
    # 首个失败是主诊断，清理错误只追加在后；只有没有主失败时才把清理失败作为最终错误。
    # 这样 Dispose 或二次 Job 查询失败不会把真正的采样失败覆盖掉。
    # @author kongweiguang
    param($PrimaryFailure, $CleanupErrors)
    $cleanupCount = @($CleanupErrors).Count
    if ($null -ne $PrimaryFailure) {
        if ($cleanupCount -gt 0) { return "$($PrimaryFailure.Exception.Message) Cleanup also failed: $($CleanupErrors -join '; ')" }
        return $PrimaryFailure
    }
    if ($cleanupCount -gt 0) { return "Owned process cleanup failed: $($CleanupErrors -join '; ')" }
    return $null
}

function Get-RequiredNumber {
    # 拒绝缺失、字符串、非有限数及非整数，避免PowerShell把空值或布尔转换成合法零。
    # @author kongweiguang
    param($Object, [string]$Name, [bool]$AllowNegative = $false)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing numeric field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($null -eq $value -or $value -is [bool] -or $value -is [string] -or $value -isnot [ValueType]) { throw "Invalid numeric field: $Name" }
    try { $number = [double]$value } catch { throw "Invalid numeric field: $Name" }
    if (![double]::IsFinite($number) -or [math]::Abs($number) -gt 9007199254740991 -or $number -ne [math]::Floor($number) -or (!$AllowNegative -and $number -lt 0)) { throw "Invalid numeric field: $Name" }
    return $number
}

function Get-RequiredBoolean {
    # StrictMode 下缺失属性和字符串 "true" 都不能冒充真实布尔证据。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name] -or $Object.PSObject.Properties[$Name].Value -isnot [bool]) { throw "Boolean field '$Name' is missing or invalid." }
    return [bool]$Object.PSObject.Properties[$Name].Value
}

function Get-RequiredFiniteNumber {
    # 几何字段允许DPI小数，但仍拒绝缺失、字符串、布尔、NaN和非正值。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing numeric field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($null -eq $value -or $value -is [bool] -or $value -is [string] -or $value -isnot [ValueType]) { throw "Invalid numeric field: $Name" }
    $number = [double]$value
    if (![double]::IsFinite($number) -or $number -le 0 -or $number -gt 9007199254740991) { throw "Invalid numeric field: $Name" }
    return $number
}

function Get-RequiredString {
    # 只接受非空字符串；数字或空实例不能作为阶段、身份或业务键证据。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing string field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($value -isnot [string] -or [string]::IsNullOrWhiteSpace($value)) { throw "Invalid string field: $Name" }
    return [string]$value
}

function Get-RequiredObject {
    # 只接受 PSCustomObject；字符串、数组、哈希表和 .NET 实例都不能冒充嵌套报告对象。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing object field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($null -eq $value) { throw "Missing object field: $Name" }
    if ($value -isnot [pscustomobject]) { throw "Invalid object field: $Name" }
    return $value
}

function Get-RequiredArray {
    # 数组字段必须保持真实数组；标量或对象即使能被 @() 包装，也不能冒充逐条证据集合。
    # @author kongweiguang
    param($Object, [string]$Name)
    if ($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]) { throw "Missing array field: $Name" }
    $value = $Object.PSObject.Properties[$Name].Value
    if ($value -isnot [Array]) { throw "Invalid array field: $Name" }
    return ,@($value)
}

function Get-PluginDigestRule {
    # 规则文本同时写入 Rust 报告并在此逐字校验，跨语言重算不能各自选择不同的排序或拼接方式。
    # @author kongweiguang
    return 'sha256 over all regular files under plugin root, sorted by ordinal UTF-8 path bytes, each file contributing uint64-le path length, path bytes, uint64-le content length, content bytes'
}

function ConvertTo-UInt64LittleEndian {
    # 显式生成小端字节，不依赖运行时 BitConverter 的端序假设。
    # @author kongweiguang
    param([long]$Value)
    if ($Value -lt 0) { throw 'Length must not be negative.' }
    return [byte[]]@(
        [byte]($Value -band 0xff),
        [byte](($Value -shr 8) -band 0xff),
        [byte](($Value -shr 16) -band 0xff),
        [byte](($Value -shr 24) -band 0xff),
        [byte](($Value -shr 32) -band 0xff),
        [byte](($Value -shr 40) -band 0xff),
        [byte](($Value -shr 48) -band 0xff),
        [byte](($Value -shr 56) -band 0xff)
    )
}

function Compare-Utf8Ordinal {
    # Rust 的 String 顺序按 UTF-8 字节比较；.NET 序数比较按 UTF-16 码元，
    # 对非 BMP 字符会不同，因此这里显式比较 UTF-8 字节序列。
    # @author kongweiguang
    param([string]$Left, [string]$Right)
    $leftBytes = [Text.Encoding]::UTF8.GetBytes($Left)
    $rightBytes = [Text.Encoding]::UTF8.GetBytes($Right)
    $length = [Math]::Min($leftBytes.Length, $rightBytes.Length)
    for ($i = 0; $i -lt $length; $i++) {
        if ($leftBytes[$i] -cne $rightBytes[$i]) { return $(if ($leftBytes[$i] -lt $rightBytes[$i]) { -1 } else { 1 }) }
    }
    return $leftBytes.Length.CompareTo($rightBytes.Length)
}

function Get-PluginContentDigest {
    # 独立枚举插件目录的实际普通文件并按共享规则重算摘要；清单、main.js 与 views
    # 都会进入摘要，任何脚本或视图字节变化都会与 Rust 报告不一致。
    # @author kongweiguang
    param([string]$PluginPath)
    $root = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $PluginPath).Path)
    $paths = [Collections.Generic.List[string]]::new()
    foreach ($file in @(Get-ChildItem -LiteralPath $root -Recurse -File -Force -ErrorAction Stop)) {
        # 符号链接和目录连接会指向插件根之外，不能按普通文件字节进入摘要；硬链接仍是同一普通文件。
        $linkType = $file.PSObject.Properties['LinkType']
        if ($null -ne $linkType -and $null -ne $linkType.Value -and $linkType.Value -notin @('HardLink')) { throw "Plugin package contains a link: $($file.FullName)" }
        $relative = $file.FullName.Substring($root.Length).TrimStart([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar).Replace('\', '/')
        if ([string]::IsNullOrEmpty($relative)) { throw "Plugin package file path is invalid: $($file.FullName)" }
        $paths.Add($relative)
    }
    $paths.Sort([System.Comparison[string]] { param($left, $right) Compare-Utf8Ordinal $left $right })
    $hash = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    try {
        foreach ($path in $paths) {
            $full = Join-Path $root $path
            $bytes = [IO.File]::ReadAllBytes($full)
            $pathBytes = [Text.Encoding]::UTF8.GetBytes($path)
            $hash.AppendData((ConvertTo-UInt64LittleEndian ([long]$pathBytes.Length)))
            $hash.AppendData($pathBytes)
            $hash.AppendData((ConvertTo-UInt64LittleEndian ([long]$bytes.Length)))
            $hash.AppendData($bytes)
        }
        return [Convert]::ToHexString($hash.GetHashAndReset()).ToLowerInvariant()
    } finally {
        $hash.Dispose()
    }
}

function Assert-PluginListPluginCopy {
    # 报告必须指向输出根内本轮实际执行的副本；副本全部文件只读，且 PS 从同一路径
    # 重算的全文件摘要与报告一致，因此原 fixture 摘要或同尺寸换入都不能冒充执行字节。
    # @author kongweiguang
    param($Report, [string]$OutputDirectory, [string]$SourcePlugin)
    $copyPath = [IO.Path]::GetFullPath((Join-Path $OutputDirectory 'plugin'))
    $copyResolved = ConvertTo-NormalListPath (Resolve-Path -LiteralPath $copyPath).Path
    $outputRoot = (ConvertTo-NormalListPath $OutputDirectory).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    if (-not $copyResolved.StartsWith($outputRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Plugin acceptance copy is outside the output root.' }
    $reportedCopy = ConvertTo-NormalListPath (Get-RequiredString $Report 'pluginCopyPath')
    if (-not $reportedCopy.Equals($copyResolved, [StringComparison]::OrdinalIgnoreCase)) { throw 'Plugin report copy path is not the acceptance copy verified by the runner.' }
    $reportedSource = ConvertTo-NormalListPath (Get-RequiredString $Report 'pluginSourcePath')
    if (-not $reportedSource.Equals((ConvertTo-NormalListPath $SourcePlugin), [StringComparison]::OrdinalIgnoreCase)) { throw 'Plugin report source path is not the requested fixture path.' }
    if ((Get-RequiredString $Report 'immutableMode') -cne 'all acceptance-copy files set read-only before startup; full-tree digest recomputed after sampling') { throw 'Plugin copy immutable mode is not the expected read-only full-tree rule.' }
    if (!(Get-RequiredBoolean $Report 'pluginCopyStable')) { throw 'Plugin acceptance copy was not proven stable during sampling.' }
    foreach ($file in @(Get-ChildItem -LiteralPath $copyResolved -Recurse -File -Force -ErrorAction Stop)) {
        if (-not $file.IsReadOnly) { throw "Plugin acceptance copy file is writable: $($file.FullName)" }
    }
    $digest = Get-PluginContentDigest $copyPath
    if ((Get-RequiredString $Report 'pluginSha256') -cne $digest) { throw 'Plugin acceptance copy digest does not match the executed bytes.' }
    return [pscustomobject]@{ CopyPath = $copyResolved; Digest = $digest }
}

function Get-RequiredElementNumber {
    # 数组元素也必须保持数值类型；字符串或布尔不能被强制转换后冒充目标索引。
    # @author kongweiguang
    param($Array, [int]$Index, [string]$Name)
    if ($null -eq $Array -or $Index -lt 0 -or $Index -ge @($Array).Count) { throw "Missing numeric element: $Name" }
    $value = @($Array)[$Index]
    if ($null -eq $value -or $value -is [bool] -or $value -is [string] -or $value -isnot [ValueType]) { throw "Invalid numeric element: $Name" }
    $number = [double]$value
    if (![double]::IsFinite($number) -or $number -ne [math]::Floor($number) -or $number -lt 0) { throw "Invalid numeric element: $Name" }
    return $number
}

function Assert-PluginListEvidence {
    # 独立核对阶段、动态步距、请求前后范围、真实构造行、同一实例与代次；
    # 缺失、旧帧、原地目标和错误类型都必须失败关闭，不能用滚动请求冒充可视结果。
    # 同时绑定来源、私有桌面、主题、平台、进程树和采样时间窗，避免只凭列表形状通过。
    # @author kongweiguang
    param($Report, $Observations, $Frames, $Samples, $ProcessTree, [string]$CurrentTheme, [string]$ExpectedDesktop, [string]$CandidatePath, [string]$CandidateSha256, [string]$ExpectedHostSha256, [string]$PluginCopyPath, [string]$PluginId)

    if ((Get-RequiredNumber $Report 'schemaVersion') -ne 3) { throw 'Plugin list report schema is invalid.' }
    if ((Get-RequiredString $Report 'theme') -cne $CurrentTheme) { throw 'Plugin list report theme does not match this run.' }
    if ((Get-RequiredString $Report 'mode') -cne 'private-desktop-native-list') { throw 'Plugin list report mode is invalid.' }
    if ((Get-RequiredNumber $Report 'totalRows') -ne 100000 -or (Get-RequiredNumber $Report 'continuousTargets') -ne 500 -or (Get-RequiredNumber $Report 'roundtripTargets') -ne 15) { throw 'Plugin list target counts are invalid.' }
    if (!(Get-RequiredBoolean $Report 'shutdownComplete') -or (Get-RequiredBoolean $Report 'physicalInputTested')) { throw 'Plugin list shutdown or input scope is invalid.' }
    if ((Get-RequiredString $Report 'rawFrames') -cne 'raw-frames.json' -or (Get-RequiredString $Report 'rowObservations') -cne 'row-observations.json') { throw 'Plugin list evidence file names are invalid.' }
    if ((Get-RequiredString $Report 'pluginDigestRule') -cne (Get-PluginDigestRule)) { throw 'Plugin list digest rule is not the shared length-prefixed file-tree rule.' }
    $meaning = Get-RequiredString $Report 'observationMeaning'
    if (!$meaning.Contains('not a frame count') -or !$meaning.Contains('physical display') -or !$meaning.Contains('sampled once at sampling start') -or !$meaning.Contains('not OS window visibility')) { throw 'Plugin list observation scope is not explicit.' }
    $inputMode = Get-RequiredString $Report 'inputMode'
    if ($inputMode -cne 'production VirtualListScrollHandle.scroll_to_item') { throw 'Plugin list input mode is invalid.' }

    # 来源必须绑定本轮活动外部内容标签和正式 RPC 插件；宿主列表 fixture、其它插件或
    # 活动标签变化都不能靠相同快照形状冒充。
    $source = Get-RequiredObject $Report 'source'
    $contentId = Get-RequiredString $source 'contentId'
    $sourcePlugin = Get-RequiredString $source 'plugin'
    $sourceView = Get-RequiredString $source 'view'
    $sourceRuntime = Get-RequiredString $source 'runtime'
    if ($contentId -ceq 'acceptance.host-list' -or $sourcePlugin -ceq 'fixture.host-list' -or $sourceRuntime -cne 'script' -or $sourcePlugin -ceq 'fixture.host-list') { throw 'Host-list fixture or non-script runtime cannot satisfy the external plugin list gate.' }
    if ($contentId -cne 'test.list-performance/records' -or $sourcePlugin -cne 'test.list-performance' -or $sourceView -cne 'records' -or $sourceRuntime -cne 'script') { throw 'Plugin list report source identity is invalid.' }
    if (!(Get-RequiredBoolean $source 'externalPlugin') -or !(Get-RequiredBoolean $source 'activeBindingVerified')) { throw 'Plugin list source is not a verified active external plugin content.' }
    $sourceTabId = Get-RequiredString $source 'tabId'
    if ((Get-RequiredString $source 'activeTabId') -cne $sourceTabId) { throw 'Plugin list source active tab is not the opened content tab.' }

    # 隔离和几何证据必须来自实际运行，不把硬件清单当作GPU通过。
    $metadata = Get-RequiredObject $Report 'metadata'
    $nativeIsolation = Get-RequiredObject $metadata 'nativeIsolation'
    if (!(Get-RequiredBoolean $nativeIsolation 'platformSupported')) { throw 'Plugin list native isolation platform is not supported.' }
    if (!(Get-RequiredBoolean $nativeIsolation 'isolationProven')) { throw 'Plugin list native isolation is not proven.' }
    # 私有桌面名称必须正好是本轮创建的名称，输入桌面不能仍是同一桌面。
    if ((Get-RequiredString $nativeIsolation 'uiThreadDesktopName') -ine $ExpectedDesktop) { throw 'Plugin list UI thread is not on this run private desktop.' }
    if ((Get-RequiredString $nativeIsolation 'inputDesktopName') -ieq $ExpectedDesktop) { throw 'Plugin list private desktop is still the input desktop.' }
    if ((Get-RequiredNumber $nativeIsolation 'processId') -ne (Get-RequiredNumber $Report 'processId')) { throw 'Plugin list native isolation process does not match the report process.' }
    # OS 窗口可见性只代表采样起点的物理窗口状态；列表 content 的 visible 在每条观测里单独核对。
    if (!(Get-RequiredBoolean $metadata 'visible')) { throw 'Plugin list window is not visible at sampling start.' }
    if ((Get-RequiredString $metadata 'visibleScope') -cne 'OS window visibility sampled once at sampling start; list.visible is renderer content visibility, not OS window visibility' -or (Get-RequiredString $metadata 'visibleSampledAt') -cne 'sampling-start') { throw 'Plugin list window visibility scope is not explicit.' }
    # active 只记录采样起点的窗口状态，不要求为 true，也不能据此宣称整个采样窗持续有焦点。
    [void](Get-RequiredBoolean $metadata 'active')
    if ((Get-RequiredString $metadata 'activeScope') -cne 'OS window active state sampled once at sampling start in a private desktop; not proof the window remained focused or received user input') { throw 'Plugin list window active scope is not explicit.' }
    if ((Get-RequiredBoolean $metadata 'debugAssertions')) { throw 'Plugin list sampling must not run with debug assertions.' }
    if ((Get-RequiredString $metadata 'targetOs') -cne (Get-ListTargetOs) -or (Get-RequiredString $metadata 'targetArch') -cne (Get-ListTargetArch)) { throw 'Plugin list target OS or architecture does not match this run.' }
    [void](Get-RequiredString $metadata 'fontFamily')
    if ($null -eq $metadata.PSObject.Properties['viewport']) { throw 'Plugin list viewport metadata is missing.' }
    $viewport = @($metadata.PSObject.Properties['viewport'].Value)
    if ($viewport.Count -ne 2) { throw 'Plugin list viewport metadata is invalid.' }
    foreach ($coordinate in $viewport) {
        if ($null -eq $coordinate -or $coordinate -is [bool] -or $coordinate -is [string] -or $coordinate -isnot [ValueType] -or ![double]::IsFinite([double]$coordinate) -or [double]$coordinate -le 0) { throw 'Plugin list viewport metadata is invalid.' }
    }
    if ([double]$viewport[0] -lt 800 -or [double]$viewport[1] -lt 600) { throw 'Plugin list viewport is smaller than 800x600.' }
    if ((Get-RequiredFiniteNumber $metadata 'scaleFactor') -le 0 -or (Get-RequiredFiniteNumber $metadata 'fontSize') -le 0) { throw 'Plugin list DPI or font size metadata is invalid.' }
    # 实际 UI 暗色值来自运行时 Theme；light/dark 必须与请求一致，system 只记录实际值，
    # 因为跟随系统时宿主无法在不读取系统外观的情况下保证某一具体明暗。
    $darkMode = Get-RequiredBoolean $metadata 'darkMode'
    if ((Get-RequiredString $metadata 'themeScope') -cne 'darkMode read from the live GPUI Theme at sampling start; system mode is not forced to a specific appearance') { throw 'Plugin list theme scope is not explicit.' }
    if ($CurrentTheme -ceq 'light' -and $darkMode) { throw 'Plugin list light theme run observed dark UI.' }
    if ($CurrentTheme -ceq 'dark' -and !$darkMode) { throw 'Plugin list dark theme run observed light UI.' }
    $fontSizeValue = Get-RequiredFiniteNumber $metadata 'fontSize'
    $minimumObservedSpan = [int][math]::Max(8, [math]::Floor(([double]$viewport[1] / ($fontSizeValue * 2.5)) / 2))

    # 墙钟与单调时钟长度只作采样窗完整性交叉核对，不推导物理显示时刻或帧率。
    $started = Get-RequiredNumber $Report 'samplingStartedUnixMs'
    $finished = Get-RequiredNumber $Report 'samplingFinishedUnixMs'
    $elapsed = Get-RequiredNumber $Report 'samplingElapsedMs'
    if ($finished -le $started -or [math]::Abs(($finished - $started) - $elapsed) -gt 10) { throw 'Plugin list sampling interval is invalid.' }
    if ((Get-RequiredNumber $Report 'minimumObservedSpan') -ne $minimumObservedSpan) { throw 'Plugin list minimum observed span does not match viewport and font metadata.' }

    # 分页计数证明目标行来自本轮新成功页；成功页清除错误文本也不能抹掉累计错误。
    $fetchWindow = Get-RequiredObject $Report 'fetchWindow'
    $fetchStart = Get-RequiredObject $fetchWindow 'start'
    $fetchEnd = Get-RequiredObject $fetchWindow 'end'
    $startSuccesses = Get-RequiredNumber $fetchStart 'successes'
    $endSuccesses = Get-RequiredNumber $fetchEnd 'successes'
    if ((Get-RequiredNumber $fetchStart 'errors') -ne 0 -or (Get-RequiredNumber $fetchEnd 'errors') -ne 0) { throw 'Plugin list observed pagination errors during sampling.' }
    if ($endSuccesses -le $startSuccesses -or $endSuccesses -le 0) { throw 'Plugin list sampling window did not add new successful fetches.' }
    if ((Get-RequiredNumber $fetchEnd 'requests') -lt $endSuccesses -or (Get-RequiredNumber $fetchStart 'requests') -lt $startSuccesses) { throw 'Plugin list fetch counters are inconsistent.' }

    $assertReady = {
        # 真实失败状态带 Failed(...) 诊断，必须按前缀识别并原样保留；普通未就绪单独报。
        # 即使状态为 Ready，renderer 或集合上的异步错误也必须立即失败并保留原文。
        # @author kongweiguang
        param($List, [string]$Context)
        $status = Get-RequiredString $List 'status'
        if ($status -cne 'Ready') {
            if ($status.StartsWith('Failed', [StringComparison]::Ordinal)) { throw "Plugin list renderer entered $status for $Context." }
            throw "Plugin list renderer is not ready and visible: $Context."
        }
        if (!(Get-RequiredBoolean $List 'visible')) { throw "Plugin list renderer is not ready and visible: $Context." }
        $listError = $List.PSObject.Properties['error']
        if ($null -ne $listError -and $null -ne $listError.Value) {
            if ($listError.Value -isnot [string] -or [string]::IsNullOrWhiteSpace($listError.Value)) { throw "Plugin list renderer error is invalid for $Context." }
            throw "Plugin list renderer error for $Context`: $($listError.Value)"
        }
        if ((Get-RequiredString $List 'plugin') -cne 'test.list-performance' -or (Get-RequiredString $List 'view') -cne 'records' -or (Get-RequiredString $List 'runtime') -cne 'script' -or !(Get-RequiredBoolean $List 'external')) { throw "Plugin list renderer source identity is invalid for $Context." }
    }

    $assertCollectionError = {
        # 分页错误进入快照门禁；字段缺失、非字符串或空字符串都不能当作无错误。
        # 累计错误计数即使被成功页清除可见文本也必须保持为零，并保证成功数不超过请求数。
        # @author kongweiguang
        param($Collection, [string]$Context)
        if ($null -eq $Collection.PSObject.Properties['error']) { throw "Plugin list collection error field is missing for $Context." }
        $requests = Get-RequiredNumber $Collection 'fetch_requests'
        $successes = Get-RequiredNumber $Collection 'fetch_successes'
        $errors = Get-RequiredNumber $Collection 'fetch_errors'
        if ($errors -ne 0) { throw "Plugin list collection recorded $errors pagination errors for $Context." }
        if ($successes -gt $requests) { throw "Plugin list collection success counter exceeds requests for $Context." }
        $collectionError = $Collection.PSObject.Properties['error'].Value
        if ($null -eq $collectionError) { return }
        if ($collectionError -isnot [string] -or [string]::IsNullOrWhiteSpace($collectionError)) { throw "Plugin list collection error is invalid for $Context." }
        throw "Plugin list collection error for $Context`: $collectionError"
    }

    # 首屏范围是整条观测链的锚点：必须从真实第0行范围开始，且不能是空范围。
    $first = Get-RequiredObject $Report 'firstScreen'
    $firstList = Get-RequiredObject $first 'list'
    & $assertReady $firstList 'first screen'
    $firstCollection = Get-RequiredObject $firstList 'collection'
    & $assertCollectionError $firstCollection 'first screen'
    if ((Get-RequiredNumber $firstCollection 'total') -ne 100000) { throw 'Plugin list first screen collection is invalid.' }
    $firstStart = Get-RequiredNumber $firstCollection 'visible_start'
    $firstEnd = Get-RequiredNumber $firstCollection 'visible_end'
    if ($firstStart -gt 0 -or $firstEnd -le 0 -or $firstEnd -gt 100000) { throw 'Plugin list first screen visible range does not anchor row 0.' }
    $firstRows = Get-RequiredArray $firstCollection 'visible_rows'
    if (@($firstRows | Where-Object { (Get-RequiredNumber $_ 'index') -eq 0 -and (Get-RequiredString $_ 'key') -ceq 'record-0' }).Count -eq 0) { throw 'Plugin list first screen did not construct row 0.' }
    $instance = Get-RequiredString $firstList 'instance'
    $generation = Get-RequiredNumber $firstList 'generation'

    # 预暖必须是逐条真实观测，而不是报告里的口头目标声明；每条也参与范围链。
    $warmup = Get-RequiredArray $Report 'warmupTargets'
    if ($warmup.Count -ne 2 -or (Get-RequiredElementNumber $warmup 0 'warmupTargets[0]') -ne 50000 -or (Get-RequiredElementNumber $warmup 1 'warmupTargets[1]') -ne 20000) { throw 'Plugin list warmup targets are invalid.' }
    $warmupObservations = Get-RequiredArray $Report 'warmupObservations'
    if ($warmupObservations.Count -ne $warmup.Count) { throw 'Plugin list warmup observations are incomplete.' }
    $previousStart = $firstStart
    $previousEnd = $firstEnd
    for ($i = 0; $i -lt $warmupObservations.Count; $i++) {
        $warmupObservation = $warmupObservations[$i]
        $warmupTarget = Get-RequiredElementNumber $warmup $i "warmupTargets[$i]"
        if ((Get-RequiredString $warmupObservation 'phase') -cne 'warmup' -or (Get-RequiredNumber $warmupObservation 'step') -ne $i) { throw 'Plugin list warmup observation phase or step is invalid.' }
        if ((Get-RequiredString $warmupObservation 'inputMode') -cne 'production VirtualListScrollHandle.scroll_to_item') { throw 'Plugin list warmup input mode is invalid.' }
        $warmupIndex = Get-RequiredNumber $warmupObservation 'index'
        if ($warmupIndex -ne $warmupTarget) { throw 'Plugin list warmup observation target does not match warmupTargets.' }
        $warmupBaselineStart = Get-RequiredNumber $warmupObservation 'baselineVisibleStart'
        $warmupBaselineEnd = Get-RequiredNumber $warmupObservation 'baselineVisibleEnd'
        if ($warmupBaselineStart -ne $previousStart -or $warmupBaselineEnd -ne $previousEnd) { throw 'Plugin list warmup baseline is not the visible range observed by the previous step.' }
        if ($warmupBaselineEnd -le $warmupBaselineStart -or $warmupBaselineEnd -gt 100000) { throw 'Plugin list warmup baseline visible range is invalid.' }
        if ($warmupIndex -ge $warmupBaselineStart -and $warmupIndex -lt $warmupBaselineEnd) { throw 'Plugin list warmup target was already visible before the scroll request.' }
        $warmupList = Get-RequiredObject $warmupObservation 'list'
        & $assertReady $warmupList 'warmup'
        if ((Get-RequiredString $warmupList 'instance') -cne $instance -or (Get-RequiredNumber $warmupList 'generation') -ne $generation) { throw 'Plugin list warmup observation is not from the same open instance and generation.' }
        $warmupCollection = Get-RequiredObject $warmupList 'collection'
        & $assertCollectionError $warmupCollection 'warmup'
        if ((Get-RequiredNumber $warmupCollection 'total') -ne 100000) { throw 'Plugin list warmup collection identity is invalid.' }
        $warmupVisibleStart = Get-RequiredNumber $warmupCollection 'visible_start'
        $warmupVisibleEnd = Get-RequiredNumber $warmupCollection 'visible_end'
        if ($warmupVisibleEnd -le $warmupVisibleStart -or $warmupVisibleEnd -gt 100000) { throw 'Plugin list warmup observed visible range is invalid.' }
        if ((Get-RequiredNumber $warmupObservation 'visibleStart') -ne $warmupVisibleStart -or (Get-RequiredNumber $warmupObservation 'visibleEnd') -ne $warmupVisibleEnd) { throw 'Plugin list warmup recorded range does not match the renderer snapshot.' }
        if ($warmupVisibleStart -eq $warmupBaselineStart -and $warmupVisibleEnd -eq $warmupBaselineEnd) { throw 'Plugin list warmup target was observed without a new visible range after the scroll request.' }
        if ($warmupIndex -lt $warmupVisibleStart -or $warmupIndex -ge $warmupVisibleEnd) { throw 'Plugin list warmup observed visible range does not contain the target row.' }
        $warmupRows = Get-RequiredArray $warmupCollection 'visible_rows'
        if (@($warmupRows | Where-Object { (Get-RequiredNumber $_ 'index') -eq $warmupIndex -and (Get-RequiredString $_ 'key') -ceq "record-$warmupIndex" }).Count -eq 0) { throw 'Plugin list warmup target business row was not constructed in the observed range.' }
        $previousStart = $warmupVisibleStart
        $previousEnd = $warmupVisibleEnd
    }

    $rows = @($Observations)
    if ($rows.Count -ne 515) { throw 'Plugin list business row observations are incomplete.' }
    $expectedRoundtrip = @(0,49999,99999,49999,0,49999,0,99999,0,49999,0,99999,49999,0,49999)
    $previousTarget = 20000
    $previousObserved = 0
    for ($i = 0; $i -lt $rows.Count; $i++) {
        $observation = $rows[$i]
        $expectedPhase = if ($i -lt 500) { 'continuous' } else { 'roundtrip' }
        if ((Get-RequiredString $observation 'phase') -cne $expectedPhase) { throw 'Plugin list observation phase is invalid.' }
        $expectedStep = if ($i -lt 500) { $i } else { $i - 500 }
        if ((Get-RequiredNumber $observation 'step') -ne $expectedStep) { throw 'Plugin list observation step is invalid.' }
        if ((Get-RequiredString $observation 'inputMode') -cne 'production VirtualListScrollHandle.scroll_to_item') { throw 'Plugin list observation input mode is invalid.' }
        $index = Get-RequiredNumber $observation 'index'
        $baselineStart = Get-RequiredNumber $observation 'baselineVisibleStart'
        $baselineEnd = Get-RequiredNumber $observation 'baselineVisibleEnd'
        # 第 i 步请求前范围必须等于第 i-1 步实际观测范围；首步继承预暖末范围，首屏是整链锚点。
        if ($baselineStart -ne $previousStart -or $baselineEnd -ne $previousEnd) { throw 'Plugin list observation baseline is not the visible range observed by the previous step.' }
        if ($baselineEnd -le $baselineStart -or $baselineEnd -gt 100000) { throw 'Plugin list baseline visible range is invalid.' }
        if ($i -lt 500) {
            # 连续阶段的可解释动态步距：目标 = 请求前可视末尾 + 当前可视跨度，至少领先一整页。
            $span = $baselineEnd - $baselineStart
            $expected = $baselineEnd + $span
            if ($index -ne $expected -or $index -le $previousTarget -or $index -ge 100000) { throw 'Plugin list continuous target does not follow the declared one-page forward step.' }
        } else {
            if ($index -ne $expectedRoundtrip[$i - 500] -or $index -eq $previousTarget) { throw 'Plugin list roundtrip target sequence is invalid.' }
        }
        if ($index -ge $baselineStart -and $index -lt $baselineEnd) { throw 'Plugin list target was already visible before the scroll request.' }
        $requested = Get-RequiredNumber $observation 'requestedNs'
        $observed = Get-RequiredNumber $observation 'observedNs'
        if ($requested -lt $previousObserved -or $observed -lt $requested -or $observed -gt ($elapsed + 1) * 1000000) { throw 'Plugin list observation timing or order is invalid.' }
        $previousObserved = $observed
        $list = Get-RequiredObject $observation 'list'
        & $assertReady $list 'observation'
        if ((Get-RequiredString $list 'instance') -cne $instance -or (Get-RequiredNumber $list 'generation') -ne $generation) { throw 'Plugin list observation is not from the same open instance and generation.' }
        $collection = Get-RequiredObject $list 'collection'
        & $assertCollectionError $collection 'observation'
        if ((Get-RequiredNumber $collection 'total') -ne 100000) { throw 'Plugin list collection identity is invalid.' }
        $visibleStart = Get-RequiredNumber $collection 'visible_start'
        $visibleEnd = Get-RequiredNumber $collection 'visible_end'
        if ($visibleEnd -le $visibleStart -or $visibleEnd -gt 100000) { throw 'Plugin list observed visible range is invalid.' }
        if ((Get-RequiredNumber $observation 'visibleStart') -ne $visibleStart -or (Get-RequiredNumber $observation 'visibleEnd') -ne $visibleEnd) { throw 'Plugin list recorded range does not match the renderer snapshot.' }
        if ($visibleStart -eq $baselineStart -and $visibleEnd -eq $baselineEnd) { throw 'Plugin list target was observed without a new visible range after the scroll request.' }
        # 连续和往返阶段都要求完整视口跨度；两行残片不能证明目标页。
        $observedSpan = $visibleEnd - $visibleStart
        if ($observedSpan -lt $minimumObservedSpan) { throw 'Plugin list observation span is smaller than the viewport-derived minimum.' }
        if ($i -lt 500) {
            # 连续阶段只接受真实前向：回到请求前范围或第0行、落后目标超过两行、
            # 跨度差超过两行都不是同一页前进，即使目标行偶然落在新范围内也必须失败。
            $baselineSpan = $baselineEnd - $baselineStart
            if ($visibleStart -lt $baselineEnd) { throw 'Plugin list continuous observation did not advance past the baseline range.' }
            # 目标可以落在新视口起点之后一页内；只要求起点越过请求前范围且目标在新范围内。
            if ([math]::Abs($observedSpan - $baselineSpan) -gt 2) { throw 'Plugin list continuous observation span drifted from the baseline range.' }
        }
        if ($index -lt $visibleStart -or $index -ge $visibleEnd) { throw 'Plugin list observed visible range does not contain the target row.' }
        $visibleRows = Get-RequiredArray $collection 'visible_rows'
        if (@($visibleRows | Where-Object { (Get-RequiredNumber $_ 'index') -eq $index -and (Get-RequiredString $_ 'key') -ceq "record-$index" }).Count -eq 0) { throw 'Plugin list target business row was not constructed in the observed range.' }
        $previousStart = $visibleStart
        $previousEnd = $visibleEnd
        $previousTarget = $index
    }

    # 原始帧只证明真实draw/present样本存在，不据此宣称固定帧数、系统合成或物理显示通过。
    $frameRows = @($Frames)
    if ($frameRows.Count -lt 1) { throw 'Plugin list raw frames are missing.' }
    $draw = 0
    $present = 0
    $drawFirst = 0; $drawSecond = 0; $presentFirst = 0; $presentSecond = 0
    $halfLimit = ($elapsed * 1000000) / 2
    foreach ($frame in $frameRows) {
        $kind = Get-RequiredString $frame 'kind'
        $frameStart = Get-RequiredNumber $frame 'startNs'
        $frameEnd = Get-RequiredNumber $frame 'endNs'
        if ($frameEnd -lt $frameStart) { throw 'Plugin list raw frame timestamps are invalid.' }
        if ($frameStart -gt ($elapsed + 1) * 1000000 -or $frameEnd -gt ($elapsed + 1) * 1000000) { throw 'Plugin list raw frame falls outside the sampling window.' }
        $firstHalf = $frameStart -lt $halfLimit
        if ($kind -ceq 'draw') { $draw++; if ($firstHalf) { $drawFirst++ } else { $drawSecond++ } } elseif ($kind -ceq 'present') { $present++; if ($firstHalf) { $presentFirst++ } else { $presentSecond++ } } else { throw 'Plugin list raw frame kind is invalid.' }
    }
    # 与 Rust 驱动的 MIN_FRAME_SAMPLES=100 对齐；前后两半各需 50 个 draw/present 样本。
    if ($draw -lt 100 -or $present -lt 100) { throw 'Plugin list has fewer than one hundred real draw and present samples.' }
    if ($drawFirst -lt 50 -or $drawSecond -lt 50 -or $presentFirst -lt 50 -or $presentSecond -lt 50) { throw 'Plugin list draw or present samples are missing from one half of the sampling window.' }
    if ($null -eq $Samples -or @($Samples).Count -lt 2) { throw 'Plugin list process samples are incomplete.' }
    # 进程树证据绑定真实桌面与脚本宿主；缺失宿主、映像目录不符或文件摘要不符都不能通过。
    if ([string]::IsNullOrWhiteSpace($CandidatePath) -or [string]::IsNullOrWhiteSpace($CandidateSha256)) { throw 'Plugin list candidate identity is missing.' }
    $processEvidence = Assert-PluginListProcessTree $ProcessTree $CandidatePath $CandidateSha256 ([int](Get-RequiredNumber $Report 'processId')) $ExpectedHostSha256 $PluginCopyPath $PluginId

    return [pscustomobject]@{
        Observations = $rows.Count
        Continuous = 500
        Roundtrip = 15
        Instance = $instance
        Generation = $generation
        ContentId = $contentId
        SourcePlugin = $sourcePlugin
        SourceView = $sourceView
        SourceRuntime = $sourceRuntime
        SourceTabId = $sourceTabId
        ProcessTree = $processEvidence
        ActiveAtSamplingStart = (Get-RequiredBoolean $metadata 'active')
        DarkMode = $darkMode
        MinimumObservedSpan = $minimumObservedSpan
        FetchWindow = $fetchWindow
        TargetSequence = (($rows | ForEach-Object { "$($_.phase):$($_.index)" }) -join ',')
        StartedUnixMs = $started
        FinishedUnixMs = $finished
        ElapsedMs = $elapsed
        DrawFrames = $draw
        PresentFrames = $present
        PhysicalDisplayMeasured = $false
        FrameCountClaimed = $false
    }
}

$executable=(Resolve-Path -LiteralPath $ExecutablePath).Path
$repositoryRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
if(-not $PluginPath){$PluginPath=Join-Path $repositoryRoot 'tests/fixtures/plugins/list-performance'}
$plugin=(Resolve-Path -LiteralPath $PluginPath).Path
$base=[IO.Path]::GetFullPath($OutputDirectory)
if(Test-Path -LiteralPath $base){throw 'The output root must not exist.'}
[void][IO.Directory]::CreateDirectory($base)
$candidate=(Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$ExpectedHostSha256=$ExpectedHostSha256.ToLowerInvariant()
if($ExpectedHostSha256 -notmatch '^[0-9a-f]{64}$'){throw 'ExpectedHostSha256 must be a lowercase or uppercase SHA256 hex digest.'}
$copyDigest=$null
Get-ListHardwareMetadata | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $base 'hardware.json') -Encoding utf8
$previous=[Environment]::GetEnvironmentVariable('KERMINAL_PLUGIN_WEBVIEW_DIRECTORY','Process')
$previousTheme=[Environment]::GetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME','Process')
$process=$null
$receipts=@()
$primaryFailure=$null
try {
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_WEBVIEW_DIRECTORY',$plugin,'Process')
    Initialize-PluginListLauncher
    foreach($currentTheme in $(if($Theme -ceq 'all'){@('light','dark','system')}else{@($Theme)})) {
        $output=Join-Path $base $currentTheme
        [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME',$currentTheme,'Process')
        $desktop='KerminalRail-settings-'+[Guid]::NewGuid().ToString('N')
        $process=[Kerminal.PluginListPerformance.NativeDesktopLauncher]::Start($executable,$repositoryRoot,$output,(Join-Path $base "$currentTheme.stdout.log"),(Join-Path $base "$currentTheme.stderr.log"),$desktop,1024,720)
        $samples=[Collections.Generic.List[object]]::new()
        # 进程树采样独立于内存采样并降低频率；宿主的短命重启仍会被任一有效样本捕获。
        $treeSamples=[Collections.Generic.List[object]]::new()
        $imageHashes=@{}
        $nextTreeAt=0
        $timer=[Diagnostics.Stopwatch]::StartNew()
        $deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        while(-not $process.WaitForExit(100)) {
            if([DateTime]::UtcNow -ge $deadline){throw 'Native list sampling timed out.'}
            $sample=Get-ListProcessSample -OwnedProcessId $process.ProcessId -ElapsedMilliseconds $timer.ElapsedMilliseconds
            if($null -ne $sample){$samples.Add($sample)}
            if($timer.ElapsedMilliseconds -ge $nextTreeAt){
                $treeSamples.Add((Get-ListProcessTreeSample -RootProcessId $process.ProcessId -ElapsedMilliseconds $timer.ElapsedMilliseconds -ImageHashes $imageHashes))
                $nextTreeAt=$timer.ElapsedMilliseconds+500
            }
        }
        $samples | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $base "$currentTheme.memory-samples.json") -Encoding utf8
        $treeSamples | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $base "$currentTheme.process-tree.json") -Encoding utf8
        $exitCode=$process.GetExitCode()
        if($exitCode -ne 0){
            $stderrPath=Join-Path $base "$currentTheme.stderr.log"
            $stderr=$(if(Test-Path -LiteralPath $stderrPath){(Get-Content -LiteralPath $stderrPath -Tail 20) -join "`n"}else{'stderr log unavailable'})
            throw "Native list sampling failed: $currentTheme; exitCode=$exitCode; stderr=$stderrPath`n$stderr"
        }
        $report=Get-Content -LiteralPath (Join-Path $output 'report.json') -Raw | ConvertFrom-Json
        $frames=@(Get-Content -LiteralPath (Join-Path $output 'raw-frames.json') -Raw | ConvertFrom-Json)
        $rows=@(Get-Content -LiteralPath (Join-Path $output 'row-observations.json') -Raw | ConvertFrom-Json)
        # 先在候选仍可回收时完成全部证据核对；任何歧义都进入finally统一清理。
        $copyPath=[IO.Path]::GetFullPath((Join-Path $output 'plugin'))
        # 宿主实际读取的是输出根内只读副本；摘要与路径都由同一函数从该副本重算。
        $summary=Assert-PluginListEvidence $report $rows $frames $samples $treeSamples $currentTheme $desktop $executable $candidate $ExpectedHostSha256 $copyPath 'test.list-performance'
        $copyEvidence=Assert-PluginListPluginCopy $report $output $plugin
        $copyDigest=$copyEvidence.Digest
        if((Get-RequiredNumber $report 'processId') -ne $process.ProcessId -or (Get-RequiredString $report 'candidateSha256') -cne $candidate){throw 'Native list candidate or process identity mismatch.'}
        $deadline=[DateTime]::UtcNow.AddSeconds(10)
        while($process.GetJobSnapshot().ActiveProcesses -gt 0 -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
        $job=$process.GetJobSnapshot()
        if($job.ActiveProcesses -ne 0){throw 'Native list sampling left running descendants.'}
        $processId=$process.ProcessId
        # Dispose 失败时保留 $process 供 finally 继续回收；成功则先清空引用，避免 finally 对已释放句柄二次查询。
        $owned=$process
        $owned.Dispose()
        $process=$null
        if(-not $owned.DesktopClosed){throw 'The private desktop did not close.'}
        $receipts+=[pscustomobject]@{Theme=$currentTheme;ProcessId=$processId;Report="$currentTheme/report.json";MemorySamples="$currentTheme/memory-samples.json";ProcessTree="$currentTheme.process-tree.json";Observations=$summary.Observations;TargetSequence=$summary.TargetSequence;Instance=$summary.Instance;Generation=$summary.Generation;ContentId=$summary.ContentId;SourcePlugin=$summary.SourcePlugin;SourceView=$summary.SourceView;SourceRuntime=$summary.SourceRuntime;SourceTabId=$summary.SourceTabId;PluginCopyPath=$copyEvidence.CopyPath;PluginSha256=$copyDigest;ExpectedHostSha256=$ExpectedHostSha256;CandidateImagePath=$summary.ProcessTree.RootImagePath;CandidateImageSha256=$summary.ProcessTree.RootImageSha256;HostProcesses=$summary.ProcessTree.HostProcesses;ProcessTreeSha256=$summary.ProcessTree.ProcessTreeSha256;ProcessTreeSamples=$summary.ProcessTree.SampleCount;ActiveAtSamplingStart=$summary.ActiveAtSamplingStart;DarkMode=$summary.DarkMode;MinimumObservedSpan=$summary.MinimumObservedSpan;FetchWindow=$summary.FetchWindow;ContinuousStepRule=(Get-RequiredString $report 'continuousStepRule');NormalExitJob=$job;DesktopHandleClosed=$true}
    }
    if((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidate){throw 'Candidate changed during sampling.'}
    [pscustomobject]@{SchemaVersion=3;CandidateSha256=$candidate;PluginSha256=$copyDigest;PluginDigestRule=(Get-PluginDigestRule);Runs=$receipts;Mode='private-desktop-native-list';PhysicalInputTested=$false;DisplayLatencyMeasured=$false;FrameCountClaimed=$false;PhysicalDisplayMeasured=$false;EvidenceScope='production native renderer row construction and visible-range changes; active state sampled once at start; not system compositing, physical display, focus continuity or input-to-display latency';MemoryScope='candidate desktop process; excludes child process memory';Hardware='hardware.json';PluginCopyScope='each run executes the immutable copy under its output root and the runner recomputes the same full-file digest'}| ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $base 'acceptance-report.json') -Encoding utf8
} catch {
    # 原始失败必须先保存；finally 里的清理诊断不能把它覆盖成"清理失败"。
    $primaryFailure=$_
} finally {
    # 清理失败只聚合为附加诊断，并继续尝试后续步骤；环境变量恢复始终执行。
    $cleanupErrors=Complete-OwnedProcessCleanup $process
    try { [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_WEBVIEW_DIRECTORY',$previous,'Process') } catch { $cleanupErrors.Add("Restore KERMINAL_PLUGIN_WEBVIEW_DIRECTORY failed: $($_.Exception.Message)") }
    try { [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME',$previousTheme,'Process') } catch { $cleanupErrors.Add("Restore KERMINAL_WEBVIEW_PROBE_THEME failed: $($_.Exception.Message)") }
    $failure=Get-CleanupFailureMessage $primaryFailure $cleanupErrors
    if($null -ne $failure){throw $failure}
}
