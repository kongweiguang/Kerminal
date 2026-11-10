# @author kongweiguang
# 仅验证进程内合成字符串与受控临时目录数据；不创建桌面、启动进程或读取用户窗口。
param([switch]$SkipSharedLauncher)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Assert-True {
    # 首个失败即终止，避免后续替身断言覆盖错误原因。
    # @author kongweiguang
    param([bool]$Condition,[string]$Message)
    if(-not $Condition){throw $Message}
}

function Assert-Throws {
    # 限定失败原因，避免语法错误或未定义变量冒充隔离断言。
    # @author kongweiguang
    param([scriptblock]$Action,[string]$Message)
    $failure=$null
    try{& $Action}catch{$failure=$_}
    Assert-True ($null -ne $failure) "Expected rejection: $Message"
    Assert-True ($failure.Exception.Message.Contains($Message)) "Wrong rejection: $failure; expected $Message"
}

function Assert-PluginRejects {
    # 严格类型反例必须失败；给出字段名时还要求拒绝原因能定位到该字段。
    # @author kongweiguang
    param([scriptblock]$Action,[string]$Field='')
    $failure=$null
    try{& $Action}catch{$failure=$_}
    Assert-True ($null -ne $failure) "Expected strict rejection: $Field"
    if($Field){Assert-True ($failure.Exception.Message.Contains($Field)) "Strict rejection did not identify ${Field}: $failure"}
}

$path=Join-Path $PSScriptRoot 'plugin-webview-native.ps1'
$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors)
Assert-True ($errors.Count -eq 0) "Invalid P3 PowerShell: $($errors -join '; ')"
foreach($name in @('Get-PluginStrictBoolean','Assert-PluginBooleanIs','Get-PluginStrictInteger','Get-PluginStrictProcessId','Get-PluginJsonObject','Get-PluginJsonObjectOrNull','Get-PluginJsonArray','Get-PluginJsonString','Get-PluginJsonNumber','Get-PluginJsonIntegerOrNull','Get-PluginJobActiveProcesses','Replace-PluginLauncherPart','Get-PluginPrivateDesktopSource','Get-PluginWebViewLauncherScript','Initialize-PluginIsolationProbe','Assert-PluginIsolationEvidence','Resolve-PluginEvidenceFile','Get-PluginHostPlatform','Get-PluginPackageFiles','Assert-PluginPackageFiles','Test-PluginWebViewResource','Assert-PluginResourceCompleteness','Assert-PluginIdentityData','Read-PluginIdentity','Stop-PluginOwnedProcess','Assert-PluginOcclusionProof','Assert-PluginWebViewBounds','Assert-PluginRendererSnapshot','Assert-PluginRootSnapshot','Assert-PluginDom','Assert-PluginWorkflowReport','Assert-PluginAcceptanceReport')){
    $matches=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name},$true))
    Assert-True ($matches.Count -eq 1) "Missing or ambiguous function: $name"
    . ([scriptblock]::Create($matches[0].Extent.Text))
}

$strictErrorField='strict.contract'
Assert-True ((Get-PluginStrictBoolean -Value $true -Field $strictErrorField) -eq $true) 'A native true boolean was rejected.'
Assert-True ((Get-PluginStrictBoolean -Value $false -Field $strictErrorField) -eq $false) 'A native false boolean was rejected.'
foreach($value in @('true','false',0,1,$null)){Assert-PluginRejects {Get-PluginStrictBoolean -Value $value -Field $strictErrorField} $strictErrorField}
Assert-PluginBooleanIs -Value $true -Field $strictErrorField -Expected $true
Assert-PluginBooleanIs -Value $false -Field $strictErrorField -Expected $false
foreach($case in @(@{Value='true';Expected=$true},@{Value='false';Expected=$false},@{Value=1;Expected=$true},@{Value=$null;Expected=$false})){
    Assert-PluginRejects {Assert-PluginBooleanIs -Value $case.Value -Field $strictErrorField -Expected $case.Expected} $strictErrorField
}
foreach($value in @([int]0,[long]0,[int]2147483647,[long]2147483648,[long][uint32]::MaxValue)){
    Assert-True ((Get-PluginStrictInteger -Value $value -Field $strictErrorField -Minimum 0 -Maximum ([long][uint32]::MaxValue)) -eq $value) 'A legitimate Int32/Int64 was rejected.'
}
foreach($value in @('0','2147483648',0.0,1.5,[double]::NaN,[double]::PositiveInfinity,$null)){
    Assert-PluginRejects {Get-PluginStrictInteger -Value $value -Field $strictErrorField -Minimum 0} $strictErrorField
}
Assert-PluginRejects {Get-PluginStrictInteger -Value -1 -Field $strictErrorField -Minimum 0} $strictErrorField
Assert-PluginRejects {Get-PluginStrictInteger -Value ([long][uint32]::MaxValue+1) -Field $strictErrorField -Maximum ([long][uint32]::MaxValue)} $strictErrorField
$strictMissing=[pscustomobject]@{}
Assert-PluginRejects {Get-PluginStrictInteger -Value $strictMissing.missing -Field 'missing'} 'missing'

$fixture=@'
function Get-WebViewLauncherSource { $PSScriptRoot; '--webview-probe --output ' }
function Replace-WebViewLauncherPart { param($source,$old,$new) }
function Stop-OwnedProbe { param($process) }
$jobSource = 'job contract'
if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher' -as [type])) {
    $source = Get-WebViewLauncherSource
    $source = Replace-WebViewLauncherPart $source 'if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))' 'if (!String.Equals(desktopName, "Default", StringComparison.OrdinalIgnoreCase))'
    $source = Replace-WebViewLauncherPart $source 'EntryPoint = "CreateDesktopW"' 'EntryPoint = "OpenDesktopW"'
    $source = Replace-WebViewLauncherPart $source 'private static extern IntPtr CreateDesktop(string desktopName, IntPtr device, IntPtr devmode, uint flags, uint desiredAccess, IntPtr securityAttributes);' 'private static extern IntPtr OpenDesktop(string desktopName, uint flags, bool inherit, uint desiredAccess);'
    $source = Replace-WebViewLauncherPart $source 'CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero)' 'OpenDesktop(desktopName, 0, false, desktopAccess)'
    $source = Replace-WebViewLauncherPart $source 'job-old' 'job-new'
    Add-Type -TypeDefinition ($source + "`n" + $jobSource) -Language CSharp
}
'@
$extracted=Get-PluginWebViewLauncherScript -LauncherSource $fixture -LauncherDirectory 'C:\test'
Assert-True (-not $extracted.Contains('OpenDesktop')) 'Default/OpenDesktop transformation was retained.'
Assert-True ($extracted.Contains("'job-old' 'job-new'")) 'An unrelated Job injection was removed.'
Assert-True ($extracted.Contains('Get-PluginPrivateDesktopSource -Source $source -JobSource $jobSource')) 'The final C# guard was not inserted.'
Assert-True ($extracted.Contains('--plugin-webview-acceptance --dev-plugin ')) 'The P3 candidate command was not selected.'
Assert-True ($extracted.Contains('Kerminal.PluginWebViewAcceptance.NativeDesktopLauncher')) 'The P3 launcher namespace was not isolated.'
Assert-Throws { Get-PluginWebViewLauncherScript -LauncherSource ($fixture.Replace('EntryPoint = "OpenDesktopW"','EntryPoint = "DifferentDesktopW"')) -LauncherDirectory 'C:\test' } 'Default desktop transformations changed'
$duplicate=$fixture.Replace("    `$source = Replace-WebViewLauncherPart `$source 'job-old' 'job-new'", "    `$source = Replace-WebViewLauncherPart `$source 'EntryPoint = `"CreateDesktopW`"' 'EntryPoint = `"OpenDesktopW`"'`n    `$source = Replace-WebViewLauncherPart `$source 'job-old' 'job-new'")
Assert-Throws { Get-PluginWebViewLauncherScript -LauncherSource $duplicate -LauncherDirectory 'C:\test' } 'Default desktop transformations changed'
Assert-Throws { Get-PluginWebViewLauncherScript -LauncherSource ($fixture.Replace("'job-old' 'job-new'", "'job-old' 'SwitchDesktop'")) -LauncherDirectory 'C:\test' } 'Unexpected desktop transformation'
Assert-Throws { Get-PluginWebViewLauncherScript -LauncherSource ($fixture.Replace('Add-Type -TypeDefinition','Changed-Type -TypeDefinition')) -LauncherDirectory 'C:\test' } 'Plugin launcher replacement changed'

$fakeCsharp=@'
EntryPoint = "CreateDesktopW"
if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))
startupInfo.StartupInfo.Desktop = desktopNamePointer;
Marshal.StringToHGlobalUni("WinSta0\\" + desktopName)
ProbeJob GetJobSnapshot
// Closes only the private desktop handle owned by this acceptance run.
IntPtr desktop = CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero);
'@
$job='ProbeJob JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE'
$protected=Get-PluginPrivateDesktopSource -Source $fakeCsharp -JobSource $job
Assert-True ($protected.Contains('EnsureNewPluginDesktop(desktopName); IntPtr desktop = CreateDesktop')) 'The collision check is not before CreateDesktop.'
Assert-True ($protected.Contains('if (error != 2)')) 'An indeterminate collision check could be accepted.'
Assert-Throws { Get-PluginPrivateDesktopSource -Source ($fakeCsharp.Replace('EntryPoint = "CreateDesktopW"','EntryPoint = "OpenDesktopW"')) -JobSource $job } 'Private desktop or suspended Job guard missing'
Assert-Throws { Get-PluginPrivateDesktopSource -Source ($fakeCsharp + ' SwitchDesktop') -JobSource $job } 'Unsafe launcher operation'
Assert-Throws { Get-PluginPrivateDesktopSource -Source ($fakeCsharp + $fakeCsharp) -JobSource $job } 'Plugin launcher replacement changed'
Assert-Throws { Get-PluginPrivateDesktopSource -Source $fakeCsharp -JobSource 'ProbeJob' } 'Job ownership contract changed'

$desktop='KerminalRail-settings-plugin-webview-' + ('a' * 32)
$candidate='f' * 64
$inputName='Default'
$evidence=[pscustomobject]@{stage='search-before';processId=1234;candidateSha256=$candidate;nativeIsolation=[pscustomobject]@{platformSupported=$true;isolationProven=$true;processId=1234;uiThreadId=99;uiThreadDesktopName=$desktop;inputDesktopName=$inputName}}
$native=[pscustomobject]@{ProcessId=1234;ThreadId=99;ThreadDesktopName=$desktop;InputDesktopName=$inputName;WindowStationName='WinSta0'}
Assert-PluginIsolationEvidence -Evidence $evidence -Native $native -Stage 'search-before' -ProcessId 1234 -Desktop $desktop -InputDesktop $inputName -Candidate $candidate -BaselineThreadId 99
foreach($case in @('stage','candidate','pid','string-pid','float-pid','null-pid','thread','string-thread','float-thread','null-thread','desktop','input','false-proof','string-proof','number-proof','null-proof','missing-proof','string-platform','missing-platform','string-evidence-pid','native-pid','native-thread','native-desktop','native-input','window-station','baseline-thread','input-equals-desktop')){
    $bad=$evidence | ConvertTo-Json -Depth 5 | ConvertFrom-Json
    $other=$native | ConvertTo-Json -Depth 5 | ConvertFrom-Json
    $expectedInput=$inputName; $baseline=[uint32]99
    switch($case){
        'stage' {$bad.stage='dialog-before'}
        'candidate' {$bad.candidateSha256='e' * 64}
        'pid' {$bad.nativeIsolation.processId=1235}
        'string-pid' {$bad.nativeIsolation.processId='1234'}
        'float-pid' {$bad.nativeIsolation.processId=1234.0}
        'null-pid' {$bad.nativeIsolation.processId=$null}
        'thread' {$bad.nativeIsolation.uiThreadId=0}
        'string-thread' {$bad.nativeIsolation.uiThreadId='99'}
        'float-thread' {$bad.nativeIsolation.uiThreadId=99.0}
        'null-thread' {$bad.nativeIsolation.uiThreadId=$null}
        'desktop' {$bad.nativeIsolation.uiThreadDesktopName='Default'}
        'input' {$bad.nativeIsolation.inputDesktopName=''}
        'false-proof' {$bad.nativeIsolation.isolationProven=$false}
        'string-proof' {$bad.nativeIsolation.isolationProven='true'}
        'number-proof' {$bad.nativeIsolation.isolationProven=1}
        'null-proof' {$bad.nativeIsolation.isolationProven=$null}
        'missing-proof' {$bad.nativeIsolation.PSObject.Properties.Remove('isolationProven')}
        'string-platform' {$bad.nativeIsolation.platformSupported='true'}
        'missing-platform' {$bad.nativeIsolation.PSObject.Properties.Remove('platformSupported')}
        'string-evidence-pid' {$bad.processId='1234'}
        'native-pid' {$other.ProcessId=1235}
        'native-thread' {$other.ThreadId=100}
        'native-desktop' {$other.ThreadDesktopName='Default'}
        'native-input' {$other.InputDesktopName='Other'}
        'window-station' {$other.WindowStationName='Service-0x0-3e7$'}
        'baseline-thread' {$baseline=100}
        'input-equals-desktop' {$expectedInput=$desktop; $bad.nativeIsolation.inputDesktopName=$desktop; $other.InputDesktopName=$desktop}
    }
    $failure=$null
    try{Assert-PluginIsolationEvidence -Evidence $bad -Native $other -Stage 'search-before' -ProcessId 1234 -Desktop $desktop -InputDesktop $expectedInput -Candidate $candidate -BaselineThreadId $baseline}catch{$failure=$_}
    Assert-True ($null -ne $failure) "Isolation accepted invalid synthetic case: $case"
}
Initialize-PluginIsolationProbe
$bmp=[byte[]]::new(66)
$bmp[0]=0x42; $bmp[1]=0x4d
[BitConverter]::GetBytes([int]54).CopyTo($bmp,10)
[BitConverter]::GetBytes([int]40).CopyTo($bmp,14)
[BitConverter]::GetBytes([int]3).CopyTo($bmp,18)
[BitConverter]::GetBytes([int]-1).CopyTo($bmp,22)
[BitConverter]::GetBytes([int16]1).CopyTo($bmp,26)
[BitConverter]::GetBytes([int16]32).CopyTo($bmp,28)
$bmp[54]=86; $bmp[55]=52; $bmp[56]=18
$bmp[58]=120; $bmp[59]=207; $bmp[60]=37
$pixels=[Kerminal.PluginWebViewAcceptance.Isolation]::InspectPage($bmp)
Assert-True ($pixels.Width -eq 3 -and $pixels.Height -eq 1 -and $pixels.Body -eq 1 -and $pixels.Proof -eq 1) 'The synthetic same-BMP page markers were miscounted.'
$black=[byte[]]$bmp.Clone()
[Array]::Clear($black,54,12)
$pixels=[Kerminal.PluginWebViewAcceptance.Isolation]::InspectPage($black)
Assert-True ($pixels.Body -eq 0 -and $pixels.Proof -eq 0) 'A black frame was accepted as page markers.'
$truncated=[byte[]]::new(65)
[Array]::Copy($bmp,$truncated,65)
Assert-Throws { [Kerminal.PluginWebViewAcceptance.Isolation]::InspectPage($truncated) } 'Incomplete BMP pixels'
$invalid=[byte[]]$bmp.Clone(); $invalid[28]=24
Assert-Throws { [Kerminal.PluginWebViewAcceptance.Isolation]::InspectPage($invalid) } 'Invalid 32-bit BMP'
Assert-Throws { Initialize-PluginIsolationProbe } 'fresh PowerShell process'
$tries=@($ast.EndBlock.Statements | Where-Object {$_ -is [Management.Automation.Language.TryStatementAst] -and $null -ne $_.Finally})
Assert-True ($tries.Count -eq 1) 'The acceptance cleanup block is missing or ambiguous.'
$finallyText=$tries[0].Finally.Extent.Text
$cleanup=[scriptblock]::Create($finallyText.Substring(1,$finallyText.Length-2))
$process=[pscustomobject]@{}
$process | Add-Member ScriptMethod GetJobSnapshot { return [pscustomobject]@{ActiveProcesses=[uint32]0} }
$process | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$process | Add-Member ScriptMethod Dispose { throw 'synthetic disposal failure' }
$names=@(); $previous=@{}
try {throw 'synthetic acceptance failure'} catch {$failure=$_}
$combined=$null
try {& $cleanup} catch {$combined=$_}
Assert-True ($combined.Exception -is [AggregateException] -and $combined.Exception.InnerExceptions.Count -eq 2) 'Cleanup failure masked the original acceptance failure.'
$process=[pscustomobject]@{TerminateAttempted=$false;DisposeAttempted=$false}
$process | Add-Member ScriptMethod GetJobSnapshot { throw 'synthetic Job query failure' }
$process | Add-Member ScriptMethod TerminateJob { param($code) $this.TerminateAttempted=$true; throw 'synthetic Job termination failure' }
$process | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$process | Add-Member ScriptMethod Dispose { $this.DisposeAttempted=$true; throw 'synthetic Job disposal failure' }
$cleanupError=$null
try {Stop-PluginOwnedProcess -Process $process} catch {$cleanupError=$_}
Assert-True ($process.TerminateAttempted -and $process.DisposeAttempted) 'An uncertain Job did not attempt both termination and disposal.'
Assert-True ($cleanupError.Exception -is [AggregateException]) 'Multiple cleanup failures were not combined.'
$cleanupMessages=($cleanupError.Exception.Flatten().InnerExceptions | ForEach-Object { $_.Message }) -join ' | '
Assert-True ($cleanupMessages.Contains('synthetic Job query failure') -and $cleanupMessages.Contains('synthetic Job termination failure') -and $cleanupMessages.Contains('synthetic Job disposal failure')) 'A query, termination or disposal failure was lost.'
$stringJob=[pscustomobject]@{TerminateAttempted=$false;DisposeAttempted=$false}
$stringJob | Add-Member ScriptMethod GetJobSnapshot { return [pscustomobject]@{ActiveProcesses='0'} }
$stringJob | Add-Member ScriptMethod TerminateJob { param($code) $this.TerminateAttempted=$true }
$stringJob | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$stringJob | Add-Member ScriptMethod Dispose { $this.DisposeAttempted=$true }
$stringJobError=$null
try {Stop-PluginOwnedProcess -Process $stringJob} catch {$stringJobError=$_}
Assert-True ($null -ne $stringJobError -and $stringJobError.Exception.Message.Contains('Invalid Job active process count')) 'A numeric-string Job count was not rejected as a C# type mismatch.'
Assert-True ($stringJob.TerminateAttempted -and $stringJob.DisposeAttempted) 'An invalid Job count skipped conservative cleanup.'
try {throw 'synthetic UI failure'} catch {$failure=$_}
$combined=$null
try {& $cleanup} catch {$combined=$_}
Assert-True ($combined.Exception -is [AggregateException] -and $combined.Exception.Flatten().InnerExceptions.Count -ge 4) 'UI and simultaneous Job cleanup errors were not all preserved.'
foreach($badPath in @('../outside','C:\\outside','folder/../outside','folder//file','/absolute')){
    Assert-Throws { Resolve-PluginEvidenceFile -Root 'C:\unused' -Relative $badPath } 'Invalid plugin evidence path'
}

$hostPlatform=Get-PluginHostPlatform
Assert-True ($hostPlatform -in @('windows-x86_64','windows-aarch64','linux-x86_64','linux-aarch64','macos-x86_64','macos-aarch64')) 'The host platform was not mapped to the protocol platform.'
$tempRoot=Join-Path ([IO.Path]::GetTempPath()) ('kerminal-p3-'+[guid]::NewGuid().ToString('N'))
try {
    [void][IO.Directory]::CreateDirectory((Join-Path $tempRoot 'web'))
    [IO.File]::WriteAllText((Join-Path $tempRoot 'kerminal-plugin.json'),'{}')
    [IO.File]::WriteAllText((Join-Path $tempRoot 'web/index.html'),'html')
    $tempFiles=@(Get-PluginPackageFiles -Root $tempRoot)
    Assert-True ($tempFiles.Count -eq 2 -and @($tempFiles | Where-Object {$_.path -ceq 'kerminal-plugin.json'}).Count -eq 1 -and @($tempFiles | Where-Object {$_.path -ceq 'web/index.html'}).Count -eq 1) 'The independent package enumeration missed a synthetic file.'
} finally {
    if(Test-Path -LiteralPath $tempRoot){Remove-Item -LiteralPath $tempRoot -Recurse -Force}
}

$manifestBytes=[Text.Encoding]::UTF8.GetBytes('{"id":"kerminal.file-search","version":"1.2.3","runtime":{"kind":"sidecar","main":{"windows-x86_64":"bin/custom-sidecar.exe"}},"views":{"search.page":{"kind":"webview","entry":"web/index.html"}},"contributes":{}}')
$manifest=[Text.Encoding]::UTF8.GetString($manifestBytes) | ConvertFrom-Json
$manifestHash=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($manifestBytes)).ToLowerInvariant()
$resourceBytes=[ordered]@{'kerminal-plugin.json'=$manifestBytes;'web/app.js'=[Text.Encoding]::UTF8.GetBytes('js');'web/index.html'=[Text.Encoding]::UTF8.GetBytes('html')}
$packageFiles=@(
    [pscustomobject]@{path='README.md';size=12},
    [pscustomobject]@{path='bin/custom-sidecar.exe';size=34},
    [pscustomobject]@{path='kerminal-plugin.json';size=$manifestBytes.Length},
    [pscustomobject]@{path='src/main.rs';size=56},
    [pscustomobject]@{path='web/app.js';size=2},
    [pscustomobject]@{path='web/index.html';size=4}
)
$observations=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
$resources=[Collections.Generic.List[object]]::new()
$stream=[IO.MemoryStream]::new(); $writer=[IO.BinaryWriter]::new($stream)
try {
    $writer.Write([Text.Encoding]::UTF8.GetBytes("kerminal-webview-plugin-v1`0"))
    $writer.Write([Convert]::FromHexString($manifestHash))
    foreach($entry in $resourceBytes.GetEnumerator()){
        $bytes=[byte[]]$entry.Value
        $fileHash=[Security.Cryptography.SHA256]::HashData($bytes)
        $fileHex=[Convert]::ToHexString($fileHash).ToLowerInvariant()
        $observations.Add($entry.Key,[pscustomobject]@{size=$bytes.Length;sha256=$fileHex})
        $resources.Add([pscustomobject]@{path=$entry.Key;size=$bytes.Length;sha256=$fileHex})
        $name=[Text.Encoding]::UTF8.GetBytes($entry.Key)
        $writer.Write([uint64]$name.Length); $writer.Write($name); $writer.Write([uint64]$bytes.Length); $writer.Write($fileHash)
    }
    $writer.Flush()
    $pluginHash=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream.ToArray())).ToLowerInvariant()
} finally {$writer.Dispose(); $stream.Dispose()}
$sidecarHash='33'*32
$identity=[pscustomobject]@{schemaVersion=1;algorithm='kerminal-webview-plugin-v1';processId=1234;theme='light';candidateSha256=$candidate;pluginId=$manifest.id;pluginVersion=$manifest.version;manifestSha256=$manifestHash;manifestSource='prepared.resources';manifestSemanticsVerified=$true;pluginSha256=$pluginHash;resourceSetComplete=$true;resourceSetDerivation='package-files+manifest';sidecarPlatform=$hostPlatform;sidecarMain='bin/custom-sidecar.exe';sidecarSha256=$sidecarHash;sidecarEvidence='package-file-only';sidecarProcessBound=$false;packageFiles=$packageFiles;resources=$resources.ToArray()}
Assert-PluginPackageFiles -Reported $packageFiles -Actual $packageFiles
Assert-PluginResourceCompleteness -Manifest $manifest -PackageFiles $packageFiles -Resources $resources.ToArray()
Assert-PluginIdentityData -Identity $identity -Manifest $manifest -ManifestHash $manifestHash -Observations $observations -PackageFiles $packageFiles -HostPlatform $hostPlatform -SidecarHash $sidecarHash -ProcessId 1234 -Theme 'light' -Candidate $candidate -Baseline $null
$next=$identity | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$next.theme='dark'; $next.processId=1235
Assert-PluginIdentityData -Identity $next -Manifest $manifest -ManifestHash $manifestHash -Observations $observations -PackageFiles $packageFiles -HostPlatform $hostPlatform -SidecarHash $sidecarHash -ProcessId 1235 -Theme 'dark' -Candidate $candidate -Baseline $identity
$reportBaseline=$identity
$reportIsolation=[pscustomobject]@{platformSupported=$true;isolationProven=$true;processId=1234;uiThreadId=99;uiThreadDesktopName=$desktop;inputDesktopName=$inputName}
$workflowFixture=Join-Path ([IO.Path]::GetTempPath()) ('kerminal-p3-workflow-'+[guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($workflowFixture)
[IO.File]::WriteAllText((Join-Path $workflowFixture 'needle-001.txt'),'one')
[IO.File]::WriteAllText((Join-Path $workflowFixture 'needle-002.txt'),'two')
$searchPaths=@([IO.Directory]::GetFiles($workflowFixture,'needle-*.txt') | ForEach-Object {[IO.Path]::GetFullPath($_)})
function New-PluginAcceptanceRenderer {
    # 真实快照字段逐项构造；可见性/代次按工作流阶段变化，父窗由外部观察值锚定。
    param([string]$Instance,[long]$Epoch,[string]$NativeId,[bool]$Visible,[bool]$BackdropPending,[long]$Parent)
    $bounds=[pscustomobject]@{x=0;y=0;width=1024;height=720}
    [pscustomobject]@{instance=$Instance;pageEpoch=$Epoch;processGeneration=1;pageReady=$true;opened=$true;failure=$null;error=$null;dom=$null;native=[pscustomobject]@{instance_id="native-$Instance";native_id=$NativeId;native_parent_id=$Parent;visible=$Visible;engine_visible=$true;loaded=$true;bounds=$bounds;clip=$bounds;scale_factor=1.25;message_count=9;parent_position_notifications=2;failure=$null;backdrop_visible=$false;backdrop_pending=$BackdropPending;backdrop_failure=$null;backdrop_captures=0;backdrop_capture_ms=@();backdrop_slow_fallback=$false}}
}
function New-PluginAcceptanceTab {
    # 标签身份与内容贡献必须与真实 File Search 扩展一致，避免合成样本换用其它内容类型。
    param([string]$TabId,[object]$Renderer)
    [pscustomobject]@{tabId=$TabId;contentId='kerminal.file-search/search-tab';savedVersion=1;savedState=[pscustomobject]@{};phase='Open';liveVersion=1;liveState=[pscustomobject]@{};viewId="view-$TabId";renderer=$Renderer}
}
function New-PluginAcceptanceRoot {
    # 空工作区必须显式给出 null 活动标签和 false centralContent，不能由字符串参数把 null 变成空串。
    param([object[]]$Tabs,$ActiveTab,[bool]$PreferencesOpen,[long]$InstanceCount)
    $items=@($Tabs)
    [pscustomobject]@{tabs=$items;activeTabId=$ActiveTab;instanceCount=$InstanceCount;centralContent=($items.Count -gt 0);status=$null;workspaceReady=$true;connectionsModalOpen=$false;pendingHostOpen=$false;selectedPluginHostId=$null;navigationGeneration=3;preferencesOpen=$PreferencesOpen;settingsReady=$true;registeredContent=$true;windowActive=$true}
}
function New-PluginAcceptanceDom {
    # DOM 回执形状来自真实 report() 脚本；rows 使用磁盘绝对路径，便于与夹具文件集合独立对照。
    param([string]$Root,[string]$Query,[object[]]$Rows,[object[]]$Trace,[bool]$Subscribed,[object[]]$Events,$Status)
    [pscustomobject]@{token='synthetic:1';root=$Root;query=$Query;status=$Status;busy=$false;rows=@($Rows);trace=@($Trace);subscribed=$Subscribed;subscribeError=$null;events=@($Events)}
}
try {
    $firstTab='tab-first-0001'
    $secondTab='tab-second-0002'
    $parent=[long]4242
    $firstInstance='instance-first-1'
    $firstNative='native-page-1'
    $reloadedNative='native-page-2'
    $readyTab=New-PluginAcceptanceTab -TabId $firstTab -Renderer (New-PluginAcceptanceRenderer -Instance $firstInstance -Epoch 2 -NativeId $firstNative -Visible $true -BackdropPending $false -Parent $parent)
    $ready=New-PluginAcceptanceRoot -Tabs @($readyTab) -ActiveTab $firstTab -PreferencesOpen $false -InstanceCount 1
    $searched=New-PluginAcceptanceDom -Root $workflowFixture -Query 'needle' -Rows $searchPaths -Trace @([pscustomobject]@{rows=1;busy=$true},[pscustomobject]@{rows=2;busy=$false}) -Subscribed $false -Events @() -Status '搜索完成'
    $cancelled=New-PluginAcceptanceDom -Root $workflowFixture -Query 'no-such-needle-for-cancel' -Rows @() -Trace @() -Subscribed $false -Events @() -Status '搜索已取消'
    $pageEvents=New-PluginAcceptanceDom -Root $workflowFixture -Query 'no-such-needle-for-cancel' -Rows @() -Trace @() -Subscribed $true -Events @([pscustomobject]@{name='tab.changed';payload=[pscustomobject]@{tab=$secondTab}}) -Status '搜索已取消'
    $draft=New-PluginAcceptanceDom -Root $workflowFixture -Query 'no-such-needle-for-cancel' -Rows @() -Trace @() -Subscribed $true -Events @([pscustomobject]@{name='tab.changed';payload=[pscustomobject]@{tab=$secondTab}}) -Status '搜索已取消'
    $notificationHiddenTab=New-PluginAcceptanceTab -TabId $firstTab -Renderer (New-PluginAcceptanceRenderer -Instance $firstInstance -Epoch 2 -NativeId $firstNative -Visible $false -BackdropPending $false -Parent $parent)
    $notificationHidden=New-PluginAcceptanceRoot -Tabs @($notificationHiddenTab) -ActiveTab $firstTab -PreferencesOpen $false -InstanceCount 1
    $notificationVisibleTab=New-PluginAcceptanceTab -TabId $firstTab -Renderer (New-PluginAcceptanceRenderer -Instance $firstInstance -Epoch 2 -NativeId $firstNative -Visible $true -BackdropPending $false -Parent $parent)
    $notificationVisible=New-PluginAcceptanceRoot -Tabs @($notificationVisibleTab) -ActiveTab $firstTab -PreferencesOpen $false -InstanceCount 1
    $occludedTab=New-PluginAcceptanceTab -TabId $firstTab -Renderer (New-PluginAcceptanceRenderer -Instance $firstInstance -Epoch 2 -NativeId $firstNative -Visible $false -BackdropPending $false -Parent $parent)
    $occluded=New-PluginAcceptanceRoot -Tabs @($occludedTab) -ActiveTab $firstTab -PreferencesOpen $true -InstanceCount 1
    $restoredTab=New-PluginAcceptanceTab -TabId $firstTab -Renderer (New-PluginAcceptanceRenderer -Instance $firstInstance -Epoch 2 -NativeId $firstNative -Visible $true -BackdropPending $false -Parent $parent)
    $restored=New-PluginAcceptanceRoot -Tabs @($restoredTab) -ActiveTab $firstTab -PreferencesOpen $false -InstanceCount 1
    $reloadedTab=New-PluginAcceptanceTab -TabId $firstTab -Renderer (New-PluginAcceptanceRenderer -Instance $firstInstance -Epoch 3 -NativeId $reloadedNative -Visible $true -BackdropPending $false -Parent $parent)
    $reloaded=New-PluginAcceptanceRoot -Tabs @($reloadedTab) -ActiveTab $firstTab -PreferencesOpen $false -InstanceCount 1
    $closed=New-PluginAcceptanceRoot -Tabs @() -ActiveTab $null -PreferencesOpen $false -InstanceCount 0
    $acceptanceReport=[pscustomobject]@{schemaVersion=1;resourcesShutdownComplete=$true;runtimeShutdownObserved=$false;quitReceipt=$true;processId=1234;candidateSha256=$candidate;theme='light';mode='private-desktop-composition';shutdownScope='composition-resources-only';sidecarRuntimeBinding='not-proven';pluginSha256=$reportBaseline.pluginSha256;sidecarSha256=$reportBaseline.sidecarSha256;sidecarMain=$reportBaseline.sidecarMain;sidecarPlatform=$hostPlatform;nativeIsolation=$reportIsolation;viewport=@(1024.0,720.0);scaleFactor=1.25;ready=$ready;searched=$searched;cancelled=$cancelled;pageEvents=$pageEvents;notificationOccluded=$notificationHidden;notificationRestored=$notificationVisible;occluded=$occluded;restored=$restored;draftAfterOcclusion=$draft;reloaded=$reloaded;closed=$closed;nativeImeTested=$false;dpiSwitchTested=$false;sleepResumeTested=$false;inputMode='DOM events through production page listeners and SDK bridge'}
    $acceptanceArgs=@{ProcessId=1234;Candidate=$candidate;Theme='light';Desktop=$desktop;InputDesktop=$inputName;ThreadId=99;PluginBaseline=$reportBaseline;HostPlatform=$hostPlatform;ExpectedParent=[uint64]$parent;FixtureRoot=$workflowFixture;ExpectedSearchPaths=$searchPaths;ViewportWidth=1024.0;ViewportHeight=720.0}
    Assert-PluginAcceptanceReport -Report $acceptanceReport @acceptanceArgs
    $jsonReport=$acceptanceReport | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    Assert-PluginAcceptanceReport -Report $jsonReport @acceptanceArgs
foreach($case in @(
    @{Name='shutdown-false-string';Field='resourcesShutdownComplete';Mutate={param($r) $r.resourcesShutdownComplete='false'}},
    @{Name='shutdown-true-string';Field='resourcesShutdownComplete';Mutate={param($r) $r.resourcesShutdownComplete='true'}},
    @{Name='shutdown-number';Field='resourcesShutdownComplete';Mutate={param($r) $r.resourcesShutdownComplete=1}},
    @{Name='shutdown-null';Field='resourcesShutdownComplete';Mutate={param($r) $r.resourcesShutdownComplete=$null}},
    @{Name='shutdown-missing';Field='resourcesShutdownComplete';Mutate={param($r) $r.PSObject.Properties.Remove('resourcesShutdownComplete')}},
    @{Name='runtime-false-string';Field='runtimeShutdownObserved';Mutate={param($r) $r.runtimeShutdownObserved='false'}},
    @{Name='quit-true-string';Field='quitReceipt';Mutate={param($r) $r.quitReceipt='true'}},
    @{Name='platform-true-string';Field='platformSupported';Mutate={param($r) $r.nativeIsolation.platformSupported='true'}},
    @{Name='isolation-true-string';Field='isolationProven';Mutate={param($r) $r.nativeIsolation.isolationProven='true'}},
    @{Name='pid-string';Field='processId';Mutate={param($r) $r.processId='1234'}},
    @{Name='pid-42-string';Field='processId';Mutate={param($r) $r.processId='42'}},
    @{Name='pid-float';Field='processId';Mutate={param($r) $r.processId=1234.0}},
    @{Name='pid-null';Field='processId';Mutate={param($r) $r.processId=$null}},
    @{Name='pid-out-of-range';Field='processId';Mutate={param($r) $r.processId=4294967296}},
    @{Name='pid-missing';Field='processId';Mutate={param($r) $r.PSObject.Properties.Remove('processId')}},
    @{Name='thread-string';Field='uiThreadId';Mutate={param($r) $r.nativeIsolation.uiThreadId='99'}},
    @{Name='isolation-missing';Field='';Mutate={param($r) $r.PSObject.Properties.Remove('nativeIsolation')}}
)){
    $badReport=$jsonReport | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $case.Mutate.Invoke($badReport)
    Assert-PluginRejects {Assert-PluginAcceptanceReport -Report $badReport @acceptanceArgs} $case.Field
}
Assert-PluginRejects {Assert-PluginAcceptanceReport -Report $acceptanceReport -ProcessId 1234 -Candidate $candidate -Theme 'light' -Desktop $desktop -InputDesktop $inputName -ThreadId 99 -PluginBaseline $null -HostPlatform $hostPlatform -ExpectedParent ([uint64]$parent) -FixtureRoot $workflowFixture -ExpectedSearchPaths $searchPaths -ViewportWidth 1024.0 -ViewportHeight 720.0}

foreach($case in @(
    @{Name='schema-missing';Field='schemaVersion';Mutate={param($r) $r.PSObject.Properties.Remove('schemaVersion')}},
    @{Name='schema-string';Field='schemaVersion';Mutate={param($r) $r.schemaVersion='1'}},
    @{Name='schema-future';Field='schemaVersion';Mutate={param($r) $r.schemaVersion=2}},
    @{Name='ready-missing';Field='ready';Mutate={param($r) $r.PSObject.Properties.Remove('ready')}},
    @{Name='ready-type';Field='ready';Mutate={param($r) $r.ready='ready'}},
    @{Name='ready-active-missing';Field='activeTabId';Mutate={param($r) $r.ready.PSObject.Properties.Remove('activeTabId')}},
    @{Name='ready-tabs-missing';Field='tabs';Mutate={param($r) $r.ready.PSObject.Properties.Remove('tabs')}},
    @{Name='ready-tabs-type';Field='tabs';Mutate={param($r) $r.ready.tabs='tabs'}},
    @{Name='ready-tabs-empty';Field='tabs';Mutate={param($r) $r.ready.tabs=@()}},
    @{Name='ready-prefs-open';Field='preferencesOpen';Mutate={param($r) $r.ready.preferencesOpen=$true}},
    @{Name='ready-central-false';Field='centralContent';Mutate={param($r) $r.ready.centralContent=$false}},
    @{Name='ready-content-id';Field='contentId';Mutate={param($r) $r.ready.tabs[0].contentId='other/content'}},
    @{Name='ready-page-not-ready';Field='pageReady';Mutate={param($r) $r.ready.tabs[0].renderer.pageReady=$false}},
    @{Name='ready-renderer-missing';Field='renderer';Mutate={param($r) $r.ready.tabs[0].PSObject.Properties.Remove('renderer')}},
    @{Name='ready-native-missing';Field='native';Mutate={param($r) $r.ready.tabs[0].renderer.PSObject.Properties.Remove('native')}},
    @{Name='ready-visible-false';Field='native.visible';Mutate={param($r) $r.ready.tabs[0].renderer.native.visible=$false}},
    @{Name='ready-parent-mismatch';Field='native_parent_id';Mutate={param($r) $r.ready.tabs[0].renderer.native.native_parent_id=4243}},
    @{Name='ready-bounds-missing';Field='bounds.width';Mutate={param($r) $r.ready.tabs[0].renderer.native.bounds.PSObject.Properties.Remove('width')}},
    @{Name='searched-missing';Field='searched';Mutate={param($r) $r.PSObject.Properties.Remove('searched')}},
    @{Name='searched-rows-type';Field='rows';Mutate={param($r) $r.searched.rows='rows'}},
    @{Name='searched-rows-missing';Field='rows';Mutate={param($r) $r.searched.PSObject.Properties.Remove('rows')}},
    @{Name='searched-rows-extra';Field='rows';Mutate={param($r) $r.searched.rows=@($searchPaths)+[IO.Path]::GetFullPath((Join-Path $workflowFixture 'needle-extra.txt'))}},
    @{Name='searched-query';Field='query';Mutate={param($r) $r.searched.query='other'}},
    @{Name='searched-busy';Field='busy';Mutate={param($r) $r.searched.busy=$true}},
    @{Name='searched-trace-type';Field='trace';Mutate={param($r) $r.searched.trace='trace'}},
    @{Name='searched-trace-missing';Field='trace';Mutate={param($r) $r.searched.PSObject.Properties.Remove('trace')}},
    @{Name='searched-trace-no-stream';Field='trace';Mutate={param($r) $r.searched.trace=@([pscustomobject]@{rows=2;busy=$false})}},
    @{Name='searched-root';Field='root';Mutate={param($r) $r.searched.root=[IO.Path]::GetFullPath((Join-Path $workflowFixture 'other'))}},
    @{Name='cancelled-missing';Field='cancelled';Mutate={param($r) $r.PSObject.Properties.Remove('cancelled')}},
    @{Name='cancelled-status-null';Field='status';Mutate={param($r) $r.cancelled.status=$null}},
    @{Name='cancelled-status-missing';Field='status';Mutate={param($r) $r.cancelled.PSObject.Properties.Remove('status')}},
    @{Name='cancelled-query';Field='query';Mutate={param($r) $r.cancelled.query='needle'}},
    @{Name='cancelled-busy';Field='busy';Mutate={param($r) $r.cancelled.busy=$true}},
    @{Name='events-missing';Field='pageEvents';Mutate={param($r) $r.PSObject.Properties.Remove('pageEvents')}},
    @{Name='events-not-subscribed';Field='subscribed';Mutate={param($r) $r.pageEvents.subscribed=$false}},
    @{Name='events-empty';Field='events';Mutate={param($r) $r.pageEvents.events=@()}},
    @{Name='events-events-missing';Field='events';Mutate={param($r) $r.pageEvents.PSObject.Properties.Remove('events')}},
    @{Name='events-payload-missing';Field='payload';Mutate={param($r) $r.pageEvents.events[0].PSObject.Properties.Remove('payload')}},
    @{Name='events-payload-tab-missing';Field='tab';Mutate={param($r) $r.pageEvents.events[0].payload.PSObject.Properties.Remove('tab')}},
    @{Name='events-payload-tab-null';Field='payload.tab';Mutate={param($r) $r.pageEvents.events[0].payload.tab=$null}},
    @{Name='events-type';Field='events';Mutate={param($r) $r.pageEvents.events='events'}},
    @{Name='events-same-tab';Field='pageEvents';Mutate={param($r) $r.pageEvents.events[0].payload.tab=$firstTab}},
    @{Name='notification-missing';Field='notificationOccluded';Mutate={param($r) $r.PSObject.Properties.Remove('notificationOccluded')}},
    @{Name='notification-prefs';Field='preferencesOpen';Mutate={param($r) $r.notificationOccluded.preferencesOpen=$true}},
    @{Name='notification-visible';Field='native.visible';Mutate={param($r) $r.notificationOccluded.tabs[0].renderer.native.visible=$true}},
    @{Name='notification-tabs-extra';Field='tabs';Mutate={param($r) $extra=$r.ready.tabs[0] | ConvertTo-Json -Depth 20 | ConvertFrom-Json; $extra.tabId=$secondTab; $extra.viewId='view-second'; $extra.renderer.instance='instance-second'; $extra.renderer.native.instance_id='native-second'; $extra.renderer.native.native_id='native-second-page'; $r.notificationOccluded.tabs=@($r.notificationOccluded.tabs[0],$extra)}},
    @{Name='notification-restored-missing';Field='notificationRestored';Mutate={param($r) $r.PSObject.Properties.Remove('notificationRestored')}},
    @{Name='notification-restored-hidden';Field='native.visible';Mutate={param($r) $r.notificationRestored.tabs[0].renderer.native.visible=$false}},
    @{Name='occluded-prefs-false';Field='preferencesOpen';Mutate={param($r) $r.occluded.preferencesOpen=$false}},
    @{Name='occluded-visible';Field='native.visible';Mutate={param($r) $r.occluded.tabs[0].renderer.native.visible=$true}},
    @{Name='restored-prefs-true';Field='preferencesOpen';Mutate={param($r) $r.restored.preferencesOpen=$true}},
    @{Name='draft-missing';Field='draftAfterOcclusion';Mutate={param($r) $r.PSObject.Properties.Remove('draftAfterOcclusion')}},
    @{Name='draft-query';Field='query';Mutate={param($r) $r.draftAfterOcclusion.query='needle'}},
    @{Name='reloaded-missing';Field='reloaded';Mutate={param($r) $r.PSObject.Properties.Remove('reloaded')}},
    @{Name='reloaded-epoch';Field='page epoch';Mutate={param($r) $r.reloaded.tabs[0].renderer.pageEpoch=$r.restored.tabs[0].renderer.pageEpoch}},
    @{Name='reloaded-instance';Field='instance';Mutate={param($r) $r.reloaded.tabs[0].renderer.instance='instance-other'}},
    @{Name='closed-missing';Field='closed';Mutate={param($r) $r.PSObject.Properties.Remove('closed')}},
    @{Name='closed-tabs';Field='tabs';Mutate={param($r) $r.closed.tabs=$r.ready.tabs}},
    @{Name='closed-active';Field='activeTabId';Mutate={param($r) $r.closed.activeTabId=$firstTab}},
    @{Name='closed-central';Field='centralContent';Mutate={param($r) $r.closed.centralContent=$true}},
    @{Name='viewport-type';Field='viewport';Mutate={param($r) $r.viewport='viewport'}},
    @{Name='viewport-size';Field='viewport';Mutate={param($r) $r.viewport=@(800.0,600.0)}},
    @{Name='scale-type';Field='scaleFactor';Mutate={param($r) $r.scaleFactor='1.25'}},
    @{Name='ime-claimed';Field='nativeImeTested';Mutate={param($r) $r.nativeImeTested=$true}},
    @{Name='input-mode-missing';Field='inputMode';Mutate={param($r) $r.PSObject.Properties.Remove('inputMode')}},
    @{Name='native-identity-changed';Field='native identity';Mutate={param($r) $r.notificationRestored.tabs[0].renderer.native.native_id='native-other'}},
    @{Name='epoch-changed';Field='page epoch';Mutate={param($r) $r.notificationOccluded.tabs[0].renderer.pageEpoch=3}}
)){
    $badReport=$jsonReport | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $case.Mutate.Invoke($badReport)
    Assert-PluginRejects {Assert-PluginAcceptanceReport -Report $badReport @acceptanceArgs} $case.Field
}
} finally {
    if(Test-Path -LiteralPath $workflowFixture){Remove-Item -LiteralPath $workflowFixture -Recurse -Force}
}


foreach($case in @('candidate-as-plugin','plugin-hash','manifest-hash','manifest-source','manifest-semantics','resource-set-incomplete','sidecar-hash','sidecar-binding','sidecar-evidence','platform','runtime-main','resource-size','resource-hash','duplicate-resource','empty-resources','baseline-plugin','baseline-sidecar','schema-version-string','schema-version-float','schema-version-null','schema-version-missing','process-id-string','manifest-semantics-string','resource-set-string','sidecar-binding-string','resource-size-string','resource-size-float','resource-size-null','baseline-manifest-semantics-string')){
    $bad=$identity | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    $otherHash=$sidecarHash; $base=$null; $otherManifest=$manifestHash; $otherPackage=$packageFiles
    switch($case){
        'candidate-as-plugin' {$bad.pluginSha256=$candidate}
        'plugin-hash' {$bad.pluginSha256='44'*32}
        'manifest-hash' {$otherManifest='44'*32}
        'manifest-source' {$bad.manifestSource='disk-read'}
        'manifest-semantics' {$bad.manifestSemanticsVerified=$false}
        'resource-set-incomplete' {$bad.resourceSetComplete=$false}
        'sidecar-hash' {$otherHash='44'*32}
        'sidecar-binding' {$bad.sidecarProcessBound=$true}
        'sidecar-evidence' {$bad.sidecarEvidence='running-process'}
        'platform' {$bad.sidecarPlatform='linux-x86_64'}
        'runtime-main' {$bad.sidecarMain='bin/not-the-runtime.exe'}
        'resource-size' {$bad.resources[0].size++}
        'resource-hash' {$bad.resources[0].sha256='44'*32}
        'duplicate-resource' {$bad.resources=@($bad.resources[0],$bad.resources[0])}
        'empty-resources' {$bad.resources=@()}
        'baseline-plugin' {$base=$identity | ConvertTo-Json -Depth 8 | ConvertFrom-Json; $base.pluginSha256='44'*32}
        'baseline-sidecar' {$base=$identity | ConvertTo-Json -Depth 8 | ConvertFrom-Json; $base.sidecarSha256='44'*32}
        'schema-version-string' {$bad.schemaVersion='1'}
        'schema-version-float' {$bad.schemaVersion=1.0}
        'schema-version-null' {$bad.schemaVersion=$null}
        'schema-version-missing' {$bad.PSObject.Properties.Remove('schemaVersion')}
        'process-id-string' {$bad.processId='1234'}
        'manifest-semantics-string' {$bad.manifestSemanticsVerified='true'}
        'resource-set-string' {$bad.resourceSetComplete='true'}
        'sidecar-binding-string' {$bad.sidecarProcessBound='false'}
        'resource-size-string' {$bad.resources[0].size='2'}
        'resource-size-float' {$bad.resources[0].size=2.0}
        'resource-size-null' {$bad.resources[0].size=$null}
        'baseline-manifest-semantics-string' {$base=$identity | ConvertTo-Json -Depth 8 | ConvertFrom-Json; $base.manifestSemanticsVerified='true'}
    }
    $errorRecord=$null
    try {Assert-PluginIdentityData -Identity $bad -Manifest $manifest -ManifestHash $otherManifest -Observations $observations -PackageFiles $otherPackage -HostPlatform $hostPlatform -SidecarHash $otherHash -ProcessId 1234 -Theme 'light' -Candidate $candidate -Baseline $base} catch {$errorRecord=$_}
    Assert-True ($null -ne $errorRecord) "Invalid plugin identity accepted: $case"
}
Assert-Throws { Assert-PluginPackageFiles -Reported $packageFiles -Actual (@($packageFiles)+[pscustomobject]@{path='web/extra.css';size=1}) } 'Frozen package inventory is absent or incomplete.'
Assert-Throws { Assert-PluginPackageFiles -Reported $packageFiles -Actual @($packageFiles | Where-Object {$_.path -cne 'src/main.rs'}) } 'Frozen package inventory is absent or incomplete.'
$sizeChanged=ConvertTo-Json -InputObject $packageFiles -Depth 4 | ConvertFrom-Json
$sizeChanged[1].size++
Assert-Throws { Assert-PluginPackageFiles -Reported $packageFiles -Actual $sizeChanged } 'Actual plugin package differs from its frozen inventory.'
$sizeString=ConvertTo-Json -InputObject $packageFiles -Depth 4 | ConvertFrom-Json
$sizeString[1].size='34'
Assert-PluginRejects { Assert-PluginPackageFiles -Reported $sizeString -Actual $packageFiles } 'packageFiles.size'
$sizeFloat=ConvertTo-Json -InputObject $packageFiles -Depth 4 | ConvertFrom-Json
$sizeFloat[1].size=34.0
Assert-PluginRejects { Assert-PluginPackageFiles -Reported $sizeFloat -Actual $packageFiles } 'packageFiles.size'
$sizeNull=ConvertTo-Json -InputObject $packageFiles -Depth 4 | ConvertFrom-Json
$sizeNull[1].size=$null
Assert-PluginRejects { Assert-PluginPackageFiles -Reported $sizeNull -Actual $packageFiles } 'packageFiles.size'
Assert-Throws { Assert-PluginIdentityData -Identity $identity -Manifest $manifest -ManifestHash $manifestHash -Observations $observations -PackageFiles @($packageFiles | Where-Object {$_.path -cne 'web/app.js'}) -HostPlatform $hostPlatform -SidecarHash $sidecarHash -ProcessId 1234 -Theme 'light' -Candidate $candidate -Baseline $null } 'Invalid frozen plugin resource evidence.'
$missingResource=@($resources.ToArray() | Where-Object {$_.path -cne 'web/app.js'})
Assert-Throws { Assert-PluginResourceCompleteness -Manifest $manifest -PackageFiles $packageFiles -Resources $missingResource } 'Frozen plugin resource set does not match the actual package declarations.'
$extraResource=@($resources.ToArray())+[pscustomobject]@{path='README.md';size=12;sha256='44'*32}
Assert-Throws { Assert-PluginResourceCompleteness -Manifest $manifest -PackageFiles $packageFiles -Resources $extraResource } 'Frozen plugin resource set does not match the actual package declarations.'
$extraPackage=@($packageFiles)+[pscustomobject]@{path='web/extra.css';size=1}
Assert-Throws { Assert-PluginResourceCompleteness -Manifest $manifest -PackageFiles $extraPackage -Resources $resources.ToArray() } 'Frozen plugin resource set does not match the actual package declarations.'
$nativeManifest=$manifest | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$nativeManifest.views=[pscustomobject]@{'native.page'=[pscustomobject]@{kind='native'}}
Assert-Throws { Assert-PluginResourceCompleteness -Manifest $nativeManifest -PackageFiles $packageFiles -Resources $resources.ToArray() } 'Native view resource completeness is not independently proven.'

$readPlugin=Join-Path ([IO.Path]::GetTempPath()) ('kerminal-p3-plugin-'+[guid]::NewGuid().ToString('N'))
$readOutput=Join-Path ([IO.Path]::GetTempPath()) ('kerminal-p3-output-'+[guid]::NewGuid().ToString('N'))
try {
    [void][IO.Directory]::CreateDirectory((Join-Path $readPlugin 'web'))
    [void][IO.Directory]::CreateDirectory((Join-Path $readPlugin 'bin'))
    [void][IO.Directory]::CreateDirectory((Join-Path $readPlugin 'src'))
    [void][IO.Directory]::CreateDirectory($readOutput)
    [IO.File]::WriteAllBytes((Join-Path $readPlugin 'kerminal-plugin.json'),$manifestBytes)
    [IO.File]::WriteAllText((Join-Path $readPlugin 'web/index.html'),'html')
    [IO.File]::WriteAllText((Join-Path $readPlugin 'web/app.js'),'js')
    [IO.File]::WriteAllText((Join-Path $readPlugin 'README.md'),'README-TEST!')
    [IO.File]::WriteAllText((Join-Path $readPlugin 'src/main.rs'),('x'*56))
    [IO.File]::WriteAllBytes((Join-Path $readPlugin 'bin/custom-sidecar.exe'),[byte[]]::new(34))
    $readIdentity=$identity | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    $readIdentity.sidecarSha256=(Get-FileHash -LiteralPath (Join-Path $readPlugin 'bin/custom-sidecar.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $readOutput 'plugin-identity.json'),($readIdentity | ConvertTo-Json -Depth 8),[Text.UTF8Encoding]::new($false))
    $loaded=Read-PluginIdentity -Output $readOutput -PluginRoot $readPlugin -ProcessId 1234 -Theme 'light' -Candidate $candidate -Baseline $null
    Assert-True ($loaded.pluginSha256 -ceq $identity.pluginSha256 -and $loaded.packageFiles.Count -eq $packageFiles.Count) 'Read-PluginIdentity did not load the synthetic frozen identity.'
    [IO.File]::WriteAllText((Join-Path $readPlugin 'web/app.js'),'jz')
    Assert-Throws { Read-PluginIdentity -Output $readOutput -PluginRoot $readPlugin -ProcessId 1234 -Theme 'light' -Candidate $candidate -Baseline $null } 'Frozen plugin resource differs from its package file.'
} finally {
    if(Test-Path -LiteralPath $readPlugin){Remove-Item -LiteralPath $readPlugin -Recurse -Force}
    if(Test-Path -LiteralPath $readOutput){Remove-Item -LiteralPath $readOutput -Recurse -Force}
}
Assert-Throws { Assert-PluginOcclusionProof -Phase 'notification' } 'not proven in the same system frame'
Assert-Throws { Assert-PluginOcclusionProof -Phase 'dialog' } 'not proven in the same system frame'
$frame=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq 'Assert-PluginWebViewFrame'},$true))[0].Extent.Text
Assert-True ($frame.Contains("if (`$Phase -in @('notification','dialog')) { Assert-PluginOcclusionProof -Phase `$Phase }")) 'The WGC runner no longer rejects unproven occlusion phases.'
$scriptText=[IO.File]::ReadAllText($path)
Assert-True ($scriptText.Contains("'KerminalRail-settings-plugin-webview-'+[guid]::NewGuid().ToString('N')") -and $scriptText.Contains('$viewportWidth=1024') -and $scriptText.Contains('$viewportHeight=720') -and $scriptText.Contains('$desktop,$viewportWidth,$viewportHeight)')) 'The launcher no longer uses a new per-run private desktop and matching logical viewport.'
Assert-True ($scriptText.Contains("SetEnvironmentVariable('KERMINAL_WEBVIEW_BACKGROUND','0','Process')")) 'The P0 input-desktop background mode is no longer explicitly disabled.'
Assert-True ($scriptText.Contains('Assert-PluginPackageFiles') -and $scriptText.Contains('Assert-PluginResourceCompleteness') -and $scriptText.Contains('Get-PluginHostPlatform') -and $scriptText.Contains('resourcesShutdownComplete') -and $scriptText.Contains('runtimeShutdownObserved')) 'The runner no longer cross-checks package completeness, host platform or shutdown scope.'
Assert-True ($scriptText.Contains('Assert-PluginAcceptanceReport -Report $report -ProcessId $process.ProcessId') -and -not $scriptText.Contains('if(-not $report.resourcesShutdownComplete')) 'The final acceptance report bypasses strict JSON type checks.'
Assert-True ($scriptText.Contains('KERMINAL_WEBVIEW_OFFSCREEN_ONLY') -and $scriptText.Contains('Missing GPUI offscreen frame') -and $scriptText.Contains('renderer.native.native_parent_id')) 'The runner no longer validates GPUI offscreen frames or the active native parent.'
$rust=[IO.File]::ReadAllText((Join-Path $PSScriptRoot '../../crates/kerminal-desktop/src/plugin_webview_acceptance.rs'))
Assert-True ($rust.Contains('"occlusionEvidence"') -and $rust.Contains('"production-root-state-and-gpui-offscreen"') -and $rust.Contains('"systemComposition": "not-observed"') -and $rust.Contains('FrameProof::OccludedState') -and $rust.Contains('window.render_to_image()')) 'The candidate no longer proves occlusion by production Root state and GPUI offscreen frames.'
Assert-True ($rust.Contains('isolation_checkpoint(cx, options, &format!("{phase}-before"))') -and $rust.Contains('isolation_checkpoint(cx, options, &format!("{phase}-after"))')) 'Capture is no longer bracketed by independent isolation checkpoints.'
Assert-True ($rust.Contains('require_quit_receipt') -and $rust.Contains('"manifestSource": "prepared.resources"') -and $rust.Contains('"resourceSetComplete": true') -and $rust.Contains('"sidecarProcessBound": false') -and $rust.Contains('"resourcesShutdownComplete"') -and $rust.Contains('"runtimeShutdownObserved"')) 'The candidate no longer fails on missing quit receipts or preserves honest identity/shutdown scope.'
Assert-True ($rust.Contains('"第二网页实例关闭"') -and $rust.Contains('v["activeTabId"].is_null()') -and $rust.Contains('v["centralContent"] == false')) 'The candidate no longer stabilizes the close and empty-workspace states before writing the report.'
Assert-True (-not $rust.Contains('"shutdownComplete"')) 'The candidate still claims unobserved runtime shutdown as complete.'
if(-not $SkipSharedLauncher){
$sharedPath=Join-Path $PSScriptRoot 'webview-native.ps1'
$shared=[IO.File]::ReadAllText($sharedPath)
$final=Get-PluginWebViewLauncherScript -LauncherSource $shared -LauncherDirectory $PSScriptRoot
Assert-True (-not $final.Contains('OpenDesktop') -and -not $final.Contains('SwitchDesktop') -and -not $final.Contains('SendInput')) 'The final extracted P0 code retained an unsafe desktop transformation.'
Assert-True ($final.Contains('ProcThreadAttributeJobList') -and $final.Contains('0x0002000D') -and $final.Contains('JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE')) 'The original atomic Job-list assembly was not preserved.'
$parsedTokens=$null; $parsedErrors=$null
[void][Management.Automation.Language.Parser]::ParseInput($final,[ref]$parsedTokens,[ref]$parsedErrors)
Assert-True ($parsedErrors.Count -eq 0) "Extracted P3 launcher is invalid: $($parsedErrors -join '; ')"
. ([scriptblock]::Create($final))
$launcher='Kerminal.PluginWebViewAcceptance.NativeDesktopLauncher' -as [type]
$owned='Kerminal.PluginWebViewAcceptance.NativeDesktopProcess' -as [type]
Assert-True ($null -ne $launcher -and $null -ne $launcher.GetMethod('Start')) 'The final extracted launcher did not compile.'
Assert-True ($null -ne $owned -and $null -ne $owned.GetMethod('GetJobSnapshot') -and $null -ne $owned.GetMethod('TerminateJob') -and $null -ne $owned.GetMethod('Dispose')) 'The compiled launcher is missing owned-Job lifecycle methods.'
}
'P3 private desktop, occlusion and identity synthetic tests passed.'
