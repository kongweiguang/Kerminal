# @author kongweiguang
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$PluginCliPath,
    [ValidateSet('light','dark','system','all')][string]$Theme='all',
    [ValidateRange(60,900)][int]$TimeoutSeconds=300
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Initialize-RestartLauncher {
    # 普通重启和有界 CLI 打包共用独占 Job；只调整参数及继任句柄，不复制生产重启逻辑。
    # @author kongweiguang
    $tokens=$null; $errors=$null
    $ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'webview-native.ps1'),[ref]$tokens,[ref]$errors)
    if($errors.Count){throw 'Cannot parse the existing native launcher.'}
    $parts=[Collections.Generic.List[string]]::new()
    foreach($name in @('Get-WebViewLauncherSource','Replace-WebViewLauncherPart','Stop-OwnedProbe')){
        $matches=@($ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -ceq $name},$true))
        if($matches.Count -ne 1){throw "Launcher function missing or ambiguous: $name"}
        $parts.Add($matches[0].Extent.Text)
    }
    $jobs=@($ast.FindAll({param($n) $n -is [Management.Automation.Language.AssignmentStatementAst] -and $n.Left.Extent.Text -ceq '$jobSource'},$true))
    $launch=@($ast.FindAll({param($n) $n -is [Management.Automation.Language.IfStatementAst] -and $n.Extent.Text.StartsWith("if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher'",[StringComparison]::Ordinal)},$true))
    if($jobs.Count -ne 1 -or $launch.Count -ne 1){throw 'Complete Job launcher not found.'}
    $parts.Add($jobs[0].Extent.Text); $parts.Add($launch[0].Extent.Text)
    $source=($parts -join "`n").Replace('$PSScriptRoot',("'"+$PSScriptRoot.Replace("'","''")+"'"))
    $command='+ " --webview-probe --output " + QuoteArgument(outputDirectory) + " --theme " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_WEBVIEW_PROBE_THEME") ?? "system")'
    if($source.Split($command).Count -ne 2){throw 'Native launcher command changed.'}
    $packageCommand='+ (packageInput == null ? "" : " package --dir " + QuoteArgument(packageInput) + " --out " + QuoteArgument(packageOutput))'
    $source=$source.Replace($command,$packageCommand).Replace('Kerminal.WebViewProbe','Kerminal.PluginRestartAcceptance')
    $compile='Add-Type -TypeDefinition ($source + "`n" + $jobSource) -Language CSharp'
    $private=@'
    $source = Replace-WebViewLauncherPart $source 'if (!String.Equals(desktopName, "Default", StringComparison.OrdinalIgnoreCase))' 'if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))'
    $source = Replace-WebViewLauncherPart $source 'EntryPoint = "OpenDesktopW"' 'EntryPoint = "CreateDesktopW"'
    $source = Replace-WebViewLauncherPart $source 'private static extern IntPtr OpenDesktop(string desktopName, uint flags, bool inherit, uint desiredAccess);' 'private static extern IntPtr CreateDesktop(string desktopName, IntPtr device, IntPtr devmode, uint flags, uint desiredAccess, IntPtr securityAttributes);'
    $source = Replace-WebViewLauncherPart $source 'OpenDesktop(desktopName, 0, false, desktopAccess)' 'CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero)'
    $source = Replace-WebViewLauncherPart $source 'startupInfo.StartupInfo.Flags = StartfUseStdHandles | 1; startupInfo.StartupInfo.ShowWindow = 4;' 'startupInfo.StartupInfo.Flags = StartfUseStdHandles;'
    $source = Replace-WebViewLauncherPart $source 'public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height)' '/* 普通重启沿用原始入口，不能意外带上打包参数。 */ public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height) { return StartCore(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, width, height, null, null); } /* 打包进程也必须在创建时加入专属 Job，参数只按 Windows 命令行规则引用。 */ public static NativeDesktopProcess StartPackage(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, string packageInput, string packageOutput) { if (String.IsNullOrWhiteSpace(packageInput) || String.IsNullOrWhiteSpace(packageOutput)) throw new ArgumentException("Package paths are required."); return StartCore(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, 0, 0, packageInput, packageOutput); } /* 所有入口复用同一安全启动及句柄所有权。 */ private static NativeDesktopProcess StartCore(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height, string packageInput, string packageOutput)'
    $source = Replace-WebViewLauncherPart $source 'public JobSnapshot GetJobSnapshot() { return ProbeJob.Query(_jobHandle); }' 'public JobSnapshot GetJobSnapshot() { return ProbeJob.Query(_jobHandle); } public bool ContainsProcess(IntPtr process) { return RestartHandle.IsInJob(process, _jobHandle); }'
'@
    if($source.Split($compile).Count -ne 2){throw 'Native launcher compile point changed.'}
    $source=$source.Replace($compile,($private+"`n"+$compile))
    $restartHandleSource=@'
namespace Kerminal.PluginRestartAcceptance {
    using System;
    using System.ComponentModel;
    using System.Runtime.InteropServices;
    using System.Text;
    public sealed class RestartHandle : IDisposable {
        private IntPtr handle;
        public int ProcessId { get; private set; }
        public string Executable { get; private set; }
        [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool member);
        [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool QueryFullProcessImageNameW(IntPtr process, uint flags, StringBuilder name, ref uint size);
        [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr process, uint milliseconds);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr handle);
        [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
        // 只持有已进入本轮Job且尚未获准退出的真实进程，句柄后续始终用于等待和退出码读取。
        public static RestartHandle Open(int pid, NativeDesktopProcess owner) {
            IntPtr process=OpenProcess(0x00101000,false,checked((uint)pid));
            if(process==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(),"Opening successor process failed");
            try {
                if(!owner.ContainsProcess(process)) throw new InvalidOperationException("Successor escaped the owned Job");
                uint size=32768; var path=new StringBuilder((int)size);
                if(!QueryFullProcessImageNameW(process,0,path,ref size)) throw new Win32Exception(Marshal.GetLastWin32Error());
                var result=new RestartHandle{handle=process,ProcessId=pid,Executable=path.ToString()}; process=IntPtr.Zero; return result;
            } finally {if(process!=IntPtr.Zero && !CloseHandle(process)) throw new Win32Exception(Marshal.GetLastWin32Error());}
        }
        // Job成员关系基于内核句柄，不通过父PID或进程名称猜测继承关系。
        public static bool IsInJob(IntPtr process,IntPtr job) {
            bool member; if(!IsProcessInJob(process,job,out member)) throw new Win32Exception(Marshal.GetLastWin32Error()); return member;
        }
        // Default桌面侧采样用户前台PID，私有桌面不得取代用户前台窗口。
        public static uint ForegroundProcessId() {uint pid; GetWindowThreadProcessId(GetForegroundWindow(),out pid); return pid;}
        // 保留同一内核对象身份直到验收结束，已退出进程的PID复用不改变结果。
        public bool WaitForExit(uint milliseconds) {
            if(handle==IntPtr.Zero) throw new ObjectDisposedException("RestartHandle");
            uint result=WaitForSingleObject(handle,milliseconds); if(result==0) return true; if(result==258) return false; throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        // 只有句柄发信号才能读取退出码，STILL_ACTIVE永远不算自然退出。
        public uint GetExitCode() {if(!WaitForExit(0)) throw new InvalidOperationException("Process is still running"); uint code; if(!GetExitCodeProcess(handle,out code)) throw new Win32Exception(Marshal.GetLastWin32Error()); return code;}
        // 退出与Job清理先由调用者确认，释放失败直接向上报告。
        public void Dispose() {if(handle!=IntPtr.Zero){if(!CloseHandle(handle)) throw new Win32Exception(Marshal.GetLastWin32Error()); handle=IntPtr.Zero;}}
    }
}
'@
    $source=$source.Replace($compile,'Add-Type -TypeDefinition ($source + "`n" + $jobSource + "`n" + $restartHandleSource) -Language CSharp')
    . ([scriptblock]::Create($source))
}

function Assert-NoRestartFailure {
    # 每代内失败收据可能晚于阶段收据出现，任何已存在失败都优先拒绝后续门禁。
    # 目录缺失、不是目录或枚举被拒绝都必须显式失败；把不可读误当成“没有失败收据”会放行后续阶段。
    # @author kongweiguang
    param([string]$RunRoot)
    if([string]::IsNullOrWhiteSpace($RunRoot) -or -not [IO.Directory]::Exists($RunRoot)){throw "Restart run root is missing or not a directory: $RunRoot"}
    try {
        $failures=@(Get-ChildItem -LiteralPath $RunRoot -Filter 'failure-*.json' -File -ErrorAction Stop)
    } catch {
        throw "Failed to enumerate restart failure receipts: $($_.Exception.Message)"
    }
    if($failures.Count){throw (Get-Content -LiteralPath $failures[0].FullName -Raw)}
}

function Assert-FinalRestartEvidence {
    # 最终必须复核四个独立退出句柄与导演Job的零存活后代；仅见成功报告不足以证明资源回收。
    # @author kongweiguang
    param($Owner,$Handles,$ExpectedCount,[string]$RunRoot)
    $settle=[DateTime]::UtcNow.AddSeconds(15)
    while($Owner.GetJobSnapshot().ActiveProcesses -gt 0 -and [DateTime]::UtcNow -lt $settle){Start-Sleep -Milliseconds 100}
    $job=$Owner.GetJobSnapshot()
    if($Handles.Count -ne $ExpectedCount -or -not $Owner.WaitForExit(0) -or $Owner.GetExitCode() -ne 0){throw 'Expected normal exits were not proven.'}
    if($job.ActiveProcesses -ne 0 -or $job.TotalProcesses -lt $ExpectedCount){throw 'Production restart left live descendants or did not create all generations.'}
    Assert-NoRestartFailure -RunRoot $RunRoot
    return $job
}

function Close-OwnedRestartProcess {
    # 停止与释放各自尝试，再把所有失败汇总；Dispose 不能抹掉等待或 Job 清理错误。
    # @author kongweiguang
    param($Process,[string]$Name)
    $failures=[Collections.Generic.List[object]]::new()
    if($null -ne $Process){
        try {
            if($Process.GetJobSnapshot().ActiveProcesses -gt 0){$Process.TerminateJob(1)}
            if(-not $Process.WaitForExit(10000)){throw "Owned process did not terminate: $Name"}
            $settle=[Diagnostics.Stopwatch]::StartNew()
            while($Process.GetJobSnapshot().ActiveProcesses -gt 0 -and $settle.ElapsedMilliseconds -lt 10000){Start-Sleep -Milliseconds 100}
            if($Process.GetJobSnapshot().ActiveProcesses -gt 0){throw "Owned Job still has descendants: $Name"}
        } catch {$failures.Add($_.Exception)}
        try {$Process.Dispose()} catch {$failures.Add($_.Exception)}
    }
    if($failures.Count -eq 1){throw $failures[0]}
    if($failures.Count -gt 1){throw [System.AggregateException]::new([System.Exception[]]$failures.ToArray())}
}

function Close-RestartResources {
    # 先尽力逐个释放句柄和桌面，再聚合所有失败；单个 Dispose 抛错不能中断剩余清理。
    # @author kongweiguang
    param($Owner,$Handles)
    $errors=[Collections.Generic.List[object]]::new()
    foreach($handle in @($Handles)){
        try{$handle.Dispose()}catch{$errors.Add($_.Exception)}
    }
    if($null -ne $Owner){
        try{$Owner.Dispose()}catch{$errors.Add($_.Exception)}
    }
    if($errors.Count -eq 1){throw $errors[0]}
    if($errors.Count -gt 1){throw [System.AggregateException]::new([System.Exception[]]$errors.ToArray())}
}

function Wait-RestartReceipt {
    # 只等待本轮完整发布的收据；失败收据和超时均拒绝继续授权下一阶段。
    # @author kongweiguang
    param([string]$Path,[string]$RunRoot,[DateTime]$Deadline,$Owner)
    while(-not (Test-Path -LiteralPath $Path -PathType Leaf)){
        Assert-NoRestartFailure -RunRoot $RunRoot
        if([DateTime]::UtcNow -ge $Deadline){throw "Timed out waiting for receipt: $Path"}
        if($Owner.GetJobSnapshot().ActiveProcesses -eq 0){throw 'All owned processes exited before the expected receipt.'}
        Start-Sleep -Milliseconds 100
    }
    # 收据落盘与失败收据可能交错；返回前再扫一次，后续阶段和最终门禁仍会覆盖返回后的迟到失败。
    Assert-NoRestartFailure -RunRoot $RunRoot
    return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Invoke-RestartPackage {
    # CLI 也可能产生后代或卡死；每次调用独占 Job、受限等待并在正常成功前确认后代归零。
    # @author kongweiguang
    param([string]$Cli,[string]$WorkingDirectory,[string]$Base,[string]$Name,[string]$InputDirectory,[int]$Timeout)
    $process=$null
    $output=Join-Path $Base $Name
    $failures=[Collections.Generic.List[object]]::new()
    try {
        $desktop='KerminalRail-settings-package-'+[Guid]::NewGuid().ToString('N')
        $process=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage($Cli,$WorkingDirectory,(Join-Path $Base "$Name-cli"),(Join-Path $Base "$Name.log"),(Join-Path $Base "$Name.stderr.log"),$desktop,$InputDirectory,$output)
        if(-not $process.WaitForExit([uint32]($Timeout*1000))){throw "CLI packaging timed out: $Name"}
        $exitCode=$process.GetExitCode()
        if($exitCode -ne 0){throw "CLI packaging failed: $Name; exitCode=$exitCode; logs=$Name.log, $Name.stderr.log"}
        $settle=[Diagnostics.Stopwatch]::StartNew()
        while($process.GetJobSnapshot().ActiveProcesses -gt 0 -and $settle.ElapsedMilliseconds -lt 10000){Start-Sleep -Milliseconds 100}
        if($process.GetJobSnapshot().ActiveProcesses -ne 0){throw "CLI packaging left live descendants: $Name"}
    } catch {$failures.Add($_.Exception)}
    try {Close-OwnedRestartProcess -Process $process -Name $Name} catch {$failures.Add($_.Exception)}
    if($failures.Count -eq 1){throw $failures[0]}
    if($failures.Count -gt 1){throw [System.AggregateException]::new([System.Exception[]]$failures.ToArray())}
}

function Get-RestartRequiredField {
    # 区分字段缺失与显式 null；缺失控件证据不能被 PowerShell 的空值转换掩盖。
    # @author kongweiguang
    param($Object,[string]$Name)
    if($null -eq $Object -or $null -eq $Object.PSObject.Properties[$Name]){throw "Missing restart evidence field: $Name"}
    return ,$Object.PSObject.Properties[$Name].Value
}

function Assert-RestartString {
    # 不允许 PowerShell 将数值或布尔值隐式转换成预期字符串。
    # @author kongweiguang
    param($Value,[string]$Expected,[string]$Field)
    if($Value -isnot [string] -or $Value -cne $Expected){throw "Invalid restart evidence: $Field"}
}

function Assert-RestartBoolean {
    # 回执中的真假必须是 JSON 布尔值，字符串和数值不能冒充证明。
    # @author kongweiguang
    param($Value,[bool]$Expected,[string]$Field)
    if($Value -isnot [bool] -or $Value -ne $Expected){throw "Invalid restart evidence: $Field"}
}

function Assert-RestartNativeIsolation {
    # startup 与 observed 都以已证明的私有桌面为前置；布尔值和桌面名必须是实际 JSON 类型。
    # @author kongweiguang
    param($Isolation,[string]$Desktop,[string]$Field)
    if($Isolation -isnot [pscustomobject]){throw "Invalid restart isolation: $Field"}
    Assert-RestartBoolean (Get-RestartRequiredField $Isolation 'isolationProven') $true "$Field isolationProven"
    Assert-RestartString (Get-RestartRequiredField $Isolation 'uiThreadDesktopName') $Desktop "$Field desktop"
}

function Assert-RestartOrdinaryMainArgs {
    # 普通 main 启动只允许一个非空 argv 字符串；字符串或空数组不能借长度检查混过。
    # @author kongweiguang
    param($Argv,[string]$Field)
    if($Argv -isnot [array] -or $Argv.Count -ne 1 -or $Argv[0] -isnot [string] -or [string]::IsNullOrWhiteSpace($Argv[0])){throw "Invalid restart ordinary-main argv: $Field"}
}

function Get-RestartUInt {
    # 严格保留 JSON 整数语义，拒绝浮点数、字符串和负数冒充代次或统计结果。
    # @author kongweiguang
    param($Value,[string]$Field)
    if($null -eq $Value -or ($Value -isnot [int] -and $Value -isnot [long] -and $Value -isnot [uint32] -and $Value -isnot [uint64])){throw "Invalid restart integer: $Field"}
    if($Value -lt 0){throw "Invalid restart integer: $Field"}
    return [uint64]$Value
}

function Assert-RestartControl {
    # 控件必须对应真实选择器、名称、可见布局及可点击能力；布局不等于物理屏幕可见。
    # @author kongweiguang
    param($Control,[string]$Selector,[string[]]$Names)
    Assert-RestartString (Get-RestartRequiredField $Control 'selector') $Selector 'control selector'
    foreach($field in @('isButton','layoutVisible','supportsClick')){Assert-RestartBoolean (Get-RestartRequiredField $Control $field) $true "control $field"}
    $name=Get-RestartRequiredField $Control 'accessibleName'
    if($name -isnot [string] -or $Names -cnotcontains $name){throw 'Invalid restart evidence: control accessibleName'}
    $disabled=Get-RestartRequiredField $Control 'accessibilityDisabled'
    if($null -ne $disabled -and ($disabled -isnot [bool] -or $disabled)){throw 'Invalid restart evidence: control accessibilityDisabled'}
    $bounds=Get-RestartRequiredField $Control 'bounds'
    foreach($field in @('x','y','width','height')){
        $value=Get-RestartRequiredField $bounds $field
        if($value -isnot [int] -and $value -isnot [long] -and $value -isnot [double] -and $value -isnot [single]){throw "Invalid restart control bounds: $field"}
        $number=[double]$value
        if([double]::IsNaN($number) -or [double]::IsInfinity($number) -or (($field -ceq 'width' -or $field -ceq 'height') -and $number -le 0)){throw "Invalid restart control bounds: $field"}
    }
}

function Assert-RestartTabControl {
    # 标签控件必须是当前绘制中的 Role::Tab，标签名可重复但 selector 和正布局必须精确。
    # @author kongweiguang
    param($Control,[string]$Selector)
    Assert-RestartString (Get-RestartRequiredField $Control 'selector') $Selector 'tab control selector'
    foreach($field in @('isTab','layoutVisible','supportsClick')){Assert-RestartBoolean (Get-RestartRequiredField $Control $field) $true "tab control $field"}
    $name=Get-RestartRequiredField $Control 'accessibleName'
    if($name -isnot [string] -or [string]::IsNullOrWhiteSpace($name)){throw 'Invalid restart evidence: tab control accessibleName'}
    $disabled=Get-RestartRequiredField $Control 'accessibilityDisabled'
    if($null -ne $disabled -and ($disabled -isnot [bool] -or $disabled)){throw 'Invalid restart evidence: tab control accessibilityDisabled'}
    $bounds=Get-RestartRequiredField $Control 'bounds'
    foreach($field in @('x','y','width','height')){
        $value=Get-RestartRequiredField $bounds $field
        if($value -isnot [int] -and $value -isnot [long] -and $value -isnot [double] -and $value -isnot [single]){throw "Invalid restart tab bounds: $field"}
        $number=[double]$value
        if([double]::IsNaN($number) -or [double]::IsInfinity($number) -or (($field -ceq 'width' -or $field -ceq 'height') -and $number -le 0)){throw "Invalid restart tab bounds: $field"}
    }
}

function Assert-RestartPersistedTab {
    # 每个标签必须独立提供 Tab ID、草稿、viewId 和 renderer 身份，空值不能由数量补足。
    # @author kongweiguang
    param($Tab,[string]$Field)
    Assert-RestartString (Get-RestartRequiredField $Tab 'contentId') 'acme.notes/notes-editor' "$Field contentId"
    $tabId=Get-RestartRequiredField $Tab 'tabId'
    if($tabId -isnot [string] -or [string]::IsNullOrWhiteSpace($tabId)){throw "Invalid restart evidence: $Field tabId"}
    $draft=Get-RestartRequiredField $Tab 'draft'
    if($draft -isnot [string] -or $draft.Length -eq 0){throw "Invalid restart evidence: $Field draft"}
    $viewId=Get-RestartRequiredField $Tab 'viewId'
    if($viewId -isnot [string] -or [string]::IsNullOrWhiteSpace($viewId)){throw "Invalid restart evidence: $Field viewId"}
    $renderer=Get-RestartRequiredField $Tab 'renderer'
    Assert-RestartString (Get-RestartRequiredField $renderer 'status') 'Ready' "$Field renderer status"
    Assert-RestartString (Get-RestartRequiredField $renderer 'plugin') 'acme.notes' "$Field renderer plugin"
    Assert-RestartString (Get-RestartRequiredField $renderer 'view') 'notes.editor' "$Field renderer view"
    if($null -ne (Get-RestartRequiredField $renderer 'error')){throw "Restart $Field renderer reports an error."}
    $instance=Get-RestartRequiredField $renderer 'instance'
    if($instance -isnot [string] -or [string]::IsNullOrWhiteSpace($instance)){throw "Invalid restart evidence: $Field instance"}
    $generation=Get-RestartUInt (Get-RestartRequiredField $renderer 'instanceGeneration') "$Field instanceGeneration"
    $processGeneration=Get-RestartUInt (Get-RestartRequiredField $renderer 'processGeneration') "$Field processGeneration"
    if($processGeneration -eq 0){throw "Invalid restart $Field process generation."}
    return [pscustomobject]@{tabId=$tabId;draft=$draft;viewId=$viewId;instance=$instance;generation=$generation;processGeneration=$processGeneration}
}

function Assert-RestartActivation {
    # 点击记录必须对应工作区标签路径、点击后的同一活动 Tab 和未重建的 viewId。
    # @author kongweiguang
    param($Activation,[hashtable]$Tabs,[string]$Field)
    $tabId=Get-RestartRequiredField $Activation 'tabId'
    if($tabId -isnot [string] -or -not $Tabs.ContainsKey($tabId)){throw "Restart activation points to an unknown tab: $Field"}
    $selector='workspace-tab-label-'+$tabId
    Assert-RestartString (Get-RestartRequiredField $Activation 'selector') $selector "$Field activation selector"
    Assert-RestartString (Get-RestartRequiredField $Activation 'action') 'gpui-pointer-dispatch' "$Field activation action"
    Assert-RestartString (Get-RestartRequiredField $Activation 'activeTabId') $tabId "$Field activeTabId"
    $draft=Get-RestartRequiredField $Activation 'draft'
    if($draft -isnot [string] -or $draft.Length -eq 0){throw "Invalid restart evidence: $Field activation draft"}
    $viewId=Get-RestartRequiredField $Activation 'viewId'
    Assert-RestartString $viewId $Tabs[$tabId].viewId "$Field activation viewId"
    Assert-RestartTabControl (Get-RestartRequiredField $Activation 'tabControl') $selector
    return [pscustomobject]@{tabId=$tabId;draft=$draft;viewId=$viewId}
}

function Assert-RestartSaveAction {
    # 保存证据必须绑定具体标签、旧状态、精确回读和动作前后可点击控件，不能只看状态字符串。
    # @author kongweiguang
    param($Action,[string]$Version,[hashtable]$Tabs,[string]$Field)
    Assert-RestartString (Get-RestartRequiredField $Action 'status') 'confirmed' "$Field save status"
    Assert-RestartString (Get-RestartRequiredField $Action 'version') $Version "$Field save version"
    $tabId=Get-RestartRequiredField $Action 'tabId'
    if($tabId -isnot [string] -or -not $Tabs.ContainsKey($tabId)){throw "Restart save points to an unknown tab: $Field"}
    $inputText=Get-RestartRequiredField $Action 'input'
    if($inputText -isnot [string] -or $inputText.Length -eq 0){throw "Invalid restart evidence: $Field save input"}
    Assert-RestartString (Get-RestartRequiredField $Action 'observedText') $inputText "$Field observedText"
    Assert-RestartString (Get-RestartRequiredField $Action 'readback') 'plugin-settings-service.state_get' "$Field save readback"
    $before=Get-RestartRequiredField $Action 'beforeText'
    if($null -ne $before -and ($before -isnot [string] -or $before -ceq $inputText)){throw "Restart save did not change plugin state: $Field"}
    Assert-RestartControl (Get-RestartRequiredField $Action 'controlBeforeClick') 'external-button-root.2.0' @('Save','保存')
    Assert-RestartControl (Get-RestartRequiredField $Action 'controlAfterClick') 'external-button-root.2.0' @('Save','保存')
    return [pscustomobject]@{tabId=$tabId;input=$inputText;beforeText=$before}
}

function Assert-RestartPersistence {
    # 标签恢复必须由具体 ID/草稿/实例/点击/保存数组解释；上一代期望独立传入，不能由布尔汇总代替。
    # processGeneration 只要求非零存在，不假设跨进程数值递增。
    # @author kongweiguang
    param($Observed,[ValidateRange(0,3)][int]$Phase,$ExpectedTabs)
    $p=Get-RestartRequiredField $Observed 'persistence'
    if((Get-RestartUInt (Get-RestartRequiredField $p 'schemaVersion') 'persistence schemaVersion') -ne 1){throw 'Invalid restart persistence schemaVersion.'}
    if((Get-RestartUInt (Get-RestartRequiredField $p 'phase') 'persistence phase') -ne $Phase){throw 'Invalid restart persistence phase.'}
    Assert-RestartBoolean (Get-RestartRequiredField $p 'createdNewTabs') ($Phase -eq 1) 'persistence createdNewTabs'
    Assert-RestartString (Get-RestartRequiredField $p 'mode') $(if($Phase -eq 0){'not-applicable-bootstrap'}elseif($Phase -eq 1){'seeded'}else{'restored'}) 'persistence mode'
    Assert-RestartString (Get-RestartRequiredField $p 'source') $(if($Phase -eq 0){'bootstrap'}elseif($Phase -eq 1){'new-content-tabs'}else{'workspace-restore'}) 'persistence source'
    if($Phase -eq 0){
        if((Get-RestartUInt (Get-RestartRequiredField $p 'notesTabCount') 'notesTabCount') -ne 0){throw 'Bootstrap persistence must not create notes tabs.'}
        foreach($field in @('tabsBeforeActions','tabsAfterActions','activationPasses','saveActions')){
            $items=Get-RestartRequiredField $p $field
            if($items -isnot [array] -or $items.Count -ne 0){throw "Bootstrap persistence has unexpected $field."}
        }
        return $null
    }
    if((Get-RestartUInt (Get-RestartRequiredField $p 'notesTabCount') 'notesTabCount') -ne 2){throw 'Restart persistence requires exactly two notes tabs.'}
    $beforeValue=Get-RestartRequiredField $p 'tabsBeforeActions'
    $afterValue=Get-RestartRequiredField $p 'tabsAfterActions'
    if($beforeValue -isnot [array] -or $beforeValue.Count -ne 2 -or $afterValue -isnot [array] -or $afterValue.Count -ne 2){throw 'Restart persistence requires two tabs on both sides of actions.'}
    $before=[ordered]@{}
    $beforeOrder=@()
    $instances=@(); $viewIds=@(); $drafts=@()
    foreach($item in $beforeValue){
        $tab=Assert-RestartPersistedTab $item 'before'
        if($before.Contains($tab.tabId)){throw 'Restart persistence has duplicate before tab IDs.'}
        $before[$tab.tabId]=$tab; $beforeOrder+=$tab
        $instances+=$tab.instance; $viewIds+=$tab.viewId; $drafts+=$tab.draft
    }
    if(@($instances | Sort-Object -Unique).Count -ne 2 -or @($viewIds | Sort-Object -Unique).Count -ne 2 -or @($drafts | Sort-Object -Unique).Count -ne 2){throw 'Restart before tabs do not have distinct instance, viewId and draft.'}
    $after=[ordered]@{}
    $afterOrder=@(); $afterDrafts=@()
    foreach($item in $afterValue){
        $tab=Assert-RestartPersistedTab $item 'after'
        if($after.Contains($tab.tabId)){throw 'Restart persistence has duplicate after tab IDs.'}
        if(-not $before.Contains($tab.tabId)){throw 'Restart persistence introduced a new after tab.'}
        if($before[$tab.tabId].viewId -cne $tab.viewId){throw 'Restart persistence rebuilt a tab during actions.'}
        $after[$tab.tabId]=$tab; $afterOrder+=$tab; $afterDrafts+=$tab.draft
    }
    if($after.Count -ne $before.Count -or @($afterDrafts | Sort-Object -Unique).Count -ne 2){throw 'Restart after tabs lost identity or distinct drafts.'}
    if($null -ne $ExpectedTabs){
        $expected=@($ExpectedTabs)
        if($expected.Count -ne 2){throw 'Previous generation did not provide exactly two expected tabs.'}
        $expectedIds=@{}
        foreach($tab in $expected){
            if(-not $before.Contains([string]$tab.tabId)){throw 'Restart restore is missing a previous tab ID.'}
            if($before[[string]$tab.tabId].draft -cne [string]$tab.draft){throw 'Restart restore changed a previous tab draft.'}
            $expectedIds[[string]$tab.tabId]=$true
        }
        if($expectedIds.Count -ne $before.Count){throw 'Restart restore changed the expected tab ID set.'}
    }
    $activations=Get-RestartRequiredField $p 'activationPasses'
    if($activations -isnot [array] -or $activations.Count -lt 2){throw 'Restart persistence requires real tab activations.'}
    $activated=@{}
    foreach($item in $activations){
        $activation=Assert-RestartActivation $item $before 'persistence'
        $activated[$activation.tabId]=$true
    }
    if($activated.Count -ne 2){throw 'Both restored tabs must be activated separately.'}
    $changed=@()
    foreach($tab in $beforeOrder){
        if($after[$tab.tabId].draft -cne $tab.draft){$changed+=$tab.tabId}
    }
    if($Phase -eq 2){
        if($changed.Count -ne 1){throw '1.1 restore must change exactly one tab draft through Count.'}
        $behavior=Get-RestartRequiredField (Get-RestartRequiredField $Observed 'newCapability') 'behavior'
        $target=Get-RestartRequiredField $behavior 'tabId'
        if($target -isnot [string] -or -not $before.Contains($target)){throw 'Count target is not one of the restored tabs.'}
        if($changed[0] -cne $target){throw 'Count target differs from the only changed restored tab.'}
        $cases=Get-RestartRequiredField $behavior 'cases'
        if($cases -isnot [array] -or $cases.Count -eq 0){throw 'Count behavior cases are missing.'}
        $lastInput=Get-RestartRequiredField $cases[-1] 'input'
        if($lastInput -isnot [string] -or $lastInput.Length -eq 0){throw 'Invalid final Count input.'}
        if($after[$target].draft -cne $lastInput){throw 'Count target draft does not match the final action input.'}
    }elseif($changed.Count -ne 0){throw 'This generation must not change persisted drafts.'}
    $savesValue=Get-RestartRequiredField $p 'saveActions'
    if($savesValue -isnot [array]){throw 'Invalid restart save actions.'}
    $saves=@()
    foreach($item in $savesValue){$saves+=Assert-RestartSaveAction $item '1.0.0' $before 'persistence'}
    if($Phase -eq 1){
        if($saves.Count -ne 2 -or @($saves | ForEach-Object tabId | Sort-Object -Unique).Count -ne 2 -or @($saves | ForEach-Object input | Sort-Object -Unique).Count -ne 2){throw '1.0 seed generation must save two distinct tabs and inputs.'}
        foreach($tab in $beforeOrder){if($tab.draft -ceq $saves[-1].input){throw '1.0 tab draft equals the final global plugin state.'}}
    }elseif($Phase -eq 2){
        if($saves.Count -ne 0){throw '1.1 restore generation must not save.'}
    }else{
        if($saves.Count -ne 1 -or $saves[0].input -cne $before[$saves[0].tabId].draft){throw 'Rollback must save the active tab draft once.'}
    }
    $checks=Get-RestartRequiredField $p 'checks'
    foreach($field in @('distinctTabIds','distinctInstances','distinctViewIds','distinctDrafts','stableViewIdPerTab','nonTargetDraftsPreserved')){
        Assert-RestartBoolean (Get-RestartRequiredField $checks $field) $true "persistence checks $field"
    }
    return [pscustomobject]@{Phase=$Phase;Tabs=@($afterOrder | ForEach-Object {[pscustomobject]@{tabId=$_.tabId;draft=$_.draft}})}
}

function Assert-RestartCompletion {
    # 完成收据必须晚于 observed、绑定其精确字节摘要并确认退出请求已受理；无 failure 不等于成功。
    # 生产 QuitReady、正常退出和关闭生命周期仍由调用方另行校验。
    # @author kongweiguang
    param($Completion,[string]$ObservedPath,[int]$Phase,[UInt64]$ProcessId,[string]$Operation,[string]$Intent)
    if(-not [IO.File]::Exists($ObservedPath)){throw "Observed receipt is missing for completion: $ObservedPath"}
    Assert-RestartString (Get-RestartRequiredField $Completion 'status') 'completed' 'completion status'
    if((Get-RestartUInt (Get-RestartRequiredField $Completion 'schemaVersion') 'completion schemaVersion') -ne 1){throw 'Invalid restart completion schemaVersion.'}
    if((Get-RestartUInt (Get-RestartRequiredField $Completion 'phase') 'completion phase') -ne $Phase){throw 'Invalid restart completion phase.'}
    if((Get-RestartUInt (Get-RestartRequiredField $Completion 'processId') 'completion processId') -ne $ProcessId){throw 'Restart completion belongs to another process.'}
    Assert-RestartString (Get-RestartRequiredField $Completion 'observed') ([IO.Path]::GetFileName($ObservedPath)) 'completion observed'
    $expectedHash=(Get-FileHash -LiteralPath $ObservedPath -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
    Assert-RestartString (Get-RestartRequiredField $Completion 'observedSha256') $expectedHash 'completion observedSha256'
    Assert-RestartString (Get-RestartRequiredField $Completion 'operation') $Operation 'completion operation'
    Assert-RestartString (Get-RestartRequiredField $Completion 'requestedIntent') $Intent 'completion requestedIntent'
    Assert-RestartString (Get-RestartRequiredField $Completion 'acceptanceBoundary') 'all-generation-workflows-finished-and-quit-requested' 'completion acceptanceBoundary'
    Assert-RestartBoolean (Get-RestartRequiredField $Completion 'quitRequestAccepted') $true 'completion quitRequestAccepted'
}

function Get-RestartScalarCount {
    # 独立逐个读取 UTF-16 代理对并拒绝孤立代理，避免把字符串长度误当 Unicode 标量数。
    # @author kongweiguang
    param([string]$Text)
    $count=[uint64]0
    for($index=0;$index -lt $Text.Length;$index++){
        $unit=$Text[$index]
        if([char]::IsHighSurrogate($unit)){
            $index++
            if($index -ge $Text.Length -or -not [char]::IsLowSurrogate($Text[$index])){throw 'Invalid Unicode surrogate in restart input.'}
        }elseif([char]::IsLowSurrogate($unit)){throw 'Invalid Unicode surrogate in restart input.'}
        $count++
    }
    return $count
}

function Get-RestartActiveRenderer {
    # 从同一活动 Tab 找到正式 renderer，禁止重复标签、旧实例或静态版本标记充当证据。
    # @author kongweiguang
    param($Observed,[string]$Version)
    $snapshot=Get-RestartRequiredField $Observed 'nativeRenderer'
    $active=Get-RestartRequiredField $snapshot 'activeTabId'
    if($active -isnot [string] -or [string]::IsNullOrWhiteSpace($active)){throw 'Invalid restart active tab.'}
    $tabs=Get-RestartRequiredField $snapshot 'tabs'
    if($tabs -isnot [array]){throw 'Invalid restart renderer tabs.'}
    $matching=@(foreach($tab in $tabs){if((Get-RestartRequiredField $tab 'tabId') -ceq $active){$tab}})
    if($matching.Count -ne 1){throw 'Restart active tab is missing or ambiguous.'}
    $renderer=Get-RestartRequiredField $matching[0] 'renderer'
    Assert-RestartString (Get-RestartRequiredField $renderer 'status') 'Ready' 'renderer status'
    Assert-RestartString (Get-RestartRequiredField $renderer 'plugin') 'acme.notes' 'renderer plugin'
    Assert-RestartString (Get-RestartRequiredField $renderer 'view') 'notes.editor' 'renderer view'
    if($null -ne (Get-RestartRequiredField $renderer 'error')){throw 'Restart renderer reports an error.'}
    $instance=Get-RestartRequiredField $renderer 'instance'
    if($instance -isnot [string] -or [string]::IsNullOrWhiteSpace($instance)){throw 'Invalid restart renderer instance.'}
    $generation=Get-RestartUInt (Get-RestartRequiredField $renderer 'instanceGeneration') 'instanceGeneration'
    $processGeneration=Get-RestartUInt (Get-RestartRequiredField $renderer 'processGeneration') 'processGeneration'
    if($processGeneration -eq 0){throw 'Invalid restart renderer process generation.'}
    $data=Get-RestartRequiredField $renderer 'data'
    Assert-RestartString (Get-RestartRequiredField $data 'marker') $Version 'renderer marker'
    return [pscustomobject]@{tab=$active;instance=$instance;generation=$generation;processGeneration=$processGeneration;data=$data}
}

function Assert-RestartEvidence {
    # 各代分别核对控件语义、动作数据和帧证据，不能把离屏绘制或 fixture 声明当成物理画面。
    # @author kongweiguang
    param($Observed,[ValidateRange(0,3)][int]$Phase)
    $version=@($null,'1.0.0','1.1.0','1.0.0')[$Phase]
    $operation=@('local-file-initial-install','local-file-update','rollback','exit')[$Phase]
    $intent=if($Phase -lt 3){'RestartForExtensions'}else{'Exit'}
    Assert-RestartString (Get-RestartRequiredField $Observed 'status') 'observed' 'status'
    if((Get-RestartUInt (Get-RestartRequiredField $Observed 'schemaVersion') 'schemaVersion') -ne 1){throw 'Invalid restart evidence: schemaVersion'}
    if((Get-RestartUInt (Get-RestartRequiredField $Observed 'phase') 'phase') -ne $Phase){throw 'Invalid restart evidence: phase'}
    if((Get-RestartUInt (Get-RestartRequiredField $Observed 'processId') 'processId') -eq 0){throw 'Invalid restart evidence: processId'}
    Assert-RestartString (Get-RestartRequiredField $Observed 'operation') $operation 'operation'
    Assert-RestartString (Get-RestartRequiredField $Observed 'requestedIntent') $intent 'requestedIntent'
    $marker=Get-RestartRequiredField $Observed 'versionMarker'
    if($Phase -eq 0){if($null -ne $marker){throw 'Bootstrap version marker must be null.'}}else{Assert-RestartString $marker $version 'versionMarker'}
    $frame=Get-RestartRequiredField $Observed 'frameEvidence'
    Assert-RestartString (Get-RestartRequiredField $frame 'source') 'gpui-render-to-image' 'frame source'
    Assert-RestartString (Get-RestartRequiredField $frame 'validation') 'dimensions-and-nonempty-alpha-only' 'frame validation'
    Assert-RestartBoolean (Get-RestartRequiredField $frame 'featurePixelsChecked') $false 'featurePixelsChecked'
    Assert-RestartString (Get-RestartRequiredField $frame 'systemComposition') 'not-observed' 'systemComposition'
    $capability=Get-RestartRequiredField $Observed 'newCapability'
    Assert-RestartString (Get-RestartRequiredField $capability 'physicalScreenVisibility') 'not-observed' 'physicalScreenVisibility'
    $semantics=Get-RestartRequiredField $capability 'viewSemantics'
    $behavior=Get-RestartRequiredField $capability 'behavior'
    if($Phase -eq 0){
        Assert-RestartString (Get-RestartRequiredField $semantics 'status') 'not-applicable-bootstrap' 'bootstrap view status'
        Assert-RestartString (Get-RestartRequiredField $behavior 'status') 'not-attempted-bootstrap' 'bootstrap behavior status'
        return
    }
    $renderer=Get-RestartActiveRenderer $Observed $version
    Assert-RestartString (Get-RestartRequiredField $semantics 'source') 'gpui-last-draw' 'view source'
    $controls=Get-RestartRequiredField $semantics 'controls'
    Assert-RestartControl (Get-RestartRequiredField $controls 'save') 'external-button-root.2.0' @('Save','保存')
    Assert-RestartControl (Get-RestartRequiredField $controls 'check') 'external-button-root.2.1' @('Check example.com','检查 example.com')
    $copy=Get-RestartRequiredField $controls 'copy'
    $count=Get-RestartRequiredField $controls 'count'
    if($Phase -ne 2){
        Assert-RestartString (Get-RestartRequiredField $semantics 'status') 'new-controls-absent' 'old view status'
        Assert-RestartString (Get-RestartRequiredField $behavior 'status') 'not-attempted-control-absent' 'old behavior status'
        if($null -ne $copy -or $null -ne $count -or $null -ne $renderer.data.PSObject.Properties['codePointCount']){throw 'Old restart version has new capability evidence.'}
        return
    }
    Assert-RestartString (Get-RestartRequiredField $semantics 'status') 'new-controls-present' 'new view status'
    Assert-RestartControl $copy 'external-button-root.2.2' @('Copy note','复制笔记')
    Assert-RestartControl $count 'external-button-root.2.3' @('Count code points','统计码点')
    Assert-RestartString (Get-RestartRequiredField $behavior 'status') 'confirmed' 'behavior status'
    Assert-RestartString (Get-RestartRequiredField $behavior 'inputAndClick') 'gpui-window-event-dispatch' 'inputAndClick'
    Assert-RestartString (Get-RestartRequiredField $behavior 'resultSource') 'native-renderer-plugin-data' 'resultSource'
    Assert-RestartBoolean (Get-RestartRequiredField $behavior 'countControlClickAdvertisedAfterResult') $true 'countControlClickAdvertisedAfterResult'
    Assert-RestartString (Get-RestartRequiredField $behavior 'asynchronousSideEffects') 'not-observed' 'asynchronousSideEffects'
    Assert-RestartBoolean (Get-RestartRequiredField $behavior 'clipboardCopyTested') $false 'clipboardCopyTested'
    Assert-RestartBoolean (Get-RestartRequiredField $behavior 'nativeImeTested') $false 'nativeImeTested'
    Assert-RestartString (Get-RestartRequiredField $behavior 'tabId') $renderer.tab 'behavior tabId'
    Assert-RestartString (Get-RestartRequiredField $behavior 'instance') $renderer.instance 'behavior instance'
    if((Get-RestartUInt (Get-RestartRequiredField $behavior 'instanceGeneration') 'behavior instanceGeneration') -ne $renderer.generation -or (Get-RestartUInt (Get-RestartRequiredField $behavior 'processGeneration') 'behavior processGeneration') -ne $renderer.processGeneration){throw 'Restart behavior belongs to another renderer generation.'}
    $cases=Get-RestartRequiredField $behavior 'cases'
    if($cases -isnot [array] -or $cases.Count -ne 2){throw 'Restart behavior requires exactly two action cases.'}
    $inputs=@(); $counts=@(); $previous=[uint64]0
    for($index=0;$index -lt 2;$index++){
        $case=$cases[$index]
        $inputText=Get-RestartRequiredField $case 'input'
        if($inputText -isnot [string] -or $inputText.Length -eq 0){throw 'Invalid restart action input.'}
        $actual=Get-RestartScalarCount $inputText
        $expected=Get-RestartUInt (Get-RestartRequiredField $case 'expectedCodePoints') 'expectedCodePoints'
        $observedCount=Get-RestartUInt (Get-RestartRequiredField $case 'observedCodePoints') 'observedCodePoints'
        if($actual -ne $expected -or $actual -ne $observedCount){throw 'Restart code point result does not match the input.'}
        $before=Get-RestartRequiredField $case 'countBeforeClick'
        if($index -eq 0){Assert-RestartString $before '' 'initial count'}elseif((Get-RestartUInt $before 'countBeforeClick') -ne $previous){throw 'Restart count did not preserve the previous result before click.'}
        if($index -gt 0 -and $previous -eq $actual){throw 'Restart action expected value already existed before click.'}
        Assert-RestartControl (Get-RestartRequiredField $case 'controlBeforeClick') 'external-button-root.2.3' @('Count code points','统计码点')
        Assert-RestartControl (Get-RestartRequiredField $case 'controlAfterClick') 'external-button-root.2.3' @('Count code points','统计码点')
        $inputs+=$inputText; $counts+=$actual; $previous=$actual
    }
    if($inputs[0] -ceq $inputs[1] -or $counts[0] -eq $counts[1]){throw 'Restart action cases must use distinct input and counts.'}
    Assert-RestartString (Get-RestartRequiredField $renderer.data 'text') $inputs[1] 'renderer final action draft'
    # 最终 nativeRenderer 是 Count 后的快照，只能证明最终值；初始空值已由首例 countBeforeClick 单独保存。
    if((Get-RestartUInt (Get-RestartRequiredField $renderer.data 'codePointCount') 'final renderer count') -ne $counts[1]){throw 'Restart final renderer count does not match last Count result.'}
    if(-not [regex]::IsMatch($inputs[1],'[\uD800-\uDBFF][\uDC00-\uDFFF]') -or -not [regex]::IsMatch($inputs[1],'[^\x00-\x7F\uD800-\uDFFF]')){throw 'Restart Unicode case lacks BMP and supplementary scalars.'}
}

$executable=(Resolve-Path -LiteralPath $ExecutablePath).Path
if(-not $PluginCliPath){$PluginCliPath=Join-Path (Split-Path $executable) 'kerminal-plugin.exe'}
$cli=(Resolve-Path -LiteralPath $PluginCliPath).Path
$repositoryRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$base=[IO.Path]::GetFullPath($OutputDirectory)
if(Test-Path -LiteralPath $base){throw 'Output root must not exist.'}
[void][IO.Directory]::CreateDirectory($base)
$candidate=(Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$fixture=Join-Path $repositoryRoot 'tests/fixtures/plugins/restart'
# 两代 Sidecar 必须先由同一 CLI 构建并校验；package 只接受已验证平台二进制目录。
$initialProject=Join-Path $fixture 'notes-1.0'
$updateProject=Join-Path $fixture 'notes-1.1'
& $cli build --project $initialProject --platform windows-x86_64
if($LASTEXITCODE -ne 0){throw 'Initial restart fixture build failed.'}
& $cli build --project $updateProject --platform windows-x86_64
if($LASTEXITCODE -ne 0){throw 'Update restart fixture build failed.'}
$initialBuild=Join-Path $initialProject '.kerminal/build'
$updateBuild=Join-Path $updateProject '.kerminal/build'
Initialize-RestartLauncher
Invoke-RestartPackage -Cli $cli -WorkingDirectory $repositoryRoot -Base $base -Name 'initial-package' -InputDirectory $initialBuild -Timeout $TimeoutSeconds
$initialPackages=@(Get-ChildItem -LiteralPath (Join-Path $base 'initial-package') -Filter '*.kplugin')
if($initialPackages.Count -ne 1){throw 'Expected one exact local initial package.'}
$initialPackage=$initialPackages[0].FullName
$initialPackageHash=(Get-FileHash -LiteralPath $initialPackage -Algorithm SHA256).Hash.ToLowerInvariant()
Invoke-RestartPackage -Cli $cli -WorkingDirectory $repositoryRoot -Base $base -Name 'update-package' -InputDirectory $updateBuild -Timeout $TimeoutSeconds
$packages=@(Get-ChildItem -LiteralPath (Join-Path $base 'update-package') -Filter '*.kplugin')
if($packages.Count -ne 1){throw 'Expected one exact local update package.'}
$package=$packages[0].FullName
$packageHash=(Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant()
$names=@('KERMINAL_CONFIG_ROOT','KERMINAL_PLUGIN_RESTART_OUTPUT','KERMINAL_PLUGIN_RESTART_INITIAL_PACKAGE','KERMINAL_PLUGIN_RESTART_PACKAGE','KERMINAL_PLUGIN_RESTART_THEME')
$previous=@{};foreach($name in $names){$previous[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
$owner=$null; $handles=[Collections.Generic.List[object]]::new(); $runs=@()
$originalFailure=$null; $cleanupFailures=[Collections.Generic.List[object]]::new()
try {
    try {
    foreach($currentTheme in $(if($Theme -ceq 'all'){@('light','dark','system')}else{@($Theme)})){
        $output=Join-Path $base $currentTheme
        $config=Join-Path $output 'config'
        [void][IO.Directory]::CreateDirectory($config)
        [Environment]::SetEnvironmentVariable('KERMINAL_CONFIG_ROOT',$config,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_RESTART_OUTPUT',$output,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_RESTART_INITIAL_PACKAGE',$initialPackage,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_RESTART_PACKAGE',$package,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_RESTART_THEME',$currentTheme,'Process')
        $desktop='KerminalRail-settings-'+[Guid]::NewGuid().ToString('N')
        $owner=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::Start($executable,$repositoryRoot,$output,(Join-Path $base "$currentTheme.stdout.log"),(Join-Path $base "$currentTheme.stderr.log"),$desktop,1200,800)
        $deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        $generationReceipts=@()
        $expectedTabs=$null
        foreach($phase in 0..3){
            Assert-NoRestartFailure -RunRoot $output
            $startup=Wait-RestartReceipt -Path (Join-Path $output "startup-$phase.json") -RunRoot $output -Deadline $deadline -Owner $owner
            $expected=$(if($phase -eq 0){$null}elseif($phase -eq 2){'1.1.0'}else{'1.0.0'})
            if((Get-RestartUInt (Get-RestartRequiredField $startup 'phase') 'startup phase') -ne $phase){throw 'Startup phase mismatch.'}
            Assert-RestartString (Get-RestartRequiredField $startup 'candidateSha256') $candidate 'startup candidate'
            if($phase -eq 0){
                if($null -ne (Get-RestartRequiredField $startup 'frozenVersion')){throw 'Bootstrap frozen version must be null.'}
            }else{
                Assert-RestartString (Get-RestartRequiredField $startup 'frozenVersion') $expected 'startup frozenVersion'
            }
            Assert-RestartNativeIsolation (Get-RestartRequiredField $startup 'nativeIsolation') $desktop 'startup'
            Assert-RestartOrdinaryMainArgs (Get-RestartRequiredField $startup 'argv') 'startup'
            $startupProcessId=Get-RestartUInt (Get-RestartRequiredField $startup 'processId') 'startup processId'
            if($startupProcessId -eq 0 -or $startupProcessId -gt 2147483647){throw 'Startup processId is not a real process identity.'}
            $startupPid=[int]$startupProcessId
            if($phase -eq 0 -and $startupProcessId -ne [uint64]$owner.ProcessId){throw 'First receipt does not belong to the original launch handle.'}
            if(@($handles | Where-Object ProcessId -eq $startupPid).Count){throw 'A process ID was reused as another generation.'}
            $handle=[Kerminal.PluginRestartAcceptance.RestartHandle]::Open($startupPid,$owner)
            $handles.Add($handle)
            if(-not [string]::Equals($handle.Executable,$executable,[StringComparison]::OrdinalIgnoreCase)){throw 'Successor executable identity mismatch.'}
            if([Kerminal.PluginRestartAcceptance.RestartHandle]::ForegroundProcessId() -eq $startupPid){throw 'Private candidate took the input desktop foreground.'}
            $ack=Join-Path $output "ack-$phase.json"
            [IO.File]::WriteAllText("$ack.tmp",(@{phase=$phase;processId=$startupPid}|ConvertTo-Json))
            [IO.File]::Move("$ack.tmp",$ack)
            $observed=Wait-RestartReceipt -Path (Join-Path $output "observed-$phase.json") -RunRoot $output -Deadline $deadline -Owner $owner
            Assert-RestartString (Get-RestartRequiredField $observed 'status') 'observed' 'observed status'
            if((Get-RestartUInt (Get-RestartRequiredField $observed 'phase') 'observed phase') -ne $phase){throw 'Observed phase mismatch.'}
            if((Get-RestartUInt (Get-RestartRequiredField $observed 'processId') 'observed processId') -ne [uint64]$handle.ProcessId){throw 'Observed receipt belongs to another process.'}
            Assert-RestartNativeIsolation (Get-RestartRequiredField $observed 'nativeIsolation') $desktop 'observed'
            Assert-RestartEvidence -Observed $observed -Phase $phase
            $persistenceGate=Assert-RestartPersistence -Observed $observed -Phase $phase -ExpectedTabs $expectedTabs
            $expectedTabs=if($null -eq $persistenceGate){$null}else{$persistenceGate.Tabs}
            $observedPath=Join-Path $output "observed-$phase.json"
            $completion=Wait-RestartReceipt -Path (Join-Path $output "completion-$phase.json") -RunRoot $output -Deadline $deadline -Owner $owner
            $operation=@('local-file-initial-install','local-file-update','rollback','exit')[$phase]
            $intent=if($phase -lt 3){'RestartForExtensions'}else{'Exit'}
            Assert-RestartCompletion -Completion $completion -ObservedPath $observedPath -Phase $phase -ProcessId ([UInt64]$handle.ProcessId) -Operation $operation -Intent $intent
            $frameVersion=$(if($phase -eq 0){'bootstrap'}else{$expected})
            $bytes=[IO.File]::ReadAllBytes((Join-Path $output "native-$phase-$frameVersion.png"))
            if($bytes.Length -lt 1024 -or $bytes[0] -ne 137 -or $bytes[1] -ne 80){throw 'Missing GPUI offscreen frame file.'}
            $remaining=[Math]::Max(0,[Math]::Min(45000,($deadline-[DateTime]::UtcNow).TotalMilliseconds))
            if(-not $handle.WaitForExit([uint32]$remaining) -or $handle.GetExitCode() -ne 0){throw 'Generation failed to exit normally through its retained handle.'}
            Assert-NoRestartFailure -RunRoot $output
            $generationReceipts+=[pscustomobject]@{Phase=$phase;ProcessId=$handle.ProcessId;ExitCode=0;Executable=$handle.Executable;Startup="startup-$phase.json";Observed="observed-$phase.json";Completion="completion-$phase.json";NativeFrame="native-$phase-$frameVersion.png"}
        }
        if($handles.Count -ne 4){throw 'Four distinct normal process exits were not proven.'}
        $job=Assert-FinalRestartEvidence -Owner $owner -Handles $handles -ExpectedCount 4 -RunRoot $output
        $logs=(@(Get-ChildItem -LiteralPath (Join-Path $config 'logs') -Filter '*.log' | ForEach-Object {[IO.File]::ReadAllText($_.FullName)}) -join "`n")
        $lifecycle=@{}
        foreach($event in @('HOST_COMPOSITION_STOP_COMPLETE','HOST_LISTENER_STOP_COMPLETE','DESKTOP_RUNTIME_STOP_COMPLETE')){
            $count=[regex]::Matches($logs,('event='+$event+'(?=\s|$)')).Count
            if($count -lt 4){throw "Production lifecycle log is incomplete: $event"}
            $lifecycle[$event]=$count
        }
        $restartSubmitted=[regex]::Matches($logs,'event=HOST_RESTART_SUBMITTED(?=\s|$)').Count
        if($restartSubmitted -ne 3){throw 'Three production restart commands were not submitted.'}
        $lifecycle['HOST_RESTART_SUBMITTED']=$restartSubmitted
        $restartCount=[regex]::Matches($logs,'event=HOST_RESTART_COMPLETE(?=\s|$)').Count
        if($restartCount -ne 3){throw 'Three production HostCoordinator restarts were not proven.'}
        $lifecycle['HOST_RESTART_COMPLETE']=$restartCount
        $quitSubmitted=[regex]::Matches($logs,'event=HOST_QUIT_SUBMITTED(?=\s|$)').Count
        $quitComplete=[regex]::Matches($logs,'event=HOST_QUIT_COMPLETE(?=\s|$)').Count
        if($quitSubmitted -ne 1 -or $quitComplete -ne 1){throw 'Final production quit command was not completed exactly once.'}
        $lifecycle['HOST_QUIT_SUBMITTED']=$quitSubmitted
        $lifecycle['HOST_QUIT_COMPLETE']=$quitComplete
        if($logs -match 'event=(HOST_(COMPOSITION|LISTENER)_STOP_FAILED|HOST_RESTART_FAILED|HOST_QUIT_FAILED)'){throw 'Production shutdown or restart reported a failure.'}
        Assert-NoRestartFailure -RunRoot $output
        $closingOwner=$owner
        # 只有全部句柄和桌面都成功释放并确认桌面关闭后才清空 owner；失败时保留引用交给 finally 继续兜底。
        Close-RestartResources -Owner $owner -Handles $handles
        if(-not $closingOwner.DesktopClosed){throw 'Owned private desktop handle did not close.'}
        $handles.Clear()
        $owner=$null
        $runs+=[pscustomobject]@{Theme=$currentTheme;Generations=$generationReceipts;ProductionLifecycleCounts=$lifecycle;NormalExitJob=$job;DesktopHandleClosed=$true}
    }
    if((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidate -or (Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant() -cne $packageHash -or (Get-FileHash -LiteralPath $initialPackage -Algorithm SHA256).Hash.ToLowerInvariant() -cne $initialPackageHash){throw 'Candidate or local package changed during acceptance.'}
    [pscustomobject]@{SchemaVersion=1;Mode='ordinary-main-production-restart';CandidateSha256=$candidate;InitialPackageSha256=$initialPackageHash;LocalUpdatePackageSha256=$packageHash;PackageSource='local-file';InitialState='empty config root; ordinary migration then formal local installer';Runs=$runs;MarketplaceInstallTested=$false;NativeImeTested=$false;DpiSwitchTested=$false;SleepResumeTested=$false}|ConvertTo-Json -Depth 10|Set-Content -LiteralPath (Join-Path $base 'acceptance-report.json') -Encoding utf8NoBOM
    } catch {$originalFailure=$_}
} finally {
    # 清理故障只进入汇总，不在 finally 直接抛出；否则会掩盖更早的业务失败。
    try {
        if($null -ne $owner){
            try{Close-OwnedRestartProcess -Process $owner -Name 'original candidate'} catch {$cleanupFailures.Add($_.Exception)}
            try{Close-RestartResources -Owner $null -Handles $handles} catch {$cleanupFailures.Add($_.Exception)}
            $handles.Clear()
        }
    } finally {
        foreach($name in $names){
            try{[Environment]::SetEnvironmentVariable($name,$previous[$name],'Process')}catch{$cleanupFailures.Add($_.Exception)}
        }
    }
}
if($null -ne $originalFailure -or $cleanupFailures.Count){
    $all=[Collections.Generic.List[object]]::new()
    if($null -ne $originalFailure){$all.Add($originalFailure.Exception)}
    foreach($failure in $cleanupFailures){$all.Add($failure)}
    if($all.Count -eq 1){throw $all[0]}
    throw [System.AggregateException]::new([System.Exception[]]$all.ToArray())
}
