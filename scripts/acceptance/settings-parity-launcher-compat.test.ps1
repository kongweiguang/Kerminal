# @author kongweiguang
# 只验证共享 launcher 的 Start 重载兼容性：从 settings-parity.ps1 提取内嵌 C# 源码，
# 在内存中编译并反射核对八/九参数签名与八参数的 false 委托。不启动原生程序、私有桌面、
# 真实用户配置、PTY 或联网；非法桌面名在 CreateDesktop 之前就被拒绝，因此调用路径
# 只走到参数校验，不创建任何窗口或进程。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    # 单测失败立即中止，避免后续反射结果掩盖首个断言失败。
    # @author kongweiguang
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Throws {
    # 只接受预期异常类型和消息片段，防止无关编译或反射错误被误判为校验生效。
    # @author kongweiguang
    param([scriptblock]$Action, [type]$ExpectedType, [string]$ExpectedMessage)
    $failure = $null
    try { & $Action } catch { $failure = $_ }
    Assert-True ($null -ne $failure) "Expected failure: $ExpectedMessage"
    $exception = $failure.Exception
    while ($null -ne $exception -and $exception -isnot $ExpectedType) { $exception = $exception.InnerException }
    Assert-True ($null -ne $exception) "Unexpected exception type: $($failure.Exception.GetType().FullName)"
    Assert-True ($exception.Message.Contains($ExpectedMessage)) "Unexpected failure: $($exception.Message)"
}

function Get-LauncherSource {
    # 与各验收消费者一致，按 AST 取唯一的 NativeDesktopLauncherSource 常量，避免用文本
    # 定位把注释或其他 here-string 误当成 launcher 源码。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][string]$Path)
    $tokens = $null
    $errors = $null
    $syntax = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw "Cannot parse the shared launcher script: $($errors -join '; ')" }
    $assignments = @($syntax.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            $node.Left.Extent.Text -ceq '$script:NativeDesktopLauncherSource'
    }, $true))
    if ($assignments.Count -ne 1 -or
        $assignments[0].Right.Expression -isnot [System.Management.Automation.Language.StringConstantExpressionAst]) {
        throw 'The shared launcher source is missing or ambiguous.'
    }
    return [string]$assignments[0].Right.Expression.Value
}

function Get-StartOverload {
    # 按参数个数区分重载；同一脚本里必须各只有一个，重复或缺失都说明兼容契约被破坏。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][type]$LauncherType, [Parameter(Mandatory = $true)][int]$Arity)
    $overloads = @($LauncherType.GetMethods() | Where-Object {
        $_.Name -ceq 'Start' -and $_.GetParameters().Count -eq $Arity -and -not $_.IsGenericMethod
    })
    Assert-True ($overloads.Count -eq 1) "Expected exactly one ${Arity}-parameter Start overload; found $($overloads.Count)."
    return $overloads[0]
}

function Get-CallArgumentConstant {
    # 读取八参数重载在调用九参数重载前压入的最后一个常量实参。C# 编译器为这个简单方法体
    # 生成 ldc.i4.0/1（false/true）；直接读 IL 能证明委托值，而不是只相信源码文本。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][System.Reflection.MethodInfo]$Overload,
        [Parameter(Mandatory = $true)][System.Reflection.MethodInfo]$Target
    )
    $body = $Overload.GetMethodBody()
    if ($null -eq $body) { throw 'The 8-parameter Start overload has no IL body.' }
    $il = $body.GetILAsByteArray()
    $module = $Overload.Module
    for ($index = 0; $index -lt $il.Length - 4; $index++) {
        # call 为 0x28，后跟 4 字节元数据 token；只接受解析后正好指向九参数重载的调用。
        if ($il[$index] -ne 0x28) { continue }
        $token = [BitConverter]::ToInt32($il, $index + 1)
        $resolved = $null
        try { $resolved = $module.ResolveMethod($token) } catch { continue }
        if ($null -eq $resolved -or $resolved.MetadataToken -ne $Target.MetadataToken) { continue }
        if ($index -lt 1) { continue }
        switch ($il[$index - 1]) {
            0x16 { return $false }  # ldc.i4.0
            0x17 { return $true }   # ldc.i4.1
        }
    }
    throw 'The 8-parameter Start overload does not call the 9-parameter overload with a literal boolean.'
}

$sourcePath = Join-Path $PSScriptRoot 'settings-parity.ps1'
$launcherSource = Get-LauncherSource -Path $sourcePath
# 只编译、不调用成功路径：类型加载只注册 P/Invoke 声明，不会创建桌面、进程或文件。
if (-not ('Kerminal.SettingsParity.NativeDesktopLauncher' -as [type])) {
    Add-Type -TypeDefinition $launcherSource -Language CSharp -ErrorAction Stop
}
$launcherType = [Kerminal.SettingsParity.NativeDesktopLauncher]

# 八参数签名必须与 HEAD 版一致：六个 string 加两个 uint，供既有派生消费者按位置调用。
$eight = Get-StartOverload -LauncherType $launcherType -Arity 8
$eightTypes = @($eight.GetParameters() | ForEach-Object { $_.ParameterType.FullName })
$expectedPrefix = @(
    'System.String', 'System.String', 'System.String',
    'System.String', 'System.String', 'System.String',
    'System.UInt32', 'System.UInt32'
)
Assert-True (($eightTypes -join ',') -ceq ($expectedPrefix -join ',')) 'The 8-parameter Start overload signature changed.'

# 九参数签名必须在八参数之后追加 bool useJobObject，agent-continuity 依赖它启用 Job。
$nine = Get-StartOverload -LauncherType $launcherType -Arity 9
$nineTypes = @($nine.GetParameters() | ForEach-Object { $_.ParameterType.FullName })
$nineNames = @($nine.GetParameters() | ForEach-Object { $_.Name })
Assert-True (($nineTypes -join ',') -ceq (($expectedPrefix + 'System.Boolean') -join ',')) 'The 9-parameter Start overload signature changed.'
Assert-True ($nineNames[8] -ceq 'useJobObject') 'The ninth Start parameter must remain useJobObject.'

# 委托行为：八参数重载必须把 false 传给九参数重载，保持 HEAD 版“无 Job Object”语义。
$delegated = Get-CallArgumentConstant -Overload $eight -Target $nine
Assert-True ($delegated -eq $false) 'The 8-parameter Start overload must delegate with useJobObject=false.'

# 调用路径证明：八参数和九参数都必须在创建桌面之前拒绝非法桌面名。这里用不存在的可执行
# 文件和不匹配的桌面名，确保只触达参数校验，不会创建私有桌面或启动任何进程。
$invalidDesktop = 'NotAPrivateDesktop'
Assert-Throws {
    $eight.Invoke($null, @(
        'C:\kerminal-compat-test-missing.exe', 'C:\', 'C:\kerminal-compat-test-output',
        'C:\kerminal-compat-test.stdout', 'C:\kerminal-compat-test.stderr',
        $invalidDesktop, [uint32]800, [uint32]600
    ))
} ([ArgumentException]) 'desktop name is outside the settings acceptance namespace'
Assert-Throws {
    $nine.Invoke($null, @(
        'C:\kerminal-compat-test-missing.exe', 'C:\', 'C:\kerminal-compat-test-output',
        'C:\kerminal-compat-test.stdout', 'C:\kerminal-compat-test.stderr',
        $invalidDesktop, [uint32]800, [uint32]600, $true
    ))
} ([ArgumentException]) 'desktop name is outside the settings acceptance namespace'

# 回归保护：八参数重载若被改成默认启用 Job 或绕过九参数入口，上面的 IL 断言会失败；
# 九参数若被删掉，八参数重载本身也无法通过参数个数查找。
Write-Host 'settings-parity.ps1 launcher compatibility tests passed.'
