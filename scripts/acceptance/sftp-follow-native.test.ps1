# @author kongweiguang
# 仅以 PowerShell AST 和进程内替身验证 fixture；不启动 WSL、SSH、原生程序或桌面。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    # 单测失败必须立即中止，避免后续清理回执掩盖先前断言。
    # @author kongweiguang
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Throws {
    # 检查错误类型对应的消息，避免把无关解析错误误判为预期失败。
    # @author kongweiguang
    param([scriptblock]$Action, [string]$ExpectedMessage)
    $failure = $null
    try { & $Action } catch { $failure = $_ }
    Assert-True ($null -ne $failure) "Expected failure: $ExpectedMessage"
    Assert-True ($failure.Exception.Message.Contains($ExpectedMessage)) "Unexpected failure: $failure"
}

function Stop-OwnedNativeProcess {
    # 替身只修改内存状态，确保 cleanup 单测不会访问任何真实进程。
    # @author kongweiguang
    param($NativeProcess)
    $NativeProcess.Stopped = $true
    $NativeProcess.Running = $false
}

function New-FakeNativeProcess {
    # 模拟仍在运行的私有桌面句柄，以检查终止和释放的先后关系。
    # @author kongweiguang
    $process = [pscustomobject]@{ Running = $true; Stopped = $false; Disposed = $false; DesktopClosed = $false }
    $process | Add-Member ScriptMethod WaitForExit { param($milliseconds) return -not $this.Running }
    $process | Add-Member ScriptMethod Dispose { $this.Disposed = $true; $this.DesktopClosed = $true }
    return $process
}

function New-FakeFixtureProcess {
    # 通过可控等待结果覆盖 listener 回收成功与超时，绝不创建真实 SSH 服务。
    # @author kongweiguang
    param([bool]$ShouldExit)
    $process = [pscustomobject]@{ ShouldExit = $ShouldExit; Waited = $false; ExitCode = 0 }
    $process | Add-Member ScriptMethod WaitForExit { param($milliseconds) $this.Waited = $true; return $this.ShouldExit }
    return $process
}

$sourcePath = Join-Path $PSScriptRoot 'sftp-follow-native.ps1'
$tokens = $null
$errors = $null
$syntax = [System.Management.Automation.Language.Parser]::ParseFile($sourcePath, [ref]$tokens, [ref]$errors)
Assert-True ($errors.Count -eq 0) "Acceptance script has parse errors: $($errors -join '; ')"
foreach ($functionName in @('Write-Utf8', 'Write-FixtureHostConfigurations', 'Set-ProcessEnvironmentValue', 'Get-ProcessEnvironmentSnapshot')) {
    $definitions = @($syntax.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $functionName
    }, $true))
    Assert-True ($definitions.Count -eq 1) "Expected exactly one function: $functionName"
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}
$mainTry = @($syntax.EndBlock.Statements | Where-Object { $_ -is [System.Management.Automation.Language.TryStatementAst] })
Assert-True ($mainTry.Count -eq 1 -and $null -ne $mainTry[0].Finally) 'Acceptance cleanup block is missing or ambiguous.'
$cleanupText = $mainTry[0].Finally.Extent.Text
$cleanupBlock = [scriptblock]::Create($cleanupText.Substring(1, $cleanupText.Length - 2))
$reportLoops = @($syntax.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.ForEachStatementAst] -and
        $node.Variable.VariablePath.UserPath -ceq 'operation' -and $node.Condition.Extent.Text -ceq '$expected'
}, $true))
Assert-True ($reportLoops.Count -eq 1) 'Acceptance report check is missing or ambiguous.'
$reportCheck = [scriptblock]::Create($reportLoops[0].Extent.Text)
$expectedAssignments = @($syntax.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$expected'
}, $true))
Assert-True ($expectedAssignments.Count -eq 1) 'Acceptance operation list is missing or ambiguous.'
$assignExpected = [scriptblock]::Create($expectedAssignments[0].Extent.Text)
$environmentAssignments = @($syntax.EndBlock.Statements | Where-Object {
    $_ -is [System.Management.Automation.Language.AssignmentStatementAst] -and $_.Left.Extent.Text -ceq '$environmentNames'
})
Assert-True ($environmentAssignments.Count -eq 1) 'Tracked environment variable list is missing or ambiguous.'
. ([scriptblock]::Create($environmentAssignments[0].Extent.Text))
foreach ($required in @('KERMINAL_RIGHT_RAIL_FOLLOW_ONLY', 'KERMINAL_RIGHT_RAIL_REFRESH_ONLY', 'KERMINAL_SFTP_FOLLOW_MANIFEST', 'KERMINAL_CONFIG_ROOT')) {
    Assert-True (@($environmentNames | Where-Object { $_ -ceq $required }).Count -eq 1) "Environment variable is not tracked exactly once: $required"
}
$hostCalls = @($syntax.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -ceq 'Write-FixtureHostConfigurations'
}, $true))
Assert-True ($hostCalls.Count -eq 1 -and $hostCalls[0].Extent.Text.Contains('-IncludeTransferChecks:$IncludeTransferChecks')) 'Transfer scope is not passed to fixture generation.'
$configAssignments = @($syntax.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -ceq 'Set-ProcessEnvironmentValue' -and
        $node.Extent.Text.Contains("-Name 'KERMINAL_CONFIG_ROOT'")
}, $true))
Assert-True ($configAssignments.Count -eq 1) 'Isolated child configuration root assignment is missing or ambiguous.'
$setChildConfigRoot = [scriptblock]::Create($configAssignments[0].Extent.Text)
$followAssignments = @($syntax.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -ceq 'Set-ProcessEnvironmentValue' -and
        $node.Extent.Text.Contains("-Name 'KERMINAL_RIGHT_RAIL_FOLLOW_ONLY'")
}, $true))
$fullScopeGuards = @($syntax.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.IfStatementAst] -and
        $node.Clauses[0].Item1.Extent.Text -ceq '$IncludeTransferChecks' -and $node.Extent.Text.Contains('$childEnvironment')
}, $true))
Assert-True ($followAssignments.Count -eq 1 -and $fullScopeGuards.Count -eq 1) 'Acceptance scope environment setup is missing or ambiguous.'
$setFollowScope = [scriptblock]::Create($followAssignments[0].Extent.Text)
$setFullScope = [scriptblock]::Create($fullScopeGuards[0].Extent.Text)
$originalEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames
$originalSetter = ${function:Set-ProcessEnvironmentValue}
$originalWriter = ${function:Write-Utf8}
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-sftp-follow-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
try {
    $fixtureRoot = Join-Path $testRoot 'child-isolation'
    . $setChildConfigRoot
    Assert-True ([Environment]::GetEnvironmentVariable('KERMINAL_CONFIG_ROOT', 'Process') -ceq (Join-Path $fixtureRoot 'environment-config')) 'Child configuration root is not the isolated sibling.'
    Set-ProcessEnvironmentValue -Name 'KERMINAL_RIGHT_RAIL_FOLLOW_ONLY' -Value 'inherited'
    Set-ProcessEnvironmentValue -Name 'KERMINAL_RIGHT_RAIL_REFRESH_ONLY' -Value ''
    $IncludeTransferChecks = $true
    . $setFollowScope
    . $setFullScope
    $childEnvironment = [Environment]::GetEnvironmentVariables('Process')
    Assert-True (-not $childEnvironment.Contains('KERMINAL_RIGHT_RAIL_FOLLOW_ONLY') -and
        -not $childEnvironment.Contains('KERMINAL_RIGHT_RAIL_REFRESH_ONLY')) 'Transfer scope retained a restrictive child environment flag.'
    $IncludeTransferChecks = $false
    . $setFollowScope
    Assert-True ([Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_FOLLOW_ONLY', 'Process') -ceq '1') 'Default follow scope changed.'
    . $assignExpected
    Assert-True (@($expected | Where-Object { $_ -ceq 'sftp-follow-disabled' }).Count -eq 1 -and
        @($expected | Where-Object { $_ -ceq 'sftp-workspace-dual-host' }).Count -eq 0) 'Default follow report scope changed.'
    foreach ($includeTransfers in @($false, $true)) {
        $config = Join-Path $testRoot "config-$includeTransfers"
        [void][IO.Directory]::CreateDirectory((Join-Path $config 'hosts'))
        Write-FixtureHostConfigurations -ConfigDirectory $config -Port 43210 -RemoteDirectory '/run/kerminal-sftp-follow-test/follow' `
            -KeyPath 'C:/isolated/client-key' -LocalPath 'C:/isolated/local-files' -IncludeTransferChecks:$includeTransfers
        $targetPath = Join-Path $config 'hosts/right-rail-fixture.toml'
        $sourcePath = Join-Path $config 'hosts/transfer-source-fixture.toml'
        Assert-True (Test-Path -LiteralPath $targetPath -PathType Leaf) 'Target host is missing.'
        Assert-True ((Test-Path -LiteralPath $sourcePath -PathType Leaf) -eq $includeTransfers) 'Source host does not match transfer scope.'
        $target = [IO.File]::ReadAllText($targetPath)
        Assert-True ($target -cmatch '(?m)^id = "right-rail-fixture"\r?$') 'Target HostId is incorrect.'
        if ($includeTransfers) {
            $source = [IO.File]::ReadAllText($sourcePath)
            Assert-True ($source -cmatch '(?m)^id = "transfer-source-fixture"\r?$') 'Source HostId is incorrect.'
            foreach ($content in @($target, $source)) {
                foreach ($field in @('host = "127.0.0.1"', 'port = 43210', 'username = "root"',
                    'credential_ref = "C:/isolated/client-key"', 'remote_start_directory = "/run/kerminal-sftp-follow-test/follow"',
                    'local_start_directory = "C:/isolated/local-files"', 'enabled = true')) {
                    Assert-True ($content.Contains($field)) "Fixture host lacks $field"
                }
            }
        }
    }

    $IncludeTransferChecks = $true
    . $assignExpected
    foreach ($required in @('sftp-workspace-dual-host', 'sftp-workspace-copy', 'sftp-workspace-drag-file',
        'sftp-workspace-drag-directory', 'sftp-workspace-tab-persist')) {
        Assert-True (@($expected | Where-Object { $_ -ceq $required }).Count -eq 1) "Required transfer operation is missing: $required"
    }
    # 仅构造内存中的校验输入，不生成、修改或伪造任何原生验收报告。
    $expected = @('fixture-operation')
    $report = [pscustomobject]@{ functional = @([pscustomobject]@{ operation = 'fixture-operation' }) }
    . $reportCheck
    $report = [pscustomobject]@{ functional = @() }
    Assert-Throws { . $reportCheck } 'Native acceptance report is missing: fixture-operation'
    $report = [pscustomobject]@{ functional = @([pscustomobject]@{ operation = 'fixture-operation' }, [pscustomobject]@{ operation = 'fixture-operation' }) }
    Assert-Throws { . $reportCheck } 'Native acceptance report has duplicate records: fixture-operation'

    foreach ($mode in @('absent', 'empty', 'value')) {
        foreach ($name in $environmentNames) {
            $value = switch ($mode) { 'absent' { $null } 'empty' { '' } 'value' { "original-$name" } }
            Set-ProcessEnvironmentValue -Name $name -Value $value
        }
        $previousEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames
        foreach ($name in $environmentNames) {
            Assert-True ($previousEnvironment[$name].Present -eq ($mode -ne 'absent')) "Cannot establish $mode state for $name"
            if ($mode -eq 'empty') { Assert-True ($previousEnvironment[$name].Value -ceq '') "Cannot establish empty value for $name" }
            Set-ProcessEnvironmentValue -Name $name -Value 'child-value'
        }
        $fixtureRoot = Join-Path $testRoot "cleanup-$mode"
        [void][IO.Directory]::CreateDirectory($fixtureRoot)
        [IO.File]::WriteAllText((Join-Path $fixtureRoot 'closed.json'), '{"listenerClosed":true,"remoteDirectoryRemoved":true}')
        [IO.File]::WriteAllText((Join-Path $fixtureRoot 'client-key'), 'fixture-key')
        $nativeProcess = New-FakeNativeProcess
        $fixtureProcess = New-FakeFixtureProcess -ShouldExit $true
        $cleanupFailures = [Collections.Generic.List[string]]::new()
        . $cleanupBlock
        $restored = Get-ProcessEnvironmentSnapshot -Names $environmentNames
        foreach ($name in $environmentNames) {
            Assert-True ($restored[$name].Present -eq $previousEnvironment[$name].Present -and
                $restored[$name].Value -ceq $previousEnvironment[$name].Value) "Environment restoration failed: $mode $name"
        }
        Assert-True ($nativeProcess.Stopped -and $nativeProcess.Disposed -and $nativeProcess.DesktopClosed) 'Private desktop cleanup was not attempted.'
        Assert-True ($fixtureProcess.Waited -and (Test-Path -LiteralPath (Join-Path $fixtureRoot 'stop.fixture') -PathType Leaf)) 'Fixture stop was not requested.'
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $fixtureRoot 'client-key'))) 'Fixture key was not removed.'
        Assert-True ($cleanupFailures.Count -eq 0) "Unexpected cleanup failure: $($cleanupFailures -join '; ')"
    }

    # 注入源主机文件未写入，验证启动前明确失败后仍停止 fixture 并删除密钥。
    $config = Join-Path $testRoot 'config-missing-source'
    [void][IO.Directory]::CreateDirectory((Join-Path $config 'hosts'))
    $fixtureRoot = Join-Path $testRoot 'cleanup-missing-source'
    [void][IO.Directory]::CreateDirectory($fixtureRoot)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'closed.json'), '{"listenerClosed":true,"remoteDirectoryRemoved":true}')
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'client-key'), 'fixture-key')
    $nativeProcess = $null
    $fixtureProcess = New-FakeFixtureProcess -ShouldExit $true
    $cleanupFailures = [Collections.Generic.List[string]]::new()
    $previousEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames
    foreach ($name in $environmentNames) { Set-ProcessEnvironmentValue -Name $name -Value 'child-after-setup' }
    ${function:Write-Utf8} = {
        param([string]$Path, [string]$Content)
        if ($Path.EndsWith('transfer-source-fixture.toml', [StringComparison]::Ordinal)) { return }
        & $originalWriter -Path $Path -Content $Content
    }
    $fixtureFailure = $null
    try {
        Write-FixtureHostConfigurations -ConfigDirectory $config -Port 43210 -RemoteDirectory '/run/kerminal-sftp-follow-test/follow' `
            -KeyPath 'C:/isolated/client-key' -LocalPath 'C:/isolated/local-files' -IncludeTransferChecks
    } catch { $fixtureFailure = $_ }
    finally { . $cleanupBlock }
    ${function:Write-Utf8} = $originalWriter
    Assert-True ($null -ne $fixtureFailure -and $fixtureFailure.Exception.Message.Contains('transfer-source-fixture')) 'Missing fixture was not rejected early.'
    Assert-True ($fixtureProcess.Waited -and -not (Test-Path -LiteralPath (Join-Path $fixtureRoot 'client-key'))) 'Fixture failure skipped resource cleanup.'
    Assert-True ($cleanupFailures.Count -eq 0) "Unexpected failure-path cleanup error: $($cleanupFailures -join '; ')"
    $restored = Get-ProcessEnvironmentSnapshot -Names $environmentNames
    foreach ($name in $environmentNames) {
        Assert-True ($restored[$name].Present -eq $previousEnvironment[$name].Present -and
            $restored[$name].Value -ceq $previousEnvironment[$name].Value) "Fixture failure did not restore $name"
    }

    # 进程已退出也不能代替远端目录删除回执，密钥清理仍需继续。
    $fixtureRoot = Join-Path $testRoot 'cleanup-unconfirmed'
    [void][IO.Directory]::CreateDirectory($fixtureRoot)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'closed.json'), '{"listenerClosed":true,"remoteDirectoryRemoved":false}')
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'client-key'), 'fixture-key')
    $previousEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames
    $nativeProcess = $null
    $fixtureProcess = New-FakeFixtureProcess -ShouldExit $true
    $cleanupFailures = [Collections.Generic.List[string]]::new()
    . $cleanupBlock
    Assert-True ($cleanupFailures.Count -eq 1 -and $cleanupFailures[0].Contains('cleanup was not confirmed')) 'Unconfirmed remote cleanup was treated as success.'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $fixtureRoot 'client-key'))) 'Key survived unconfirmed remote cleanup.'

    # 环境恢复和 listener 等待各自失败时仍尝试其余资源，不把未知回收结果当作成功。
    $fixtureRoot = Join-Path $testRoot 'cleanup-errors'
    [void][IO.Directory]::CreateDirectory($fixtureRoot)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'client-key'), 'fixture-key')
    $previousEnvironment = Get-ProcessEnvironmentSnapshot -Names $environmentNames
    foreach ($name in $environmentNames) { Set-ProcessEnvironmentValue -Name $name -Value 'child-before-errors' }
    ${function:Set-ProcessEnvironmentValue} = {
        param([string]$Name, [AllowNull()][object]$Value)
        if ($Name -ceq 'KERMINAL_RIGHT_RAIL_FOLLOW_ONLY') { throw 'injected environment error' }
        & $originalSetter -Name $Name -Value $Value
    }
    $nativeProcess = New-FakeNativeProcess
    $fixtureProcess = New-FakeFixtureProcess -ShouldExit $false
    $cleanupFailures = [Collections.Generic.List[string]]::new()
    . $cleanupBlock
    ${function:Set-ProcessEnvironmentValue} = $originalSetter
    Assert-True ($cleanupFailures.Count -eq 2) "Expected two cleanup errors: $($cleanupFailures -join '; ')"
    Assert-True (($cleanupFailures -join '; ').Contains('Environment restoration failed') -and
        ($cleanupFailures -join '; ').Contains('SSH fixture did not exit')) 'Cleanup errors were not preserved.'
    Assert-True ($nativeProcess.Stopped -and $nativeProcess.Disposed -and $fixtureProcess.Waited) 'Cleanup stopped after an earlier error.'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $fixtureRoot 'client-key'))) 'Key survived an unrelated cleanup failure.'
    $restored = Get-ProcessEnvironmentSnapshot -Names $environmentNames
    foreach ($name in $environmentNames | Where-Object { $_ -cne 'KERMINAL_RIGHT_RAIL_FOLLOW_ONLY' }) {
        Assert-True ($restored[$name].Present -eq $previousEnvironment[$name].Present -and
            $restored[$name].Value -ceq $previousEnvironment[$name].Value) "Other environment restoration was skipped: $name"
    }
    Write-Output 'SFTP follow fixture AST and isolation tests passed.'
} finally {
    ${function:Set-ProcessEnvironmentValue} = $originalSetter
    ${function:Write-Utf8} = $originalWriter
    foreach ($name in $environmentNames) {
        $previous = $originalEnvironment[$name]
        Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
    }
    if (Test-Path -LiteralPath $testRoot) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}
