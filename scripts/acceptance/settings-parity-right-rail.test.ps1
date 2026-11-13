# @author kongweiguang
# 右栏外层入口与内层隔离断言的纯内存回归；不启动应用、真实窗口或仓库外进程，
# pwsh 子进程只在受控临时 HOME 下验证 PSModulePath 序列行为后立即退出。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    # 保留首个断言失败的定位，避免后续合成样本掩盖原因。
    # @author kongweiguang
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Failure {
    # 校验目标拒绝原因，不能用缺少函数、语法错误或其他异常冒充回归通过。
    # @author kongweiguang
    param([scriptblock]$Action, [string]$Expected, [string]$Case)
    $failure = $null
    try { & $Action } catch { $failure = $_.Exception.Message }
    Assert-True ($null -ne $failure -and $failure.Contains($Expected)) "Case $Case expected '$Expected'; actual: $failure"
}

function Restore-ProcessEnvironment {
    # [Environment]::SetEnvironmentVariable 的 [string] 形参把 $null 绑成空串，会把“原本未设置”
    # 还原成“已设置但为空”，两者在 GetEnvironmentVariable 下分别返回 $null 与 ''。要真正恢复
    # 进入前的进程环境，未设置项必须用 Remove-Item 删除；已设置项才写回原值。
    # @author kongweiguang
    param([hashtable]$Snapshot)

    foreach ($name in $Snapshot.Keys) {
        if ($null -eq $Snapshot[$name]) {
            Remove-Item -LiteralPath ("Env:" + $name) -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable([string]$name, [string]$Snapshot[$name], 'Process')
        }
    }
}

function Get-IsolationFunctions {
    # 从仓库脚本按 AST 提取待测函数，避免执行脚本主体；任何锚点漂移都直接失败。
    # @author kongweiguang
    param([string]$Path, [string[]]$Names)
    $tokens = $null; $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    Assert-True ($errors.Count -eq 0) "Isolation script does not parse: $Path"
    $parts = [Collections.Generic.List[string]]::new()
    foreach ($name in $Names) {
        $found = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true))
        Assert-True ($found.Count -eq 1) "Function missing or ambiguous in ${Path}: $name"
        $parts.Add($found[0].Extent.Text)
    }
    return ($parts -join "`n")
}

$wrapperPath = Join-Path $PSScriptRoot 'settings-parity-right-rail.ps1'
$innerPath = Join-Path $PSScriptRoot 'settings-parity.ps1'
Assert-True ([IO.File]::Exists($wrapperPath)) 'Right-rail wrapper is missing.'
Assert-True ([IO.File]::Exists($innerPath)) 'Shared settings-parity script is missing.'

# ── 1. wrapper 参数绑定主路径：Assert-RightRailHost 不再是 Mandatory string ──
# -File 下省略的可选 [string] 绑定为空字符串；Mandatory string 会拒绝空串并让
# 文档承诺的“省略 -RightRailPowerShell”入口必败（P1-1）。
$wrapperFunctions = Get-IsolationFunctions -Path $wrapperPath -Names @('Assert-RightRailHost', 'Assert-RightRailLaunchArguments')
. ([scriptblock]::Create($wrapperFunctions))
$hostAst = [System.Management.Automation.Language.Parser]::ParseInput(
    (Get-IsolationFunctions -Path $wrapperPath -Names @('Assert-RightRailHost')), [ref]$null, [ref]$null)
$hasMandatoryString = @($hostAst.FindAll({
    param($node) $node -is [System.Management.Automation.Language.ParameterAst] -and
    $node.Name.VariablePath.UserPath -ceq 'RequestedPowerShell' -and
    @($node.Attributes | Where-Object { $_.TypeName.FullName -match 'Mandatory' }).Count -gt 0
}, $true)).Count
Assert-True ($hasMandatoryString -eq 0) 'Assert-RightRailHost must not declare Mandatory on RequestedPowerShell; -File omits bind as empty string.'
$trusted = Assert-RightRailHost -RequestedPowerShell ''
Assert-True ($trusted.Equals([IO.Path]::GetFullPath([Environment]::ProcessPath), [StringComparison]::OrdinalIgnoreCase)) 'Omitted -RightRailPowerShell must fall back to the current pwsh executable.'
Assert-Failure { Assert-RightRailHost -RequestedPowerShell 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' } 'cannot prove the requested PowerShell version' 'different explicit pwsh is rejected'

# ── 2. 启动参数守卫：按宿主实测开关表分类，只作用于第一个 -File 之前 ──
# 宿主（7.6.9 实测）用显式开关表解析：-c/-co/-com/-comm/-comma 被接受为 Command，
# -fi/-f、-nop、-noni 同样被识别，而 -n/-no/-commandw/-set/-m 被宿主以用法错误拒绝；
# 因此守卫必须枚举“宿主实测可接受的缩写”，不能用 startsWith 推导（推导会把 -c 判成
# 歧义放行，而宿主真的执行了 Command）。冒号形式被宿主整体拒绝（退出码 64），无需守卫；
# -File 之后是脚本实参，不参与开关分类，否则会误拒合法启动。
$launchGuardSource = Get-IsolationFunctions -Path $wrapperPath -Names @('Assert-RightRailLaunchArguments')
Assert-True ($launchGuardSource.Contains('ToLowerInvariant()')) 'Launch-argument guard must compare case-insensitively; uppercase variants would bypass the reject list.'
Assert-True ($launchGuardSource.Contains("'-comm' = 'command'") -and $launchGuardSource.Contains("'-c' = 'command'")) 'Launch-argument guard must map the host-accepted -comm/-c abbreviations to Command.'
Assert-True ($launchGuardSource.Contains("'-enc' = 'encodedcommand'") -and $launchGuardSource.Contains("'-ec' = 'encodedcommand'")) 'Launch-argument guard must map the host-accepted -enc/-ec abbreviations to EncodedCommand.'
Assert-True ($launchGuardSource.Contains("'-i' = 'interactive'") -and $launchGuardSource.Contains("'-int' = 'interactive'")) 'Launch-argument guard must map the host-accepted -i/-int abbreviations to Interactive.'
Assert-True ($launchGuardSource.Contains("'-ep' = 'executionpolicy'")) 'Launch-argument guard must map the host-accepted -ep abbreviation to ExecutionPolicy.'
Assert-True ($launchGuardSource.Contains("'-fi' = 'file'")) 'Launch-argument guard must map the host-accepted -fi abbreviation to File.'
Assert-True ($launchGuardSource.Contains("'-nop' = 'noprofile'")) 'Launch-argument guard must map the host-accepted -nop abbreviation to NoProfile.'
Assert-True ($launchGuardSource.Contains("'-noe' = 'noexit'")) 'Launch-argument guard must map the host-accepted -noe abbreviation to NoExit.'
Assert-True ($launchGuardSource.Contains("'-commandw' = 'commandwithargs'")) 'Launch-argument guard must map the host-accepted -commandw abbreviation to CommandWithArgs.'
# -ExecutionPolicy/-WorkingDirectory 等会吃掉下一个 token；守卫必须先跳过值再分类，
# 否则值恰好叫 "-Interactive" 时会被误判（虽然宿主会拒绝该值，但守卫不应依赖宿主报错）。
Assert-True ($launchGuardSource.Contains('$skipValueIndex') -and $launchGuardSource.Contains('$valueParameters')) 'Launch-argument guard must skip value tokens of value-taking parameters before classifying switches.'

# ── 2b. 守卫真实启动行为：真实 pwsh 启动只含守卫函数的沙盒副本 ──
# 沙盒脚本 = 从 wrapper 按 AST 提取的守卫函数 + 用自身 $PSCommandPath 调用守卫，
# 不包含环境重建与内层启动，因此不会启动应用或真实窗口。沙盒里 -File 目标就是
# 沙盒自身，无需修改任何守卫逻辑，真实命令行语义完整保留。
$guardSandbox = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-right-rail-guard-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($guardSandbox)
try {
    $sandboxWrapper = Join-Path $guardSandbox 'settings-parity-right-rail.ps1'
    $sandboxSource = @"
`$ErrorActionPreference = 'Stop'
$launchGuardSource
Assert-RightRailLaunchArguments -ScriptPath `$PSCommandPath
Write-Output 'GUARD-PASSED'
"@
    [IO.File]::WriteAllText($sandboxWrapper, $sandboxSource, [Text.UTF8Encoding]::new($false))
    function Invoke-GuardCase {
        # 以完整参数行真实启动沙盒 wrapper，返回 (exitCode, 输出, 是否超时)。标准输入重定向
        # 并设超时，避免任何交互等待挂起测试进程；输出用异步读取而不是事件处理器，
        # 因为事件处理脚本块在线程池线程上没有 runspace。超时后先杀进程树，管道随之关闭，
        # 已写出的守卫消息仍能读到——这正是 -NoExit 形态要保留的证据。
        # @author kongweiguang
        param([string]$Label, [string]$Arguments, [int]$TimeoutMs = 60000)
        $psi = [Diagnostics.ProcessStartInfo]::new()
        $psi.FileName = (Join-Path $PSHOME 'pwsh.exe')
        $psi.Arguments = $Arguments
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.RedirectStandardInput = $true
        $psi.WorkingDirectory = $guardSandbox
        $p = [Diagnostics.Process]::new()
        $p.StartInfo = $psi
        [void]$p.Start()
        $stdoutTask = $p.StandardOutput.ReadToEndAsync()
        $stderrTask = $p.StandardError.ReadToEndAsync()
        $timedOut = -not $p.WaitForExit($TimeoutMs)
        if ($timedOut) {
            $p.Kill($true)
            [void]$p.WaitForExit(10000)
        }
        # 进程退出或被杀后管道关闭，读取任务必然完成；有界等待防止异常残留。
        [void][Threading.Tasks.Task]::WaitAll(@($stdoutTask, $stderrTask), 10000)
        $exitCode = if ($timedOut) { $null } else { $p.ExitCode }
        $output = ($stdoutTask.Result + $stderrTask.Result)
        $p.Dispose()
        return [pscustomobject]@{ Label = $Label; ExitCode = $exitCode; TimedOut = $timedOut; Output = $output }
    }
    $sandboxTarget = '-File "' + $sandboxWrapper + '"'
    # 安全合法入口：完整名、缩写、大小写变体都必须通过。
    $pass = Invoke-GuardCase 'full-form' ('-NoProfile -NonInteractive ' + $sandboxTarget)
    Assert-True ($pass.ExitCode -eq 0 -and $pass.Output.Contains('GUARD-PASSED')) "Safe full-form launch must pass the guard: $($pass.Output)"
    $passAbbrev = Invoke-GuardCase 'abbrev-form' ('-NoPro -NonI -fi "' + $sandboxWrapper + '"')
    Assert-True ($passAbbrev.ExitCode -eq 0 -and $passAbbrev.Output.Contains('GUARD-PASSED')) "Abbreviated -NoPro -NonI -fi launch must pass the guard: $($passAbbrev.Output)"
    $passCase = Invoke-GuardCase 'case-form' ('-NOPROFILE -NONINTERACTIVE -FILE "' + $sandboxWrapper + '"')
    Assert-True ($passCase.ExitCode -eq 0 -and $passCase.Output.Contains('GUARD-PASSED')) "Uppercase launch must pass the guard: $($passCase.Output)"
    $passArgs = Invoke-GuardCase 'script-args' ('-NoProfile -NonInteractive ' + $sandboxTarget + ' -Interactive -ec x -WorkbenchOnly')
    Assert-True ($passArgs.ExitCode -eq 0 -and $passArgs.Output.Contains('GUARD-PASSED')) "Arguments after -File are script parameters and must not be classified as switches: $($passArgs.Output)"
    # -EncodedCommand 与 -File 同时出现时 -File 覆盖前者（宿主实测脚本正常执行），
    # 守卫必须放行而不是把它当成内联执行拒绝，否则会误拒合法启动。
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('Write-Output INLINE'))
    $passEncoded = Invoke-GuardCase 'encoded-then-file' ('-NoProfile -NonInteractive -EncodedCommand ' + $encoded + ' ' + $sandboxTarget)
    Assert-True ($passEncoded.ExitCode -eq 0 -and $passEncoded.Output.Contains('GUARD-PASSED') -and -not $passEncoded.Output.Contains('INLINE')) "-EncodedCommand followed by -File must reach the script (host gives -File precedence): $($passEncoded.Output)"
    # 真实守卫拒绝：这些形态下脚本确实会运行，守卫消息必须出现且退出码非零。
    $rejectInteractiveBeforeFile = Invoke-GuardCase 'interactive-before-file' ('-NoProfile -NonInteractive -i ' + $sandboxTarget)
    Assert-True ($rejectInteractiveBeforeFile.ExitCode -ne 0 -and $rejectInteractiveBeforeFile.Output.Contains('must not be launched interactively')) "-i before -File must be rejected: $($rejectInteractiveBeforeFile.Output)"
    $rejectInteractiveCase = Invoke-GuardCase 'interactive-uppercase' ('-NoProfile -NonInteractive -INTERACTIVE ' + $sandboxTarget)
    Assert-True ($rejectInteractiveCase.ExitCode -ne 0 -and $rejectInteractiveCase.Output.Contains('must not be launched interactively')) "Uppercase -INTERACTIVE must be rejected: $($rejectInteractiveCase.Output)"
    $rejectEp = Invoke-GuardCase 'abbrev-executionpolicy' ('-NoProfile -NonInteractive -ep Bypass ' + $sandboxTarget)
    Assert-True ($rejectEp.ExitCode -ne 0 -and $rejectEp.Output.Contains('execution policy')) "Abbreviated -ep must be rejected: $($rejectEp.Output)"
    # -NoExit 让宿主在脚本（和守卫）结束后继续存活，所以判据是“守卫消息已写出且进程没有
    # 自行退出”，而不是普通非零退出；这正是外层 StartProbe 会被挂死、必须拒绝的形态。
    $rejectNoExit = Invoke-GuardCase 'noexit' ('-NoProfile -NonInteractive -NoExit ' + $sandboxTarget) 20000
    Assert-True ($rejectNoExit.TimedOut -and $rejectNoExit.Output.Contains('must exit after -File')) "-NoExit must be rejected before the host stays alive: timedOut=$($rejectNoExit.TimedOut) out=$($rejectNoExit.Output)"
    $rejectNoExitAbbrev = Invoke-GuardCase 'noexit-abbrev' ('-NoProfile -NonInteractive -noe ' + $sandboxTarget) 20000
    Assert-True ($rejectNoExitAbbrev.TimedOut -and $rejectNoExitAbbrev.Output.Contains('must exit after -File')) "-noe must be rejected before the host stays alive: timedOut=$($rejectNoExitAbbrev.TimedOut) out=$($rejectNoExitAbbrev.Output)"
    # 文档主入口是 `pwsh -NoProfile -File ...`（见业务文档），而内层脚本由 wrapper
    # 显式以 -NonInteractive 启动，外层交互性不影响隔离；守卫因此只强制 -NoProfile，
    # 缺 -NonInteractive 必须放行，显式 -Interactive 仍然拒绝。
    $passNoNonInteractive = Invoke-GuardCase 'documented-entry' ('-NoProfile ' + $sandboxTarget)
    Assert-True ($passNoNonInteractive.ExitCode -eq 0 -and $passNoNonInteractive.Output.Contains('GUARD-PASSED')) "Documented entry `pwsh -NoProfile -File` must pass the guard: $($passNoNonInteractive.Output)"
    $rejectNoProfile = Invoke-GuardCase 'missing-noprofile' ('-NonInteractive ' + $sandboxTarget)
    Assert-True ($rejectNoProfile.ExitCode -ne 0 -and $rejectNoProfile.Output.Contains('-NoProfile before -File')) "Missing -NoProfile must be rejected: $($rejectNoProfile.Output)"
    # 冒号形式由宿主整体拒绝（用法错误，退出码 64），脚本根本不会启动；因此守卫不需要
    # 分类它们，这里固定宿主事实，避免把“守卫放行”误读成“冒号形式安全”。
    $colonForm = Invoke-GuardCase 'colon-form-host-rejected' ('-NoProfile -NonInteractive -command:x ' + $sandboxTarget)
    Assert-True ($colonForm.ExitCode -eq 64 -and -not $colonForm.Output.Contains('GUARD-PASSED')) "Colon forms must be rejected by the host before the script runs (exit 64): $($colonForm.Output)"
    # 宿主在进入脚本前就执行了 -Command，脚本内守卫不可达；这里只固定“内联代码执行、
    # 脚本未运行”这一宿主事实，不声称守卫阻止了它。
    $inlineCommand = Invoke-GuardCase 'inline-command-limitation' ('-NoProfile -NonInteractive -comm "Write-Output INLINE" ' + $sandboxTarget)
    Assert-True ($inlineCommand.Output.Contains('INLINE') -and -not $inlineCommand.Output.Contains('GUARD-PASSED')) "-Command runs inline code before the script; the in-script guard cannot observe it: $($inlineCommand.Output)"
} finally {
    Remove-Item -LiteralPath $guardSandbox -Recurse -Force
}

# ── 2c. 守卫静态边界：冒号形式不参与分类（宿主已拒绝）、-File 目标校验仍存在 ──
Assert-True ($launchGuardSource.Contains("Contains(':')")) 'Colon forms are rejected by the host itself; the guard must not classify them as normal switches.'
Assert-True ($launchGuardSource.Contains('unexpected -File target')) 'The -File target identity check must remain.'

# ── 2d. 成功路径日志保留：只移动本轮归属路径，失败不改判成功 ──
# wrapper 成功后删除 outer root；若不同时把 outer stdout/stderr 移入 run root 证据目录，
# 内层控制台原始输出会随目录一起消失。该函数只处理已通过归属与 reparse 核对的路径，
# 移动失败返回原因而不抛异常——证据补录失败不能把验收改判为成功或失败。
$logMoveSource = Get-IsolationFunctions -Path $wrapperPath -Names @('Assert-OwnedRunRootPath', 'Move-RightRailOuterLogs')
. ([scriptblock]::Create($logMoveSource))
$wrapperText = [IO.File]::ReadAllText($wrapperPath)
Assert-True ($wrapperText.Contains('Move-RightRailOuterLogs -RunRoot $runRoot')) 'The wrapper finally block must preserve outer logs into the owned run root on the success path.'
# 失败/强制中止路径的证据策略不变：内层 .runner- 目录与 stderr 打印是诊断依据，外层
# 只删除自己的 outer root。移动必须显式受 -not $forcedAbort 约束，否则将来改动会在
# 失败路径上把日志搬走，破坏既有策略。
Assert-True ($wrapperText.Contains('if (-not $forcedAbort -and [IO.Directory]::Exists($runRoot)) {')) 'Outer log preservation must stay gated on the success path (-not $forcedAbort).'
$logProbeRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-right-rail-logs-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($logProbeRoot)
try {
    $diagRoot = Join-Path $logProbeRoot 'target\diagnostics\settings-parity'
    $runRoot = Join-Path $diagRoot 'run-log-test'
    $outerRoot = Join-Path $diagRoot 'outer-log-test'
    foreach ($dir in @($runRoot, $outerRoot)) { [void][IO.Directory]::CreateDirectory($dir) }
    $stdoutPath = Join-Path $outerRoot 'outer-stdout.log'
    $stderrPath = Join-Path $outerRoot 'outer-stderr.log'
    [IO.File]::WriteAllText($stdoutPath, "stdout-payload`n", [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($stderrPath, "stderr-payload`n", [Text.UTF8Encoding]::new($false))
    $moved = Move-RightRailOuterLogs -RunRoot $runRoot -DiagnosticsRoot $diagRoot -StdoutPath $stdoutPath -StderrPath $stderrPath
    Assert-True ($null -eq $moved.failure) "Success path must move both outer logs: $($moved.failure)"
    Assert-True ($moved.moved.Count -eq 2) 'Success path must report both moved logs.'
    $movedStdout = Join-Path $runRoot 'outer-stdout.log'
    $movedStderr = Join-Path $runRoot 'outer-stderr.log'
    Assert-True ([IO.File]::Exists($movedStdout) -and [IO.File]::Exists($movedStderr)) 'Moved logs must exist under the run root with the outer- prefix.'
    Assert-True ([IO.File]::ReadAllText($movedStdout).Contains('stdout-payload')) 'Moved stdout must keep its original content.'
    Assert-True ([IO.File]::ReadAllText($movedStderr).Contains('stderr-payload')) 'Moved stderr must keep its original content.'
    Assert-True (-not [IO.File]::Exists($stdoutPath) -and -not [IO.File]::Exists($stderrPath)) 'Originals must be gone after a successful move.'

    # 清理/移动失败：目标已存在时拒绝覆盖，源文件保留，失败被如实返回而不是抛出。
    [IO.File]::WriteAllText($stdoutPath, "second-stdout`n", [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($stderrPath, "second-stderr`n", [Text.UTF8Encoding]::new($false))
    $collision = Move-RightRailOuterLogs -RunRoot $runRoot -DiagnosticsRoot $diagRoot -StdoutPath $stdoutPath -StderrPath $stderrPath
    Assert-True ($null -ne $collision.failure -and $collision.failure.Contains('Refusing to replace existing evidence log')) "Existing evidence must not be overwritten: $($collision.failure)"
    Assert-True ([IO.File]::Exists($stdoutPath) -and [IO.File]::Exists($stderrPath)) 'Failed move must keep the source logs for diagnosis.'
    Assert-True ([IO.File]::ReadAllText($movedStdout).Contains('stdout-payload')) 'Failed move must not modify the existing evidence file.'

    # 归属校验失败：目标 run root 在诊断根之外时不得移动任何文件。
    $escapedRunRoot = Join-Path $logProbeRoot 'escaped-run'
    [void][IO.Directory]::CreateDirectory($escapedRunRoot)
    $escaped = Move-RightRailOuterLogs -RunRoot $escapedRunRoot -DiagnosticsRoot $diagRoot -StdoutPath $stdoutPath -StderrPath $stderrPath
    Assert-True ($null -ne $escaped.failure -and $escaped.failure.Contains('escaped the owned diagnostics root')) "Run root outside the diagnostics root must be refused: $($escaped.failure)"
    Assert-True ([IO.File]::Exists($stdoutPath) -and [IO.File]::Exists($stderrPath)) 'Refused move must leave the source logs untouched.'

    # 源文件缺失：不是失败，也不应创建空文件。
    $emptyRoot = Join-Path $diagRoot 'run-empty'
    [void][IO.Directory]::CreateDirectory($emptyRoot)
    $empty = Move-RightRailOuterLogs -RunRoot $emptyRoot -DiagnosticsRoot $diagRoot -StdoutPath (Join-Path $outerRoot 'absent-stdout.log') -StderrPath (Join-Path $outerRoot 'absent-stderr.log')
    Assert-True ($null -eq $empty.failure -and $empty.moved.Count -eq 0) 'Missing outer logs must be a no-op, not a failure.'
    Assert-True (@([IO.Directory]::GetFileSystemEntries($emptyRoot)).Count -eq 0) 'Missing outer logs must not create empty evidence files.'
} finally {
    if (Test-Path -LiteralPath $logProbeRoot) { Remove-Item -LiteralPath $logProbeRoot -Recurse -Force }
}

# ── 3. 内层隔离断言的三段 PSModulePath 白名单（P1-2）──
# pwsh 子进程把缺失默认路径补齐为“受控用户;系统;PSHOME”三段并逐字保留预设完整序列；
# 单条目断言永远失败。内层断言改为精确三段白名单，真实用户路径不在白名单内。
$innerFunctions = Get-IsolationFunctions -Path $innerPath -Names @('Assert-NoReparseComponent', 'Assert-RightRailOuterIsolation')
Assert-True ($innerFunctions.Contains('actualModuleEntries.Count -ne 3')) 'Outer isolation must assert exactly the controlled user/system/PSHOME module sequence.'
Assert-True (-not $innerFunctions.Contains('$psHome =')) 'Do not assign $psHome; it collides with the constant automatic variable $PSHOME.'
Assert-True (-not $innerFunctions.Contains('$home =')) 'Do not assign $home; it collides with the read-only automatic variable $HOME.'
Assert-True ($innerFunctions.Contains('$runningPsHome =')) 'Running PSHOME must be captured under a non-colliding name.'

# ── 4. 受控 HOME 下真实执行内层断言：三段序列通过，真实用户路径与多段被拒绝 ──
$probeRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-right-rail-parity-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($probeRoot)
try {
    $diagRoot = Join-Path $probeRoot 'target\diagnostics\settings-parity'
    $outerRoot = Join-Path $diagRoot 'outer-test'
    $homeDir = Join-Path $outerRoot 'home'
    foreach ($dir in @(
            (Join-Path $homeDir 'Documents\PowerShell\Modules'),
            (Join-Path $homeDir 'AppData\Roaming'),
            (Join-Path $homeDir 'AppData\Local'),
            (Join-Path $outerRoot 'temp'),
            (Join-Path $outerRoot 'program-data'),
            (Join-Path $outerRoot 'public'),
            (Join-Path $probeRoot 'scripts\acceptance'))) {
        [void][IO.Directory]::CreateDirectory($dir)
    }
    $trustedModules = [IO.Path]::GetFullPath([IO.Path]::Combine($PSHOME, 'Modules'))
    foreach ($module in @('PSReadLine', 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management')) {
        Assert-True ([IO.Directory]::Exists([IO.Path]::Combine($trustedModules, $module))) "Running pwsh trusted modules are incomplete: $module"
    }
    $systemModulesRoot = [IO.Path]::GetFullPath(
        [IO.Path]::Combine([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles), 'PowerShell', 'Modules'))
    $sequence = (Join-Path $homeDir 'Documents\PowerShell\Modules') + ';' + $systemModulesRoot + ';' + $trustedModules
    $previous = @{}
    foreach ($name in @('KERMINAL_RIGHT_RAIL_OUTER_ROOT', 'KERMINAL_RIGHT_RAIL_TRUSTED_MODULES', 'PSModulePath',
            'USERPROFILE', 'HOME', 'HOMEDRIVE', 'HOMEPATH', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'TMPDIR', 'ProgramData', 'PUBLIC')) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    # 点源加载函数后设置受控环境再执行断言；finally 恢复全部进程环境变量。
    . ([scriptblock]::Create($innerFunctions))
    try {
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_OUTER_ROOT', $outerRoot, 'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_TRUSTED_MODULES', $trustedModules, 'Process')
        [Environment]::SetEnvironmentVariable('PSModulePath', $sequence, 'Process')
        [Environment]::SetEnvironmentVariable('USERPROFILE', $homeDir, 'Process')
        [Environment]::SetEnvironmentVariable('HOME', $homeDir, 'Process')
        [Environment]::SetEnvironmentVariable('HOMEDRIVE', [IO.Path]::GetPathRoot($homeDir).TrimEnd('\'), 'Process')
        [Environment]::SetEnvironmentVariable('HOMEPATH', $homeDir.Substring([IO.Path]::GetPathRoot($homeDir).Length - 1), 'Process')
        [Environment]::SetEnvironmentVariable('APPDATA', (Join-Path $homeDir 'AppData\Roaming'), 'Process')
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', (Join-Path $homeDir 'AppData\Local'), 'Process')
        [Environment]::SetEnvironmentVariable('TEMP', (Join-Path $outerRoot 'temp'), 'Process')
        [Environment]::SetEnvironmentVariable('TMP', (Join-Path $outerRoot 'temp'), 'Process')
        [Environment]::SetEnvironmentVariable('TMPDIR', (Join-Path $outerRoot 'temp'), 'Process')
        [Environment]::SetEnvironmentVariable('ProgramData', (Join-Path $outerRoot 'program-data'), 'Process')
        [Environment]::SetEnvironmentVariable('PUBLIC', (Join-Path $outerRoot 'public'), 'Process')
        Assert-RightRailOuterIsolation -ScriptRoot (Join-Path $probeRoot 'scripts\acceptance')
        # 负向：真实用户模块路径、额外条目与顺序颠倒都必须失败关闭。
        $realUserModules = [IO.Path]::GetFullPath(
            [IO.Path]::Combine([Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments), 'PowerShell', 'Modules'))
        [Environment]::SetEnvironmentVariable('PSModulePath', "$realUserModules;$trustedModules", 'Process')
        Assert-Failure { Assert-RightRailOuterIsolation -ScriptRoot (Join-Path $probeRoot 'scripts\acceptance') } 'must contain exactly the controlled user, system, and PSHOME module entries' 'real user module path is rejected'
        [Environment]::SetEnvironmentVariable('PSModulePath', "$sequence;C:\temp\extra", 'Process')
        Assert-Failure { Assert-RightRailOuterIsolation -ScriptRoot (Join-Path $probeRoot 'scripts\acceptance') } 'must contain exactly the controlled user, system, and PSHOME module entries' 'extra entry is rejected'
        [Environment]::SetEnvironmentVariable('PSModulePath', "$trustedModules;$systemModulesRoot;$(Join-Path $homeDir 'Documents\PowerShell\Modules')", 'Process')
        Assert-Failure { Assert-RightRailOuterIsolation -ScriptRoot (Join-Path $probeRoot 'scripts\acceptance') } 'module entry is not the expected path' 'reordered sequence is rejected'
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_OUTER_ROOT', $null, 'Process')
        [Environment]::SetEnvironmentVariable('PSModulePath', $sequence, 'Process')
        Assert-Failure { Assert-RightRailOuterIsolation -ScriptRoot (Join-Path $probeRoot 'scripts\acceptance') } 'must be started through scripts/acceptance/settings-parity-right-rail.ps1' 'missing outer marker is rejected'
    } finally {
        Restore-ProcessEnvironment -Snapshot $previous
    }

    # ── 5. pwsh 子进程对三段序列的保留行为：受控 HOME 下真实启动一次 ──
    # 该行为是白名单设计的实证前提；系统段目录可能不存在（MSIX 安装），断言只看路径。
    # 环境重写发生在独立 try/finally 内：无论断言成功、失败或抛异常，进程级
    # USERPROFILE/HOME/PSModulePath 都必须恢复到本测试进入前的值，当前用户 shell
    # 不被残留的受控路径污染。
    $childEnvironment = $null
    # 子进程要真正执行内层隔离断言，必须同时拿到外层标记；第 4 节的 finally 已把它们
    # 还原，这里重新设置并在 finally 一并恢复，避免受控环境泄漏到当前用户 shell。
    $childPrevious = @{}
    foreach ($name in @('KERMINAL_RIGHT_RAIL_OUTER_ROOT', 'KERMINAL_RIGHT_RAIL_TRUSTED_MODULES',
            'USERPROFILE', 'HOME', 'HOMEDRIVE', 'HOMEPATH', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP',
            'TMPDIR', 'ProgramData', 'PUBLIC', 'PSModulePath')) {
        $childPrevious[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    try {
        $env:USERPROFILE = $homeDir
        $env:HOME = $homeDir
        $env:HOMEDRIVE = [IO.Path]::GetPathRoot($homeDir).TrimEnd('\')
        $env:HOMEPATH = $homeDir.Substring([IO.Path]::GetPathRoot($homeDir).Length - 1)
        $env:APPDATA = Join-Path $homeDir 'AppData\Roaming'
        $env:LOCALAPPDATA = Join-Path $homeDir 'AppData\Local'
        $env:TEMP = Join-Path $outerRoot 'temp'
        $env:TMP = $env:TEMP
        $env:TMPDIR = $env:TEMP
        $env:ProgramData = Join-Path $outerRoot 'program-data'
        $env:PUBLIC = Join-Path $outerRoot 'public'
        $env:PSModulePath = $sequence
        $env:KERMINAL_RIGHT_RAIL_OUTER_ROOT = $outerRoot
        $env:KERMINAL_RIGHT_RAIL_TRUSTED_MODULES = $trustedModules
        # 点源场景放在受控子进程执行：本进程已经加载了被测函数与断言基础设施，
        # 改写 HOME 后再点源会话级代码属于“当前用户 shell 内运行受控外代码”，
        # 改在无 profile 的子 pwsh 里完成同样的加载与断言，结果一致且完全隔离。
        $dotSourceScript = Join-Path $probeRoot 'dot-source-probe.ps1'
        $dotSourceBody = @'
$ErrorActionPreference = 'Stop'
$functions = [IO.File]::ReadAllText($args[0])
$parts = [Collections.Generic.List[string]]::new()
foreach ($name in @('Assert-NoReparseComponent', 'Assert-RightRailOuterIsolation')) {
    $tokens = $null; $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseInput($functions, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw 'probe source does not parse' }
    $found = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true))
    if ($found.Count -ne 1) { throw "function missing: $name" }
    $parts.Add($found[0].Extent.Text)
}
. ([scriptblock]::Create(($parts -join "`n")))
Assert-RightRailOuterIsolation -ScriptRoot $args[1]
'dot-source-probe-ok'
'@
        [IO.File]::WriteAllText($dotSourceScript, $dotSourceBody, [Text.UTF8Encoding]::new($false))
        $childValue = & pwsh -NoLogo -NoProfile -NonInteractive -File $dotSourceScript $innerPath (Join-Path $probeRoot 'scripts\acceptance') 2>&1
        $childExit = $LASTEXITCODE
        $childText = (($childValue | ForEach-Object { [string]$_ }) -join "`n")
        Assert-True ($childExit -eq 0) "Controlled child dot-source probe failed (exit=$childExit): $childText"
        Assert-True ($childText.Contains('dot-source-probe-ok')) 'Controlled child dot-source probe did not report success.'
        $childEnvironment = & pwsh -NoLogo -NoProfile -NonInteractive -Command { $env:PSModulePath }
    } finally {
        # 恢复顺序无关紧要：全部写回进入前的进程级值；原本未设置的项必须删除而不是写成空串。
        Restore-ProcessEnvironment -Snapshot $childPrevious
    }
    $childEntries = @([string]$childEnvironment -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
    Assert-True ($childEntries.Count -eq 3) "pwsh child must preserve the three controlled module entries; found $($childEntries.Count)."
    $realUserModules = [IO.Path]::GetFullPath(
        [IO.Path]::Combine([Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments), 'PowerShell', 'Modules'))
    Assert-True (-not ([string]$childEnvironment).Contains($realUserModules)) 'pwsh child must not surface the real user module path under a controlled HOME.'
    Assert-True ($childEntries[2].Equals($trustedModules, [StringComparison]::OrdinalIgnoreCase)) 'pwsh child sequence must keep PSHOME\Modules last.'
    # 三段序列的隔离语义边界（机器级第二段实证）：第二段上的模块同样会被自动加载命中，
    # 它是机器管理员位置而非用户可写目录；本测试只断言序列形状与真实用户路径缺席，
    # 不声称“只有 PSHOME 能命中”。
    Assert-True ($childEntries[1].Equals($systemModulesRoot, [StringComparison]::OrdinalIgnoreCase)) 'pwsh child sequence must keep the machine-level module segment second.'
} finally {
    if (Test-Path -LiteralPath $probeRoot) { Remove-Item -LiteralPath $probeRoot -Recurse -Force }
}

'Right-rail entry and PSModulePath isolation regression passed; no app, window, or real user data was touched.'
