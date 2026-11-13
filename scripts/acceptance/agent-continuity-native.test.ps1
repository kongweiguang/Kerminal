# @author kongweiguang
# 仅用 PowerShell AST、临时文件和进程内对象验证包装脚本；不启动原生程序、Cargo 或私有桌面。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    # 单测失败立即中止，避免后续清理结果掩盖先前断言。
    # @author kongweiguang
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Throws {
    # 只接受预期消息，防止无关语法或运行时错误被误判为成功校验。
    # @author kongweiguang
    param([scriptblock]$Action, [string]$ExpectedMessage)
    $failure = $null
    try { & $Action } catch { $failure = $_ }
    Assert-True ($null -ne $failure) "Expected failure: $ExpectedMessage"
    Assert-True ($failure.Exception.Message.Contains($ExpectedMessage)) "Unexpected failure: $failure"
}

$sourcePath = Join-Path $PSScriptRoot 'agent-continuity-native.ps1'
$tokens = $null
$errors = $null
$syntax = [System.Management.Automation.Language.Parser]::ParseFile($sourcePath, [ref]$tokens, [ref]$errors)
Assert-True ($errors.Count -eq 0) "Agent continuity wrapper has parse errors: $($errors -join '; ')"
$sourceText = [IO.File]::ReadAllText($sourcePath)

foreach ($forbidden in @('GetProcessById(', 'Kill($true)', 'Stop-Process', 'taskkill', 'Set-ExecutionPolicy')) {
    Assert-True (-not $sourceText.Contains($forbidden)) "Wrapper must not use PID-based kill or policy bypass: $forbidden"
}
Assert-True ($sourceText.Contains('Terminate(1)')) 'Wrapper must retain exact-handle termination fallback.'
Assert-True ($sourceText.Contains('TerminateJobAndWait(1, 30000)')) 'Wrapper must terminate and verify the owned job object.'
Assert-True ($sourceText.Contains('JobAssigned')) 'Wrapper does not record whether the launcher assigned a job object.'
Assert-True ($sourceText.Contains('JobCleanupVerified')) 'Wrapper does not require verified job-object cleanup.'
Assert-True ($sourceText.Contains('JobCleanupForced')) 'Wrapper does not record forced job termination.'
Assert-True ($sourceText -match '(?s)elseif \(\$jobCleanupForced\).*natural descendant reclamation was not proven') 'Wrapper must reject forced job termination as natural reclamation.'
Assert-True ($sourceText.Contains('-and -not $jobCleanupForced')) 'Descendant proof must exclude forced termination.'
Assert-True ($sourceText.Contains('knownFolderApiCovered = $false')) 'Wrapper must not claim KnownFolder API isolation.'
Assert-True ($sourceText.Contains('userTokenIsolated = $false')) 'Wrapper must not claim same-user token isolation.'
Assert-True ($sourceText.Contains('otherKerminalEnvironmentCleared = $otherKerminalEnvironmentCleared')) 'Wrapper must report the observed inherited KERMINAL clearing state, not a fixed true.'
# 回执不能在清理循环尚未执行时就声称已清除；标志必须初始为 false 并在循环后置真。
Assert-True ($sourceText.Contains('$otherKerminalEnvironmentCleared = $false')) 'Wrapper must initialize the environment-clearing receipt as false.'
Assert-True ($sourceText -match '(?s)foreach \(\$name in \$inheritedKerminalNames\).*?\$otherKerminalEnvironmentCleared = \$true') 'Wrapper must only report clearing after the inherited-switch loop completes.'
Assert-True ($sourceText.Contains('Get-InheritedKerminalEnvironmentNames')) 'Wrapper must enumerate inherited KERMINAL switches.'
# 外层宿主参数是隔离前提：静态检查入口必须先核对本脚本路径与 -NoProfile/-NonInteractive，
# 否则交互式或加载 profile 的 pwsh 会在隔离目录创建前就写入用户历史。
Assert-True ($sourceText.Contains('Assert-OuterShellArguments -SourceRoot $PSScriptRoot')) 'Wrapper must validate its own pwsh host arguments.'
$hostCheckIndex = $sourceText.IndexOf('Assert-OuterShellArguments -SourceRoot $PSScriptRoot')
$firstCreateIndex = $sourceText.IndexOf('[void][IO.Directory]::CreateDirectory($runRoot)')
Assert-True ($hostCheckIndex -ge 0 -and $firstCreateIndex -gt $hostCheckIndex) 'Host-argument validation must run before creating the run directory.'
Assert-True ($sourceText -match '(?s)foreach \(\$name in \$inheritedKerminalNames\).*Set-ProcessEnvironmentValue -Name \$name -Value \$null') 'Wrapper must clear inherited KERMINAL switches before launch.'
Assert-True ($sourceText.Contains('public\s+static\s+NativeDesktopProcess\s+Start\s*\(')) 'Wrapper must verify the shared Start signatures instead of substring presence.'
Assert-True ($sourceText.Contains('Assert-AgentContinuityStartContract')) 'Wrapper must validate the 8- and 9-parameter Start overload contract.'
Assert-True ($sourceText.Contains('exactly the 8-parameter compatibility overload and the 9-parameter job overload')) 'Wrapper must pin both Start overloads.'
Assert-True ($sourceText.Contains('bool\s+useJobObject')) 'Wrapper must pin the bool job-object parameter.'
Assert-True ($sourceText.Contains('useJobObject=false')) 'Wrapper must require the compatibility overload to keep the no-job behavior.'
Assert-True ($sourceText.Contains('StartCore')) 'Wrapper must verify the shared StartCore contract.'
Assert-True ($sourceText -match '(?s)NativeDesktopLauncher\]::Start\(.*?\$true\s*\)') 'Wrapper must opt into the launcher job-object containment.'
Assert-True ($sourceText.Contains('--right-rail-acceptance --agent-continuity-only')) 'Wrapper does not pin the agent-only entry command.'
Assert-True ($sourceText.Contains('WaitForExit(30000)')) 'Wrapper lacks a bounded exact-handle termination wait.'
$outsideIndex = $sourceText.IndexOf('Assert-PathOutsideUserConfiguration -Path $runRoot')
$createIndex = $sourceText.IndexOf('[void][IO.Directory]::CreateDirectory($runRoot)')
Assert-True ($outsideIndex -ge 0 -and $createIndex -gt $outsideIndex) 'User-directory rejection must run before creating the run directory.'

foreach ($functionName in @(
    'Assert-True',
    'Get-ProcessEnvironmentSnapshot',
    'Set-ProcessEnvironmentValue',
    'Write-Utf8',
    'Test-PathWithinRoot',
    'Assert-RunDirectoryText',
    'Assert-OuterShellArguments',
    'Get-InheritedKerminalEnvironmentNames',
    'Get-AcceptanceExecutableHash',
    'Get-StartOverloadSignatures',
    'Get-CSharpMethodBody',
    'Assert-AgentContinuityStartContract',
    'Get-AgentContinuityDesktopLauncherSource',
    'Assert-AgentContinuityCapture',
    'Assert-AgentContinuityReport',
    'Assert-AgentContinuityDirectories'
)) {
    $definitions = @($syntax.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $functionName
    }, $true))
    Assert-True ($definitions.Count -eq 1) "Expected exactly one function: $functionName"
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}

$environmentAssignments = @($syntax.EndBlock.Statements | Where-Object {
    $_ -is [System.Management.Automation.Language.AssignmentStatementAst] -and $_.Left.Extent.Text -ceq '$environmentNames'
})
Assert-True ($environmentAssignments.Count -eq 1) 'Tracked environment variable list is missing or ambiguous.'
. ([scriptblock]::Create($environmentAssignments[0].Extent.Text))
foreach ($required in @(
    'USERPROFILE', 'HOME', 'HOMEDRIVE', 'HOMEPATH', 'APPDATA', 'LOCALAPPDATA',
    'TEMP', 'TMP', 'TMPDIR', 'PATH', 'PSModulePath',
    'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME',
    'KERMINAL_CONFIG_ROOT', 'KERMINAL_RIGHT_RAIL_FIXTURE_MANIFEST',
    'KERMINAL_SFTP_FOLLOW_MANIFEST', 'KERMINAL_SETTINGS_ACCEPTANCE_SCOPE'
)) {
    Assert-True (@($environmentNames | Where-Object { $_ -ceq $required }).Count -eq 1) "Environment variable is not tracked exactly once: $required"
}

$launcher = Get-AgentContinuityDesktopLauncherSource -SourceRoot $PSScriptRoot
Assert-True ($launcher.Contains('--right-rail-acceptance --agent-continuity-only --output-dir')) 'Launcher replacement lost the agent-only command.'
Assert-True (-not $launcher.Contains('--settings-acceptance')) 'Launcher replacement retained the settings entry command.'
Assert-True ($launcher.Contains('namespace Kerminal.AgentContinuityAcceptance')) 'Launcher namespace was not isolated.'
foreach ($required in @(
    'CreateDesktopW', 'QuoteArgument', 'KerminalRail-settings-',
    'CreateJobObject', 'AssignProcessToJobObject', 'TerminateJobObject', 'QueryActiveProcessCount'
)) {
    Assert-True ($launcher.Contains($required)) "Launcher replacement lost: $required"
}

# 共享 launcher 的兼容重载回归：分别确认八/九参数签名与八参数到九参数的 false 委托，
# 再构造错误形态（三重重载、八参数传 true、八参数直接调用 StartCore、九参数末位不是
# bool）确认校验会失败关闭。全部是内存字符串检查，不编译 C#、不启动进程、不碰桌面。
$startSignatures = @(Get-StartOverloadSignatures -Source $launcher)
Assert-True ($startSignatures.Count -eq 2) 'Shared launcher must expose the 8- and 9-parameter Start overloads.'
Assert-True (@($startSignatures | Where-Object { $_.Parameters.Count -eq 8 }).Count -eq 1) 'Shared launcher must keep one 8-parameter compatibility overload.'
Assert-True (@($startSignatures | Where-Object { $_.Parameters.Count -eq 9 }).Count -eq 1) 'Shared launcher must keep one 9-parameter job overload.'
Assert-AgentContinuityStartContract -Source $launcher
$eightOverload = @($startSignatures | Where-Object { $_.Parameters.Count -eq 8 })[0]
$eightBody = Get-CSharpMethodBody -Source $launcher -CloseParenIndex $eightOverload.Close
Assert-True ($eightBody -match '(?s)\bStart\s*\([^)]*,\s*false\s*\)\s*;') 'The 8-parameter overload must delegate with useJobObject=false.'
Assert-True (-not $eightBody.Contains('StartCore')) 'The 8-parameter overload must not bypass the 9-parameter entry.'
Assert-True ($eightBody -notmatch '\btrue\b') 'The 8-parameter overload must not enable the job object.'

$wrongOverloads = @(
    @{ Name = 'three overloads'; Source = $launcher.Replace(
            'public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height)',
            'public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height, string extra)' + [Environment]::NewLine + '        public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height)')
        Expected = 'exactly the 8-parameter compatibility overload and the 9-parameter job overload' },
    @{ Name = 'compatibility overload enables job'; Source = $launcher.Replace(
            'return Start(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, width, height, false);',
            'return Start(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, width, height, true);')
        Expected = 'must delegate to the 9-parameter overload with useJobObject=false' },
    @{ Name = 'compatibility overload bypasses entry'; Source = $launcher.Replace(
            'return Start(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, width, height, false);',
            'return StartCore(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, false, command, false);')
        Expected = 'must delegate to the 9-parameter overload with useJobObject=false' },
    @{ Name = 'ninth parameter is not the bool switch'; Source = $launcher.Replace(
            'uint width, uint height, bool useJobObject)',
            'uint width, uint height, int useJobObject)')
        Expected = 'ninth Start parameter must remain the bool job-object switch' }
)
foreach ($case in $wrongOverloads) {
    Assert-Throws { Assert-AgentContinuityStartContract -Source $case.Source } $case.Expected
}

Assert-RunDirectoryText -Text 'C:\kerminal-agent-continuity\run'
Assert-Throws { Assert-RunDirectoryText -Text ('C:\run' + [char]0x2018 + 'x') } 'Unicode single-quote'
Assert-Throws { Assert-RunDirectoryText -Text ('C:\run' + [char]0x2019 + 'x') } 'Unicode single-quote'
Assert-Throws { Assert-RunDirectoryText -Text ('C:\run' + [char]1 + 'x') } 'control'

# 外层宿主只接受指向本脚本的 -File 加 -NoProfile/-NonInteractive；任一缺失都要在创建目录前失败，
# 因为 profile/交互模式会改写用户状态，无法用后续隔离目录或 Job 回收来补偿。
$ownScript = Join-Path $PSScriptRoot 'agent-continuity-native.ps1'
Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @(
    '-NoProfile', '-NonInteractive', '-File', $ownScript, '-Width', '1440'
)
Assert-Throws {
    Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @('-NoProfile', '-File', $ownScript)
} 'requires -NonInteractive'
Assert-Throws {
    Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @('-NonInteractive', '-File', $ownScript)
} 'requires -NoProfile'
Assert-Throws {
    Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @('-NoProfile', '-NonInteractive')
} 'must be launched with pwsh.exe'
Assert-Throws {
    Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @(
        '-NoProfile', '-NonInteractive', '-File', (Join-Path $PSScriptRoot 'settings-parity.ps1')
    )
} 'must be launched with pwsh.exe'
Assert-Throws {
    Assert-OuterShellArguments -SourceRoot $PSScriptRoot -Arguments @(
        '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $ownScript
    )
} 'unsupported pwsh host arguments'

Assert-True (Test-PathWithinRoot -Path 'C:\runs\a\out' -Root 'C:\runs\a') 'Nested path was not recognized.'
Assert-True (-not (Test-PathWithinRoot -Path 'C:\runs\ab\out' -Root 'C:\runs\a')) 'Sibling prefix was treated as nested.'

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-agent-continuity-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
try {
    $fakeExecutable = Join-Path $testRoot 'kerminal.exe'
    [IO.File]::WriteAllText($fakeExecutable, 'prebuilt acceptance executable for hash validation')
    $hash = Get-AcceptanceExecutableHash -Path $fakeExecutable
    Assert-True ($hash -cmatch '^[0-9a-f]{64}$') 'Executable identity helper did not return a SHA256 digest.'
    [IO.File]::Delete($fakeExecutable)
    Assert-Throws { Get-AcceptanceExecutableHash -Path $fakeExecutable } 'executable is missing'

    $environmentProbe = @('KERMINAL_AGENT_CONTINUITY_TEST_A', 'KERMINAL_AGENT_CONTINUITY_TEST_B')
    $originalProbe = Get-ProcessEnvironmentSnapshot -Names $environmentProbe
    try {
        foreach ($name in $environmentProbe) {
            Set-ProcessEnvironmentValue -Name $name -Value "probe-$name"
        }
        $inherited = @(Get-InheritedKerminalEnvironmentNames)
        foreach ($name in $environmentProbe) {
            Assert-True (@($inherited | Where-Object { $_ -ceq $name }).Count -eq 1) "Inherited probe switch was not enumerated: $name"
        }
        Assert-True (@($inherited | Where-Object { $_ -ceq 'KERMINAL_CONFIG_ROOT' }).Count -eq 0) 'The pinned config root must not be treated as an inherited switch.'
    }
    finally {
        foreach ($name in $environmentProbe) {
            $previous = $originalProbe[$name]
            Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
        }
    }

    $environmentProbe = @('KERMINAL_AGENT_CONTINUITY_TEST_A', 'KERMINAL_AGENT_CONTINUITY_TEST_B')
    $originalProbe = Get-ProcessEnvironmentSnapshot -Names $environmentProbe
    try {
        foreach ($mode in @('absent', 'empty', 'value')) {
            foreach ($name in $environmentProbe) {
                $value = switch ($mode) { 'absent' { $null } 'empty' { '' } 'value' { "original-$name" } }
                Set-ProcessEnvironmentValue -Name $name -Value $value
            }
            $snapshot = Get-ProcessEnvironmentSnapshot -Names $environmentProbe
            foreach ($name in $environmentProbe) {
                Assert-True ($snapshot[$name].Present -eq ($mode -ne 'absent')) "Cannot establish $mode state for $name"
                Set-ProcessEnvironmentValue -Name $name -Value 'child-value'
            }
            foreach ($name in $environmentProbe) {
                $previous = $snapshot[$name]
                Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
            }
            $restored = Get-ProcessEnvironmentSnapshot -Names $environmentProbe
            foreach ($name in $environmentProbe) {
                Assert-True ($restored[$name].Present -eq $snapshot[$name].Present -and
                    $restored[$name].Value -ceq $snapshot[$name].Value) "Environment restoration failed: $mode $name"
            }
        }
    }
    finally {
        foreach ($name in $environmentProbe) {
            $previous = $originalProbe[$name]
            Set-ProcessEnvironmentValue -Name $name -Value $(if ($previous.Present) { $previous.Value } else { $null })
        }
    }

    $runRoot = Join-Path $testRoot 'run'
    $outputDirectory = Join-Path $runRoot 'output'
    $configDirectory = Join-Path $outputDirectory 'config'
    $profilesDirectory = Join-Path $configDirectory 'profiles'
    [void][IO.Directory]::CreateDirectory($profilesDirectory)
    Write-Utf8 -Path (Join-Path $profilesDirectory 'agent-continuity-local.toml') -Content (@(
        'schema_version = 1',
        'id = "agent-continuity-local"',
        'name = "Agent Continuity Local"',
        'shell = "pwsh.exe"',
        'args = ["-NoLogo", "-NoProfile"]',
        'env = {}',
        ('cwd = "' + $outputDirectory.Replace('\', '\\') + '"'),
        'is_default = true',
        'sort_order = 0',
        'created_at = "0"',
        'updated_at = "0"'
    ) -join "`n")
    $operations = @(
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
    $writePng = {
        param([string]$Path)
        # 只构造验证器需要的最小 PNG 头；不解码像素，也不声称是真实渲染图。
        $bytes = [byte[]]::new(33)
        foreach ($pair in @(@(0, 137), @(1, 80), @(2, 78), @(3, 71), @(4, 13), @(5, 10), @(6, 26), @(7, 10))) {
            $bytes[$pair[0]] = [byte]$pair[1]
        }
        $bytes[11] = 13
        $bytes[12] = 73
        $bytes[13] = 72
        $bytes[14] = 68
        $bytes[15] = 82
        $bytes[19] = 1
        $bytes[23] = 1
        [IO.File]::WriteAllBytes($Path, $bytes)
    }
    $functional = @()
    foreach ($operation in $operations) {
        & $writePng (Join-Path $outputDirectory "functional-$operation.png")
        $functional += [pscustomobject]@{
            operation = $operation
            outcome = 'fixture-outcome'
            evidence = [pscustomobject]@{
                file = "functional-$operation.png"
                width = 1
                height = 1
                pixelSha256 = ('0' * 64)
            }
        }
    }
    $newReport = {
        [pscustomobject]@{
            schemaVersion = 6
            captureMode = 'native_local_agent_continuity_same_tab_split_without_fixture'
            captureBackend = 'native_window_render_to_image'
            fixtureHostId = 'agent-continuity-local'
            fixtureHostKeyPolicy = 'not_applicable_local_profile_without_remote_fixture'
            windowVisible = $false
            requestedWindowWidth = 1440
            requestedWindowHeight = 900
            windowWidth = 1440.0
            windowHeight = 900.0
            configDirectory = $configDirectory
            fixtureConfigSource = $configDirectory
            nativeIsolation = [pscustomobject]@{
                platformSupported = $true
                uiThreadId = 77
                uiThreadDesktopName = 'KerminalRail-settings-agent-test'
                inputDesktopName = 'Default'
                processId = 1234
                isolationProven = $true
            }
            themes = @()
            functional = $functional
        }
    }
    $report = & $newReport
    Assert-AgentContinuityReport -Report $report -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory `
        -DesktopName 'KerminalRail-settings-agent-test' -ProcessId 1234 -Width 1440 -Height 900
    Assert-AgentContinuityDirectories -RunDirectory $runRoot -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory

    $bad = & $newReport
    $bad.captureMode = 'hidden-native-window'
    Assert-Throws { Assert-AgentContinuityReport -Report $bad -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory `
        -DesktopName 'KerminalRail-settings-agent-test' -ProcessId 1234 -Width 1440 -Height 900 } 'captureMode'
    $bad = & $newReport
    $bad.functional = @($functional + $functional[0])
    Assert-Throws { Assert-AgentContinuityReport -Report $bad -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory `
        -DesktopName 'KerminalRail-settings-agent-test' -ProcessId 1234 -Width 1440 -Height 900 } 'checkpoint count'
    $removedEvidence = Join-Path $outputDirectory 'functional-agent-continuity-started.png'
    [IO.File]::Delete($removedEvidence)
    Assert-Throws { Assert-AgentContinuityReport -Report $report -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory `
        -DesktopName 'KerminalRail-settings-agent-test' -ProcessId 1234 -Width 1440 -Height 900 } 'evidence is missing'
    & $writePng $removedEvidence

    [void][IO.Directory]::CreateDirectory((Join-Path $configDirectory 'hosts'))
    Assert-Throws { Assert-AgentContinuityDirectories -RunDirectory $runRoot -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory } 'contains hosts'
    [IO.Directory]::Delete((Join-Path $configDirectory 'hosts'))
    [void][IO.Directory]::CreateDirectory((Join-Path $outputDirectory 'agent-continuity-leftover'))
    Assert-Throws { Assert-AgentContinuityDirectories -RunDirectory $runRoot -OutputDirectory $outputDirectory -ConfigDirectory $configDirectory } 'was not cleaned'
    [IO.Directory]::Delete((Join-Path $outputDirectory 'agent-continuity-leftover'))
}
finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}

Write-Host 'agent-continuity-native.ps1 static tests passed.'
