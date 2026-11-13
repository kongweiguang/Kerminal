# @author kongweiguang
# 仅解析脚本并使用内存合成回执；不运行打包、窗口、插件或用户进程。
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Assert-True {
    # 保留首个断言失败的定位，避免后续合成样本掩盖原因。
    # @author kongweiguang
    param([bool]$Condition,[string]$Message)
    if(-not $Condition){throw $Message}
}

function Assert-Failure {
    # 校验目标拒绝原因，不能用缺少函数、语法错误或其他异常冒充回归通过。
    # @author kongweiguang
    param([scriptblock]$Action,[string]$Expected,[string]$Case)
    $failure=$null
    try{& $Action}catch{$failure=$_.Exception.Message}
    Assert-True ($null -ne $failure -and $failure.Contains($Expected)) "Case $Case expected '$Expected'; actual: $failure"
}

function New-SyntheticButton {
    # 合成完整语义形状，使每一项缺失都可以独立触发失败门禁。
    # @author kongweiguang
    param([string]$Selector,[string]$Name)
    return [pscustomobject]@{selector=$Selector;isButton=$true;accessibleName=$Name;layoutVisible=$true;supportsClick=$true;accessibilityDisabled=$null;bounds=[pscustomobject]@{x=10;y=20;width=90;height=28}}
}

function New-SyntheticTabControl {
    # 标签控件与真实 Role::Tab 同形，但仅用于内存断言。
    # @author kongweiguang
    param([string]$TabId)
    return [pscustomobject]@{selector=('workspace-tab-label-'+$TabId);isTab=$true;role='Tab';accessibleName='Workspace Notes';layoutVisible=$true;supportsClick=$true;accessibilityDisabled=$null;bounds=[pscustomobject]@{x=10;y=20;width=140;height=32}}
}

function New-SyntheticPersistedTab {
    # 每个标签独立携带草稿、viewId 和实例身份。
    # @author kongweiguang
    param([string]$TabId,[string]$Draft,[string]$ViewId,[string]$Instance)
    return [pscustomobject]@{tabId=$TabId;contentId='acme.notes/notes-editor';draft=$Draft;viewId=$ViewId;renderer=[pscustomobject]@{status='Ready';error=$null;plugin='acme.notes';view='notes.editor';instance=$Instance;instanceGeneration=1;processGeneration=4}}
}

function New-SyntheticActivation {
    # 点击记录绑定标签 selector、活动身份和未重建 viewId。
    # @author kongweiguang
    param([string]$TabId,[string]$Draft,[string]$ViewId)
    return [pscustomobject]@{tabId=$TabId;selector=('workspace-tab-label-'+$TabId);action='gpui-pointer-dispatch';activeTabId=$TabId;draft=$Draft;viewId=$ViewId;tabControl=(New-SyntheticTabControl $TabId)}
}

function New-SyntheticSaveAction {
    # 保存样本包含旧状态、输入、控件和独立回读。
    # @author kongweiguang
    param([string]$TabId,[string]$Text,[string]$Before)
    return [pscustomobject]@{status='confirmed';version='1.0.0';tabId=$TabId;input=$Text;beforeText=$Before;beforeReadback=$null;observedText=$Text;readback='plugin-settings-service.state_get';controlBeforeClick=(New-SyntheticButton 'external-button-root.2.0' 'Save');controlAfterClick=(New-SyntheticButton 'external-button-root.2.0' 'Save');instance=('instance-'+$TabId);instanceGeneration=1;processGeneration=4}
}

function New-SyntheticPersistence {
    # 三个版本阶段使用不同动作集合；phase 2 只改变首标签草稿。
    # @author kongweiguang
    param([ValidateRange(1,3)][int]$Phase)
    $afterA=$(if($Phase -eq 1){'seed alpha'}else{'count alpha'})
    $finalB='seed gamma'
    $activations=@(
        (New-SyntheticActivation 'tab-a' 'seed alpha' 'view-a'),
        (New-SyntheticActivation 'tab-b' $finalB 'view-b'),
        (New-SyntheticActivation 'tab-a' $afterA 'view-a')
    )
    $saves=@()
    if($Phase -eq 1){$saves=@((New-SyntheticSaveAction 'tab-a' 'seed alpha' $null),(New-SyntheticSaveAction 'tab-b' 'seed beta' 'seed alpha'))}
    elseif($Phase -eq 3){$saves=@((New-SyntheticSaveAction 'tab-a' 'count alpha' 'seed beta'))}
    return [pscustomobject]@{
        schemaVersion=1;phase=$Phase;mode=$(if($Phase -eq 1){'seeded'}else{'restored'});source=$(if($Phase -eq 1){'new-content-tabs'}else{'workspace-restore'});createdNewTabs=($Phase -eq 1);notesTabCount=2
        tabsBeforeActions=@((New-SyntheticPersistedTab 'tab-a' $(if($Phase -eq 3){'count alpha'}else{'seed alpha'}) 'view-a' 'instance-a'),(New-SyntheticPersistedTab 'tab-b' $finalB 'view-b' 'instance-b'))
        tabsAfterActions=@((New-SyntheticPersistedTab 'tab-a' $afterA 'view-a' 'instance-a'),(New-SyntheticPersistedTab 'tab-b' $finalB 'view-b' 'instance-b'))
        activationPasses=$activations;saveActions=$saves
        checks=[pscustomobject]@{distinctTabIds=$true;distinctInstances=$true;distinctViewIds=$true;distinctDrafts=$true;stableViewIdPerTab=$true;nonTargetDraftsPreserved=$true}
    }
}

function New-SyntheticPersistenceObserved {
    # 只把持久化样本与 Count 目标/最终输入包成 observed 形状，便于独立测试阶段门禁。
    # @author kongweiguang
    param($Persistence)
    $behavior=[pscustomobject]@{tabId='tab-a'}
    if($Persistence.phase -eq 2){
        $behavior | Add-Member NoteProperty cases @([pscustomobject]@{input='count alpha'})
    }
    return [pscustomobject]@{persistence=$Persistence;newCapability=[pscustomobject]@{behavior=$behavior}}
}

function New-SyntheticReceipt {
    # 只构造内存契约样本；它们不能作为实际 GPUI 或插件运行证据。
    # @author kongweiguang
    param([ValidateRange(0,3)][int]$Phase)
    $versions=@($null,'1.0.0','1.1.0','1.0.0')
    $operations=@('local-file-initial-install','local-file-update','rollback','exit')
    $intent=if($Phase -lt 3){'RestartForExtensions'}else{'Exit'}
    $controls=[pscustomobject]@{
        save=(New-SyntheticButton 'external-button-root.2.0' 'Save')
        check=(New-SyntheticButton 'external-button-root.2.1' 'Check example.com')
        copy=$null;count=$null
    }
    $semantics=[pscustomobject]@{status='new-controls-absent';source='gpui-last-draw';controls=$controls}
    $behavior=[pscustomobject]@{status='not-attempted-control-absent'}
    if($Phase -eq 0){
        $semantics=[pscustomobject]@{status='not-applicable-bootstrap'}
        $behavior=[pscustomobject]@{status='not-attempted-bootstrap'}
    }elseif($Phase -eq 2){
        $controls.copy=New-SyntheticButton 'external-button-root.2.2' 'Copy note'
        $controls.count=New-SyntheticButton 'external-button-root.2.3' 'Count code points'
        $semantics.status='new-controls-present'
        $input2='probe1234longer猫'+[char]::ConvertFromUtf32(0x1F642)
        $behavior=[pscustomobject]@{
            status='confirmed';inputAndClick='gpui-window-event-dispatch';resultSource='native-renderer-plugin-data';asynchronousSideEffects='not-observed';countControlClickAdvertisedAfterResult=$true
            tabId='tab-1';instance='instance-1';instanceGeneration=2;processGeneration=3;clipboardCopyTested=$false;nativeImeTested=$false
            cases=@(
                [pscustomobject]@{input='probe1234short';countBeforeClick='';expectedCodePoints=14;observedCodePoints=14;controlBeforeClick=(New-SyntheticButton 'external-button-root.2.3' 'Count code points');controlAfterClick=(New-SyntheticButton 'external-button-root.2.3' 'Count code points')},
                [pscustomobject]@{input=$input2;countBeforeClick=14;expectedCodePoints=17;observedCodePoints=17;controlBeforeClick=(New-SyntheticButton 'external-button-root.2.3' 'Count code points');controlAfterClick=(New-SyntheticButton 'external-button-root.2.3' 'Count code points')}
            )
        }
    }
    $receipt=[pscustomobject]@{
        schemaVersion=1;status='observed';phase=$Phase;processId=(4321+$Phase);operation=$operations[$Phase];requestedIntent=$intent;versionMarker=$versions[$Phase]
        frameEvidence=[pscustomobject]@{source='gpui-render-to-image';validation='dimensions-and-nonempty-alpha-only';featurePixelsChecked=$false;systemComposition='not-observed'}
        newCapability=[pscustomobject]@{physicalScreenVisibility='not-observed';viewSemantics=$semantics;behavior=$behavior}
        nativeRenderer=[pscustomobject]@{activeTabId='tab-1';tabs=@([pscustomobject]@{tabId='tab-1';renderer=[pscustomobject]@{status='Ready';error=$null;plugin='acme.notes';view='notes.editor';instance='instance-1';instanceGeneration=2;processGeneration=3;data=[pscustomobject]@{marker=$versions[$Phase]}}})}
    }
    if($Phase -eq 2){
        $data=$receipt.nativeRenderer.tabs[0].renderer.data
        $data | Add-Member NoteProperty text $input2
        $data | Add-Member NoteProperty codePointCount 17
    }
    # 回执实际由 ConvertFrom-Json 解析，先往返以覆盖其整数和数组类型。
    return ($receipt | ConvertTo-Json -Depth 20 -Compress | ConvertFrom-Json)
}

$path=Join-Path $PSScriptRoot 'plugin-restart-native.ps1'
$tokens=$null;$errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors)
Assert-True ($errors.Count -eq 0) "Restart script syntax errors: $($errors -join '; ')"
foreach($name in @('Get-RestartRequiredField','Assert-RestartString','Assert-RestartBoolean','Assert-RestartNativeIsolation','Assert-RestartOrdinaryMainArgs','Get-RestartUInt','Assert-RestartControl','Assert-RestartTabControl','Assert-RestartPersistedTab','Assert-RestartActivation','Assert-RestartSaveAction','Assert-RestartPersistence','Assert-RestartCompletion','Get-RestartScalarCount','Get-RestartActiveRenderer','Assert-RestartEvidence','Assert-NoRestartFailure','Assert-FinalRestartEvidence','Close-OwnedRestartProcess','Close-RestartResources','Wait-RestartReceipt','Invoke-RestartPackage')){
    $definitions=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name},$true))
    Assert-True ($definitions.Count -eq 1) "Missing or ambiguous function: $name"
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}
$phaseLoops=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.ForEachStatementAst] -and $node.Variable.VariablePath.UserPath -ceq 'phase'},$true))
Assert-True ($phaseLoops.Count -eq 1) 'Missing or ambiguous four-generation loop.'
$loopText=$phaseLoops[0].Extent.Text
$receiptIndex=$loopText.IndexOf('$observed=Wait-RestartReceipt',[StringComparison]::Ordinal)
$assertIndex=$loopText.IndexOf('Assert-RestartEvidence -Observed $observed -Phase $phase',[StringComparison]::Ordinal)
$persistenceIndex=$loopText.IndexOf('Assert-RestartPersistence -Observed $observed -Phase $phase',[StringComparison]::Ordinal)
$completionIndex=$loopText.IndexOf('$completion=Wait-RestartReceipt',[StringComparison]::Ordinal)
$completionAssertIndex=$loopText.IndexOf('Assert-RestartCompletion -Completion $completion',[StringComparison]::Ordinal)
$exitIndex=$loopText.IndexOf('$handle.WaitForExit',[StringComparison]::Ordinal)
Assert-True ($receiptIndex -ge 0 -and $assertIndex -gt $receiptIndex -and $persistenceIndex -gt $assertIndex -and $completionIndex -gt $persistenceIndex -and $completionAssertIndex -gt $completionIndex -and $exitIndex -gt $completionAssertIndex) 'Each generation must pass evidence, persistence and completion gates before exit is accepted.'
$scriptText=[IO.File]::ReadAllText($path)
$outputCreateIndex=$scriptText.IndexOf('[void][IO.Directory]::CreateDirectory($config)',[StringComparison]::Ordinal)
$failureScanIndex=$scriptText.IndexOf('Assert-NoRestartFailure -RunRoot $output',[StringComparison]::Ordinal)
Assert-True ($outputCreateIndex -ge 0 -and $failureScanIndex -gt $outputCreateIndex) 'Per-theme output directory must be created before the first failure scan.'
foreach($required in @('$deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)','GetJobSnapshot().ActiveProcesses -ne 0','TerminateJob(1)','WaitForExit(0)','DesktopClosed',"'KerminalRail-settings-'+[Guid]::NewGuid().ToString('N')",'event=HOST_QUIT_SUBMITTED','event=HOST_QUIT_COMPLETE','completion-$phase.json','Assert-RestartCompletion -Completion $completion',"Assert-RestartNativeIsolation (Get-RestartRequiredField `$startup 'nativeIsolation') `$desktop 'startup'","Assert-RestartNativeIsolation (Get-RestartRequiredField `$observed 'nativeIsolation') `$desktop 'observed'","Assert-RestartOrdinaryMainArgs (Get-RestartRequiredField `$startup 'argv') 'startup'","Get-RestartUInt (Get-RestartRequiredField `$observed 'processId')","Get-RestartUInt (Get-RestartRequiredField `$startup 'processId')")){
    Assert-True ($scriptText.Contains($required)) "Existing lifecycle or private desktop guard is missing: $required"
}
Assert-True (-not $scriptText.Contains('$startup.argv.Count')) 'Ordinary-main argv still uses loose string length comparison.'
Assert-True (-not $scriptText.Contains('$observed.processId -ne')) 'Observed processId still uses loose PowerShell comparison.'
$closeResourcesIndex=$scriptText.LastIndexOf('Close-RestartResources -Owner $owner -Handles $handles',[StringComparison]::Ordinal)
$desktopClosedIndex=$scriptText.LastIndexOf('if(-not $closingOwner.DesktopClosed)',[StringComparison]::Ordinal)
$ownerClearIndex=$scriptText.LastIndexOf('$owner=$null',[StringComparison]::Ordinal)
Assert-True ($closeResourcesIndex -ge 0 -and $desktopClosedIndex -gt $closeResourcesIndex -and $ownerClearIndex -gt $desktopClosedIndex) 'Successful cleanup must retain owner for fallback until close and desktop checks pass.'

foreach($phase in 0..3){Assert-RestartEvidence -Observed (New-SyntheticReceipt $phase) -Phase $phase}
$zh=New-SyntheticReceipt 2
$zh.newCapability.viewSemantics.controls.save.accessibleName='保存'
$zh.newCapability.viewSemantics.controls.check.accessibleName='检查 example.com'
$zh.newCapability.viewSemantics.controls.copy.accessibleName='复制笔记'
$zh.newCapability.viewSemantics.controls.count.accessibleName='统计码点'
$zh.newCapability.viewSemantics.controls.count.accessibilityDisabled=$false
foreach($case in $zh.newCapability.behavior.cases){$case.controlBeforeClick.accessibleName='统计码点';$case.controlAfterClick.accessibleName='统计码点'}
Assert-RestartEvidence -Observed $zh -Phase 2
Assert-True ((Get-RestartScalarCount ('猫'+[char]::ConvertFromUtf32(0x1F642))) -eq 2) 'UTF-16 surrogate pair was counted as two scalars.'
Assert-True ((Get-RestartScalarCount ('e'+[char]0x0301+[char]::ConvertFromUtf32(0x10000)+[char]::ConvertFromUtf32(0x10FFFF))) -eq 4) 'Combining characters and supplementary boundaries must count as separate scalars.'
Assert-Failure {Get-RestartScalarCount ([string][char]0xD800)} 'Invalid Unicode surrogate' 'lone high surrogate'
Assert-Failure {Get-RestartScalarCount ([string][char]0xDC00)} 'Invalid Unicode surrogate' 'lone low surrogate'
$isolation=[pscustomobject]@{isolationProven=$true;uiThreadDesktopName='desk-test'}
Assert-RestartNativeIsolation $isolation 'desk-test' 'synthetic'
$badIsolation=[pscustomobject]@{isolationProven='true';uiThreadDesktopName='desk-test'}
Assert-Failure {Assert-RestartNativeIsolation $badIsolation 'desk-test' 'synthetic'} 'Invalid restart evidence: synthetic isolationProven' 'isolation string bool'
$badIsolation=[pscustomobject]@{isolationProven=$true;uiThreadDesktopName='desk-other'}
Assert-Failure {Assert-RestartNativeIsolation $badIsolation 'desk-test' 'synthetic'} 'Invalid restart evidence: synthetic desktop' 'isolation wrong desktop'
Assert-Failure {Assert-RestartNativeIsolation ([pscustomobject]@{isolationProven=$true}) 'desk-test' 'synthetic'} 'Missing restart evidence field: uiThreadDesktopName' 'isolation missing desktop'
Assert-RestartOrdinaryMainArgs @('C:\kerminal.exe') 'synthetic'
Assert-Failure {Assert-RestartOrdinaryMainArgs 'C:\kerminal.exe' 'synthetic'} 'Invalid restart ordinary-main argv: synthetic' 'argv string'
Assert-Failure {Assert-RestartOrdinaryMainArgs @() 'synthetic'} 'Invalid restart ordinary-main argv: synthetic' 'argv empty'
Assert-Failure {Assert-RestartOrdinaryMainArgs @(42) 'synthetic'} 'Invalid restart ordinary-main argv: synthetic' 'argv nonstring'

foreach($scenario in @(
    @('missing-schema','Missing restart evidence field: schemaVersion'),
    @('wrong-schema','Invalid restart evidence: schemaVersion'),
    @('missing-frame','Missing restart evidence field: frameEvidence'),
    @('frame-source','Invalid restart evidence: frame source'),
    @('frame-validation','Invalid restart evidence: frame validation'),
    @('frame-pixels','Invalid restart evidence: featurePixelsChecked'),
    @('frame-string-bool','Invalid restart evidence: featurePixelsChecked'),
    @('composition','Invalid restart evidence: systemComposition'),
    @('physical-screen','Invalid restart evidence: physicalScreenVisibility'),
    @('bootstrap-status','Invalid restart evidence: bootstrap view status'),
    @('bootstrap-behavior','Invalid restart evidence: bootstrap behavior status'),
    @('bootstrap-marker','Bootstrap version marker must be null'),
    @('operation','Invalid restart evidence: operation'),
    @('intent','Invalid restart evidence: requestedIntent'),
    @('phase-string','Invalid restart integer: phase'),
    @('process-string','Invalid restart integer: processId'),
    @('process-zero','Invalid restart evidence: processId')
)){
    $bad=New-SyntheticReceipt 0
    switch($scenario[0]){
        'missing-schema' {$bad.PSObject.Properties.Remove('schemaVersion')}
        'wrong-schema' {$bad.schemaVersion=2}
        'missing-frame' {$bad.PSObject.Properties.Remove('frameEvidence')}
        'frame-source' {$bad.frameEvidence.source='screenshot'}
        'frame-validation' {$bad.frameEvidence.validation='feature-pixels'}
        'frame-pixels' {$bad.frameEvidence.featurePixelsChecked=$true}
        'frame-string-bool' {$bad.frameEvidence.featurePixelsChecked='false'}
        'composition' {$bad.frameEvidence.systemComposition='observed'}
        'physical-screen' {$bad.newCapability.physicalScreenVisibility='visible'}
        'bootstrap-status' {$bad.newCapability.viewSemantics.status='new-controls-absent'}
        'bootstrap-behavior' {$bad.newCapability.behavior.status='confirmed'}
        'bootstrap-marker' {$bad.versionMarker='1.0.0'}
        'operation' {$bad.operation='marketplace-install'}
        'intent' {$bad.requestedIntent='Exit'}
        'phase-string' {$bad.phase='0'}
        'process-string' {$bad.processId='4321'}
        'process-zero' {$bad.processId=0}
    }
    Assert-Failure {Assert-RestartEvidence $bad 0} $scenario[1] $scenario[0]
}

foreach($phase in @(1,3)){
    foreach($scenario in @(
        @('missing-save','Missing restart evidence field: selector'),
        @('bad-save-role','Invalid restart evidence: control isButton'),
        @('bad-check-name','Invalid restart evidence: control accessibleName'),
        @('bad-layout','Invalid restart evidence: control layoutVisible'),
        @('bad-click','Invalid restart evidence: control supportsClick'),
        @('bad-disabled','Invalid restart evidence: control accessibilityDisabled'),
        @('missing-disabled','Missing restart evidence field: accessibilityDisabled'),
        @('bad-selector','Invalid restart evidence: control selector'),
        @('bad-bounds','Invalid restart control bounds: width'),
        @('negative-height','Invalid restart control bounds: height'),
        @('bad-coordinate','Invalid restart control bounds: x'),
        @('missing-count','Missing restart evidence field: count'),
        @('unexpected-copy','Old restart version has new capability evidence'),
        @('unexpected-count','Old restart version has new capability evidence'),
        @('unexpected-data','Old restart version has new capability evidence'),
        @('wrong-view-status','Invalid restart evidence: old view status'),
        @('wrong-behavior','Invalid restart evidence: old behavior status'),
        @('wrong-source','Invalid restart evidence: view source'),
        @('wrong-marker','Invalid restart evidence: renderer marker'),
        @('wrong-view','Invalid restart evidence: renderer view'),
        @('renderer-error','Restart renderer reports an error'),
        @('missing-active-tab','Restart active tab is missing or ambiguous'),
        @('duplicate-active-tab','Restart active tab is missing or ambiguous'),
        @('invalid-process-generation','Invalid restart renderer process generation')
    )){
        $bad=New-SyntheticReceipt $phase
        $controls=$bad.newCapability.viewSemantics.controls
        $renderer=$bad.nativeRenderer.tabs[0].renderer
        switch($scenario[0]){
            'missing-save' {$controls.save=$null}
            'bad-save-role' {$controls.save.isButton=$false}
            'bad-check-name' {$controls.check.accessibleName='wrong'}
            'bad-layout' {$controls.check.layoutVisible=$false}
            'bad-click' {$controls.save.supportsClick=$false}
            'bad-disabled' {$controls.check.accessibilityDisabled=$true}
            'missing-disabled' {$controls.check.PSObject.Properties.Remove('accessibilityDisabled')}
            'bad-selector' {$controls.save.selector='another-button'}
            'bad-bounds' {$controls.save.bounds.width=0}
            'negative-height' {$controls.check.bounds.height=-1}
            'bad-coordinate' {$controls.check.bounds.x='10'}
            'missing-count' {$controls.PSObject.Properties.Remove('count')}
            'unexpected-copy' {$controls.copy=New-SyntheticButton 'external-button-root.2.2' 'Copy note'}
            'unexpected-count' {$controls.count=New-SyntheticButton 'external-button-root.2.3' 'Count code points'}
            'unexpected-data' {$renderer.data | Add-Member NoteProperty codePointCount $null}
            'wrong-view-status' {$bad.newCapability.viewSemantics.status='new-controls-present'}
            'wrong-behavior' {$bad.newCapability.behavior.status='confirmed'}
            'wrong-source' {$bad.newCapability.viewSemantics.source='fixture'}
            'wrong-marker' {$renderer.data.marker='1.1.0'}
            'wrong-view' {$renderer.view='notes.panel'}
            'renderer-error' {$renderer.error='failed'}
            'missing-active-tab' {$bad.nativeRenderer.activeTabId='other'}
            'duplicate-active-tab' {$bad.nativeRenderer.tabs+=@($bad.nativeRenderer.tabs[0])}
            'invalid-process-generation' {$renderer.processGeneration=0}
        }
        Assert-Failure {Assert-RestartEvidence $bad $phase} $scenario[1] "phase $phase $($scenario[0])"
    }
}

foreach($scenario in @(
    @('missing-copy','Missing restart evidence field: selector'),
    @('bad-count','Invalid restart evidence: control supportsClick'),
    @('wrong-source','Invalid restart evidence: inputAndClick'),
    @('wrong-result-source','Invalid restart evidence: resultSource'),
    @('click-not-advertised','Invalid restart evidence: countControlClickAdvertisedAfterResult'),
    @('clipboard-claim','Invalid restart evidence: clipboardCopyTested'),
    @('async-claim','Invalid restart evidence: asynchronousSideEffects'),
    @('ime-claim','Invalid restart evidence: nativeImeTested'),
    @('wrong-tab','Invalid restart evidence: behavior tabId'),
    @('wrong-instance','Invalid restart evidence: behavior instance'),
    @('wrong-instance-generation','Restart behavior belongs to another renderer generation'),
    @('wrong-process-generation','Restart behavior belongs to another renderer generation'),
    @('wrong-final-draft','Invalid restart evidence: renderer final action draft'),
    @('final-renderer-count','Restart final renderer count does not match last Count result'),
    @('missing-final-renderer-count','Missing restart evidence field: codePointCount'),
    @('wrong-case-count','Restart behavior requires exactly two action cases'),
    @('wrong-expected','Restart code point result does not match the input'),
    @('wrong-observed','Restart code point result does not match the input'),
    @('string-number','Invalid restart integer: observedCodePoints'),
    @('float-number','Invalid restart integer: observedCodePoints'),
    @('wrong-initial','Invalid restart evidence: initial count'),
    @('wrong-before','Restart count did not preserve the previous result'),
    @('stale-result','Restart action expected value already existed before click'),
    @('not-clickable-before','Invalid restart evidence: control supportsClick'),
    @('not-clickable-after','Invalid restart evidence: control supportsClick'),
    @('disabled-after','Invalid restart evidence: control accessibilityDisabled'),
    @('zero-width-after','Invalid restart control bounds: width'),
    @('unicode-missing','Restart Unicode case lacks BMP and supplementary scalars'),
    @('lone-surrogate','Invalid Unicode surrogate')
)){
    $bad=New-SyntheticReceipt 2
    $behavior=$bad.newCapability.behavior
    $cases=$behavior.cases
    switch($scenario[0]){
        'missing-copy' {$bad.newCapability.viewSemantics.controls.copy=$null}
        'bad-count' {$bad.newCapability.viewSemantics.controls.count.supportsClick=$false}
        'wrong-source' {$behavior.inputAndClick='os-input'}
        'wrong-result-source' {$behavior.resultSource='fixture'}
        'click-not-advertised' {$behavior.countControlClickAdvertisedAfterResult=$false}
        'clipboard-claim' {$behavior.clipboardCopyTested=$true}
        'async-claim' {$behavior.asynchronousSideEffects='healthy'}
        'ime-claim' {$behavior.nativeImeTested=$true}
        'wrong-tab' {$behavior.tabId='other'}
        'wrong-instance' {$behavior.instance='other'}
        'wrong-instance-generation' {$behavior.instanceGeneration=4}
        'wrong-process-generation' {$behavior.processGeneration=4}
        'wrong-final-draft' {$bad.nativeRenderer.tabs[0].renderer.data.text='other draft'}
        'final-renderer-count' {$bad.nativeRenderer.tabs[0].renderer.data.codePointCount=18}
        'missing-final-renderer-count' {$bad.nativeRenderer.tabs[0].renderer.data.PSObject.Properties.Remove('codePointCount')}
        'wrong-case-count' {$behavior.cases=@($cases[0])}
        'wrong-expected' {$cases[1].expectedCodePoints=18}
        'wrong-observed' {$cases[1].observedCodePoints=18}
        'string-number' {$cases[1].observedCodePoints='17'}
        'float-number' {$cases[1].observedCodePoints=[double]17}
        'wrong-initial' {$cases[0].countBeforeClick=14}
        'wrong-before' {$cases[1].countBeforeClick=0}
        'stale-result' {$cases[1].input='another text 1';$cases[1].expectedCodePoints=14;$cases[1].observedCodePoints=14}
        'not-clickable-before' {$cases[0].controlBeforeClick.supportsClick=$false}
        'not-clickable-after' {$cases[1].controlAfterClick.supportsClick=$false}
        'disabled-after' {$cases[1].controlAfterClick.accessibilityDisabled=$true}
        'zero-width-after' {$cases[1].controlAfterClick.bounds.width=0}
        'unicode-missing' {$cases[1].input='abc12';$cases[1].expectedCodePoints=5;$cases[1].observedCodePoints=5;$bad.nativeRenderer.tabs[0].renderer.data.text='abc12';$bad.nativeRenderer.tabs[0].renderer.data.codePointCount=5}
        'lone-surrogate' {$cases[1].input=[string][char]0xD800}
    }
    Assert-Failure {Assert-RestartEvidence $bad 2} $scenario[1] $scenario[0]
}
$root=[IO.Path]::GetTempPath()
$bootstrap=[pscustomobject]@{schemaVersion=1;phase=0;mode='not-applicable-bootstrap';source='bootstrap';createdNewTabs=$false;notesTabCount=0;tabsBeforeActions=@();tabsAfterActions=@();activationPasses=@();saveActions=@();checks=[pscustomobject]@{bootstrap=$true}}
$bootstrapGate=Assert-RestartPersistence -Observed ([pscustomobject]@{persistence=$bootstrap}) -Phase 0 -ExpectedTabs $null
Assert-True ($null -eq $bootstrapGate) 'Bootstrap persistence must not return restored tabs.'
$gate1=Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved (New-SyntheticPersistence 1)) -Phase 1 -ExpectedTabs $null
Assert-True ($gate1.Tabs.Count -eq 2 -and $gate1.Tabs[0].draft -ceq 'seed alpha' -and $gate1.Tabs[1].draft -ceq 'seed gamma') 'Synthetic seeded tabs were not returned for cross-generation linkage.'
$gate2=Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved (New-SyntheticPersistence 2)) -Phase 2 -ExpectedTabs $gate1.Tabs
Assert-True ($gate2.Tabs.Count -eq 2 -and $gate2.Tabs[0].draft -ceq 'count alpha' -and $gate2.Tabs[1].draft -ceq 'seed gamma') 'Synthetic restored tabs did not expose the single target change.'
$gate3=Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved (New-SyntheticPersistence 3)) -Phase 3 -ExpectedTabs $gate2.Tabs
Assert-True ($gate3.Tabs.Count -eq 2 -and $gate3.Tabs[0].draft -ceq 'count alpha') 'Synthetic rollback tabs did not preserve drafts before local save.'

$bad=New-SyntheticPersistence 2
$bad.tabsAfterActions[1].tabId='tab-c'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'introduced a new after tab' 'persistence new tab'
$bad=New-SyntheticPersistence 2
$bad.tabsAfterActions=@($bad.tabsAfterActions[0])
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'two tabs' 'persistence missing tab'
$bad=New-SyntheticPersistence 2
$bad.tabsBeforeActions[1].draft='drifted'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'changed a previous tab draft' 'persistence draft drift'
$bad=New-SyntheticPersistence 2
$bad.tabsBeforeActions[1].tabId='tab-c'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'introduced a new after tab' 'persistence missing previous tab'
$bad=New-SyntheticPersistence 2
$bad.tabsBeforeActions[1].tabId='tab-a'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'duplicate before tab IDs' 'persistence duplicate tab'
$bad=New-SyntheticPersistence 2
$bad.tabsBeforeActions[1].viewId='view-a'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'distinct instance, viewId and draft' 'persistence duplicate view'
$bad=New-SyntheticPersistence 2
$bad.tabsBeforeActions[1].draft=''
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'draft' 'persistence empty draft'
$bad=New-SyntheticPersistence 2
$bad.source='new-content-tabs'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'persistence source' 'persistence source'
$bad=New-SyntheticPersistence 2
$bad.createdNewTabs=$true
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'createdNewTabs' 'persistence created tabs claim'
$bad=New-SyntheticPersistence 2
$bad.tabsAfterActions[1].draft='drifted'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'exactly one tab draft' 'persistence non-target changed'
$bad=New-SyntheticPersistence 2
$bad.tabsAfterActions[0].draft='seed alpha'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'exactly one tab draft' 'persistence target unchanged'
$bad=New-SyntheticPersistence 2
$observedBad=New-SyntheticPersistenceObserved $bad
$observedBad.newCapability.behavior.tabId='tab-b'
Assert-Failure {Assert-RestartPersistence -Observed $observedBad -Phase 2 -ExpectedTabs $gate1.Tabs} 'differs from the only changed' 'persistence count target mismatch'
$observedBad=New-SyntheticPersistenceObserved $bad
$observedBad.newCapability.behavior.tabId='tab-new'
Assert-Failure {Assert-RestartPersistence -Observed $observedBad -Phase 2 -ExpectedTabs $gate1.Tabs} 'not one of the restored tabs' 'persistence count target new tab'
$bad=New-SyntheticPersistence 2
$observedBad=New-SyntheticPersistenceObserved $bad
$observedBad.newCapability.behavior.cases[-1].input='other count'
Assert-Failure {Assert-RestartPersistence -Observed $observedBad -Phase 2 -ExpectedTabs $gate1.Tabs} 'does not match the final action input' 'persistence count draft linkage'
$bad=New-SyntheticPersistence 1
$bad.saveActions=@($bad.saveActions[0])
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 1 -ExpectedTabs $null} 'save two distinct tabs' 'persistence seed missing save'
$bad=New-SyntheticPersistence 1
$bad.tabsBeforeActions[1].draft='seed beta'
$bad.tabsAfterActions[1].draft='seed beta'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 1 -ExpectedTabs $null} 'final global plugin state' 'persistence global fallback'
$bad=New-SyntheticPersistence 3
$bad.saveActions[0].input='other'
$bad.saveActions[0].observedText='other'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 3 -ExpectedTabs $gate2.Tabs} 'save the active tab draft once' 'persistence rollback wrong save'
$bad=New-SyntheticPersistence 2
$bad.activationPasses[0].selector='workspace-tab-hitbox-tab-a'
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'activation selector' 'persistence fake activation path'
$bad=New-SyntheticPersistence 2
$bad.activationPasses[0].tabControl.isTab=$false
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 2 -ExpectedTabs $gate1.Tabs} 'tab control isTab' 'persistence tab role'
$bad=$bootstrap
$bad.tabsBeforeActions=@(New-SyntheticPersistedTab 'tab-a' 'draft' 'view-a' 'instance-a')
Assert-Failure {Assert-RestartPersistence -Observed (New-SyntheticPersistenceObserved $bad) -Phase 0 -ExpectedTabs $null} 'Bootstrap persistence has unexpected' 'bootstrap nonempty tabs'

$completionRoot=Join-Path $root ('kerminal-restart-completion-'+[Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($completionRoot)
try {
    $observedPath=Join-Path $completionRoot 'observed-1.json'
    [IO.File]::WriteAllText($observedPath,'{"phase":1}')
    $completionHash=(Get-FileHash -LiteralPath $observedPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $completion=[pscustomobject]@{schemaVersion=1;status='completed';phase=1;processId=42;observed='observed-1.json';observedSha256=$completionHash;operation='local-file-update';requestedIntent='RestartForExtensions';quitRequestAccepted=$true;acceptanceBoundary='all-generation-workflows-finished-and-quit-requested'}
    Assert-RestartCompletion -Completion $completion -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'
    $bad=$completion.PSObject.Copy();$bad.status='observed'
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion status' 'completion status'
    $bad=$completion.PSObject.Copy();$bad.observedSha256=('0'*64)
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'observedSha256' 'completion hash'
    $bad=$completion.PSObject.Copy();$bad.phase=2
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion phase' 'completion phase'
    $bad=$completion.PSObject.Copy();$bad.processId=43
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'another process' 'completion process'
    $bad=$completion.PSObject.Copy();$bad.observed='observed-2.json'
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion observed' 'completion observed name'
    $bad=$completion.PSObject.Copy();$bad.operation='exit'
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion operation' 'completion operation'
    $bad=$completion.PSObject.Copy();$bad.requestedIntent='Exit'
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion requestedIntent' 'completion intent'
    $bad=$completion.PSObject.Copy();$bad.acceptanceBoundary='partial'
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion acceptanceBoundary' 'completion boundary'
    $bad=$completion.PSObject.Copy();$bad.quitRequestAccepted=$false
    Assert-Failure {Assert-RestartCompletion -Completion $bad -ObservedPath $observedPath -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'completion quitRequestAccepted' 'completion quit request'
    Assert-Failure {Assert-RestartCompletion -Completion $completion -ObservedPath (Join-Path $completionRoot 'missing-1.json') -Phase 1 -ProcessId 42 -Operation 'local-file-update' -Intent 'RestartForExtensions'} 'Observed receipt is missing' 'completion missing observed'
} finally {Remove-Item -LiteralPath $completionRoot -Recurse -Force}

function New-SyntheticFinalOwner {
    # 用内存对象分别控制 Job 活跃/总数、退出等待和退出码，不启动任何进程。
    # @author kongweiguang
    param([int]$Active,[int]$Total,[int]$ExitCode,[bool]$Wait)
    $owner=[pscustomobject]@{Active=$Active;Total=$Total;ExitCode=$ExitCode;Wait=$Wait}
    $owner | Add-Member ScriptMethod GetJobSnapshot {[pscustomobject]@{ActiveProcesses=$this.Active;TotalProcesses=$this.Total}}
    $owner | Add-Member ScriptMethod WaitForExit {param($milliseconds) return $this.Wait}
    $owner | Add-Member ScriptMethod GetExitCode {return $this.ExitCode}
    return $owner
}
$finalRoot=Join-Path $root ('kerminal-restart-final-'+[Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($finalRoot)
try {
    $handles=@([pscustomobject]@{},[pscustomobject]@{},[pscustomobject]@{},[pscustomobject]@{})
    $finalOwner=New-SyntheticFinalOwner 0 4 0 $true
    $finalJob=Assert-FinalRestartEvidence -Owner $finalOwner -Handles $handles -ExpectedCount 4 -RunRoot $finalRoot
    Assert-True ($finalJob.TotalProcesses -eq 4 -and $finalJob.ActiveProcesses -eq 0) 'Final evidence did not return the synthetic zero-live Job.'
    Assert-Failure {Assert-FinalRestartEvidence -Owner (New-SyntheticFinalOwner 0 4 0 $true) -Handles @($handles[0],$handles[1],$handles[2]) -ExpectedCount 4 -RunRoot $finalRoot} 'Expected normal exits were not proven' 'final missing handle'
    Assert-Failure {Assert-FinalRestartEvidence -Owner (New-SyntheticFinalOwner 0 4 0 $false) -Handles $handles -ExpectedCount 4 -RunRoot $finalRoot} 'Expected normal exits were not proven' 'final process still running'
    Assert-Failure {Assert-FinalRestartEvidence -Owner (New-SyntheticFinalOwner 0 4 3 $true) -Handles $handles -ExpectedCount 4 -RunRoot $finalRoot} 'Expected normal exits were not proven' 'final nonzero exit'
    Assert-Failure {Assert-FinalRestartEvidence -Owner (New-SyntheticFinalOwner 0 3 0 $true) -Handles $handles -ExpectedCount 4 -RunRoot $finalRoot} 'left live descendants or did not create all generations' 'final Job total'
} finally {Remove-Item -LiteralPath $finalRoot -Recurse -Force}

$failureRoot=Join-Path $root ('kerminal-restart-failure-'+[Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($failureRoot)
try {
    Assert-NoRestartFailure -RunRoot $failureRoot
    $receiptPath=Join-Path $failureRoot 'startup-1.json'
    [IO.File]::WriteAllText($receiptPath,'{"phase":1}')
    $settled=Wait-RestartReceipt -Path $receiptPath -RunRoot $failureRoot -Deadline ([DateTime]::UtcNow.AddSeconds(1)) -Owner $null
    Assert-True ($settled.phase -eq 1) 'Existing receipt was not returned.'
    [IO.File]::WriteAllText((Join-Path $failureRoot 'failure-late.json'),'{"status":"failed"}')
    Assert-Failure {Assert-NoRestartFailure -RunRoot $failureRoot} 'failed' 'final-only late failure receipt'
    Assert-Failure {Assert-NoRestartFailure -RunRoot (Join-Path $failureRoot 'missing')} 'missing or not a directory' 'missing run root'
    # 用局部命令覆盖模拟目录枚举拒绝，不依赖修改真实目录 ACL。
    Assert-Failure {
        & {
            function Get-ChildItem { throw [UnauthorizedAccessException]::new('synthetic enumeration denied') }
            Assert-NoRestartFailure -RunRoot $failureRoot
        }
    } 'Failed to enumerate restart failure receipts' 'unreadable run root'
    Assert-Failure {Wait-RestartReceipt -Path $receiptPath -RunRoot $failureRoot -Deadline ([DateTime]::UtcNow.AddSeconds(1)) -Owner $null} 'failed' 'failure racing an existing receipt'
} finally {Remove-Item -LiteralPath $failureRoot -Recurse -Force}

function New-CleanupThrows([string]$Message,[string]$Method){
    # 用内存对象模拟每个清理步骤独立失败，验证后续资源和原异常不被吞掉。
    # @author kongweiguang
    $object=[pscustomobject]@{Disposed=$false;Message=$Message;Method=$Method}
    $object | Add-Member ScriptMethod GetJobSnapshot {[pscustomobject]@{ActiveProcesses=1}}
    $object | Add-Member ScriptMethod TerminateJob {throw [InvalidOperationException]::new($this.Message+'-stop')}
    $object | Add-Member ScriptMethod WaitForExit {param($milliseconds) return $true}
    $object | Add-Member ScriptMethod Dispose {$this.Disposed=$true;throw [InvalidOperationException]::new($this.Message)}
    return $object
}
$failure=New-CleanupThrows 'handle-dispose' 'Dispose'
$owner=New-CleanupThrows 'owner-dispose' 'Dispose'
$caught=$null
try{Close-RestartResources -Owner $owner -Handles @($failure)}catch{$caught=$_}
Assert-True ($null -ne $caught -and $caught.Exception -is [AggregateException] -and $caught.Exception.InnerExceptions.Count -eq 2) 'Cleanup stops after the first handle or loses a failure.'
Assert-True ($failure.Disposed -and $owner.Disposed) 'Cleanup did not continue to every owned resource.'
$process=New-CleanupThrows 'process-dispose' 'Dispose'
$caught=$null
try{Close-OwnedRestartProcess -Process $process -Name 'synthetic'}catch{$caught=$_}
Assert-True ($null -ne $caught -and $caught.Exception.InnerExceptions.Count -eq 2 -and $process.Disposed) 'Process stop and Dispose failures are not aggregated.'
$original=[InvalidOperationException]::new('original acceptance failure')
$aggregate=[AggregateException]::new([Exception[]]@($original,$caught.Exception.Flatten().InnerExceptions[0],$caught.Exception.Flatten().InnerExceptions[1]))
Assert-True ($aggregate.Flatten().InnerExceptions.Count -eq 3 -and $aggregate.Flatten().InnerExceptions[0].Message -ceq 'original acceptance failure') 'Original failure is not preserved before cleanup failures.'

# 用假打包器让业务失败与 Dispose 失败同时发生，验证 Invoke-RestartPackage 不会让清理异常掩盖首个错误。
if($null -eq ('Kerminal.PluginRestartAcceptance.NativeDesktopLauncher' -as [type])){
    Add-Type -TypeDefinition @'
namespace Kerminal.PluginRestartAcceptance {
    public sealed class SyntheticPackagingJob { public int ActiveProcesses { get { return 0; } } }
    public sealed class SyntheticPackagingProcess {
        public bool WaitForExit(uint milliseconds) { return true; }
        public int GetExitCode() { return 3; }
        public SyntheticPackagingJob GetJobSnapshot() { return new SyntheticPackagingJob(); }
        public void Dispose() { throw new System.InvalidOperationException("synthetic cleanup dispose"); }
    }
    public static class NativeDesktopLauncher {
        public static SyntheticPackagingProcess StartPackage(string cli, string workingDirectory, string cliRoot, string log, string errorLog, string desktop, string input, string output) { return new SyntheticPackagingProcess(); }
    }
}
'@
}
$packageRoot=Join-Path $root ('kerminal-restart-package-'+[Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($packageRoot)
try {
    $caught=$null
    try{Invoke-RestartPackage -Cli 'synthetic-cli' -WorkingDirectory $packageRoot -Base $packageRoot -Name 'synthetic-package' -InputDirectory $packageRoot -Timeout 1}catch{$caught=$_}
    $inner=if($null -ne $caught){$caught.Exception.Flatten().InnerExceptions}else{@()}
    Assert-True ($inner.Count -eq 2 -and $inner[0].Message.Contains('CLI packaging failed') -and $inner[1].Message.Contains('synthetic cleanup dispose')) ("Packaging failure is masked or its cleanup failure is lost: "+(($inner | ForEach-Object Message) -join ' | '))
} finally {Remove-Item -LiteralPath $packageRoot -Recurse -Force}

# 两代夹具共用同一 Rust 截断实现；直接编译并执行其单元测试，避免另写一份 JS 逻辑。
$fixtureSource = Join-Path $PSScriptRoot '../../tests/support/sidecars/note_text.rs'
Assert-True ([IO.File]::ReadAllText($fixtureSource).Contains('chars().take(MAX_NOTE_TEXT)')) 'Restart fixture does not use code-point-safe noteText truncation.'
$fixtureTestRoot = Join-Path ([IO.Path]::GetTempPath()) ('kerminal-note-text-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($fixtureTestRoot)
try {
    $fixtureTestExe = Join-Path $fixtureTestRoot 'note_text_tests.exe'
    & rustc --edition 2024 --test $fixtureSource -o $fixtureTestExe
    Assert-True ($LASTEXITCODE -eq 0) 'Restart fixture Unicode scalar test does not compile.'
    & $fixtureTestExe
    Assert-True ($LASTEXITCODE -eq 0) 'Restart fixture Unicode scalar runtime boundary test failed.'
} finally {
    Remove-Item -LiteralPath $fixtureTestRoot -Recurse -Force
}

Assert-True ($scriptText.Contains("'asynchronousSideEffects') 'not-observed'")) 'Missing asynchronous side-effect boundary gate.'
$tryCount=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.TryStatementAst] -and $null -ne $node.Finally},$true)).Count
Assert-True ($tryCount -ge 1 -and $scriptText.Contains('Assert-NoRestartFailure -RunRoot $output')) 'Final generation failure scan is missing.'
$finallyBlocks=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.TryStatementAst] -and $null -ne $node.Finally},$true))
$outerFinally=$finallyBlocks | Sort-Object {$_.Extent.EndOffset} | Select-Object -Last 1
Assert-True ($null -ne $outerFinally -and -not $outerFinally.Finally.Extent.Text.Contains('throw ')) 'Outer cleanup throws inside finally and can mask the original failure.'
'P7 synthetic four-generation receipt gates passed; no native process was started.'
