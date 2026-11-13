# @author kongweiguang
# 仅启动 Agent-only 原生验收：复用现有私有桌面 launcher，但把入口收紧到
# --agent-continuity-only，并把用户目录、模块搜索路径和临时目录全部指向本轮隔离根。
# 该脚本不构建、不读取用户历史、不按 PID 结束进程；后代回收依赖 launcher 的 Job Object，
# 只有 active process 计数归零并关闭私有桌面后才报告成功，超时或计数未归零直接失败。
# 必须用 pwsh.exe -NoProfile -NonInteractive -File 启动；启动参数在入口处核对并 fail-closed，
# 因为包装进程一旦加载用户 profile 或进入交互模式，就会写入用户历史并污染隔离结论。
[CmdletBinding()]
param(
    [string]$ExecutablePath = '',
    [ValidateSet('debug', 'release')]
    [string]$BuildProfile = 'debug',
    [string]$RunDirectory = '',
    [ValidateRange(800, 3840)]
    [uint32]$Width = 1440,
    [ValidateRange(600, 2160)]
    [uint32]$Height = 900,
    [ValidateRange(30, 1800)]
    [int]$TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    # 验收前置条件失败必须立即中止，避免后面用默认值或旧回执掩盖错误。
    # @author kongweiguang
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Get-ProcessEnvironmentSnapshot {
    # 只复制变量是否存在和值，用于 finally 精确恢复，不修改父进程的持久环境。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string[]]$Names)
    $environment = [Environment]::GetEnvironmentVariables('Process')
    $snapshot = @{}
    foreach ($name in $Names) {
        $snapshot[$name] = @{ Present = $environment.Contains($name); Value = $environment[$name] }
    }
    return $snapshot
}

function Set-ProcessEnvironmentValue {
    # PowerShell 会把传给 .NET string 参数的 $null 绑定成空串；必须显式 NullString 才能删除变量，
    # 否则恢复“原本不存在”的变量时会留下空值并改变子进程的存在性判断。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Name, [AllowNull()][object]$Value)
    if ($null -eq $Value) {
        [Environment]::SetEnvironmentVariable($Name, [NullString]::Value, 'Process')
    } else {
        [Environment]::SetEnvironmentVariable($Name, [string]$Value, 'Process')
    }
}

function Write-Utf8 {
    # 回执使用无 BOM UTF-8，避免 JSON 读取方因 BOM 解析失败。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Content)
    [IO.File]::WriteAllText($Path, $Content, [Text.UTF8Encoding]::new($false))
}

function Test-PathWithinRoot {
    # 只按完整路径边界比较，防止 C:\run 误匹配 C:\run-other 或依赖当前目录。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Root)
    $pathValue = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $rootValue = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    return $pathValue.Equals($rootValue, [StringComparison]::OrdinalIgnoreCase) -or
        $pathValue.StartsWith($rootValue + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Assert-RunDirectoryText {
    # 触发文件路径会被嵌入 PowerShell 单引号字面量；智能单引号会提前闭合字面量，
    # 必须在启动前拒绝，而不是依赖生产引用 helper 只转义 ASCII 单引号。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { throw 'Agent continuity run directory is empty.' }
    foreach ($character in $Text.ToCharArray()) {
        if ([char]::IsControl($character) -or
            $character -in @([char]0x2018, [char]0x2019, [char]0x201A, [char]0x201B)) {
            throw 'Agent continuity run directory contains a control or Unicode single-quote terminator.'
        }
    }
}

function Assert-NoReparsePoint {
    # 验收目录必须落在真实目录上；junction/symlink 可能把输出或清理边界引到用户目录。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path)
    $current = [IO.Path]::GetFullPath($Path)
    while (-not [string]::IsNullOrWhiteSpace($current)) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Agent continuity path traverses a reparse point: $current"
            }
        }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ([string]::IsNullOrWhiteSpace($parent) -or $parent -eq $current) { break }
        $current = $parent
    }
}

function Get-InheritedKerminalEnvironmentNames {
    # 除本轮即将设置的配置根外，所有继承的 KERMINAL_* 开关都必须清除并纳入恢复快照；
    # 未枚举的并行验收或用户 shell 变量不能改变 Agent-only 的隔离结论。
    # @author kongweiguang
    @(
        [Environment]::GetEnvironmentVariables('Process').Keys |
            Where-Object { $_ -like 'KERMINAL_*' -and $_ -ine 'KERMINAL_CONFIG_ROOT' } |
            ForEach-Object { [string]$_ } |
            Sort-Object -Unique
    )
}

function Assert-PathOutsideUserConfiguration {
    # 输出不能位于真实用户配置/主目录下；这里只比较路径身份，不读取用户内容。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path)
    $userProfile = [Environment]::GetFolderPath('UserProfile')
    if ([string]::IsNullOrWhiteSpace($userProfile)) {
        throw 'Cannot resolve the real user profile for isolation checks.'
    }
    $roots = @($userProfile)
    foreach ($name in @('USERPROFILE', 'HOME', 'APPDATA', 'LOCALAPPDATA', 'KERMINAL_CONFIG_ROOT')) {
        $value = [Environment]::GetEnvironmentVariable($name, 'Process')
        if (-not [string]::IsNullOrWhiteSpace($value)) { $roots += $value }
    }
    foreach ($root in $roots) {
        if (Test-PathWithinRoot -Path $Path -Root $root) {
            throw "Agent continuity run directory must not be under a real user configuration root: $root"
        }
    }
}

function Resolve-Pwsh7 {
    # profile 使用裸 pwsh.exe，应用按 PATH 查找；启动前把首个命中固定下来并验证版本，
    # 避免用户 PATH 中的旧版或 shim 被当成验收 shell。
    # @author kongweiguang
    $commands = @(Get-Command -Name 'pwsh.exe' -CommandType Application -ErrorAction Stop)
    if ($commands.Count -eq 0 -or [string]::IsNullOrWhiteSpace([string]$commands[0].Source)) {
        throw 'pwsh.exe 7 was not found on PATH.'
    }
    $path = [IO.Path]::GetFullPath([string]$commands[0].Source)
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "pwsh.exe is missing: $path" }
    $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($path)
    $major = if ($version.ProductMajorPart -gt 0) { $version.ProductMajorPart } else { $version.FileMajorPart }
    if ($major -ne 7) { throw "Agent continuity requires pwsh 7, found version $major at $path." }
    $pwshHome = Split-Path -Parent $path
    $modules = Join-Path $pwshHome 'Modules'
    if (-not (Test-Path -LiteralPath $modules -PathType Container)) {
        throw "pwsh 7 built-in module directory is missing: $modules"
    }
    return [pscustomobject]@{ Path = $path; Home = $pwshHome; Modules = $modules }
}

function Get-AcceptanceExecutableHash {
    # 包装只记录本轮实际启动的构建产物身份；feature 缺失由主程序的 exit(2) 和
    # 专属报告缺失分别暴露，不再扫描二进制字符串或依赖外部构建回执。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Acceptance executable is missing: $Path"
    }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-OuterShellArguments {
    # 静态测试用 scriptblock 点源函数时 $PSScriptRoot 为空，所以源根与宿主参数都显式传入，
    # 脚本入口与测试才共用同一判定；任何缺项都必须在创建目录前失败关闭。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$SourceRoot,
        [AllowEmptyCollection()][string[]]$Arguments = @()
    )
    $missing = [Collections.Generic.List[string]]::new()
    $hasNoProfile = $false
    $hasNonInteractive = $false
    $fileIndex = -1
    for ($index = 0; $index -lt $Arguments.Count; $index++) {
        $value = [string]$Arguments[$index]
        if ($value -ieq '-NoProfile') { $hasNoProfile = $true; continue }
        if ($value -ieq '-NonInteractive') { $hasNonInteractive = $true; continue }
        # -NoLogo 只是关闭启动横幅，不改变 profile 或交互语义，允许保留常见启动写法。
        if ($value -ieq '-NoLogo') { continue }
        if ($value -ieq '-File') {
            $fileIndex = $index
            break
        }
        $missing.Add($value)
    }
    # 允许用 -File 启动，但必须显式指向本包装脚本；否则外层执行的是别的脚本，
    # 本文件里的隔离和清理门禁根本没有运行。
    $fileIsOwned = $false
    if ($fileIndex -ge 0 -and $fileIndex + 1 -lt $Arguments.Count) {
        $candidate = [IO.Path]::GetFullPath([string]$Arguments[$fileIndex + 1])
        $expected = [IO.Path]::GetFullPath((Join-Path $SourceRoot 'agent-continuity-native.ps1'))
        $fileIsOwned = $candidate.Equals($expected, [StringComparison]::OrdinalIgnoreCase)
    }
    if (-not $fileIsOwned) {
        throw 'Agent continuity wrapper must be launched with pwsh.exe -NoProfile -NonInteractive -File <agent-continuity-native.ps1>; refusing an interactive or profile-loading host.'
    }
    if (-not $hasNoProfile) {
        throw 'Agent continuity wrapper requires -NoProfile on the pwsh command line; refusing to continue with an unproven outer shell.'
    }
    if (-not $hasNonInteractive) {
        throw 'Agent continuity wrapper requires -NonInteractive on the pwsh command line; refusing to continue with an interactive host.'
    }
    if ($missing.Count -ne 0) {
        throw "Agent continuity wrapper received unsupported pwsh host arguments: $($missing -join ' ')"
    }
}

function Get-StartOverloadSignatures {
    # 共享 launcher 现在同时保留八参数兼容重载和九参数 Job 入口，旧的“Start 必须唯一”断言
    # 会把兼容重载误判成歧义。这里按括号配对收集每个 Start 的参数表，而不是用 IndexOf(')')
    # 截断，参数里出现默认值或泛型时也不会错位。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Source)

    $signatures = [Collections.Generic.List[object]]::new()
    foreach ($header in [regex]::Matches($Source, 'public\s+static\s+NativeDesktopProcess\s+Start\s*\(')) {
        $open = $header.Index + $header.Length - 1
        $depth = 0
        $close = -1
        for ($index = $open; $index -lt $Source.Length; $index++) {
            if ($Source[$index] -eq '(') { $depth++ }
            elseif ($Source[$index] -eq ')') {
                $depth--
                if ($depth -eq 0) { $close = $index; break }
            }
        }
        if ($close -lt 0) {
            throw 'The private desktop launcher Start signature is not closed.'
        }
        $parameters = @(
            $Source.Substring($open + 1, $close - $open - 1) -split ',' |
                ForEach-Object { $_.Trim() } |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
        )
        $signatures.Add([pscustomobject]@{
            HeaderIndex = $header.Index
            Close = $close
            Parameters = $parameters
        })
    }
    return $signatures
}

function Get-CSharpMethodBody {
    # 取方法体时按花括号配对并跳过字符串字面量，避免字符串中的花括号提前结束扫描；
    # 只用于核对 Start 重载，不承担通用 C# 解析。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][int]$CloseParenIndex
    )

    $openBrace = $Source.IndexOf('{', $CloseParenIndex + 1)
    if ($openBrace -lt 0) {
        throw 'The private desktop launcher Start body is missing.'
    }
    $depth = 0
    $inString = $false
    $escaped = $false
    for ($index = $openBrace; $index -lt $Source.Length; $index++) {
        $character = $Source[$index]
        if ($inString) {
            if ($escaped) { $escaped = $false }
            elseif ($character -eq '\') { $escaped = $true }
            elseif ($character -eq '"') { $inString = $false }
            continue
        }
        if ($character -eq '"') { $inString = $true; continue }
        if ($character -eq '{') { $depth++ }
        elseif ($character -eq '}') {
            $depth--
            if ($depth -eq 0) {
                return $Source.Substring($openBrace + 1, $index - $openBrace - 1)
            }
        }
    }
    throw 'The private desktop launcher Start body is not closed.'
}

function Assert-AgentContinuityStartContract {
    # Agent-only 运行必须走带 Job 的九参数入口；共享 launcher 为旧消费者恢复了八参数兼容重载，
    # 因此这里不再要求 Start 唯一，而是精确接受“八参数兼容 + 九参数 Job”两个形态：八参数只能
    # 以 false 委托九参数（保持旧消费者的无 Job 行为），九参数末位必须仍是 bool useJobObject。
    # 缺失、重复、三重重载、末位不是 bool、参数类型被换或八参数直接调用 StartCore 都失败关闭，
    # 防止用错误重载绕过 Job 回收。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Source)

    $signatures = @(Get-StartOverloadSignatures -Source $Source)
    if ($signatures.Count -ne 2) {
        throw "The private desktop launcher must expose exactly the 8-parameter compatibility overload and the 9-parameter job overload; found $($signatures.Count) Start method(s)."
    }
    $expectedPrefix = @(
        'string executable',
        'string workingDirectory',
        'string outputDirectory',
        'string stdoutPath',
        'string stderrPath',
        'string desktopName',
        'uint width',
        'uint height'
    )
    $eight = @($signatures | Where-Object { $_.Parameters.Count -eq 8 })
    $nine = @($signatures | Where-Object { $_.Parameters.Count -eq 9 })
    if ($eight.Count -ne 1 -or $nine.Count -ne 1) {
        throw 'The private desktop launcher must expose one 8-parameter and one 9-parameter Start overload.'
    }
    foreach ($overload in @($eight[0], $nine[0])) {
        for ($index = 0; $index -lt $expectedPrefix.Count; $index++) {
            if ($overload.Parameters[$index] -cne $expectedPrefix[$index]) {
                throw "The Start overload parameter $index must remain '$($expectedPrefix[$index])'."
            }
        }
    }
    if ($nine[0].Parameters[8] -cnotmatch '^bool\s+useJobObject$') {
        throw 'The ninth Start parameter must remain the bool job-object switch.'
    }
    # 八参数重载只能转交九参数并显式传 false；如果它直接调用 StartCore、传 true 或另建
    # 启动路径，旧消费者会被静默升级为 Job 语义或绕过九参数入口的共享校验。
    $eightBody = Get-CSharpMethodBody -Source $Source -CloseParenIndex $eight[0].Close
    $delegations = [regex]::Matches($eightBody, '\bStart\s*\(')
    if ($delegations.Count -ne 1 -or
        $eightBody -notmatch '(?s)\bStart\s*\([^)]*,\s*false\s*\)\s*;' -or
        $eightBody -match '\btrue\b' -or
        $eightBody.Contains('StartCore') -or
        $eightBody.Contains('CreateProcess')) {
        throw 'The 8-parameter Start overload must delegate to the 9-parameter overload with useJobObject=false.'
    }
    # 九参数重载必须把 useJobObject 透传给 StartCore，不能自行改成固定值。
    $nineBody = Get-CSharpMethodBody -Source $Source -CloseParenIndex $nine[0].Close
    if ([regex]::Matches($nineBody, '\bStartCore\s*\(').Count -ne 1 -or
        $nineBody -notmatch '(?s)\bStartCore\s*\([^)]*useJobObject') {
        throw 'The 9-parameter Start overload must forward useJobObject to StartCore.'
    }
}

function Get-AgentContinuityDesktopLauncherSource {
    # 复用既有 launcher 的私有桌面、句柄白名单和流重定向；只替换唯一入口命令，
    # 不复制 Win32 细节，也不修改共享脚本本身。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$SourceRoot)
    $tokens = $null
    $errors = $null
    $sourcePath = Join-Path $SourceRoot 'settings-parity.ps1'
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
    $new = '+ " --right-rail-acceptance --agent-continuity-only --output-dir " + QuoteArgument(outputDirectory)'
    $first = $source.IndexOf($old, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $source.IndexOf($old, $first + $old.Length, [StringComparison]::Ordinal) -ge 0) {
        throw 'The existing launcher command has changed; refusing an unverified replacement.'
    }
    $result = $source.Replace($old, $new).Replace(
        'namespace Kerminal.SettingsParity',
        'namespace Kerminal.AgentContinuityAcceptance'
    )
    # 共享脚本仍在返工，不能把“字符串里出现过这些词”当成可调用契约。这里提取出的正文是
    # C#，不能交给 PowerShell 解析器；用锚定正则核对八参数兼容重载和九参数 Job 重载的
    # 精确形态与委托关系，再逐项核对私有桌面、Job 与代理回读属性，避免旧静态检查冒充最终兼容证明。
    Assert-AgentContinuityStartContract -Source $result
    $startCoreHeaders = [regex]::Matches(
        $result,
        'private\s+static\s+NativeDesktopProcess\s+StartCore\s*\('
    )
    if ($startCoreHeaders.Count -ne 1) {
        throw 'The private desktop launcher must expose exactly one StartCore method.'
    }
    $startCoreOpen = $startCoreHeaders[0].Index + $startCoreHeaders[0].Length - 1
    $startCoreClose = $result.IndexOf(')', $startCoreOpen)
    if ($startCoreClose -lt 0) {
        throw 'The private desktop launcher StartCore signature is not closed.'
    }
    $startCoreParameters = @(
        $result.Substring($startCoreOpen + 1, $startCoreClose - $startCoreOpen - 1) -split ',' |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    if ($startCoreParameters.Count -ne 9) {
        throw "The private desktop launcher StartCore signature changed; expected 9 parameters, found $($startCoreParameters.Count)."
    }
    foreach ($required in @(
        'CreateDesktopW',
        'WinSta0',
        'QuoteArgument',
        'KerminalRail-settings-',
        'CreateJobObject',
        'AssignProcessToJobObject',
        'TerminateJobObject',
        'QueryActiveProcessCount',
        'JobAssigned',
        'JobCleanupVerified',
        'JobCleanupForced',
        'JobActiveProcessesAtExit',
        'DesktopClosed'
    )) {
        if (-not $result.Contains($required)) {
            throw "The private desktop launcher source lost a required safety element: $required"
        }
    }
    return $result
}

function Assert-AgentContinuityCapture {
    # 只接受本轮输出目录内的 PNG 证据，并核对文件名、PNG 签名和 IHDR 尺寸，防止报告指向
    # 目录外路径、空文件或尺寸不符的占位物。这里不解码像素，因此它证明的是“证据文件存在且
    # 尺寸一致”，可见画面本身仍由 Rust 侧稳定帧采样和全透明拒绝负责，不能据此宣称视觉通过。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Capture,
        [Parameter(Mandatory = $true)][string]$OutputDirectory,
        [Parameter(Mandatory = $true)][string]$Operation
    )
    $fileName = [string]$Capture.file
    if ($fileName -cne "functional-$Operation.png" -or [IO.Path]::GetFileName($fileName) -cne $fileName) {
        throw "Functional evidence file name is invalid: $Operation $fileName"
    }
    $path = Join-Path $OutputDirectory $fileName
    if (-not (Test-PathWithinRoot -Path $path -Root $OutputDirectory)) {
        throw "Functional evidence escapes the output directory: $fileName"
    }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Functional evidence is missing: $fileName"
    }
    $bytes = [IO.File]::ReadAllBytes($path)
    if ($bytes.Length -lt 24) { throw "Functional evidence is too small: $fileName" }
    $signature = [byte[]]@(137, 80, 78, 71, 13, 10, 26, 10)
    for ($index = 0; $index -lt $signature.Length; $index++) {
        if ($bytes[$index] -ne $signature[$index]) { throw "Functional evidence is not a PNG: $fileName" }
    }
    if ([Text.Encoding]::ASCII.GetString($bytes, 12, 4) -cne 'IHDR') {
        throw "Functional evidence has no PNG IHDR chunk: $fileName"
    }
    $pngWidth = [uint64]$bytes[16] * 16777216 + [uint64]$bytes[17] * 65536 + [uint64]$bytes[18] * 256 + [uint64]$bytes[19]
    $pngHeight = [uint64]$bytes[20] * 16777216 + [uint64]$bytes[21] * 65536 + [uint64]$bytes[22] * 256 + [uint64]$bytes[23]
    if ($pngWidth -ne [uint64]$Capture.width -or $pngHeight -ne [uint64]$Capture.height) {
        throw "Functional evidence dimensions differ from the report: $fileName"
    }
    # pixelSha256 是 renderer 原始像素摘要，不是 PNG 文件摘要；这里不重算或比较文件哈希，
    # 只要求报告字段格式合法，避免把“文件像 PNG”冒充成可见画面证据。真实渲染结论仍依赖
    # Rust 侧 save_frame 的全透明拒绝、IHDR 尺寸和稳定帧等待，以及下方 checkpoint 唯一性。
    if ([string]$Capture.pixelSha256 -cnotmatch '^[0-9a-f]{64}$') {
        throw "Functional evidence pixel digest is invalid: $fileName"
    }
}

function Assert-AgentContinuityReport {
    # 运行后仍以报告和真实证据闭环：模式、隔离、配置根、进程身份、12 个检查点
    # 以及空的远程采集列表必须同时成立，不能只凭进程退出码声称验收完成。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$OutputDirectory,
        [Parameter(Mandatory = $true)][string]$ConfigDirectory,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][uint32]$Width,
        [Parameter(Mandatory = $true)][uint32]$Height
    )
    Assert-True ([int]$Report.schemaVersion -eq 6) 'Agent continuity report schemaVersion must be 6.'
    Assert-True ([string]$Report.captureMode -ceq 'native_local_agent_continuity_same_tab_split_without_fixture') 'Agent continuity report captureMode is unexpected.'
    Assert-True ([string]$Report.captureBackend -ceq 'native_window_render_to_image') 'Agent continuity report captureBackend is unexpected.'
    Assert-True ([string]$Report.fixtureHostId -ceq 'agent-continuity-local') 'Agent continuity report host identity is unexpected.'
    Assert-True ([string]$Report.fixtureHostKeyPolicy -ceq 'not_applicable_local_profile_without_remote_fixture') 'Agent continuity report host-key policy is unexpected.'
    Assert-True (-not [bool]$Report.windowVisible) 'Agent continuity window must stay hidden.'
    Assert-True ([uint32]$Report.requestedWindowWidth -eq $Width) 'Agent continuity requested width differs from the launcher.'
    Assert-True ([uint32]$Report.requestedWindowHeight -eq $Height) 'Agent continuity requested height differs from the launcher.'
    Assert-True ([double]$Report.windowWidth -gt 0 -and [double]$Report.windowHeight -gt 0) 'Agent continuity window size is invalid.'
    $reportedConfig = [IO.Path]::GetFullPath([string]$Report.configDirectory)
    $reportedSource = [IO.Path]::GetFullPath([string]$Report.fixtureConfigSource)
    $expectedConfig = [IO.Path]::GetFullPath($ConfigDirectory)
    Assert-True ($reportedConfig.Equals($expectedConfig, [StringComparison]::OrdinalIgnoreCase)) 'Agent continuity report configDirectory is not the isolated output config.'
    Assert-True ($reportedSource.Equals($expectedConfig, [StringComparison]::OrdinalIgnoreCase)) 'Agent continuity report fixtureConfigSource is not the isolated output config.'
    $isolation = $Report.nativeIsolation
    Assert-True ($null -ne $isolation) 'Agent continuity report has no native isolation evidence.'
    Assert-True ([bool]$isolation.platformSupported) 'Agent continuity isolation is not platform-supported.'
    Assert-True ([bool]$isolation.isolationProven) 'Agent continuity isolation is not proven.'
    Assert-True ($null -ne $isolation.uiThreadId) 'Agent continuity isolation has no UI thread identity.'
    Assert-True ([string]$isolation.uiThreadDesktopName -ceq $DesktopName) 'Agent continuity UI thread does not own the private desktop.'
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$isolation.inputDesktopName)) 'Agent continuity isolation has no input desktop identity.'
    Assert-True ([string]$isolation.inputDesktopName -cne $DesktopName) 'Agent continuity desktop is the input desktop.'
    Assert-True ([int]$isolation.processId -eq $ProcessId) 'Agent continuity isolation process identity differs from the launcher handle.'
    Assert-True (@($Report.themes).Count -eq 0) 'Agent continuity report must not contain remote theme captures.'
    foreach ($remoteProperty in @('tmuxPresentations', 'containerPresentations')) {
        $property = $Report.PSObject.Properties[$remoteProperty]
        Assert-True ($null -eq $property -or @($property.Value).Count -eq 0) "Agent continuity report contains remote captures: $remoteProperty"
    }
    $expectedOperations = @(
        'agent-continuity-launcher-ready',
        'agent-continuity-started',
        'agent-continuity-draft-before-switch',
        'agent-continuity-other-tab-active',
        'agent-continuity-hidden-output-observed',
        'agent-continuity-returned',
        'agent-continuity-resumed-input',
        'agent-continuity-exited',
        'agent-continuity-same-tab-second-pane-ready',
        'agent-continuity-same-tab-focus-switched',
        'agent-continuity-same-tab-continuity-preserved',
        'agent-continuity-same-tab-second-pane-reclaimed'
    )
    $records = @($Report.functional)
    Assert-True ($records.Count -eq $expectedOperations.Count) 'Agent continuity report checkpoint count is unexpected.'
    foreach ($operation in $expectedOperations) {
        $matches = @($records | Where-Object { [string]$_.operation -ceq $operation })
        Assert-True ($matches.Count -eq 1) "Agent continuity report checkpoint is missing or duplicated: $operation"
        $record = $matches[0]
        Assert-True (-not [string]::IsNullOrWhiteSpace([string]$record.outcome)) "Agent continuity checkpoint has no outcome: $operation"
        Assert-AgentContinuityCapture -Capture $record.evidence -OutputDirectory $OutputDirectory -Operation $operation
    }
}

function Assert-AgentContinuityDirectories {
    # 成功路径必须留下全新本地 profile、报告和证据，同时不得出现 host、known_hosts、
    # vault/key 或未清理的 agent-continuity 运行目录。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunDirectory,
        [Parameter(Mandatory = $true)][string]$OutputDirectory,
        [Parameter(Mandatory = $true)][string]$ConfigDirectory
    )
    Assert-True (Test-PathWithinRoot -Path $OutputDirectory -Root $RunDirectory) 'Agent continuity output is outside the run directory.'
    $profilePath = Join-Path $ConfigDirectory 'profiles\agent-continuity-local.toml'
    if (-not (Test-Path -LiteralPath $profilePath -PathType Leaf)) {
        throw 'Agent continuity local profile is missing.'
    }
    $profile = [IO.File]::ReadAllText($profilePath)
    $workingDirectory = Join-Path $OutputDirectory ''
    $workingDirectory = $workingDirectory.TrimEnd('\')
    foreach ($requiredLine in @(
        'schema_version = 1',
        'id = "agent-continuity-local"',
        'shell = "pwsh.exe"',
        'args = ["-NoLogo", "-NoProfile"]',
        ('cwd = "' + $workingDirectory.Replace('\', '\\') + '"'),
        'is_default = true'
    )) {
        if (-not $profile.Contains($requiredLine)) {
            throw "Agent continuity local profile lacks: $requiredLine"
        }
    }
    if (Test-Path -LiteralPath (Join-Path $ConfigDirectory 'hosts')) {
        throw 'Agent continuity config unexpectedly contains hosts.'
    }
    $forbidden = @(Get-ChildItem -LiteralPath $ConfigDirectory -Recurse -Force -File -ErrorAction Stop | Where-Object {
        $_.Name -match '^(known_hosts|vault.*|id_.*)$' -or $_.Extension -in @('.key', '.pem')
    })
    if ($forbidden.Count -ne 0) {
        throw "Agent continuity config contains credential or host-key material: $($forbidden.Name -join ', ')"
    }
    $leftoverRuns = @(Get-ChildItem -LiteralPath $OutputDirectory -Force -Directory -ErrorAction Stop | Where-Object {
        $_.Name -like 'agent-continuity-*'
    })
    if ($leftoverRuns.Count -ne 0) {
        throw "Agent continuity run directory was not cleaned: $($leftoverRuns.Name -join ', ')"
    }
}

function Assert-NoResidualProcess {
    # Job Object 的 active-process 计数是回收主证据；这里再用只读 CIM 交叉核对命令行引用与后代，
    # 排除当前进程及其祖先，避免启动包装时把 run directory 写进父 shell 命令行而误判。
    # 只读快照、不按 PID 结束任何进程；短暂轮询给正常关闭留出收敛窗口，超时仍失败关闭。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunDirectory,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [long]$CreationTimeUtcTicks = 0,
        [int]$WaitMilliseconds = 3000
    )
    $excluded = [Collections.Generic.HashSet[int]]::new()
    [void]$excluded.Add($PID)
    [void]$excluded.Add($ProcessId)
    $current = $PID
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        $snapshot = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
        $byParent = @{}
        foreach ($entry in $snapshot) {
            $parentId = [int]$entry.ParentProcessId
            if (-not $byParent.ContainsKey($parentId)) {
                $byParent[$parentId] = [Collections.Generic.List[object]]::new()
            }
            $byParent[$parentId].Add($entry)
        }
        $ancestor = $PID
        for ($depth = 0; $depth -lt 32; $depth++) {
            $entry = $snapshot | Where-Object { [int]$_.ProcessId -eq $ancestor } | Select-Object -First 1
            if ($null -eq $entry) { break }
            $parent = [int]$entry.ParentProcessId
            if ($parent -le 0 -or -not $excluded.Add($parent)) { break }
            $ancestor = $parent
        }

        $descendants = [Collections.Generic.List[object]]::new()
        $pending = [Collections.Generic.Queue[int]]::new()
        $pending.Enqueue($ProcessId)
        $visited = [Collections.Generic.HashSet[int]]::new()
        [void]$visited.Add($ProcessId)
        while ($pending.Count -gt 0) {
            $parentId = $pending.Dequeue()
            if (-not $byParent.ContainsKey($parentId)) { continue }
            foreach ($child in $byParent[$parentId]) {
                $childId = [int]$child.ProcessId
                if ($childId -le 0 -or -not $visited.Add($childId) -or $excluded.Contains($childId)) { continue }
                # 父 PID 可能被复用；若启动进程的创建时间可用，则只把不早于它的记录当作后代。
                $createdTicks = 0L
                if ($null -ne $child.CreationDate) {
                    try { $createdTicks = ([datetime]$child.CreationDate).ToUniversalTime().Ticks } catch { $createdTicks = 0L }
                }
                if ($CreationTimeUtcTicks -gt 0 -and $createdTicks -gt 0 -and $createdTicks -lt $CreationTimeUtcTicks) {
                    continue
                }
                $descendants.Add($child)
                $pending.Enqueue($childId)
            }
        }

        $residual = @($snapshot | Where-Object {
            -not $excluded.Contains([int]$_.ProcessId) -and
            -not [string]::IsNullOrWhiteSpace([string]$_.CommandLine) -and
            ([string]$_.CommandLine).IndexOf($RunDirectory, [StringComparison]::OrdinalIgnoreCase) -ge 0
        })
        if ($descendants.Count -eq 0 -and $residual.Count -eq 0) { return }
        if ($deadline.ElapsedMilliseconds -ge $WaitMilliseconds) {
            $details = @($descendants + $residual) |
                Sort-Object -Property ProcessId -Unique |
                ForEach-Object { '{0}:{1}' -f $_.ProcessId, $_.Name }
            throw "Residual process still references or descends from the isolated run; cleanup is unproven: $($details -join ', ')"
        }
        Start-Sleep -Milliseconds 100
    }
}

if ($PSVersionTable.PSEdition -ne 'Core' -or $PSVersionTable.PSVersion.Major -lt 7) {
    throw 'agent-continuity-native.ps1 must be launched by PowerShell 7 (pwsh.exe).'
}
if ([Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    throw 'agent-continuity-native.ps1 only supports Windows private desktops.'
}
# 包装进程自身的宿主参数属于隔离边界：用户 profile、交互式历史或未知启动开关都不能被静默接受。
# 在创建任何目录或修改环境之前 fail-closed，避免不满足隔离前提时仍留下部分运行现场。
Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @(
    [Environment]::GetCommandLineArgs() | Select-Object -Skip 1
)

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$pwsh = Resolve-Pwsh7
$executable = if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
    Join-Path $repositoryRoot "target\$BuildProfile\kerminal.exe"
} else {
    [IO.Path]::GetFullPath($ExecutablePath)
}

if ([string]::IsNullOrWhiteSpace($RunDirectory)) {
    $runBase = Join-Path $repositoryRoot 'target\acceptance\agent-continuity-native'
    $runRoot = Join-Path $runBase ([Guid]::NewGuid().ToString('N'))
} elseif ([IO.Path]::IsPathRooted($RunDirectory)) {
    $runRoot = [IO.Path]::GetFullPath($RunDirectory)
} else {
    $runRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot $RunDirectory))
}
Assert-RunDirectoryText -Text $runRoot
Assert-PathOutsideUserConfiguration -Path $runRoot
Assert-NoReparsePoint -Path $runRoot
if (Test-Path -LiteralPath $runRoot) {
    throw "Agent continuity run directory already exists: $runRoot"
}
$outputDirectory = Join-Path $runRoot 'output'
$configDirectory = Join-Path $outputDirectory 'config'
$profileRoot = Join-Path $runRoot 'environment\user'
$appDataRoaming = Join-Path $profileRoot 'AppData\Roaming'
$appDataLocal = Join-Path $profileRoot 'AppData\Local'
$tempRoot = Join-Path $runRoot 'environment\Temp'
$environmentConfig = Join-Path $runRoot 'environment-config'
$xdgConfig = Join-Path $runRoot 'environment\xdg\config'
$xdgData = Join-Path $runRoot 'environment\xdg\data'
$xdgCache = Join-Path $runRoot 'environment\xdg\cache'
$xdgState = Join-Path $runRoot 'environment\xdg\state'
$desktopName = 'KerminalRail-settings-agent-{0}' -f [Guid]::NewGuid().ToString('N')
$stdoutPath = Join-Path $runRoot 'app-stdout.log'
$stderrPath = Join-Path $runRoot 'app-stderr.log'

$inheritedKerminalNames = @(Get-InheritedKerminalEnvironmentNames)
$environmentNames = @(
    'USERPROFILE', 'HOME', 'HOMEDRIVE', 'HOMEPATH', 'APPDATA', 'LOCALAPPDATA',
    'TEMP', 'TMP', 'TMPDIR', 'PATH', 'PSModulePath',
    'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME',
    'POWERSHELL_TELEMETRY_OPTOUT', 'POWERSHELL_UPDATECHECK',
    'DOTNET_CLI_TELEMETRY_OPTOUT', 'DOTNET_NOLOGO', 'DOTNET_CLI_HOME', 'NUGET_PACKAGES',
    'KERMINAL_CONFIG_ROOT', 'KERMINAL_RIGHT_RAIL_FIXTURE_MANIFEST',
    'KERMINAL_SFTP_FOLLOW_MANIFEST', 'KERMINAL_SETTINGS_ACCEPTANCE_SCOPE'
)
$previousEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames
$previousKerminalEnvironment = if ($inheritedKerminalNames.Count -gt 0) {
    Get-ProcessEnvironmentSnapshot -Names $inheritedKerminalNames
} else {
    @{}
}
$nativeProcess = $null
$runFailure = $null
$cleanupFailures = [Collections.Generic.List[string]]::new()
$desktopClosed = $false
$jobAssigned = $false
# 只有清除继承开关的循环完整执行后才置真；回执不能固定写 true，否则清理中途失败时
# 会把“没来得及清完”报成已隔离。
$otherKerminalEnvironmentCleared = $false
$jobCleanupVerified = $false
$jobCleanupForced = $false
$jobActiveProcessesAtExit = $null
$report = $null
$processId = $null
$processCreationTimeUtcTicks = 0L
$exitCode = $null
$executableHash = $null

try {
    [void][IO.Directory]::CreateDirectory($runRoot)
    foreach ($directory in @(
        $environmentConfig,
        $profileRoot,
        (Join-Path $profileRoot 'Documents\PowerShell\Modules'),
        (Join-Path $appDataRoaming 'Microsoft\Windows\PowerShell\PSReadLine'),
        (Join-Path $appDataLocal 'Microsoft\Windows\PowerShell'),
        (Join-Path $profileRoot '.dotnet'),
        (Join-Path $profileRoot '.nuget\packages'),
        $tempRoot,
        $xdgConfig,
        $xdgData,
        $xdgCache,
        $xdgState
    )) {
        [void][IO.Directory]::CreateDirectory($directory)
    }
    if (Test-Path -LiteralPath $outputDirectory) {
        throw "Agent continuity output directory already exists: $outputDirectory"
    }

    $drive = [IO.Path]::GetPathRoot($profileRoot)
    if ([string]::IsNullOrWhiteSpace($drive)) { throw 'Cannot resolve the isolated home drive.' }
    $drive = $drive.TrimEnd('\')
    if ($drive -notmatch '^[A-Za-z]:$') {
        throw "Agent continuity isolated home must be on a local drive, found root: $drive"
    }
    $homePath = $profileRoot.Substring($drive.Length)
    if (-not $homePath.StartsWith('\')) { $homePath = '\' + $homePath }
    $basePath = if ($previousEnvironment['PATH'].Present) { [string]$previousEnvironment['PATH'].Value } else { '' }
    $childPath = if ([string]::IsNullOrWhiteSpace($basePath)) { $pwsh.Home } else { $pwsh.Home + ';' + $basePath }

    Set-ProcessEnvironmentValue -Name 'USERPROFILE' -Value $profileRoot
    Set-ProcessEnvironmentValue -Name 'HOME' -Value $profileRoot
    Set-ProcessEnvironmentValue -Name 'HOMEDRIVE' -Value $drive
    Set-ProcessEnvironmentValue -Name 'HOMEPATH' -Value $homePath
    Set-ProcessEnvironmentValue -Name 'APPDATA' -Value $appDataRoaming
    Set-ProcessEnvironmentValue -Name 'LOCALAPPDATA' -Value $appDataLocal
    Set-ProcessEnvironmentValue -Name 'TEMP' -Value $tempRoot
    Set-ProcessEnvironmentValue -Name 'TMP' -Value $tempRoot
    Set-ProcessEnvironmentValue -Name 'TMPDIR' -Value $tempRoot
    Set-ProcessEnvironmentValue -Name 'PATH' -Value $childPath
    Set-ProcessEnvironmentValue -Name 'PSModulePath' -Value $pwsh.Modules
    Set-ProcessEnvironmentValue -Name 'XDG_CONFIG_HOME' -Value $xdgConfig
    Set-ProcessEnvironmentValue -Name 'XDG_DATA_HOME' -Value $xdgData
    Set-ProcessEnvironmentValue -Name 'XDG_CACHE_HOME' -Value $xdgCache
    Set-ProcessEnvironmentValue -Name 'XDG_STATE_HOME' -Value $xdgState
    Set-ProcessEnvironmentValue -Name 'POWERSHELL_TELEMETRY_OPTOUT' -Value '1'
    Set-ProcessEnvironmentValue -Name 'POWERSHELL_UPDATECHECK' -Value 'Off'
    Set-ProcessEnvironmentValue -Name 'DOTNET_CLI_TELEMETRY_OPTOUT' -Value '1'
    Set-ProcessEnvironmentValue -Name 'DOTNET_NOLOGO' -Value '1'
    Set-ProcessEnvironmentValue -Name 'DOTNET_CLI_HOME' -Value (Join-Path $profileRoot '.dotnet')
    Set-ProcessEnvironmentValue -Name 'NUGET_PACKAGES' -Value (Join-Path $profileRoot '.nuget\packages')
    foreach ($name in $inheritedKerminalNames) {
        # 已纳入 $environmentNames 的旧值会在 finally 恢复；这里先移除，避免子进程继承任意开关。
        Set-ProcessEnvironmentValue -Name $name -Value $null
    }
    $otherKerminalEnvironmentCleared = $true
    Set-ProcessEnvironmentValue -Name 'KERMINAL_CONFIG_ROOT' -Value $environmentConfig
    Set-ProcessEnvironmentValue -Name 'KERMINAL_RIGHT_RAIL_FIXTURE_MANIFEST' -Value $null
    Set-ProcessEnvironmentValue -Name 'KERMINAL_SFTP_FOLLOW_MANIFEST' -Value $null
    Set-ProcessEnvironmentValue -Name 'KERMINAL_SETTINGS_ACCEPTANCE_SCOPE' -Value $null

    $executableHash = Get-AcceptanceExecutableHash -Path $executable
    $launcherSource = Get-AgentContinuityDesktopLauncherSource -SourceRoot $PSScriptRoot
    if (-not ('Kerminal.AgentContinuityAcceptance.NativeDesktopLauncher' -as [type])) {
        Add-Type -TypeDefinition $launcherSource -Language CSharp
    }
    $nativeProcess = [Kerminal.AgentContinuityAcceptance.NativeDesktopLauncher]::Start(
        $executable,
        $repositoryRoot,
        $outputDirectory,
        $stdoutPath,
        $stderrPath,
        $desktopName,
        [uint32]$Width,
        [uint32]$Height,
        $true
    )
    # 句柄身份在启动后立即固定：即使随后超时，失败回执仍能指向本轮精确进程，
    # 后续只通过该句柄或所属 Job Object 终止，不按可能复用的 PID 查找或结束进程。
    $processId = [int]$nativeProcess.ProcessId
    $processCreationTimeUtcTicks = [long]$nativeProcess.CreationTimeUtcTicks
    $jobAssigned = [bool]$nativeProcess.JobAssigned
    if (-not $jobAssigned) {
        throw 'The launcher did not assign the owned acceptance process to a job object; descendant cleanup cannot be proven.'
    }
    if (-not $nativeProcess.WaitForExit([uint32]($TimeoutSeconds * 1000))) {
        throw [TimeoutException]::new("Agent continuity acceptance timed out after $TimeoutSeconds seconds; the owned job object will be terminated and verified before cleanup is reported.")
    }
    $exitCode = [int]$nativeProcess.GetExitCode()
    if ($exitCode -ne 0) { throw "Agent continuity acceptance exited with code $exitCode." }

    $reportPath = Join-Path $outputDirectory 'report.json'
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
        throw 'Agent continuity report.json is missing.'
    }
    try { $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json }
    catch { throw 'Agent continuity report.json is not valid JSON.' }
    Assert-AgentContinuityReport -Report $report -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory `
        -DesktopName $desktopName -ProcessId $processId -Width $Width -Height $Height
    Assert-AgentContinuityDirectories -RunDirectory $runRoot -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory
    Assert-NoResidualProcess -RunDirectory $runRoot -ProcessId $processId -CreationTimeUtcTicks $processCreationTimeUtcTicks
}
catch {
    $runFailure = $_
}
finally {
    if ($null -ne $nativeProcess) {
        $wasRunning = -not $nativeProcess.WaitForExit(0)
        if ($wasRunning) {
            try {
                if ($jobAssigned) {
                    $nativeProcess.TerminateJobAndWait(1, 30000)
                } else {
                    $nativeProcess.Terminate(1)
                    $cleanupFailures.Add('The launcher-owned process was terminated by its exact handle, but no job object proves descendant cleanup.')
                }
                if (-not $nativeProcess.WaitForExit(30000)) {
                    throw 'The launcher-owned process did not exit after owned process-tree termination.'
                }
            }
            catch {
                $cleanupFailures.Add("Owned process-tree termination failed: $($_.Exception.Message)")
            }
        }
        if ($nativeProcess.WaitForExit(0)) {
            try {
                $nativeProcess.Dispose()
            }
            catch {
                $cleanupFailures.Add("Private desktop/job cleanup failed: $($_.Exception.Message)")
            }
            finally {
                # Dispose 会在关闭句柄前核对 Job active process 计数；即使在关闭其它句柄时抛错，
                # 这些身份与计数仍要写入回执，避免把“句柄关闭失败”误读成没有后代证据。
                $desktopClosed = [bool]$nativeProcess.DesktopClosed
                $jobAssigned = [bool]$nativeProcess.JobAssigned
                $jobCleanupVerified = [bool]$nativeProcess.JobCleanupVerified
                $jobCleanupForced = [bool]$nativeProcess.JobCleanupForced
                $jobActiveProcessesAtExit = [uint32]$nativeProcess.JobActiveProcessesAtExit
            }
        } else {
            $cleanupFailures.Add('The launcher-owned process is still running; the private desktop and job object were left open and cleanup is unproven.')
        }
    }
    foreach ($name in $environmentNames) {
        try {
            $previous = $previousEnvironment[$name]
            Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
        }
        catch {
            $cleanupFailures.Add("Environment restoration failed for ${name}: $($_.Exception.Message)")
        }
    }
    foreach ($name in $inheritedKerminalNames) {
        try {
            $previous = $previousKerminalEnvironment[$name]
            Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
        }
        catch {
            $cleanupFailures.Add("Inherited KERMINAL environment restoration failed for ${name}: $($_.Exception.Message)")
        }
    }
}

# 证明门禁必须计入 cleanupFailures，确保失败回执也落盘；不能只在成功分支抛错而丢掉句柄/Job 回读。
if ($null -ne $nativeProcess) {
    if (-not $desktopClosed) {
        $cleanupFailures.Add('The private desktop handle did not report a confirmed close; refusing to report success.')
    }
    if (-not $jobAssigned) {
        $cleanupFailures.Add('The launcher did not expose an owned job object, so descendant cleanup is unproven.')
    } elseif ($jobCleanupForced) {
        # 强制终止只能证明 Job 被清空，不能证明后代自然退出；成功回执必须拒绝这种冒充。
        $cleanupFailures.Add('The owned job object required forced termination; natural descendant reclamation was not proven.')
    } elseif (-not $jobCleanupVerified) {
        $cleanupFailures.Add("The owned job object did not verify an empty process set at exit (active=$jobActiveProcessesAtExit).")
    }
}

$failureMessages = [Collections.Generic.List[string]]::new()
if ($null -ne $runFailure) { $failureMessages.Add([string]$runFailure.Exception.Message) }
foreach ($message in $cleanupFailures) { $failureMessages.Add($message) }
if ($failureMessages.Count -ne 0) {
    $failureText = $failureMessages -join '; '
    $descendantTreeProven = $jobAssigned -and $jobCleanupVerified -and -not $jobCleanupForced
    if (Test-Path -LiteralPath $runRoot) {
        try {
            $failureResult = [ordered]@{
                schemaVersion = 1
                status = 'failed'
                processId = $processId
                processCreationTimeUtcTicks = $processCreationTimeUtcTicks
                exitCode = $exitCode
                privateDesktopClosed = $desktopClosed
                jobAssigned = $jobAssigned
                jobCleanupVerified = $jobCleanupVerified
                jobCleanupForced = $jobCleanupForced
                jobActiveProcessesAtExit = $jobActiveProcessesAtExit
                descendantTreeProven = $descendantTreeProven
                executableSha256 = $executableHash
                runDirectory = $runRoot
                outputDirectory = $outputDirectory
                knownFolderApiCovered = $false
                userTokenIsolated = $false
                otherKerminalEnvironmentCleared = $otherKerminalEnvironmentCleared
                isolationScope = 'Process environment, output paths, private desktop and owned job object only; Windows KnownFolder API and same-user token are not redirected.'
                failure = $failureText
                cleanupFailures = @($cleanupFailures)
            }
            Write-Utf8 -Path (Join-Path $runRoot 'process-result.json') -Content ((ConvertTo-Json -InputObject $failureResult) + [Environment]::NewLine)
        }
        catch {
            $failureText = $failureText + '; failed to write process-result.json: ' + $_.Exception.Message
        }
    }
    throw $failureText
}
$descendantTreeProven = $jobAssigned -and $jobCleanupVerified -and -not $jobCleanupForced

$result = [ordered]@{
    schemaVersion = 1
    status = 'passed'
    processId = $processId
    processCreationTimeUtcTicks = $processCreationTimeUtcTicks
    exitCode = $exitCode
    privateDesktopClosed = $desktopClosed
    desktopName = $desktopName
    jobAssigned = $jobAssigned
    jobCleanupVerified = $jobCleanupVerified
    jobCleanupForced = $jobCleanupForced
    jobActiveProcessesAtExit = $jobActiveProcessesAtExit
    descendantTreeProven = $descendantTreeProven
    executableSha256 = $executableHash
    runDirectory = $runRoot
    outputDirectory = $outputDirectory
    reportCaptureMode = [string]$report.captureMode
    knownFolderApiCovered = $false
    userTokenIsolated = $false
    otherKerminalEnvironmentCleared = $otherKerminalEnvironmentCleared
    launcherCleanupProven = $desktopClosed -and $descendantTreeProven
    cleanupScope = 'Exact launcher process, private desktop handle, and owned job object; the job active process count was verified zero at exit without forced termination. Windows KnownFolder API and same-user token are not redirected.'
}
Write-Utf8 -Path (Join-Path $runRoot 'process-result.json') -Content ((ConvertTo-Json -InputObject $result) + [Environment]::NewLine)
Write-Host "Agent continuity acceptance completed: $runRoot"
