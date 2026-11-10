# @author kongweiguang
# 只解析脚本、编译内存探针并使用合成回执；不创建桌面、不启动进程、不读取用户窗口。
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Assert-True {
    # 首个失败即终止，避免后续替身断言覆盖真实原因。
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

function Assert-ExtensionsRejects {
    # 严格类型反例必须失败；给出字段名时还要求拒绝原因能定位到该字段。
    # @author kongweiguang
    param([scriptblock]$Action,[string]$Field='')
    $failure=$null
    try{& $Action}catch{$failure=$_}
    Assert-True ($null -ne $failure) "Expected strict rejection: $Field"
    if($Field){Assert-True ($failure.Exception.Message.Contains($Field)) "Strict rejection did not identify ${Field}: $failure"}
}

function New-SyntheticExtensionsReport {
    # 构造与候选报告同形的内存回执，便于逐一破坏每个独立证据门禁。
    # @author kongweiguang
    param([string]$Desktop,[string]$Candidate,[string]$PackageHash)
    return ([ordered]@{
        schemaVersion=1
        mode='private-desktop-composition'
        processId=1234
        theme='light'
        desktopName=$Desktop
        shutdownScope='composition-resources-only'
        isolationScope='candidate-win32-thread-snapshot'
        quitReceipt=$true
        resourcesShutdownComplete=$true
        runtimeShutdownObserved=$false
        candidateSha256=$Candidate
        localPackageSha256=$PackageHash
        nativeIsolation=[pscustomobject]@{platformSupported=$true;isolationProven=$true;processId=1234;uiThreadId=99;uiThreadDesktopName=$Desktop;inputDesktopName='Default';foregroundWindowPid=4321}
        viewport=@(1200.0,800.0)
        scaleFactor=1.25
        frameEvidence=[pscustomobject]@{source='gpui-render-to-image';validation='png-structure-crc-and-dimensions';featurePixelsChecked=$false;systemComposition='not-observed'}
        market=[pscustomobject]@{page=[pscustomobject]@{catalog_entries=2;busy=$false;offline=$false;error=$null}}
        selection=[pscustomobject]@{page=[pscustomobject]@{selected='acme.market-two'}}
        catalogConfirmation=[pscustomobject]@{page=[pscustomobject]@{confirmation_kind='install';confirmation_id='acme.market-two'}}
        catalogInstalled=[pscustomobject]@{page=[pscustomobject]@{pending=1;busy=$false;confirmation_visible=$false;pending_restart=$true}}
        confirmation=[pscustomobject]@{page=[pscustomobject]@{confirmation_visible=$true;busy=$false;confirmation_kind='file';confirmation_id='acme.file-search';confirmation_permissions=@('full-access')}}
        installed=[pscustomobject]@{page=[pscustomobject]@{pending=2;busy=$false;confirmation_visible=$false;pending_restart=$true;error=$null}}
        pendingCountReadBack=2
        pendingPluginIds=@('acme.market-two','acme.file-search')
    } | ConvertTo-Json -Depth 10 | ConvertFrom-Json)
}

$path=Join-Path $PSScriptRoot 'plugin-extensions-native.ps1'
$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors)
Assert-True ($errors.Count -eq 0) "Invalid P5 PowerShell: $($errors -join '; ')"
foreach($name in @('Get-ExtensionsStrictBoolean','Assert-ExtensionsBoolean','Get-ExtensionsStrictInteger','Get-ExtensionsStrictNumber','Get-ExtensionsStrictString','Assert-ExtensionsString','Assert-ExtensionsDigest','Get-ExtensionsStringSet','Get-ExtensionsSnapshotPage','Get-ExtensionsJobActiveProcesses','Get-ExtensionsFrameDimensions','Assert-ExtensionsFrameDimensions','Initialize-ExtensionsNativeEvidence','Read-ExtensionsPngEvidence','Assert-ExtensionsAcceptanceReport','Stop-ExtensionsOwnedProcess','Stop-ExtensionsFixture','Initialize-ExtensionsLauncher')){
    $matches=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name},$true))
    Assert-True ($matches.Count -eq 1) "Missing or ambiguous function: $name"
    $definition=$matches[0].Extent.Text
    if($name -ceq 'Initialize-ExtensionsLauncher'){
        # 动态脚本块不会继承脚本级 $PSScriptRoot；把变量节点绑定到本次真实目录，避免误读当前目录。
        $definitionTokens=$null;$definitionErrors=$null
        $definitionAst=[Management.Automation.Language.Parser]::ParseInput($definition,[ref]$definitionTokens,[ref]$definitionErrors)
        Assert-True ($definitionErrors.Count -eq 0) 'The extracted launcher initializer is invalid.'
        $quoted="'"+$PSScriptRoot.Replace("'","''")+"'"
        $rootVariables=@($definitionAst.FindAll({param($node) $node -is [Management.Automation.Language.VariableExpressionAst] -and $node.VariablePath.UserPath -ceq 'PSScriptRoot'},$true) | Sort-Object {$_.Extent.StartOffset} -Descending)
        foreach($variable in $rootVariables){$definition=$definition.Remove($variable.Extent.StartOffset,$variable.Extent.Text.Length).Insert($variable.Extent.StartOffset,('('+$quoted+')'))}
    }
    . ([scriptblock]::Create($definition))
}

$field='strict.contract'
Assert-True ((Get-ExtensionsStrictBoolean -Value $true -Field $field) -eq $true) 'A native true boolean was rejected.'
Assert-True ((Get-ExtensionsStrictBoolean -Value $false -Field $field) -eq $false) 'A native false boolean was rejected.'
foreach($value in @('true','false',0,1,$null)){Assert-ExtensionsRejects {Get-ExtensionsStrictBoolean -Value $value -Field $field} $field}
Assert-ExtensionsBoolean -Value $true -Field $field -Expected $true
Assert-ExtensionsBoolean -Value $false -Field $field -Expected $false
foreach($case in @(@{Value='true';Expected=$true},@{Value='false';Expected=$false},@{Value=1;Expected=$true},@{Value=$null;Expected=$false})){
    Assert-ExtensionsRejects {Assert-ExtensionsBoolean -Value $case.Value -Field $field -Expected $case.Expected} $field
}
foreach($value in @([int]0,[long]0,[int]2147483647,[long]2147483648,[long][uint32]::MaxValue)){
    Assert-True ((Get-ExtensionsStrictInteger -Value $value -Field $field -Minimum 0 -Maximum ([long][uint32]::MaxValue)) -eq $value) 'A legitimate Int32/Int64 was rejected.'
}
foreach($value in @('0','2147483648',0.0,1.5,[double]::NaN,[double]::PositiveInfinity,$null)){
    Assert-ExtensionsRejects {Get-ExtensionsStrictInteger -Value $value -Field $field -Minimum 0} $field
}
Assert-ExtensionsRejects {Get-ExtensionsStrictInteger -Value -1 -Field $field -Minimum 0} $field
Assert-ExtensionsRejects {Get-ExtensionsStrictInteger -Value ([long][uint32]::MaxValue+1) -Field $field -Maximum ([long][uint32]::MaxValue)} $field
foreach($value in @([int]1200,[long]1200,[double]1200.0,[decimal]1.5)){
    Assert-True ((Get-ExtensionsStrictNumber -Value $value -Field $field) -gt 0) 'A legitimate JSON number was rejected.'
}
foreach($value in @('1200',$true,$false,$null,[double]::NaN,[double]::PositiveInfinity)){Assert-ExtensionsRejects {Get-ExtensionsStrictNumber -Value $value -Field $field} $field}
Assert-True ((Get-ExtensionsStrictString -Value 'value' -Field $field) -ceq 'value') 'A legitimate string was rejected.'
foreach($value in @('',[string]::Empty,$null,1,$true)){Assert-ExtensionsRejects {Get-ExtensionsStrictString -Value $value -Field $field} $field}
[void](Assert-ExtensionsString -Value 'expected' -Field $field -Expected 'expected')
Assert-ExtensionsRejects {Assert-ExtensionsString -Value 'other' -Field $field -Expected 'expected'} $field
$digest='a'*64
Assert-True ((Assert-ExtensionsDigest -Value $digest -Field $field) -ceq $digest) 'A lowercase SHA-256 digest was rejected.'
foreach($value in @(('A'*64),('a'*63),'not-a-digest',1,$null)){Assert-ExtensionsRejects {Assert-ExtensionsDigest -Value $value -Field $field} $field}
$set=Get-ExtensionsStringSet -Value @('one','two') -Field $field
Assert-True ($set.Count -eq 2) 'A unique string array was rejected.'
foreach($value in @('one',@('one','one'),@('one',1),@('', 'two'),$null)){Assert-ExtensionsRejects {Get-ExtensionsStringSet -Value $value -Field $field} $field}
$page=Get-ExtensionsSnapshotPage -Snapshot ([pscustomobject]@{page=[pscustomobject]@{value=1}}) -Field 'snapshot'
Assert-True ($page.value -eq 1) 'A valid page snapshot was rejected.'
Assert-ExtensionsRejects {Get-ExtensionsSnapshotPage -Snapshot ([pscustomobject]@{}) -Field 'snapshot'} 'Missing page snapshot'
Assert-ExtensionsRejects {Get-ExtensionsSnapshotPage -Snapshot ([pscustomobject]@{page='invalid'}) -Field 'snapshot'} 'Missing page snapshot'

$desktop='KerminalRail-settings-'+('a'*32)
$candidate='c'*64
$packageHash='d'*64
$report=New-SyntheticExtensionsReport -Desktop $desktop -Candidate $candidate -PackageHash $packageHash
$dimensions=Assert-ExtensionsAcceptanceReport -Report $report -ProcessId 1234 -Candidate $candidate -PackageHash $packageHash -Theme 'light' -Desktop $desktop -InputDesktop 'Default'
Assert-True ($dimensions.Width -eq 1500 -and $dimensions.Height -eq 1000 -and $dimensions.Scale -eq 1.25) 'The synthetic viewport did not derive the expected pixel dimensions.'
$jsonReport=$report | ConvertTo-Json -Depth 10 | ConvertFrom-Json
[void](Assert-ExtensionsAcceptanceReport -Report $jsonReport -ProcessId 1234 -Candidate $candidate -PackageHash $packageHash -Theme 'light' -Desktop $desktop -InputDesktop 'Default')

foreach($case in @(
    @{Name='mode';Field='mode';Mutate={param($r) $r.mode='hidden-window'}},
    @{Name='process-string';Field='processId';Mutate={param($r) $r.processId='1234'}},
    @{Name='process-other';Field='processId';Mutate={param($r) $r.processId=1235}},
    @{Name='theme';Field='theme';Mutate={param($r) $r.theme='dark'}},
    @{Name='desktop';Field='desktopName';Mutate={param($r) $r.desktopName='KerminalRail-settings-'+('b'*32)}},
    @{Name='shutdown-scope';Field='shutdownScope';Mutate={param($r) $r.shutdownScope='entire-runtime'}},
    @{Name='isolation-scope';Field='isolationScope';Mutate={param($r) $r.isolationScope='independent-resample'}},
    @{Name='quit-false';Field='quitReceipt';Mutate={param($r) $r.quitReceipt=$false}},
    @{Name='quit-string';Field='quitReceipt';Mutate={param($r) $r.quitReceipt='true'}},
    @{Name='resources-false';Field='resourcesShutdownComplete';Mutate={param($r) $r.resourcesShutdownComplete=$false}},
    @{Name='resources-string';Field='resourcesShutdownComplete';Mutate={param($r) $r.resourcesShutdownComplete='true'}},
    @{Name='runtime-true';Field='runtimeShutdownObserved';Mutate={param($r) $r.runtimeShutdownObserved=$true}},
    @{Name='runtime-string';Field='runtimeShutdownObserved';Mutate={param($r) $r.runtimeShutdownObserved='false'}},
    @{Name='candidate';Field='candidateSha256';Mutate={param($r) $r.candidateSha256='e'*64}},
    @{Name='candidate-uppercase';Field='candidateSha256';Mutate={param($r) $r.candidateSha256='C'*64}},
    @{Name='package';Field='localPackageSha256';Mutate={param($r) $r.localPackageSha256='e'*64}},
    @{Name='platform-string';Field='platformSupported';Mutate={param($r) $r.nativeIsolation.platformSupported='true'}},
    @{Name='platform-false';Field='platformSupported';Mutate={param($r) $r.nativeIsolation.platformSupported=$false}},
    @{Name='isolation-string';Field='isolationProven';Mutate={param($r) $r.nativeIsolation.isolationProven='true'}},
    @{Name='isolation-process-string';Field='isolation.processId';Mutate={param($r) $r.nativeIsolation.processId='1234'}},
    @{Name='isolation-process-other';Field='isolation.processId';Mutate={param($r) $r.nativeIsolation.processId=1235}},
    @{Name='ui-thread-zero';Field='uiThreadId';Mutate={param($r) $r.nativeIsolation.uiThreadId=0}},
    @{Name='ui-thread-string';Field='uiThreadId';Mutate={param($r) $r.nativeIsolation.uiThreadId='99'}},
    @{Name='ui-desktop';Field='uiThreadDesktopName';Mutate={param($r) $r.nativeIsolation.uiThreadDesktopName='Default'}},
    @{Name='input-desktop';Field='inputDesktopName';Mutate={param($r) $r.nativeIsolation.inputDesktopName='Other'}},
    @{Name='input-equals-desktop';Field='inputDesktopName';Mutate={param($r) $r.nativeIsolation.inputDesktopName=$desktop}},
    @{Name='foreground-candidate';Field='foregroundWindowPid';Mutate={param($r) $r.nativeIsolation.foregroundWindowPid=1234}},
    @{Name='viewport-width';Field='viewport.width';Mutate={param($r) $r.viewport=@(1198.0,800.0)}},
    @{Name='viewport-string';Field='viewport.width';Mutate={param($r) $r.viewport=@('1200',800.0)}},
    @{Name='viewport-extra';Field='viewport';Mutate={param($r) $r.viewport=@(1200.0,800.0,1.0)}},
    @{Name='scale-string';Field='scaleFactor';Mutate={param($r) $r.scaleFactor='1.25'}},
    @{Name='scale-zero';Field='scaleFactor';Mutate={param($r) $r.scaleFactor=0}},
    @{Name='frame-source';Field='frame.source';Mutate={param($r) $r.frameEvidence.source='wgc'}},
    @{Name='frame-validation';Field='frame.validation';Mutate={param($r) $r.frameEvidence.validation='header-only'}},
    @{Name='frame-pixels-claim';Field='featurePixelsChecked';Mutate={param($r) $r.frameEvidence.featurePixelsChecked=$true}},
    @{Name='frame-pixels-string';Field='featurePixelsChecked';Mutate={param($r) $r.frameEvidence.featurePixelsChecked='false'}},
    @{Name='frame-composition';Field='systemComposition';Mutate={param($r) $r.frameEvidence.systemComposition='observed'}},
    @{Name='market-missing-page';Field='page';Mutate={param($r) $r.market=[pscustomobject]@{}}},
    @{Name='market-count';Field='catalog_entries';Mutate={param($r) $r.market.page.catalog_entries=3}},
    @{Name='market-count-string';Field='catalog_entries';Mutate={param($r) $r.market.page.catalog_entries='2'}},
    @{Name='market-offline';Field='offline';Mutate={param($r) $r.market.page.offline=$true}},
    @{Name='market-error';Field='error';Mutate={param($r) $r.market.page.error='failed'}},
    @{Name='selection';Field='selected';Mutate={param($r) $r.selection.page.selected='acme.market-one'}},
    @{Name='catalog-kind';Field='confirmation_kind';Mutate={param($r) $r.catalogConfirmation.page.confirmation_kind='file'}},
    @{Name='catalog-id';Field='confirmation_id';Mutate={param($r) $r.catalogConfirmation.page.confirmation_id='other'}},
    @{Name='catalog-pending';Field='catalogInstalled.pending';Mutate={param($r) $r.catalogInstalled.page.pending=2}},
    @{Name='catalog-busy';Field='catalogInstalled.busy';Mutate={param($r) $r.catalogInstalled.page.busy=$true}},
    @{Name='catalog-restart';Field='catalogInstalled.pending_restart';Mutate={param($r) $r.catalogInstalled.page.pending_restart=$false}},
    @{Name='file-visible';Field='confirmation_visible';Mutate={param($r) $r.confirmation.page.confirmation_visible=$false}},
    @{Name='file-kind';Field='confirmation_kind';Mutate={param($r) $r.confirmation.page.confirmation_kind='install'}},
    @{Name='file-id';Field='confirmation_id';Mutate={param($r) $r.confirmation.page.confirmation_id='other'}},
    @{Name='file-permission';Field='confirmation_permissions';Mutate={param($r) $r.confirmation.page.confirmation_permissions=@('read-only')}},
    @{Name='file-permission-string';Field='confirmation_permissions';Mutate={param($r) $r.confirmation.page.confirmation_permissions='full-access'}},
    @{Name='installed-pending';Field='installed.pending';Mutate={param($r) $r.installed.page.pending=1}},
    @{Name='installed-busy';Field='installed.busy';Mutate={param($r) $r.installed.page.busy=$true}},
    @{Name='installed-restart';Field='installed.pending_restart';Mutate={param($r) $r.installed.page.pending_restart=$false}},
    @{Name='installed-error';Field='error';Mutate={param($r) $r.installed.page.error='failed'}},
    @{Name='pending-count';Field='pendingCountReadBack';Mutate={param($r) $r.pendingCountReadBack=3}},
    @{Name='pending-count-string';Field='pendingCountReadBack';Mutate={param($r) $r.pendingCountReadBack='2'}},
    @{Name='pending-missing';Field='pendingPluginIds';Mutate={param($r) $r.pendingPluginIds=@('acme.market-two')}},
    @{Name='pending-extra';Field='pendingPluginIds';Mutate={param($r) $r.pendingPluginIds=@('acme.market-two','acme.file-search','acme.extra')}},
    @{Name='pending-duplicate';Field='pendingPluginIds';Mutate={param($r) $r.pendingPluginIds=@('acme.market-two','acme.market-two')}},
    @{Name='pending-string';Field='pendingPluginIds';Mutate={param($r) $r.pendingPluginIds='acme.market-two'}}
)){
    $bad=$jsonReport | ConvertTo-Json -Depth 10 | ConvertFrom-Json
    $case.Mutate.Invoke($bad)
    $failure=$null
    try{Assert-ExtensionsAcceptanceReport -Report $bad -ProcessId 1234 -Candidate $candidate -PackageHash $packageHash -Theme 'light' -Desktop $desktop -InputDesktop 'Default'}catch{$failure=$_}
    Assert-True ($null -ne $failure) "Acceptance report accepted invalid synthetic case: $($case.Name)"
}

Initialize-ExtensionsNativeEvidence
$pngBytes=[Convert]::FromBase64String('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==')
$png=[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng($pngBytes)
Assert-True ($png.Width -eq 1 -and $png.Height -eq 1 -and $png.IdatBytes -eq 13 -and $png.Chunks -eq 3 -and $png.Sha256 -cmatch '^[0-9a-f]{64}$') 'The synthetic PNG chunk evidence was misparsed.'
$mutated=[byte[]]$pngBytes.Clone(); $mutated[0]=0
Assert-Throws {[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng($mutated)} 'PNG signature is invalid'
$mutated=[byte[]]$pngBytes.Clone(); $mutated[45]=$mutated[45] -bxor 1
Assert-Throws {[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng($mutated)} 'PNG chunk CRC is invalid'
$truncated=[byte[]]::new($pngBytes.Length-12)
[Array]::Copy($pngBytes,$truncated,$truncated.Length)
Assert-Throws {[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng($truncated)} 'PNG IEND is missing'
$trailing=[byte[]]::new($pngBytes.Length+1)
[Array]::Copy($pngBytes,$trailing,$pngBytes.Length)
Assert-Throws {[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng($trailing)} 'PNG has trailing bytes'
Assert-Throws {[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng([byte[]]::new(10))} 'PNG is too short'
$frame=[pscustomobject]@{Width=1500;Height=1000;IdatBytes=13;Chunks=3;Sha256=('a'*64)}
Assert-ExtensionsFrameDimensions -Evidence $frame -Expected ([pscustomobject]@{Width=1500;Height=1000}) -Phase 'synthetic'
foreach($case in @(
    @{Evidence=[pscustomobject]@{Width=1499;Height=1000;IdatBytes=13;Chunks=3;Sha256=('a'*64)};Message='dimensions'},
    @{Evidence=[pscustomobject]@{Width=1500;Height=1000;IdatBytes=0;Chunks=3;Sha256=('a'*64)};Message='image stream'},
    @{Evidence=[pscustomobject]@{Width=1500;Height=1000;IdatBytes=13;Chunks=2;Sha256=('a'*64)};Message='image stream'},
    @{Evidence=[pscustomobject]@{Width=1500;Height=1000;IdatBytes=13;Chunks=3;Sha256='bad'};Message='digest'}
)){
    Assert-Throws {Assert-ExtensionsFrameDimensions -Evidence $case.Evidence -Expected ([pscustomobject]@{Width=1500;Height=1000}) -Phase 'synthetic'} $case.Message
}
$dimensions=Get-ExtensionsFrameDimensions -Viewport @(1200,800) -Scale 1.0 -Stage 'synthetic'
Assert-True ($dimensions.Width -eq 1200 -and $dimensions.Height -eq 800) 'A 1.0 scale viewport was miscomputed.'
foreach($case in @(
    @{Viewport=@(1190,800);Scale=1.0;Message='Unexpected logical viewport'},
    @{Viewport=@(1200,800);Scale='1.0';Message='scaleFactor'},
    @{Viewport=@(1200,800);Scale=0;Message='Invalid scale factor'},
    @{Viewport='1200x800';Scale=1.0;Message='Invalid viewport'}
)){
    Assert-Throws {Get-ExtensionsFrameDimensions -Viewport $case.Viewport -Scale $case.Scale -Stage 'synthetic'} $case.Message
}

$healthy=[pscustomobject]@{Disposed=$false;DesktopClosed=$true}
$healthy | Add-Member ScriptMethod GetJobSnapshot { return [pscustomobject]@{ActiveProcesses=[uint32]0} }
$healthy | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$healthy | Add-Member ScriptMethod TerminateJob { param($code) }
$healthy | Add-Member ScriptMethod Dispose { $this.Disposed=$true }
Stop-ExtensionsOwnedProcess -Process $healthy
Assert-True $healthy.Disposed 'A normally stopped candidate was not disposed.'
$unclosed=[pscustomobject]@{Disposed=$false;DesktopClosed=$false}
$unclosed | Add-Member ScriptMethod GetJobSnapshot { return [pscustomobject]@{ActiveProcesses=[uint32]0} }
$unclosed | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$unclosed | Add-Member ScriptMethod TerminateJob { param($code) }
$unclosed | Add-Member ScriptMethod Dispose { $this.Disposed=$true }
$unclosedError=$null
try{Stop-ExtensionsOwnedProcess -Process $unclosed}catch{$unclosedError=$_}
Assert-True ($null -ne $unclosedError -and $unclosedError.Exception.Message.Contains('desktop handle did not close')) 'An unclosed desktop handle was accepted.'
$broken=[pscustomobject]@{TerminateAttempted=$false;DisposeAttempted=$false}
$broken | Add-Member ScriptMethod GetJobSnapshot { throw 'synthetic Job query failure' }
$broken | Add-Member ScriptMethod TerminateJob { param($code) $this.TerminateAttempted=$true; throw 'synthetic Job termination failure' }
$broken | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$broken | Add-Member ScriptMethod Dispose { $this.DisposeAttempted=$true; throw 'synthetic Job disposal failure' }
$brokenError=$null
try{Stop-ExtensionsOwnedProcess -Process $broken}catch{$brokenError=$_}
Assert-True ($broken.TerminateAttempted -and $broken.DisposeAttempted) 'An uncertain Job did not attempt termination and disposal.'
Assert-True ($brokenError.Exception -is [AggregateException]) 'Multiple cleanup failures were not combined.'
$messages=($brokenError.Exception.Flatten().InnerExceptions | ForEach-Object {$_.Message}) -join ' | '
foreach($expected in @('synthetic Job query failure','synthetic Job termination failure','synthetic Job disposal failure')){Assert-True ($messages.Contains($expected)) "A cleanup failure was lost: $expected"}
$stringJob=[pscustomobject]@{Disposed=$false;DesktopClosed=$true}
$stringJob | Add-Member ScriptMethod GetJobSnapshot { return [pscustomobject]@{ActiveProcesses='0'} }
$stringJob | Add-Member ScriptMethod TerminateJob { param($code) }
$stringJob | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$stringJob | Add-Member ScriptMethod Dispose { $this.Disposed=$true }
$stringJobError=$null
try{Stop-ExtensionsOwnedProcess -Process $stringJob}catch{$stringJobError=$_}
Assert-True ($null -ne $stringJobError -and $stringJobError.Exception.Message.Contains('Invalid Job active process count')) 'A numeric-string Job count was not rejected.'
Assert-True ($stringJob.Disposed) 'An invalid Job count skipped disposal.'

$fixtureRoot=Join-Path ([IO.Path]::GetTempPath()) ('kerminal-p5-fixture-'+[guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($fixtureRoot)
try {
    $fixture=[pscustomobject]@{HasExited=$true;Disposed=$false}
    $fixture | Add-Member ScriptMethod Kill { throw 'must not kill an exited fixture' }
    $fixture | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
    $fixture | Add-Member ScriptMethod Dispose { $this.Disposed=$true }
    $stdout=[Threading.Tasks.Task]::FromResult('fixture stdout text')
    $stderr=[Threading.Tasks.Task]::FromResult('fixture stderr text')
    Stop-ExtensionsFixture -Fixture $fixture -StdoutTask $stdout -StderrTask $stderr -Base $fixtureRoot
    Assert-True ($fixture.Disposed) 'A stopped fixture was not disposed.'
    Assert-True ([IO.File]::ReadAllText((Join-Path $fixtureRoot 'fixture.stdout.log')).Contains('fixture stdout text')) 'Fixture stdout was not retained.'
    Assert-True ([IO.File]::ReadAllText((Join-Path $fixtureRoot 'fixture.stderr.log')).Contains('fixture stderr text')) 'Fixture stderr was not retained.'
    $badFixture=[pscustomobject]@{HasExited=$false;Disposed=$false}
    $badFixture | Add-Member ScriptMethod Kill { throw 'synthetic fixture kill failure' }
    $badFixture | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $false }
    $badFixture | Add-Member ScriptMethod Dispose { $this.Disposed=$true; throw 'synthetic fixture disposal failure' }
    $badFixtureError=$null
    try{Stop-ExtensionsFixture -Fixture $badFixture -StdoutTask $stdout -StderrTask $stderr -Base $fixtureRoot}catch{$badFixtureError=$_}
    Assert-True ($badFixtureError.Exception -is [AggregateException] -and $badFixture.Disposed) 'Fixture cleanup failures were not aggregated after disposal.'
    $badMessages=($badFixtureError.Exception.Flatten().InnerExceptions | ForEach-Object {$_.Message}) -join ' | '
    foreach($expected in @('synthetic fixture kill failure','did not stop','synthetic fixture disposal failure')){Assert-True ($badMessages.Contains($expected)) "A fixture cleanup failure was lost: $expected"}
} finally {
    if(Test-Path -LiteralPath $fixtureRoot){Remove-Item -LiteralPath $fixtureRoot -Recurse -Force}
}

$tries=@($ast.EndBlock.Statements | Where-Object {$_ -is [Management.Automation.Language.TryStatementAst] -and $null -ne $_.Finally})
Assert-True ($tries.Count -eq 1) 'The acceptance cleanup block is missing or ambiguous.'
$finallyText=$tries[0].Finally.Extent.Text
$cleanup=[scriptblock]::Create($finallyText.Substring(1,$finallyText.Length-2))
$process=[pscustomobject]@{Disposed=$false;DesktopClosed=$false}
$process | Add-Member ScriptMethod GetJobSnapshot { return [pscustomobject]@{ActiveProcesses=[uint32]0} }
$process | Add-Member ScriptMethod WaitForExit { param($milliseconds) return $true }
$process | Add-Member ScriptMethod TerminateJob { param($code) }
$process | Add-Member ScriptMethod Dispose { $this.Disposed=$true; throw [InvalidOperationException]::new('synthetic dispose failure') }
$fixture=$null; $fixtureOutput=$null; $fixtureError=$null; $names=@(); $previous=@{}; $base=$fixtureRoot
try{throw [InvalidOperationException]::new('synthetic original acceptance failure')}catch{$failure=$_}
$combined=$null
try{& $cleanup}catch{$combined=$_}
Assert-True ($combined.Exception -is [AggregateException]) 'Cleanup failure replaced or hid the original failure outside an aggregate.'
$inner=$combined.Exception.Flatten().InnerExceptions
Assert-True ($inner.Count -eq 3 -and $inner[0].Message -ceq 'synthetic original acceptance failure') 'The original failure is not first or cleanup failures were lost.'
$allInner=($inner | ForEach-Object {$_.Message}) -join ' | '
Assert-True ($allInner.Contains('synthetic dispose failure') -and $allInner.Contains('desktop handle did not close') -and $process.Disposed) 'The final cleanup did not attempt disposal and preserve both cleanup failures.'
$failure=$null; $process=$null; $fixture=$null; $names=@(); $previous=@{}
$cleanupError=$null
try{& $cleanup}catch{$cleanupError=$_}
Assert-True ($null -eq $cleanupError) 'A clean finally path returned an unexpected error.'

# 初始化器只编译受控 C# 启动器；不调用 Start，因此不会创建桌面或启动候选进程。
if($null -eq ('Kerminal.PluginExtensionsAcceptance.NativeDesktopLauncher' -as [type])){
    Initialize-ExtensionsLauncher
}
$launcher='Kerminal.PluginExtensionsAcceptance.NativeDesktopLauncher' -as [type]
Assert-True ($null -ne $launcher -and $null -ne $launcher.GetMethod('Start')) 'The P5 private-desktop launcher did not compile.'
Assert-True ([IO.File]::ReadAllText($path).Contains("'KerminalRail-settings-'")) 'The launcher no longer enforces the private desktop prefix.'

$scriptText=[IO.File]::ReadAllText($path)
foreach($required in @(
    'resourcesShutdownComplete','runtimeShutdownObserved','shutdownScope','isolationScope',
    'Assert-ExtensionsAcceptanceReport','Get-ExtensionsStrictInteger','Get-ExtensionsStrictBoolean',
    'Read-ExtensionsPngEvidence','png-structure-crc-and-dimensions','NativeEvidence','InputDesktop',
    'Stop-ExtensionsOwnedProcess','Stop-ExtensionsFixture','KERMINAL_PLUGIN_EXTENSIONS_DESKTOP',
    'NormalExitJobMembers','NonJobProcessesProven=$false','IndependentDesktopResample=$false',
    'FrameFeaturePixelsChecked=$false','SystemComposition=''not-observed''',
    "'KerminalRail-settings-'+[Guid]::NewGuid().ToString('N')",',$stderrPath,$desktop,1200,800)',
    "if(Test-Path -LiteralPath `$output){throw"
)){
    Assert-True ($scriptText.Contains($required)) "Required P5 evidence or cleanup gate is missing: $required"
}
Assert-True (-not $scriptText.Contains('report.shutdownComplete')) 'The runner still accepts the ambiguous shutdownComplete flag.'
foreach($forbidden in @('SwitchDesktop','SendInput','SetForegroundWindow')){Assert-True (-not $scriptText.Contains($forbidden)) "Unsafe desktop operation remains: $forbidden"}
$assertFunction=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq 'Assert-ExtensionsAcceptanceReport'},$true))[0].Extent.Text
Assert-True ($assertFunction.Contains('Get-ExtensionsStrictInteger') -and $assertFunction.Contains('Assert-ExtensionsBoolean') -and $assertFunction.Contains('Get-ExtensionsStringSet')) 'The final report gate lost strict type checks.'

$rust=[IO.File]::ReadAllText((Join-Path $PSScriptRoot '../../crates/kerminal-desktop/src/plugin_extensions_acceptance.rs'))
foreach($required in @('"resourcesShutdownComplete"','"runtimeShutdownObserved"','"shutdownScope"','"isolationScope"','"quitReceipt"','"pendingPluginIds"','"frameEvidence"','"featurePixelsChecked": false','"systemComposition": "not-observed"','KERMINAL_PLUGIN_EXTENSIONS_DESKTOP','combine_acceptance_results','validate_pending_install','validate_private_desktop')){
    Assert-True ($rust.Contains($required)) "The candidate no longer preserves honest P5 evidence: $required"
}
Assert-True (-not $rust.Contains('"shutdownComplete"')) 'The candidate still claims unobserved runtime shutdown as complete.'
Assert-True (-not $rust.Contains('"pendingCountReadBack" = json!(state.pending.len())')) 'The candidate still reports a count without authoritative pending identities.'
'P5 extensions strict-report, PNG and cleanup synthetic tests passed; no desktop or process was started.'
