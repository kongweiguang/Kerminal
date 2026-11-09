# @author kongweiguang
[CmdletBinding()]
param(
    [string]$ExecutablePath,
    [string]$OutputDirectory,
    [ValidateSet('debug', 'release')]
    [string]$BuildProfile = 'debug',
    [ValidateRange(800, 3840)]
    [int]$Width = 1360,
    [ValidateRange(600, 2160)]
    [int]$Height = 900,
    [ValidateRange(30, 600)]
    [int]$TimeoutSeconds = 180
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-HostEditorDesktopLauncherSource {
    # 复用已经用于设置与插件验收的 Win32 私有桌面进程所有权实现。
    # @author kongweiguang
    $tokens = $null
    $errors = $null
    $path = Join-Path $PSScriptRoot 'settings-parity.ps1'
    $syntax = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw '无法解析私有桌面 launcher 源文件。' }
    $assignments = @($syntax.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            $node.Left.Extent.Text -ceq '$script:NativeDesktopLauncherSource'
    }, $true))
    if ($assignments.Count -ne 1 -or
        $assignments[0].Right.Expression -isnot [System.Management.Automation.Language.StringConstantExpressionAst]) {
        throw '私有桌面 launcher 源缺失或不唯一。'
    }
    $source = $assignments[0].Right.Expression.Value
    $oldCommand = '+ " --settings-acceptance --output-dir " + QuoteArgument(outputDirectory)'
    $newCommand = '+ " --host-editor-acceptance --output-dir " + QuoteArgument(outputDirectory)'
    $first = $source.IndexOf($oldCommand, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $source.IndexOf($oldCommand, $first + $oldCommand.Length, [StringComparison]::Ordinal) -ge 0) {
        throw '私有桌面 launcher 的启动参数已变化，拒绝未经核对的替换。'
    }
    $oldEnvironment = 'variables["GIT_CONFIG_GLOBAL"] = System.IO.Path.Combine(outputDirectory, "config", ".acceptance-sync", "global.gitconfig");'
    $newEnvironment = 'variables["KERMINAL_CONFIG_ROOT"] = System.IO.Path.Combine(outputDirectory, "config");' + [Environment]::NewLine +
        '            ' + $oldEnvironment
    $environmentIndex = $source.IndexOf($oldEnvironment, [StringComparison]::Ordinal)
    if ($environmentIndex -lt 0 -or $source.IndexOf($oldEnvironment, $environmentIndex + $oldEnvironment.Length, [StringComparison]::Ordinal) -ge 0) {
        throw '私有桌面 launcher 的环境构造已变化，拒绝未经核对的替换。'
    }
    return $source.Replace($oldCommand, $newCommand).
        Replace($oldEnvironment, $newEnvironment).
        Replace('namespace Kerminal.SettingsParity', 'namespace Kerminal.HostEditorAcceptance')
}

function Stop-OwnedHostEditorProcess {
    # 只结束 launcher 返回的本轮进程，并等它退出后再关闭私有桌面句柄。
    # @author kongweiguang
    param([Parameter(Mandatory)]$NativeProcess)
    $process = $null
    try { $process = [Diagnostics.Process]::GetProcessById($NativeProcess.ProcessId) }
    catch [ArgumentException] { $process = $null }
    if ($null -ne $process) {
        try { if (-not $process.HasExited) { $process.Kill($true) } }
        finally { $process.Dispose() }
    }
    if (-not $NativeProcess.WaitForExit(10000)) {
        $NativeProcess.Terminate(1)
        if (-not $NativeProcess.WaitForExit(10000)) { throw '验收子进程在终止后仍未退出。' }
    }
}

function Assert-HostEditorCapture {
    # PNG 必须匹配物理 viewport，且实际正文/状态栏/保存按钮均须具有有效绘制边界。
    # @author kongweiguang
    param(
        [Parameter(Mandatory)]$Capture,
        [Parameter(Mandatory)][string]$RunDirectory,
        [Parameter(Mandatory)][uint32]$ExpectedWidth,
        [Parameter(Mandatory)][uint32]$ExpectedHeight
    )
    $fileName = [string]$Capture.file
    if ([string]::IsNullOrWhiteSpace($fileName) -or [IO.Path]::GetFileName($fileName) -cne $fileName) {
        throw "截图记录不是单个文件名：$fileName"
    }
    $path = Join-Path $RunDirectory $fileName
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "截图缺失：$fileName" }
    $bytes = [IO.File]::ReadAllBytes($path)
    if ($bytes.Length -lt 24 -or [Convert]::ToHexString([byte[]]$bytes[0..7]) -cne '89504E470D0A1A0A') {
        throw "截图不是有效 PNG：$fileName"
    }
    $actualWidth = [uint32]$bytes[16] * 16777216 + [uint32]$bytes[17] * 65536 + [uint32]$bytes[18] * 256 + [uint32]$bytes[19]
    $actualHeight = [uint32]$bytes[20] * 16777216 + [uint32]$bytes[21] * 65536 + [uint32]$bytes[22] * 256 + [uint32]$bytes[23]
    if ($actualWidth -ne $ExpectedWidth -or $actualHeight -ne $ExpectedHeight -or
        $Capture.width -ne $ExpectedWidth -or $Capture.height -ne $ExpectedHeight) {
        throw "截图尺寸不符：$fileName expected=${ExpectedWidth}x${ExpectedHeight}, actual=${actualWidth}x${actualHeight}"
    }
    if ([string]$Capture.pixelSha256 -notmatch '^[0-9a-f]{64}$') {
        throw "截图像素摘要无效：$fileName"
    }
    foreach ($part in @('body', 'statusLane', 'saveButton')) {
        $bounds = $Capture.editorGeometry.$part
        foreach ($coordinate in @('x', 'y', 'width', 'height')) {
            if (-not [double]::IsFinite([double]$bounds.$coordinate)) {
                throw "截图的 $part 几何包含无效值：$fileName"
            }
        }
        if ($bounds.width -le 0 -or $bounds.height -le 0) {
            throw "截图的 $part 没有有效绘制面积：$fileName"
        }
    }
}

if (-not $IsWindows) {
    throw '此 PowerShell 启动器使用 Windows 私有桌面；Linux 请在独立 Xvfb DISPLAY 下启动 host editor acceptance。'
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
if (-not $ExecutablePath) {
    $buildArguments = @('build', '--locked', '-p', 'kerminal-desktop', '--features', 'right-rail-acceptance')
    if ($BuildProfile -ceq 'release') { $buildArguments += '--release' }
    Push-Location $repositoryRoot
    try {
        & cargo @buildArguments
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }
    }
    finally { Pop-Location }
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repositoryRoot 'target' }
    if (-not [IO.Path]::IsPathRooted($targetRoot)) { $targetRoot = Join-Path $repositoryRoot $targetRoot }
    $profileDirectory = if ($BuildProfile -ceq 'release') { 'release' } else { 'debug' }
    $ExecutablePath = Join-Path $targetRoot "$profileDirectory\kerminal.exe"
}
$executable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw "找不到验收 executable：$executable" }
$candidateHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()

$baseDirectory = Join-Path $repositoryRoot 'target\diagnostics\host-editor-acceptance'
[void][IO.Directory]::CreateDirectory($baseDirectory)
$runId = [Guid]::NewGuid().ToString('N')
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $baseDirectory $runId }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $OutputDirectory) { throw "验收输出目录已存在：$OutputDirectory" }
[void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($OutputDirectory))

$nativeProcess = $null
$stdoutPath = Join-Path $baseDirectory "$runId-stdout.log"
$stderrPath = Join-Path $baseDirectory "$runId-stderr.log"
try {
    if (-not ('Kerminal.HostEditorAcceptance.NativeDesktopLauncher' -as [type])) {
        Add-Type -TypeDefinition (Get-HostEditorDesktopLauncherSource) -Language CSharp
    }
    $desktopName = 'KerminalRail-settings-host-{0}' -f $runId
    $nativeProcess = [Kerminal.HostEditorAcceptance.NativeDesktopLauncher]::Start(
        $executable, $repositoryRoot, $OutputDirectory, $stdoutPath, $stderrPath,
        $desktopName, [uint32]$Width, [uint32]$Height)
    if (-not $nativeProcess.WaitForExit([uint32]($TimeoutSeconds * 1000))) {
        Stop-OwnedHostEditorProcess -NativeProcess $nativeProcess
        throw "主机编辑器验收超时：$OutputDirectory"
    }
    $exitCode = $nativeProcess.GetExitCode()
    $processId = $nativeProcess.ProcessId
    $nativeProcess.Dispose()
    if (-not $nativeProcess.DesktopClosed) { throw '私有桌面没有关闭。' }
    $nativeProcess = $null

    if ($exitCode -ne 0) {
        $diagnostics = if (Test-Path -LiteralPath $stderrPath) { Get-Content -LiteralPath $stderrPath -Raw } else { '' }
        throw "主机编辑器验收退出码为 $exitCode。$([Environment]::NewLine)$diagnostics"
    }
    if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidateHash) {
        throw '验收运行期间候选 executable 发生变化。'
    }
    # 正常退出与私有桌面关闭是独立事实，持久化回执供后续矩阵引用；配置目录继续保留读回证据。
    # @author kongweiguang
    $processResult = [ordered]@{
        processId = $processId
        exitCode = $exitCode
        desktopClosed = $true
        candidateSha256 = $candidateHash
        requestedWidth = $Width
        requestedHeight = $Height
    }
    $processResult | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'process-result.json') -Encoding utf8NoBOM

    $reportPath = Join-Path $OutputDirectory 'report.json'
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { throw "验收报告缺失：$reportPath" }
    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    if ($report.schemaVersion -ne 2) { throw '验收报告不是受支持的 schema v2。' }
    if ($report.locale -cne 'zh-CN') { throw '验收进程没有固定为隔离的中文界面。' }
    if (@($report.captures).Count -ne 74 -or @($report.captures.file | Select-Object -Unique).Count -ne 74) {
        throw '验收截图数量不是 54 个分类、7 个探测阶段、6 个保存回读、2 个私钥来源、1 个长错误详情和 4 个展开场景，或存在重复文件。'
    }
    $sectionsByHost = [ordered]@{
        ssh = @('general', 'proxy', 'jump-hosts', 'terminal', 'network', 'files')
        sftp = @('general', 'proxy', 'jump-hosts', 'network', 'files')
        local = @('general', 'startup')
        rdp = @('general', 'display')
        serial = @('general', 'serial')
        telnet = @('general')
    }
    $expected = @(foreach ($theme in @('light', 'dark', 'system')) {
        foreach ($entry in $sectionsByHost.GetEnumerator()) {
            foreach ($section in $entry.Value) { "$theme/$($entry.Key)/$section" }
        }
    })
    $observed = @($report.captures | Where-Object {
        $_.hostType -ne 'ssh-probe' -and
            $_.section -notin @('saved-readback', 'private-key-file', 'private-key-paste', 'long-error-details') -and
            $_.section -notlike 'dense-*'
    } | ForEach-Object { "$($_.theme)/$($_.hostType)/$($_.section)" })
    if ($observed.Count -ne 54 -or @($observed | Select-Object -Unique).Count -ne 54 -or
        @($expected | Where-Object { $_ -notin $observed }).Count -gt 0) {
        throw '报告缺少一项或多项主题、主机类型与适用分类截图。'
    }
    $expectedStages = @('before', 'running', 'cancelled', 'retry-running', 'input-changed', 'retry', 'late-response')
    $observedStages = @($report.probeLifecycle | ForEach-Object { $_.stage })
    if ($observedStages.Count -ne $expectedStages.Count -or
        @($expectedStages | Where-Object { $_ -notin $observedStages }).Count -gt 0) {
        throw '报告缺少测试运行、取消、输入变更、重试或迟到回执状态。'
    }
    $cancelled = @($report.probeLifecycle | Where-Object stage -CEQ 'cancelled')[0]
    $changed = @($report.probeLifecycle | Where-Object stage -CEQ 'input-changed')[0]
    $late = @($report.probeLifecycle | Where-Object stage -CEQ 'late-response')[0]
    if ($cancelled.running -or -not $cancelled.cancelled -or $cancelled.errorPresent -or $null -ne $cancelled.outcome -or
        $changed.running -or $changed.errorPresent -or $null -ne $changed.outcome -or
        $late.running -or $late.errorPresent -or $late.outcome -cne 'ssh-authenticated' -or
        $report.probeRequestsStarted -ne 3 -or $report.probeRequestsCompleted -ne 3) {
        throw '探测取消、输入变更或不同类别的迟到回执没有形成有效证据。'
    }
    $geometryBefore = ConvertTo-Json -InputObject $report.probeLifecycle[0].editorGeometry -Depth 4 -Compress
    foreach ($stage in $report.probeLifecycle) {
        $geometryAtStage = ConvertTo-Json -InputObject $stage.editorGeometry -Depth 4 -Compress
        if ($geometryAtStage -cne $geometryBefore) { throw "探测阶段 $($stage.stage) 的正文或底栏几何发生变化。" }
    }
    $sources = @($report.privateKeySources | ForEach-Object { $_.source })
    if ($sources.Count -ne 2 -or @($sources | Select-Object -Unique).Count -ne 2 -or
        @('file', 'paste' | Where-Object { $_ -notin $sources }).Count -gt 0) {
        throw '私钥文件和粘贴内容没有分别形成截图证据。'
    }
    foreach ($source in $report.privateKeySources) {
        $sourceCaptures = @($report.captures | Where-Object {
            $_.hostType -ceq 'ssh' -and $_.section -ceq "private-key-$($source.source)" -and
                $_.file -ceq $source.captureFile -and $_.interactionMode -clike 'production-method input preparation*'
        })
        if ($sourceCaptures.Count -ne 1 -or [string]::IsNullOrWhiteSpace($source.selectedBy)) {
            throw "私钥来源没有关联唯一且明确标记输入准备的截图：$($source.source)"
        }
    }
    $longError = $report.longError
    if ($longError.requestsStarted -ne 1 -or -not $longError.messagePreserved -or
        $longError.messageCharacterCount -lt 100 -or
        -not $longError.detailsPopupOpen -or
        @($report.captures | Where-Object {
            $_.hostType -ceq 'ssh-error' -and $_.section -ceq 'long-error-details' -and
                $_.file -ceq $longError.captureFile -and $_.interactionMode -clike 'native-pointer*'
        }).Count -ne 1) {
        throw '长中文错误没有经真实探测回调与详情弹层形成独立证据。'
    }
    $longErrorGeometry = ConvertTo-Json -InputObject $longError.editorGeometry -Depth 4 -Compress
    if ($longErrorGeometry -cne $geometryBefore) { throw '长错误详情挤压了正文或底栏。' }
    $expectedDenseScenes = @('proxy-enabled', 'two-jumps-top', 'two-jumps-lower', 'rdp-windowed')
    $denseScenes = @($report.denseScenes | ForEach-Object { $_.scene })
    if ($denseScenes.Count -ne 4 -or @($denseScenes | Select-Object -Unique).Count -ne 4 -or
        @($expectedDenseScenes | Where-Object { $_ -notin $denseScenes }).Count -gt 0) {
        throw '代理、两跳上部、第二跳下部和 RDP 窗口模式没有分别形成展开场景证据。'
    }
    foreach ($dense in $report.denseScenes) {
        $denseCaptures = @($report.captures | Where-Object {
            $_.hostType -ceq $dense.hostType -and $_.section -ceq "dense-$($dense.scene)" -and
                $_.file -ceq $dense.captureFile -and $_.interactionMode -clike 'native-pointer controls*'
        })
        $denseBefore = ConvertTo-Json -InputObject $dense.baselineGeometry -Depth 4 -Compress
        $denseAfter = ConvertTo-Json -InputObject $dense.editorGeometry -Depth 4 -Compress
        if ($denseCaptures.Count -ne 1 -or -not $dense.footerStable -or -not $dense.preparedValuesMatch -or
            $denseBefore -cne $denseAfter -or [string]::IsNullOrWhiteSpace($dense.inputPreparation) -or
            -not [double]::IsFinite([double]$dense.scrollOffsetY)) {
            throw "展开场景未核对样本值、实际正文与底栏几何，或没有关联唯一截图：$($dense.scene)"
        }
        if ($dense.scene -ceq 'proxy-enabled' -and
            (-not $dense.proxyEnabled -or 'connections-proxy-toggle' -notin $dense.nativeInteractions)) {
            throw '代理展开没有经过真实复选框点击。'
        }
        if ($dense.scene -in @('two-jumps-top', 'two-jumps-lower')) {
            if ($dense.jumpCount -ne 2 -or -not $dense.secondJumpPrivateKey -or
                @($dense.nativeInteractions | Where-Object { $_ -ceq 'connections-add-jump' }).Count -ne 2 -or
                @($dense.nativeInteractions | Where-Object { $_ -clike 'connections-jump-*-auth-PrivateKey' }).Count -ne 1) {
                throw '两跳展开没有经过两次真实添加按钮和第二跳私钥认证点击。'
            }
            if ($dense.scene -ceq 'two-jumps-top' -and $dense.scrollOffsetY -ne 0) {
                throw '两跳上部截图没有位于真实正文顶部。'
            }
            if ($dense.scene -ceq 'two-jumps-lower' -and
                (-not $dense.secondJumpLowerFocused -or -not $dense.secondJumpLowerVisible -or $dense.scrollOffsetY -ge 0)) {
                throw '第二跳下部没有同时证明真实焦点、输入可见与正文滚动。'
            }
        }
        if ($dense.scene -ceq 'rdp-windowed' -and
            (-not $dense.rdpWindowed -or 'host-rdp-display-mode/1' -notin $dense.nativeInteractions)) {
            throw 'RDP 宽高截图没有经过真实窗口模式 Radio 点击。'
        }
    }
    $savedTypes = @($report.persistence | ForEach-Object { $_.hostType })
    if ($savedTypes.Count -ne 6 -or @($savedTypes | Select-Object -Unique).Count -ne 6 -or
        @($sectionsByHost.Keys | Where-Object { $_ -notin $savedTypes }).Count -gt 0 -or
        @($report.persistence | Where-Object { -not $_.readbackMatches -or [string]::IsNullOrWhiteSpace($_.savedId) }).Count -gt 0) {
        throw '六类主机未全部完成真实隔离保存与重开回读。'
    }
    foreach ($saved in $report.persistence) {
        if (@($report.captures | Where-Object { $_.hostType -ceq $saved.hostType -and $_.section -ceq 'saved-readback' -and $_.file -ceq $saved.captureFile }).Count -ne 1) {
            throw "保存回读没有关联唯一截图：$($saved.hostType)"
        }
    }
    $expectedConfig = [IO.Path]::GetFullPath((Join-Path $OutputDirectory 'config'))
    if ([IO.Path]::GetFullPath($report.configDirectory) -cne $expectedConfig) {
        throw '验收配置根没有绑定到本轮独立输出目录。'
    }
    if (-not $report.layoutStable -or -not $report.windowGeometryStable) { throw '报告显示窗口、正文或底栏几何发生变化。' }
    if (-not $report.nativeIsolation.isolationProven -or
        $report.nativeIsolation.processId -ne $processId -or
        -not ([string]$report.nativeIsolation.uiThreadDesktopName).StartsWith('KerminalRail-settings-host-')) {
        throw '验收进程没有证明它运行在本轮私有桌面。'
    }
    $physicalWidth = [uint32][Math]::Round($Width * [double]$report.scaleFactor)
    $physicalHeight = [uint32][Math]::Round($Height * [double]$report.scaleFactor)
    foreach ($capture in $report.captures) {
        Assert-HostEditorCapture -Capture $capture -RunDirectory $OutputDirectory `
            -ExpectedWidth $physicalWidth -ExpectedHeight $physicalHeight
    }
    Write-Output "主机编辑器验收产物：$OutputDirectory"
}
finally {
    if ($null -ne $nativeProcess) {
        try { Stop-OwnedHostEditorProcess -NativeProcess $nativeProcess }
        finally { $nativeProcess.Dispose() }
    }
}
