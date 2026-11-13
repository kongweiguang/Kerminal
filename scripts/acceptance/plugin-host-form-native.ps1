# 插件主机类型表单的三主题私有桌面验收；真实 http-check Sidecar 插件、生产面板与结果 Tab。
# @author kongweiguang
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$PluginPath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('light','dark','system','all')][string]$Theme='all',
    [ValidateRange(60,600)][int]$TimeoutSeconds=180
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Assert-PluginHostFormTrue {
    # 每个门禁都带主题与证据路径，失败时能直接定位到具体进程产物。
    # @author kongweiguang
    param([bool]$Condition,[string]$Message)
    if(-not $Condition){throw $Message}
}

function Assert-PluginHostFormPng {
    # 只核对真实 PNG 头与 IHDR 尺寸，空帧或截断文件不能冒充离线截图。
    # @author kongweiguang
    param([string]$Path,[string]$Phase)
    if(-not (Test-Path -LiteralPath $Path -PathType Leaf)){throw "Missing GPUI offscreen frame: $Phase"}
    $bytes=[IO.File]::ReadAllBytes($Path)
    if($bytes.Length -lt 24 -or $bytes[0] -ne 137 -or $bytes[1] -ne 80 -or $bytes[2] -ne 78 -or $bytes[3] -ne 71){throw "Invalid PNG: $Phase"}
    $width=([int]$bytes[16] -shl 24) -bor ([int]$bytes[17] -shl 16) -bor ([int]$bytes[18] -shl 8) -bor [int]$bytes[19]
    $height=([int]$bytes[20] -shl 24) -bor ([int]$bytes[21] -shl 16) -bor ([int]$bytes[22] -shl 8) -bor [int]$bytes[23]
    if($width -lt 100 -or $height -lt 100){throw "Empty GPUI offscreen frame: $Phase"}
}
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

$repository=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$executable=[IO.Path]::GetFullPath($ExecutablePath)
$plugin=[IO.Path]::GetFullPath($PluginPath)
$base=[IO.Path]::GetFullPath($OutputDirectory)
if(Test-Path -LiteralPath $base){throw 'Host form acceptance output must not exist.'}
[void][IO.Directory]::CreateDirectory($base)
Initialize-RestartLauncher
$candidate=(Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$previous=@{}
$names=@('KERMINAL_PLUGIN_HOST_FORM_OUTPUT','KERMINAL_PLUGIN_HOST_FORM_PLUGIN','KERMINAL_PLUGIN_HOST_FORM_THEME')
foreach($name in $names){$previous[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
$process=$null
$failure=$null
$receipts=@()
try {
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_HOST_FORM_PLUGIN',$plugin,'Process')
    foreach($currentTheme in $(if($Theme -ceq 'all'){@('light','dark','system')}else{@($Theme)})) {
        $output=Join-Path $base $currentTheme
        # 输出根由候选原子创建；脚本预先建目录会让“必须为新目录”的隔离门禁失败。
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_HOST_FORM_OUTPUT',$output,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_HOST_FORM_THEME',$currentTheme,'Process')
        $desktop='KerminalRail-settings-plugin-host-form-'+[guid]::NewGuid().ToString('N')
        $process=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::Start($executable,$repository,$output,(Join-Path $base "$currentTheme.stdout.log"),(Join-Path $base "$currentTheme.stderr.log"),$desktop,1200,800)
        if(-not $process.WaitForExit([uint32]($TimeoutSeconds*1000))){throw "Host form acceptance timed out: $currentTheme"}
        if($process.GetExitCode() -ne 0){throw "Host form acceptance failed: $currentTheme; stderr=$(Join-Path $base "$currentTheme.stderr.log")"}
        $report=Get-Content -LiteralPath (Join-Path $output 'report.json') -Raw | ConvertFrom-Json
        Assert-PluginHostFormTrue ($report.candidateSha256 -ceq $candidate) "Candidate hash mismatch: $currentTheme"
        Assert-PluginHostFormTrue ($report.shutdownComplete -eq $true) "Shutdown incomplete: $currentTheme"
        Assert-PluginHostFormTrue ($report.theme -ceq $currentTheme) "Theme mismatch: $currentTheme"
        Assert-PluginHostFormTrue ($report.hostType -ceq 'kerminal.http-check/http-endpoint') "Host type mismatch: $currentTheme"
        Assert-PluginHostFormTrue (-not [string]::IsNullOrWhiteSpace([string]$report.hostId)) "Host id missing: $currentTheme"
        Assert-PluginHostFormTrue (-not [string]::IsNullOrWhiteSpace([string]$report.testStatus)) "host.test status missing: $currentTheme"
        Assert-PluginHostFormTrue ($report.savedHost.configuration.url -ceq 'https://example.com/') "Saved host configuration missing url: $currentTheme"
        Assert-PluginHostFormTrue ($report.resultTab.contentId -ceq 'kerminal.http-check/http-result') "Result tab mismatch: $currentTheme"
        Assert-PluginHostFormTrue ($report.nativeIsolation.isolationProven -eq $true) "Isolation not proven: $currentTheme"
        Assert-PluginHostFormPng (Join-Path $output 'host-form.png') "$currentTheme form"
        Assert-PluginHostFormPng (Join-Path $output 'host-result.png') "$currentTheme result"
        $settle=[Diagnostics.Stopwatch]::StartNew()
        while($process.GetJobSnapshot().ActiveProcesses -gt 0 -and $settle.ElapsedMilliseconds -lt 15000){Start-Sleep -Milliseconds 100}
        Assert-PluginHostFormTrue ($process.GetJobSnapshot().ActiveProcesses -eq 0) "Live descendants: $currentTheme"
        $processId=$process.ProcessId
        # 桌面句柄在 Dispose 时关闭；关闭后仍读取同一对象的终态，不能提前误报。
        $process.Dispose()
        Assert-PluginHostFormTrue ($process.DesktopClosed -eq $true) "Private desktop handle not closed: $currentTheme"
        $process=$null
        $receipts+=[pscustomobject]@{Theme=$currentTheme;ProcessId=$processId;Report="$currentTheme/report.json";DesktopName=$desktop;DesktopHandleClosed=$true;HostId=$report.hostId;TestStatus=$report.testStatus;ResultTab=$report.resultTab.contentId}
    }
    if((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidate){throw 'Candidate changed during acceptance.'}
    [pscustomobject]@{SchemaVersion=1;CandidateSha256=$candidate;PluginDirectory=$plugin;Mode='private-desktop-production-host-form';Runs=$receipts;NetworkProbeScope='example.com is unreachable in this environment; plugin explicit failure text is the observed host.test outcome';InputMode='production panel form controls; no synthetic pointer input'}|ConvertTo-Json -Depth 8|Set-Content -LiteralPath (Join-Path $base 'acceptance-report.json') -Encoding utf8NoBOM
} catch {
    $failure=$_
} finally {
    foreach($name in $names){try{[Environment]::SetEnvironmentVariable($name,$previous[$name],'Process')}catch{}}
    if($null -ne $process){try{if($process.GetJobSnapshot().ActiveProcesses -gt 0){$process.TerminateJob(1)}}catch{};try{$process.Dispose()}catch{}}
}
if($null -ne $failure){throw $failure}
