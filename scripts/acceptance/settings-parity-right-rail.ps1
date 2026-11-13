# @author kongweiguang
param(
    [string]$RightRailPowerShell,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$PassThrough
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# 右栏验收的 Add-Type 和 cmdlet 自动加载发生在本进程。这里必须先用受控 HOME/APPDATA/TEMP
# 和本次 pwsh 自带的 Modules 重建自己的进程环境，再由共享 launcher 启动内层脚本；
# 直接运行 settings-parity.ps1 或使用 Windows PowerShell 5.1 都会在入口失败关闭。
# @author kongweiguang
function Assert-RightRailHost {
    param(
        # 不使用 Mandatory：-File 启动方式下省略的可选 [string] 形参绑定为空字符串而非 $null，
        # Mandatory string 会拒绝空串，导致文档承诺的“省略 -RightRailPowerShell”主路径必败；
        # 空串在这里与 $null 同样走“信任当前进程可执行文件”的分支，显式传值仍受全路径与同进程校验。
        [string]$RequestedPowerShell
    )

    if ($PSVersionTable.PSVersion.Major -ne 7 -or $PSVersionTable.PSEdition -cne 'Core') {
        throw 'Right-rail acceptance must be started with PowerShell 7 Core; Windows PowerShell 5.1 is rejected.'
    }
    $current = [Environment]::ProcessPath
    if ([string]::IsNullOrWhiteSpace($current)) {
        throw 'Could not determine the current PowerShell 7 executable.'
    }
    $current = [IO.Path]::GetFullPath($current)
    $requested = if ([string]::IsNullOrWhiteSpace($RequestedPowerShell)) {
        $current
    } else {
        if (-not [IO.Path]::IsPathRooted($RequestedPowerShell)) {
            throw 'RightRailPowerShell must be a full path to the PowerShell 7 executable that launched this wrapper.'
        }
        [IO.Path]::GetFullPath($RequestedPowerShell)
    }
    if (-not $requested.Equals($current, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Right-rail acceptance cannot prove the requested PowerShell version ($requested); start the wrapper with the same pwsh 7 executable or omit -RightRailPowerShell."
    }
    if (-not [IO.File]::Exists($current)) {
        throw "The PowerShell 7 executable was not found: $current"
    }
    return $current
}

function Assert-NoReparseAncestors {
    # 创建受控目录前逐级检查已存在的父目录，防止 junction/symlink 把新目录带到用户位置。
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    $cursor = $full
    while ($true) {
        if ([IO.Directory]::Exists($cursor)) {
            $attributes = [IO.File]::GetAttributes($cursor)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label contains a reparse point: $cursor"
            }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent -or $parent.FullName.Equals($cursor, [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        $cursor = $parent.FullName
    }
}

function Assert-NoReparseTree {
    # 删除本轮 outer root 前遍历所有子项；.NET 递归删除不能穿过 reparse point 越界。
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if (-not [IO.Directory]::Exists($Path)) {
        return
    }
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push([IO.Path]::GetFullPath($Path))
    while ($pending.Count -gt 0) {
        $current = $pending.Pop()
        $attributes = [IO.File]::GetAttributes($current)
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "$Label contains a reparse point: $current"
        }
        foreach ($entry in [IO.Directory]::GetFileSystemEntries($current)) {
            $entryAttributes = [IO.File]::GetAttributes($entry)
            if (($entryAttributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label contains a reparse point: $entry"
            }
            if (($entryAttributes -band [IO.FileAttributes]::Directory) -ne 0) {
                $pending.Push($entry)
            }
        }
    }
}

function Assert-RightRailLaunchArguments {
    # profile 一旦执行就无法在脚本内撤销；这里只能核对启动参数确实要求 pwsh 跳过 profile，
    # 并把“必须由干净环境直接启动”作为前置条件失败关闭。外层是否 -NonInteractive 不影响
    # 隔离（内层脚本由本 wrapper 显式以 -NonInteractive 启动），因此不强制该开关，
    # 以免拒掉业务文档记录的 `pwsh -NoProfile -File ...` 主入口。
    # pwsh 宿主（本机 7.6.9 实证）用显式开关表解析，而非通用前缀匹配：-c/-co/-com/-comm/
    # -comma 被接受为 Command，-fi/-f、-nop、-noni 同样被识别；但 -n/-no/-commandw/-set/
    # -m 被宿主以用法错误拒绝。因此本函数按“宿主实测可接受的缩写 → 解析结果”建表，
    # 不用 startsWith 推导：推导会把 -c 判成歧义放行，而宿主实际执行了 Command。
    # 冒号形式（-command:xxx、-NoProfile:$false）一律被宿主以用法错误拒绝（实证退出码 64），
    # 因此不可达、无需守卫；-File 之后的实参全部成为脚本参数、不再有开关语义（argv 实证），
    # 所以拒绝分类只作用于第一个 -File 之前。
    # 守卫的覆盖边界（实测）：-Command/-CommandWithArgs 优先于 -File，宿主执行完内联代码后
    # 根本不运行本脚本；-EncodedCommand 在同时给出 -File 时被 -File 覆盖，脚本正常执行。
    # 所以本守卫能拦的是“脚本仍会运行”的形态（-Interactive/-NoExit/-ExecutionPolicy），
    # 对已经发生的内联执行只能是防御性声明，不能声称被脚本内守卫阻止。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$ScriptPath,
        # 默认读本进程真实命令行；显式传入只用于回归测试构造宿主已拒绝/不可达的 argv 形态，
        # 生产调用点保持不传，避免测试入口与真实入口走不同代码路径。
        [string[]]$Arguments = @([Environment]::GetCommandLineArgs())
    )

    # pwsh 7.6 宿主“会接受”的缩写 → 解析结果映射（本机 7.6.9 逐个实测：-c/-co/-com/-comm/
    # -comma 都执行了 Command；-e/-ec/-enc/-encodedcommand 都解析为 EncodedCommand；-i/-in/
    # -int/-interactive 都是 Interactive；-ep/-ex/-executionpolicy 都是 ExecutionPolicy；
    # -f/-fi/-fil 都是 File；-nop/-nopro/-noprofile 都是 NoProfile；-noni/-nonin/
    # -noninteractive 都是 NonInteractive）。宿主用显式开关表而不是通用前缀匹配（-commandw、
    # -no、-n、-set、-m 都被宿主以用法错误拒绝），所以不能用 startsWith 推导：推导会把 -c
    # 判成歧义放行，而宿主真的执行了 Command。表里同时保留宿主已拒绝的更长前缀，是为了
    # 让“将来某版本放宽前缀”时仍按危险参数处理；当前它们不可达，不影响合法启动。
    $acceptedAbbreviations = @{
        '-command' = 'command'; '-comman' = 'command'; '-comma' = 'command'
        '-comm' = 'command'; '-com' = 'command'; '-co' = 'command'; '-c' = 'command'
        '-encodedcommand' = 'encodedcommand'; '-encodedcomman' = 'encodedcommand'
        '-encodedcomma' = 'encodedcommand'; '-encodedcomm' = 'encodedcommand'
        '-encodedcom' = 'encodedcommand'; '-encodedco' = 'encodedcommand'
        '-encodedc' = 'encodedcommand'; '-encoded' = 'encodedcommand'
        '-encode' = 'encodedcommand'; '-encod' = 'encodedcommand'
        '-enco' = 'encodedcommand'; '-enc' = 'encodedcommand'
        '-ec' = 'encodedcommand'; '-e' = 'encodedcommand'
        '-interactive' = 'interactive'; '-interactiv' = 'interactive'
        '-interacti' = 'interactive'; '-interact' = 'interactive'
        '-interac' = 'interactive'; '-intera' = 'interactive'
        '-inter' = 'interactive'; '-inte' = 'interactive'; '-int' = 'interactive'
        '-in' = 'interactive'; '-i' = 'interactive'
        '-executionpolicy' = 'executionpolicy'; '-executionpolic' = 'executionpolicy'
        '-executionpoli' = 'executionpolicy'; '-executionpol' = 'executionpolicy'
        '-executionpo' = 'executionpolicy'; '-executionp' = 'executionpolicy'
        '-execution' = 'executionpolicy'; '-executio' = 'executionpolicy'
        '-executi' = 'executionpolicy'; '-execut' = 'executionpolicy'
        '-execu' = 'executionpolicy'; '-exec' = 'executionpolicy'
        '-exe' = 'executionpolicy'; '-ex' = 'executionpolicy'; '-ep' = 'executionpolicy'
        '-file' = 'file'; '-fil' = 'file'; '-fi' = 'file'; '-f' = 'file'
        '-noprofile' = 'noprofile'; '-noprofil' = 'noprofile'; '-noprofi' = 'noprofile'
        '-noprof' = 'noprofile'; '-nopro' = 'noprofile'; '-nop' = 'noprofile'
        '-noninteractive' = 'noninteractive'; '-noninteractiv' = 'noninteractive'
        '-noninteracti' = 'noninteractive'; '-noninteract' = 'noninteractive'
        '-nonintera' = 'noninteractive'; '-noninter' = 'noninteractive'
        '-noninte' = 'noninteractive'; '-nonint' = 'noninteractive'
        '-nonin' = 'noninteractive'; '-noni' = 'noninteractive'
        '-noexit' = 'noexit'; '-noexi' = 'noexit'; '-noex' = 'noexit'; '-noe' = 'noexit'
        '-commandwithargs' = 'commandwithargs'; '-commandwitharg' = 'commandwithargs'
        '-commandwithar' = 'commandwithargs'; '-commandwitha' = 'commandwithargs'
        '-commandwith' = 'commandwithargs'; '-commandwit' = 'commandwithargs'
        '-commandwi' = 'commandwithargs'; '-commandw' = 'commandwithargs'
    }
    $arguments = @($Arguments)
    $fileIndex = -1
    # 宿主按“参数名 + 独立值 token”解析：-ExecutionPolicy Bypass、-WorkingDirectory .、
    # -WindowStyle Hidden 这类参数会吃掉下一个 token。若逐个 token 分类，值恰好叫
    # "-Interactive" 时会被误判成开关；这里记录需要跳过的值位置，只对真正的开关名分类。
    $valueParameters = @(
        'command', 'commandwithargs', 'encodedcommand', 'executionpolicy', 'inputformat',
        'outputformat', 'settingsfile', 'configurationname', 'configurationfile',
        'custompipename', 'windowstyle', 'workingdirectory'
    )
    $skipValueIndex = -1
    for ($index = 1; $index -lt $arguments.Count; $index++) {
        # PowerShell 的参数名匹配不区分大小写；先归一小写再解析。
        $rawArgument = ([string]$arguments[$index]).ToLowerInvariant()
        if ($index -eq $skipValueIndex) {
            # 上一个参数的值，不是开关名。
            continue
        }
        if (-not $rawArgument.StartsWith('-') -or $rawArgument.Length -lt 2 -or $rawArgument.Contains(':')) {
            # 裸 token 不是参数名；冒号形式已被宿主以用法错误整体拒绝（实证退出码 64），
            # 属于从未成功启动过的调用形态，两者都不参与守卫分类。
            continue
        }
        $name = $rawArgument
        # 查宿主实测可接受的缩写映射；不在表内的形式按宿主实测行为要么被宿主拒绝、
        # 要么不是受监视参数，放行以免误拒合法启动。
        if (-not $acceptedAbbreviations.Contains($name)) {
            continue
        }
        $resolved = [string]$acceptedAbbreviations[$name]
        if ($resolved -eq 'file') {
            $fileIndex = $index
            # 宿主在第一个 -File 之后把全部 token 原样交给脚本（实测脚本 argv 收到
            # "-Interactive"/"-ec" 原文），它们不再有开关语义；继续分类会把合法脚本
            # 实参误判成危险开关，直接拒绝合法入口。
            break
        }
        if ($valueParameters -contains $resolved) {
            $skipValueIndex = $index + 1
        }
        if ($resolved -eq 'interactive') {
            # 策略拒绝而非“宿主会覆盖”的断言：显式 -Interactive 会让外层宿主进入交互形态，
            # 验收运行可能等待输入；隔离本身由内层脚本强制 -NonInteractive，但外层也不应
            # 被要求成交互式。
            throw 'Right-rail acceptance must not be launched interactively; -Interactive (including abbreviations like -i/-int) can make the acceptance wait on input.'
        }
        if ($resolved -eq 'executionpolicy') {
            throw 'Right-rail acceptance must not override the execution policy; run it from a clean pwsh 7 session instead.'
        }
        if ($resolved -eq 'noexit') {
            # 实证：-NoExit -File 会在脚本结束后留下交互式宿主，外层 StartProbe 的
            # WaitForExit 永远等不到退出，验收挂死；必须作为启动参数错误拒绝。
            throw 'Right-rail acceptance must exit after -File completes; -NoExit (including -noe) leaves an interactive host running and hangs the acceptance launcher.'
        }
        if ($resolved -in @('command', 'commandwithargs')) {
            # 实证：-Command/-CommandWithArgs 优先于 -File，宿主执行内联代码且根本不运行
            # 本脚本，因此这里的分支在真实启动下不可达；保留它只是防御性声明，不能据此
            # 声称脚本内守卫阻止了已经发生的内联执行。
            throw 'Right-rail acceptance must be launched with -File directly; -Command/-CommandWithArgs execute inline code and never run this script, so this branch is defense-in-depth only.'
        }
    }
    if ($fileIndex -lt 0 -or $fileIndex + 1 -ge $arguments.Count) {
        throw 'Right-rail acceptance must be launched as pwsh -NoProfile -File settings-parity-right-rail.ps1.'
    }
    $expected = [IO.Path]::GetFullPath($ScriptPath)
    $actual = [IO.Path]::GetFullPath([string]$arguments[$fileIndex + 1])
    if (-not $actual.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Right-rail acceptance was launched with an unexpected -File target: $actual"
    }
    # -File 之前的参数才影响宿主状态（profile 是否执行、是否交互、策略是否被改写）；
    # -File 之后的 token 是脚本实参，宿主不会按开关解析，不能据其拒绝合法启动。
    # 只强制 -NoProfile：profile 已经执行就无法在脚本内撤销，这是唯一不可补救的状态。
    # 不强制外层 -NonInteractive：文档入口是 `pwsh -NoProfile -File ...`，而内层脚本
    # 由本 wrapper 显式以 -NoInteractive 启动，交互性不影响隔离；额外强制会让文档
    # 主路径必败（与 P1-1 同类）。显式 -Interactive 仍在上面的分类里拒绝。
    $hasNoProfile = $false
    $skipValueIndex = -1
    for ($index = 1; $index -lt $fileIndex; $index++) {
        $token = ([string]$arguments[$index]).ToLowerInvariant()
        if ($index -eq $skipValueIndex) {
            continue
        }
        if (-not $token.StartsWith('-') -or $token.Contains(':')) {
            continue
        }
        $tokenName = $token
        if (-not $acceptedAbbreviations.Contains($tokenName)) {
            continue
        }
        $tokenResolved = [string]$acceptedAbbreviations[$tokenName]
        if ($valueParameters -contains $tokenResolved) {
            $skipValueIndex = $index + 1
        }
        if ($tokenResolved -eq 'noprofile') { $hasNoProfile = $true }
    }
    if (-not $hasNoProfile) {
        throw 'Right-rail acceptance must be launched with -NoProfile before -File; a profile already executed in this process cannot be undone by the script.'
    }
}


function Assert-OwnedRunRootPath {
    # 强制中止后的清理只能作用于本轮 wrapper 生成的 run 名；先核对完整路径仍在诊断根下，
    # 再从目标到根逐级拒绝 reparse point，避免清理跟随 junction 删到用户目录。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $resolvedPath = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd($separators)
    $prefix = $resolvedRoot + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label escaped the owned diagnostics root."
    }
    $cursor = $resolvedPath
    while ($true) {
        if ([IO.Directory]::Exists($cursor)) {
            $attributes = [IO.File]::GetAttributes($cursor)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label contains a reparse point: $cursor"
            }
        }
        if ($cursor.Equals($resolvedRoot, [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent -or $parent.FullName.Equals($cursor, [StringComparison]::OrdinalIgnoreCase)) {
            throw "$Label escaped the owned diagnostics root."
        }
        $cursor = $parent.FullName
    }
}

function Remove-OwnedRightRailRunEnvironment {
    # 强制中止时内层可能来不及写 cleanup 回执；外层只删除自己确认拥有的 run root 临时目录，
    # 截图、报告和日志保留。无法证明所有权或遇到 reparse point 时保留材料并失败关闭，
    # 不把“已尝试删除”写成“已回收”。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][string]$DiagnosticsRoot
    )

    $result = [ordered]@{
        environmentRemoved = $false
        configRemoved = $false
        cleanupFailure = $null
        checkedAtUtc = [DateTime]::UtcNow.ToString('o')
    }
    if (-not [IO.Directory]::Exists($RunRoot)) {
        $result.environmentRemoved = $true
        $result.configRemoved = $true
        return $result
    }
    try {
        Assert-OwnedRunRootPath -Path $RunRoot -Root $DiagnosticsRoot -Label 'Right-rail abort run root'
        foreach ($name in @('home', 'temp', 'program-data', 'public')) {
            $path = [IO.Path]::Combine($RunRoot, $name)
            if (-not [IO.Directory]::Exists($path)) {
                continue
            }
            Assert-OwnedRunRootPath -Path $path -Root $RunRoot -Label "Right-rail abort environment $name"
            # 路径链无 reparse 还不够：home 内的 AppData 子目录也可能被替换成 junction，
            # 递归删除前逐项确认整棵子树都属于本轮普通目录。
            Assert-NoReparseTree -Path $path -Label "Right-rail abort environment $name"
            [IO.Directory]::Delete($path, $true)
        }
        $remainingEnvironment = @('home', 'temp', 'program-data', 'public' |
            Where-Object { [IO.Directory]::Exists([IO.Path]::Combine($RunRoot, $_)) })
        $result.environmentRemoved = $remainingEnvironment.Count -eq 0
        $config = [IO.Path]::Combine($RunRoot, 'config')
        if ([IO.Directory]::Exists($config)) {
            Assert-OwnedRunRootPath -Path $config -Root $RunRoot -Label 'Right-rail abort config'
            Assert-NoReparseTree -Path $config -Label 'Right-rail abort config'
            [IO.Directory]::Delete($config, $true)
        }
        $result.configRemoved = -not [IO.Directory]::Exists($config)
    } catch {
        $result.cleanupFailure = $_.Exception.Message
    }
    return $result
}

function Move-RightRailOuterLogs {
    # 成功路径下 outer stdout/stderr 是内层控制台输出的唯一直接副本（内层已把 .runner-
    # 流日志移入 run root 证据目录），静默删除会让成功运行的原始输出无处可查。这里把两个
    # 流文件移动到本轮 run root 证据目录，命名加 outer- 前缀避免与内层 stdout.log/stderr.log
    # 冲突。只操作已通过归属与 reparse 核对的本轮路径；移动失败返回原因而不抛异常，
    # 由调用方决定是否影响退出码——证据补录失败不能改判验收结果，但必须如实记录。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][string]$DiagnosticsRoot,
        [Parameter(Mandatory = $true)][string]$StdoutPath,
        [Parameter(Mandatory = $true)][string]$StderrPath
    )

    $result = [ordered]@{
        moved = @()
        failure = $null
    }
    try {
        Assert-OwnedRunRootPath -Path $RunRoot -Root $DiagnosticsRoot -Label 'Right-rail outer log destination'
        foreach ($pair in @(@('outer-stdout.log', $StdoutPath), @('outer-stderr.log', $StderrPath))) {
            if (-not [IO.File]::Exists($pair[1])) {
                continue
            }
            $destination = [IO.Path]::Combine($RunRoot, $pair[0])
            if ([IO.File]::Exists($destination)) {
                # 不覆盖既有证据：重名只可能来自同 run 的重复补录，保留原文件并记录。
                throw "Refusing to replace existing evidence log: $destination"
            }
            [IO.File]::Move($pair[1], $destination)
            $result.moved += $pair[0]
        }
    } catch {
        $result.failure = $_.Exception.Message
    }
    return $result
}

$trustedPowerShell = Assert-RightRailHost -RequestedPowerShell $RightRailPowerShell
Assert-RightRailLaunchArguments -ScriptPath $PSCommandPath

$scriptRoot = $PSScriptRoot
$repositoryRoot = [IO.Path]::GetFullPath([IO.Path]::Combine($scriptRoot, '..', '..'))
$diagnosticsRoot = [IO.Path]::GetFullPath([IO.Path]::Combine($repositoryRoot, 'target', 'diagnostics', 'settings-parity'))
$outerRoot = [IO.Path]::Combine(
    $diagnosticsRoot,
    # 方法调用参数列表内逗号会拆分成独立实参，-f 只拿到一个操作数而抛“索引超出参数表”；
    # 必须把格式化表达式整体括起来，让两个值都进入 -f。
    ('outer-{0}-{1}' -f [DateTime]::Now.ToString('yyyyMMdd-HHmmssfff'), [Guid]::NewGuid().ToString('N')))
if ([IO.Directory]::Exists($outerRoot) -or [IO.File]::Exists($outerRoot)) {
    throw "Refusing to reuse an existing right-rail outer root: $outerRoot"
}

Assert-NoReparseAncestors -Path $diagnosticsRoot -Label 'Right-rail diagnostics root'
# 受控 HOME 还不存在时先固化真实用户模块路径：重建环境后 USERPROFILE 指向受控目录，
# 真实路径的捕获必须发生在重写之前，否则无法证明启动前没有导入真实用户模块。
$userDocuments = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
$realUserModules = if ([string]::IsNullOrWhiteSpace($userDocuments)) {
    $null
} else {
    [IO.Path]::GetFullPath([IO.Path]::Combine($userDocuments, 'PowerShell', 'Modules'))
}
$systemModulesRoot = [IO.Path]::GetFullPath(
    [IO.Path]::Combine([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles), 'PowerShell', 'Modules'))
[void][IO.Directory]::CreateDirectory($outerRoot)
$outerHome = [IO.Path]::Combine($outerRoot, 'home')
$appData = [IO.Path]::Combine($outerHome, 'AppData', 'Roaming')
$localAppData = [IO.Path]::Combine($outerHome, 'AppData', 'Local')
$historyRoot = [IO.Path]::Combine($appData, 'Microsoft', 'Windows', 'PowerShell', 'PSReadLine')
$tempRoot = [IO.Path]::Combine($outerRoot, 'temp')
$programData = [IO.Path]::Combine($outerRoot, 'program-data')
$publicRoot = [IO.Path]::Combine($outerRoot, 'public')
foreach ($directory in @($outerHome, $appData, $localAppData, $historyRoot, $tempRoot, $programData, $publicRoot)) {
    [void][IO.Directory]::CreateDirectory($directory)
}
Assert-NoReparseTree -Path $outerRoot -Label 'Right-rail outer root'
$systemRoot = [Environment]::GetEnvironmentVariable('SystemRoot')
$systemDrive = [Environment]::GetEnvironmentVariable('SystemDrive')
$windowsDir = [Environment]::GetEnvironmentVariable('WINDIR')
if ([string]::IsNullOrWhiteSpace($systemRoot) -or
    [string]::IsNullOrWhiteSpace($systemDrive) -or
    [string]::IsNullOrWhiteSpace($windowsDir)) {
    throw 'The Windows system environment is incomplete for the right-rail outer launch.'
}
$homeRoot = [IO.Path]::GetPathRoot($outerHome)
if ([string]::IsNullOrWhiteSpace($homeRoot)) {
    throw 'The right-rail outer HOME has no drive root.'
}
# pwsh 子进程启动时会把缺失的默认模块路径补齐为
# “<USERPROFILE>\Documents\PowerShell\Modules;<ProgramFiles>\PowerShell\Modules;<PSHOME>\Modules”
# （本机 7.6.9 实测：受控 USERPROFILE 下派生路径跟随受控目录，系统路径不受 ProgramFiles
# 环境变量影响而来自已知文件夹）。只写单一 PSHOME 条目必然被子进程追加为三段，
# 单条目断言永远失败，因此这里预先写入精确的三段序列，让子进程逐字保留；
# 用户段指向受控 HOME 派生目录，真实用户模块路径永远不会出现在子进程里。
# 注意三段序列的隔离语义：第二段是机器级位置（机器管理员安装的模块，非用户可写），
# 本机实证它上面的模块同样会被自动加载命中，因此隔离保证是“无真实用户路径、无
# 受控根外用户可写路径”，不是“只有 PSHOME\Modules 能命中”。
$controlledUserModules = [IO.Path]::GetFullPath([IO.Path]::Combine($outerHome, 'Documents', 'PowerShell', 'Modules'))
$trustedModules = [IO.Path]::GetFullPath([IO.Path]::Combine($PSHOME, 'Modules'))
$expectedModuleSequence = "$controlledUserModules;$systemModulesRoot;$trustedModules"
foreach ($trustedModule in @('PSReadLine', 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management')) {
    if (-not [IO.Directory]::Exists([IO.Path]::Combine($trustedModules, $trustedModule))) {
        throw "The trusted PowerShell module directory is missing ${trustedModule}: $trustedModules"
    }
}
# 启动前必须证明本进程没有从真实用户模块路径导入任何东西；路径无法定位时视为
# 已满足（无路径即无法导入），否则逐个已加载模块的 ModuleBase 拒绝真实用户目录。
if ($null -ne $realUserModules) {
    foreach ($loadedModule in @(Get-Module)) {
        $moduleBase = [string]$loadedModule.ModuleBase
        if (-not [string]::IsNullOrWhiteSpace($moduleBase) -and
            $moduleBase.StartsWith($realUserModules + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            throw "The outer pwsh already imported a module from the real user module path before isolation: $moduleBase"
        }
    }
}
# 受控用户模块目录必须先存在且为空：它进入 PSModulePath 后，真实用户模块路径不在
# 序列里、受控用户段又没有内容，因此自动加载不会命中任何用户可写位置；预置的空
# 目录同时阻止 pwsh 把它当作缺失路径重新推导。第二段（Program Files\PowerShell\Modules）
# 是机器级位置而非用户可写目录，本机实证它上面的模块同样会被自动加载命中——它的
# 语义是“机器管理员安装的可信模块”，不是“只有 PSHOME 才能命中”；若机器上存在该
# 目录，其内容会参与解析，隔离保证限定为“无真实用户路径、无受控根外可写路径”。
[void][IO.Directory]::CreateDirectory($controlledUserModules)

# 这些值在 Add-Type 与任何 cmdlet 自动加载之前写入；内层脚本会再次读取实际环境值，
# 而不是只信任 KERMINAL_RIGHT_RAIL_* 标记。
[Environment]::SetEnvironmentVariable('USERPROFILE', $outerHome, 'Process')
[Environment]::SetEnvironmentVariable('HOME', $outerHome, 'Process')
[Environment]::SetEnvironmentVariable('HOMEDRIVE', $homeRoot.TrimEnd('\'), 'Process')
[Environment]::SetEnvironmentVariable('HOMEPATH', $outerHome.Substring($homeRoot.Length - 1), 'Process')
[Environment]::SetEnvironmentVariable('APPDATA', $appData, 'Process')
[Environment]::SetEnvironmentVariable('LOCALAPPDATA', $localAppData, 'Process')
[Environment]::SetEnvironmentVariable('TEMP', $tempRoot, 'Process')
[Environment]::SetEnvironmentVariable('TMP', $tempRoot, 'Process')
[Environment]::SetEnvironmentVariable('TMPDIR', $tempRoot, 'Process')
[Environment]::SetEnvironmentVariable('ProgramData', $programData, 'Process')
[Environment]::SetEnvironmentVariable('PUBLIC', $publicRoot, 'Process')
[Environment]::SetEnvironmentVariable('PSModulePath', $expectedModuleSequence, 'Process')
[Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_OUTER_ROOT', $outerRoot, 'Process')
[Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_TRUSTED_MODULES', $trustedModules, 'Process')
foreach ($name in @(
        'KERMINAL_CONFIG_ROOT',
        'KERMINAL_SETTINGS_ACCEPTANCE_SCOPE',
        'KERMINAL_WORKBENCH_ACCEPTANCE',
        'KERMINAL_WORKBENCH_MOTION_ACCEPTANCE',
        'KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE',
        'KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL',
        'KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY',
        'KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED')) {
    [Environment]::SetEnvironmentVariable($name, '', 'Process')
}

$settingsScript = [IO.Path]::Combine($scriptRoot, 'settings-parity.ps1')
# run 名由外层生成并传给内层，强制中止时外层才能只清理本轮确实拥有的 run root 临时目录；
# 时间戳与随机后缀同时防止误命中其他会话的运行。
$runName = 'run-{0}-{1}' -f [DateTime]::Now.ToString('yyyyMMdd-HHmmssfff'), [Guid]::NewGuid().ToString('N')
$runRoot = [IO.Path]::Combine($diagnosticsRoot, $runName)
[Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_RUN_NAME', $runName, 'Process')
$settingsSource = [IO.File]::ReadAllText($settingsScript)
$sourceMarker = '$script:NativeDesktopLauncherSource = @'''
$sourceStart = $settingsSource.IndexOf($sourceMarker, [StringComparison]::Ordinal)
if ($sourceStart -lt 0) {
    throw 'The shared native launcher source was not found in settings-parity.ps1.'
}
$contentStart = $settingsSource.IndexOf("`n", $sourceStart, [StringComparison]::Ordinal)
if ($contentStart -lt 0) {
    throw 'The shared native launcher source has no opening line.'
}
$contentStart += 1
$sourceEnd = $settingsSource.IndexOf("`n'@", $contentStart, [StringComparison]::Ordinal)
if ($sourceEnd -lt 0) {
    throw 'The shared native launcher source has no closing marker.'
}
$launcherSource = $settingsSource.Substring($contentStart, $sourceEnd - $contentStart)
if (-not ('Kerminal.SettingsParity.NativeDesktopLauncher' -as [type])) {
    # 模块限定名避免 profile 中同名函数覆盖 Add-Type；PSModulePath 已在上面改写为
    # 受控三段序列（受控用户段为空、末段是本 pwsh 的可信 Modules），因此这里的自动
    # 加载不会命中真实用户模块路径。
    Microsoft.PowerShell.Utility\Add-Type -TypeDefinition $launcherSource -Language CSharp
}

foreach ($argument in $PassThrough) {
    if ($argument -match '^-(RightRailSplitSwitch|WorkbenchOnly|RightRailPowerShell)(:|$)') {
        throw "The right-rail wrapper owns the $argument argument; remove it from the pass-through arguments."
    }
}
$innerArguments = [Collections.Generic.List[string]]::new()
foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $settingsScript)) {
    $innerArguments.Add($argument)
}
foreach ($argument in $PassThrough) {
    $innerArguments.Add([string]$argument)
}
foreach ($argument in @('-WorkbenchOnly', '-RightRailSplitSwitch', '-RightRailPowerShell', $trustedPowerShell)) {
    $innerArguments.Add($argument)
}

$stdoutPath = [IO.Path]::Combine($outerRoot, 'outer-stdout.log')
$stderrPath = [IO.Path]::Combine($outerRoot, 'outer-stderr.log')
$desktopName = 'KerminalRail-settings-{0}' -f [Guid]::NewGuid().ToString('N')
$nativeProcess = $null
$exitCode = 1
$timedOut = $false
$jobCleanupVerified = $false
$jobForced = $false
$forcedAbort = $false
$abortReason = $null
$abortCleanup = $null
try {
    $nativeProcess = [Kerminal.SettingsParity.NativeDesktopLauncher]::StartProbe(
        $trustedPowerShell,
        [string[]]$innerArguments.ToArray(),
        $repositoryRoot,
        $outerRoot,
        $stdoutPath,
        $stderrPath,
        $desktopName,
        $true
    )
    $timedOut = -not $nativeProcess.WaitForExit(3600000)
    if ($timedOut) {
        $forcedAbort = $true
        $abortReason = 'outer-timeout'
        $jobForced = $true
        $nativeProcess.TerminateJobAndWait(1, 10000)
    }
    $exitCode = [int]$nativeProcess.GetExitCode()
    if ($exitCode -ne 0) {
        # 内层异常退出时不能假定它完成了自己的 finally 清理；外层只在 Job 已归零后
        # 清理本轮拥有的临时环境，截图、报告和日志继续保留用于诊断。
        $forcedAbort = $true
        $abortReason = "inner-exit-$exitCode"
    }
    $jobCleanupVerified = $nativeProcess.WaitForJobEmpty(5000)
    if (-not $jobCleanupVerified) {
        $forcedAbort = $true
        $abortReason = 'outer-job-not-empty'
        $jobForced = $true
        $nativeProcess.TerminateJobAndWait(1, 10000)
        throw 'The right-rail outer process left descendant processes in its owned job.'
    }
    if ($timedOut) {
        throw 'The right-rail outer process timed out and its owned process tree was terminated.'
    }
} catch {
    # 任何异常都意味着内层可能没有完成自己的清理回执；先记下原因，finally 再执行
    # 只针对本轮 run root 的有界清理，避免把未回收当成已回收。
    $forcedAbort = $true
    if ($null -eq $abortReason) {
        $abortReason = $_.Exception.Message
    }
    [Console]::Error.WriteLine('Right-rail outer launch failed: ' + $_.Exception.Message)
    $exitCode = 1
} finally {
    if ($null -ne $nativeProcess) {
        try {
            if (-not $nativeProcess.WaitForExit(0)) {
                $nativeProcess.TerminateJobAndWait(1, 10000)
            }
            if (-not $nativeProcess.WaitForJobEmpty(0)) {
                $jobForced = $true
                $nativeProcess.TerminateJobAndWait(1, 10000)
            }
            # 上面可能刚强制结束 Job；重新确认 active process 归零后才允许清理 run root。
            $jobCleanupVerified = $nativeProcess.WaitForJobEmpty(5000)
            $nativeProcess.Dispose()
        } catch {
            $exitCode = 1
            [Console]::Error.WriteLine('Right-rail outer process cleanup failed: ' + $_.Exception.Message)
        }
    }
    if ($forcedAbort) {
        # 记录清理前 run root 是否真实存在；不存在时不能把“没有可删对象”写成
        # “已清理并保留了截图/报告”。
        $runRootExisted = [IO.Directory]::Exists($runRoot)
        # Job 未归零时进程可能仍持有 run root 下的文件；此时删除既不可靠也无法证明
        # 归属，改为保留材料并把清理失败写进回执，不伪造回收。
        if ($jobCleanupVerified -or -not $runRootExisted) {
            $abortCleanup = Remove-OwnedRightRailRunEnvironment -RunRoot $runRoot -DiagnosticsRoot $diagnosticsRoot
        } else {
            $abortCleanup = [ordered]@{
                environmentRemoved = $false
                configRemoved = $false
                cleanupFailure = 'owned job was not proven empty; run-root environment and config were preserved'
                checkedAtUtc = [DateTime]::UtcNow.ToString('o')
            }
        }
        # 回执只记录外层实际能证明的范围：内层是否写了 cleanup 文件无法在此确认，
        # 因此显式标记不可用，也不声称 PTY 已回收。
        $receipt = [ordered]@{
            schemaVersion = 1
            runName = $runName
            forcedAbort = [bool]$forcedAbort
            abortReason = $abortReason
            innerExitCode = $exitCode
            forcedJobTermination = [bool]$jobForced
            jobCleanupVerified = [bool]$jobCleanupVerified
            environmentRemoved = [bool]$abortCleanup.environmentRemoved
            configRemoved = [bool]$abortCleanup.configRemoved
            cleanupFailure = $abortCleanup.cleanupFailure
            innerProcessCleanupReceiptAvailable = $false
            runRootExistedBeforeCleanup = [bool]$runRootExisted
            runnerLogDirectory = '.runner-' + $runName
            # 内层 runner 日志目录同样属于本轮产物，但属于诊断证据；外层不删除它，
            # 只记录是否存在，避免把日志丢失当成正常清理。
            runnerLogDirectoryPreserved = [IO.Directory]::Exists(
                [IO.Path]::Combine($diagnosticsRoot, '.runner-' + $runName))
            evidencePreserved = [bool]$runRootExisted
            ptyReclamationVerified = $false
            limitation = if ($runRootExisted) {
                'outer wrapper cleaned only its owned run-root environment and config after proving containment and no reparse points; screenshots, reports, and logs were preserved; inner cleanup receipt and PTY reclamation are not verified'
            } else {
                'no run root existed at abort time, so there was nothing to preserve or clean; inner cleanup receipt and PTY reclamation are not verified'
            }
            checkedAtUtc = $abortCleanup.checkedAtUtc
        }
        # 回执优先留在本轮 run root（截图和报告所在处）；run root 尚不存在时只能写外层
        # root 并同步打印，外层 root 随后删除，不能声称文件回执已经保留。
        $receiptPath = $null
        try {
            if ([IO.Directory]::Exists($runRoot)) {
                Assert-OwnedRunRootPath -Path $runRoot -Root $diagnosticsRoot -Label 'Right-rail abort receipt root'
                $receiptPath = [IO.Path]::Combine($runRoot, 'right-rail-outer-abort-cleanup.json')
            } else {
                $receiptPath = [IO.Path]::Combine($outerRoot, 'right-rail-outer-abort-cleanup.json')
            }
            [IO.File]::WriteAllText(
                $receiptPath,
                ($receipt | ConvertTo-Json -Depth 4),
                [Text.UTF8Encoding]::new($false))
            [Console]::Error.WriteLine(($receipt | ConvertTo-Json -Depth 4 -Compress))
        } catch {
            $exitCode = 1
            [Console]::Error.WriteLine('Right-rail abort cleanup receipt could not be written: ' + $_.Exception.Message)
        }
        if ($null -ne $abortCleanup.cleanupFailure) {
            $exitCode = 1
            [Console]::Error.WriteLine('Right-rail abort cleanup was not fully provable: ' + $abortCleanup.cleanupFailure)
        }
    }
    if ($exitCode -ne 0 -and [IO.File]::Exists($stderrPath)) {
        [Console]::Error.WriteLine([IO.File]::ReadAllText($stderrPath))
    }
    if ([IO.Directory]::Exists($outerRoot)) {
        # outer root 是本轮在诊断根下创建的临时目录；删除前重新核对归属和 reparse 链，
        # 防止运行期间被替换成指向用户目录的 junction。
        Assert-OwnedRunRootPath -Path $outerRoot -Root $diagnosticsRoot -Label 'Right-rail outer root'
        Assert-NoReparseTree -Path $outerRoot -Label 'Right-rail outer root'
        # 成功路径下 outer stdout/stderr 是内层控制台输出的唯一直接副本（内层已把
        # .runner- 流日志移入 run root 证据目录），无声删除会让成功运行的原始输出
        # 无处可查；这里把两个流文件移动到本轮 run root 证据目录，命名加 outer- 前缀
        # 避免与内层 stdout.log/stderr.log 冲突。只操作已通过归属与 reparse 核对的
        # 本轮路径，移动失败不改变退出码也不伪造成功，只如实记录；失败路径（含
        # run root 不存在或强制中止）保持原有“stderr 打印 + outerRoot 删除”策略。
        if (-not $forcedAbort -and [IO.Directory]::Exists($runRoot)) {
            $logMove = Move-RightRailOuterLogs -RunRoot $runRoot -DiagnosticsRoot $diagnosticsRoot -StdoutPath $stdoutPath -StderrPath $stderrPath
            if ($null -ne $logMove.failure) {
                # 移动只是补充诊断材料；失败时 outerRoot 仍被删除，与既有策略一致，
                # 不能因为日志保不住就改判本轮验收结果。
                [Console]::Error.WriteLine('Right-rail outer log preservation failed: ' + $logMove.failure)
            }
        }
        [IO.Directory]::Delete($outerRoot, $true)
    }
}

exit $exitCode
