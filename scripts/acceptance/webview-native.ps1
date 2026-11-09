# @author kongweiguang
param(
    [Parameter(Mandatory = $true)][string]$ExecutablePath,
    [string]$OutputRoot,
    [ValidateRange(60, 900)][int]$TimeoutSeconds = 300
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-WebViewLauncherSource {
    # 按语法树读取既有启动器，并校验每个替换点唯一，避免不匹配时误启动普通用户桌面。
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'settings-parity.ps1'), [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw '无法解析私有桌面启动器' }
    $matches = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$script:NativeDesktopLauncherSource' }, $true))
    if ($matches.Count -ne 1 -or $matches[0].Right.Expression -isnot [System.Management.Automation.Language.StringConstantExpressionAst]) { throw '私有桌面启动器不唯一' }
    $source = $matches[0].Right.Expression.Value
    $replacements = @{
        '+ " --settings-acceptance --output-dir " + QuoteArgument(outputDirectory)' = '+ " --webview-probe --output " + QuoteArgument(outputDirectory) + " --theme " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_WEBVIEW_PROBE_THEME") ?? "system")'
        '+ " --width " + width.ToString(CultureInfo.InvariantCulture)' = ''
        '+ " --height " + height.ToString(CultureInfo.InvariantCulture)' = ''
        'if (variables.TryGetValue("KERMINAL_SETTINGS_ACCEPTANCE_SCOPE", out var scope) && scope == "suggestions")' = 'if (true)'
        'var profileRoot = System.IO.Path.Combine(outputDirectory, "config");' = 'var profileRoot = outputDirectory + "-profile"; System.IO.Directory.CreateDirectory(System.IO.Path.Combine(profileRoot, "AppData", "Local")); System.IO.Directory.CreateDirectory(System.IO.Path.Combine(profileRoot, "AppData", "Roaming"));'
        'variables["USERPROFILE"] = profileRoot;' = 'var systemRoot = System.IO.Directory.GetParent(Environment.SystemDirectory)?.FullName; if (String.IsNullOrWhiteSpace(systemRoot) || !System.IO.Path.IsPathFullyQualified(systemRoot)) throw new InvalidOperationException("无法确定 Windows 系统目录"); var systemDrive = System.IO.Path.GetPathRoot(systemRoot)?.TrimEnd(System.IO.Path.DirectorySeparatorChar); if (String.IsNullOrWhiteSpace(systemDrive)) throw new InvalidOperationException("无法确定 Windows 系统盘"); var programData = System.IO.Path.Combine(profileRoot, "ProgramData"); var temp = System.IO.Path.Combine(profileRoot, "Temp"); System.IO.Directory.CreateDirectory(programData); System.IO.Directory.CreateDirectory(temp); variables["SystemRoot"] = systemRoot; variables["windir"] = systemRoot; variables["SystemDrive"] = systemDrive; variables["ProgramData"] = programData; variables["ALLUSERSPROFILE"] = programData; variables["TEMP"] = temp; variables["TMP"] = temp; variables["USERPROFILE"] = profileRoot;'
        'namespace Kerminal.SettingsParity' = 'namespace Kerminal.WebViewProbe'
    }
    foreach ($entry in $replacements.GetEnumerator()) {
        $first = $source.IndexOf($entry.Key, [StringComparison]::Ordinal)
        if ($first -lt 0 -or $source.IndexOf($entry.Key, $first + $entry.Key.Length, [StringComparison]::Ordinal) -ge 0) { throw "启动器替换点发生变化：$($entry.Key)" }
        $source = $source.Replace($entry.Key, $entry.Value)
    }
    return $source
}

function Replace-WebViewLauncherPart {
    # 设置启动器来自另一脚本，必须在结构完全匹配时才注入作业约束。
    param([string]$Source, [string]$Old, [string]$New)
    $first = $Source.IndexOf($Old, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $Source.IndexOf($Old, $first + $Old.Length, [StringComparison]::Ordinal) -ge 0) { throw "私有桌面启动器作业注入点发生变化：$Old" }
    return $Source.Replace($Old, $New)
}

function Stop-OwnedProbe {
    # Job 在候选首线程恢复前绑定，只终止本轮继承的进程；避免 PID 复用和按名称误杀。
    param([Parameter(Mandatory)]$Process)
    $snapshot = $Process.GetJobSnapshot()
    if ($snapshot.ActiveProcesses -gt 0) { $Process.TerminateJob(1) }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ($Process.GetJobSnapshot().ActiveProcesses -gt 0 -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
    if ($Process.GetJobSnapshot().ActiveProcesses -gt 0) { throw '验收 Job 仍有活动进程' }
    if (-not $Process.WaitForExit(10000)) { $Process.Terminate(1) }
    if (-not $Process.WaitForExit(10000)) { throw '探针进程未退出' }
}

$jobSource = @'
namespace Kerminal.WebViewProbe {
    public sealed class JobSnapshot {
        public uint TotalProcesses, ActiveProcesses, TerminatedProcesses;
    }
    internal static class ProbeJob {
        [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
        private struct Accounting {
            public long UserTime, KernelTime, PeriodUserTime, PeriodKernelTime;
            public uint PageFaults, TotalProcesses, ActiveProcesses, TerminatedProcesses;
        }
        [System.Runtime.InteropServices.DllImport("kernel32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode, SetLastError=true)]
        private static extern System.IntPtr CreateJobObjectW(System.IntPtr attributes, string name);
        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
        private static extern bool AssignProcessToJobObject(System.IntPtr job, System.IntPtr process);
        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
        private static extern bool QueryInformationJobObject(System.IntPtr job, int informationClass, out Accounting information, uint length, out uint written);
        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
        private static extern bool TerminateJobObject(System.IntPtr job, uint code);
        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
        internal static extern uint ResumeThread(System.IntPtr thread);
        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
        private static extern bool CloseHandle(System.IntPtr handle);

        // 在挂起的首线程运行前完成绑定，使候选创建的所有继承子进程进入同一个 Job。
        internal static System.IntPtr CreateAndAssign(System.IntPtr process) {
            var job=CreateJobObjectW(System.IntPtr.Zero,null);
            if(job==System.IntPtr.Zero) throw new System.ComponentModel.Win32Exception(System.Runtime.InteropServices.Marshal.GetLastWin32Error(),"创建验收 Job 失败");
            if(!AssignProcessToJobObject(job,process)) {
                int error=System.Runtime.InteropServices.Marshal.GetLastWin32Error();
                if(!CloseHandle(job)) throw new System.ComponentModel.Win32Exception(System.Runtime.InteropServices.Marshal.GetLastWin32Error(),"验收 Job 绑定失败且句柄未关闭");
                throw new System.ComponentModel.Win32Exception(error,"候选无法绑定验收 Job");
            }
            return job;
        }
        // 使用 Job 内核计数而不是重新打开 PID，能够检查已退出父进程留下的后代。
        internal static JobSnapshot Query(System.IntPtr job) {
            if(job==System.IntPtr.Zero) throw new System.ObjectDisposedException("acceptance job");
            Accounting stats; uint written;
            if(!QueryInformationJobObject(job,1,out stats,(uint)System.Runtime.InteropServices.Marshal.SizeOf(typeof(Accounting)),out written))
                throw new System.ComponentModel.Win32Exception(System.Runtime.InteropServices.Marshal.GetLastWin32Error(),"读取验收 Job 状态失败");
            return new JobSnapshot { TotalProcesses=stats.TotalProcesses, ActiveProcesses=stats.ActiveProcesses, TerminatedProcesses=stats.TerminatedProcesses };
        }
        // 只作用于此次独占 Job；关闭前仍须回读活动计数。
        internal static void Terminate(System.IntPtr job,uint code) {
            if(!TerminateJobObject(job,code)) throw new System.ComponentModel.Win32Exception(System.Runtime.InteropServices.Marshal.GetLastWin32Error(),"终止验收 Job 失败");
        }
        // 启动失败时仍尝试终止与关闭，并把错误交给调用方而非冒充成功。
        internal static string Abort(System.IntPtr job) {
            bool killed=TerminateJobObject(job,1); int killError=System.Runtime.InteropServices.Marshal.GetLastWin32Error();
            bool closed=CloseHandle(job); int closeError=System.Runtime.InteropServices.Marshal.GetLastWin32Error();
            return killed && closed ? null : "启动失败后的 Job 清理失败：terminate="+killError+", close="+closeError;
        }
    }
}
'@

$captureSource = @'
using System;
using System.IO;
namespace Kerminal.WebViewProbe {
    public static class Capture {
        public sealed class Evidence { public int Width,Height; public long PagePixels,ProofPixels,OverlayPixels; public string File; }
        // 独立解析单个 WGC 输出的原始 BMP，所有计数必须来自同一帧。
        public static Evidence Inspect(string path,bool modal) {
            long length=new FileInfo(path).Length;
            if(length<54 || length>268435510) throw new InvalidDataException("系统帧文件尺寸无效");
            byte[] bytes=File.ReadAllBytes(path);
            if(bytes[0]!=0x42 || bytes[1]!=0x4d || BitConverter.ToInt32(bytes,14)!=40 || BitConverter.ToInt16(bytes,28)!=32 || BitConverter.ToInt32(bytes,30)!=0)
                throw new InvalidDataException("系统帧不是未压缩的 32 位 BMP");
            int offset=BitConverter.ToInt32(bytes,10), width=BitConverter.ToInt32(bytes,18), rawHeight=BitConverter.ToInt32(bytes,22);
            if(width<=0 || width>8192 || rawHeight==0 || rawHeight < -8192 || rawHeight>8192 || offset<54)
                throw new InvalidDataException("系统帧尺寸无效");
            int height=Math.Abs(rawHeight);
            if((long)offset+(long)width*height*4>bytes.LongLength) throw new InvalidDataException("系统帧像素不完整");
            long page=0,proof=0,overlay=0;
            for(int i=offset;i<offset+width*height*4;i+=4) {
                bool body=Matches(bytes,i,18,52,86,modal);
                if(body) page++;
                if(!body && Matches(bytes,i,37,207,120,modal)) proof++;
                if(Matches(bytes,i,230,58,145,false)) overlay++;
            }
            return new Evidence{Width=width,Height=height,PagePixels=page,ProofPixels=proof,OverlayPixels=overlay,File=path};
        }
        // 只接受真实主题蒙层的离散强度；黑屏和无关深色不能充当网页。
        static bool Matches(byte[] bytes,int i,int r,int g,int b,bool modal) {
            int[] scales=modal ? new int[]{100,95,80} : new int[]{100};
            foreach(int scale in scales) {
                if(Math.Abs(bytes[i]-(b*scale+50)/100)<=3 && Math.Abs(bytes[i+1]-(g*scale+50)/100)<=3 && Math.Abs(bytes[i+2]-(r*scale+50)/100)<=3) return true;
            }
            return false;
        }
    }
}
'@

$executable = (Resolve-Path -LiteralPath $ExecutablePath).Path
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $OutputRoot) { $OutputRoot = Join-Path $repositoryRoot ("target/diagnostics/webview-native/" + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0,8)) }
$OutputRoot = [IO.Path]::GetFullPath($OutputRoot)
if (Test-Path -LiteralPath $OutputRoot) { throw '输出根目录必须不存在' }
[void][IO.Directory]::CreateDirectory($OutputRoot)
$candidateHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher' -as [type])) {
    $source = Get-WebViewLauncherSource
    # WGC 要求输入桌面；只放行 Default，窗口自身先隐藏、加 NOACTIVATE 后才显示在 Z 序底部。
    $source = Replace-WebViewLauncherPart $source 'if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))' 'if (!String.Equals(desktopName, "Default", StringComparison.OrdinalIgnoreCase))'
    # Default 桌面已经存在；取得自己的句柄用于启动和清理，不能尝试创建或切换输入桌面。
    $source = Replace-WebViewLauncherPart $source 'EntryPoint = "CreateDesktopW"' 'EntryPoint = "OpenDesktopW"'
    $source = Replace-WebViewLauncherPart $source 'private static extern IntPtr CreateDesktop(string desktopName, IntPtr device, IntPtr devmode, uint flags, uint desiredAccess, IntPtr securityAttributes);' 'private static extern IntPtr OpenDesktop(string desktopName, uint flags, bool inherit, uint desiredAccess);'
    $source = Replace-WebViewLauncherPart $source 'CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero)' 'OpenDesktop(desktopName, 0, false, desktopAccess)'
    $source = Replace-WebViewLauncherPart $source 'startupInfo.StartupInfo.Flags = StartfUseStdHandles;' 'startupInfo.StartupInfo.Flags = StartfUseStdHandles | 1; startupInfo.StartupInfo.ShowWindow = 4;'
    $source = Replace-WebViewLauncherPart $source 'private IntPtr _processHandle;' 'private IntPtr _processHandle; private IntPtr _jobHandle;'
    $source = Replace-WebViewLauncherPart $source 'internal NativeDesktopProcess(IntPtr desktopHandle, IntPtr processHandle, int processId, string desktopName)' 'internal NativeDesktopProcess(IntPtr desktopHandle, IntPtr processHandle, int processId, string desktopName, IntPtr jobHandle)'
    $source = Replace-WebViewLauncherPart $source '_processHandle = processHandle;' '_processHandle = processHandle; _jobHandle = jobHandle;'
    $jobMethods = @'
        // Job 句柄只归启动器所有，返回的计数不会受 PID 重用影响。
        public JobSnapshot GetJobSnapshot() { return ProbeJob.Query(_jobHandle); }
        // 只终止与候选首线程一起绑定的进程集合。
        public void TerminateJob(uint code) { ProbeJob.Terminate(_jobHandle, code); }
'@
    $source = Replace-WebViewLauncherPart $source '// Keeps the private desktop alive until its only child process has exited, then closes both owned handles.' ($jobMethods + "`n" + '// Keeps the private desktop alive until its only child process has exited, then closes both owned handles.')
    $source = Replace-WebViewLauncherPart $source 'throw new InvalidOperationException("The private desktop cannot close while its acceptance process is running.");' 'throw new InvalidOperationException("The private desktop cannot close while its acceptance process is running."); if (_jobHandle != IntPtr.Zero && ProbeJob.Query(_jobHandle).ActiveProcesses != 0) throw new InvalidOperationException("The acceptance job still has live processes.");'
    $source = Replace-WebViewLauncherPart $source '_disposed = _processHandle == IntPtr.Zero && _desktopHandle == IntPtr.Zero;' 'if (_jobHandle != IntPtr.Zero) { if (!NativeDesktopLauncher.CloseHandle(_jobHandle)) { if (firstError == 0) firstError = Marshal.GetLastWin32Error(); } else _jobHandle = IntPtr.Zero; } _disposed = _processHandle == IntPtr.Zero && _desktopHandle == IntPtr.Zero && _jobHandle == IntPtr.Zero;'
    $source = Replace-WebViewLauncherPart $source 'ProcessInformation processInformation = new ProcessInformation();' 'ProcessInformation processInformation = new ProcessInformation(); IntPtr job = IntPtr.Zero;'
    $source = Replace-WebViewLauncherPart $source 'CreateNoWindow | ExtendedStartupInfoPresent | CreateUnicodeEnvironment, environment' 'CreateNoWindow | ExtendedStartupInfoPresent | CreateUnicodeEnvironment | 0x00000004, environment'
    $source = Replace-WebViewLauncherPart $source 'processInformation = info;' 'processInformation = info; job = ProbeJob.CreateAndAssign(processInformation.Process); if (ProbeJob.ResumeThread(processInformation.Thread) == uint.MaxValue) throw new Win32Exception(Marshal.GetLastWin32Error(), "恢复验收进程失败");'
    $source = Replace-WebViewLauncherPart $source 'new NativeDesktopProcess(desktop, processInformation.Process, unchecked((int)processInformation.ProcessId), desktopName)' 'new NativeDesktopProcess(desktop, processInformation.Process, unchecked((int)processInformation.ProcessId), desktopName, job)'
    $source = Replace-WebViewLauncherPart $source 'desktopTransferred = true;' 'desktopTransferred = true; job = IntPtr.Zero;'
    $source = Replace-WebViewLauncherPart $source 'if (!desktopTransferred)' 'string jobFailure = job == IntPtr.Zero ? null : ProbeJob.Abort(job); if (!desktopTransferred)'
    $source = Replace-WebViewLauncherPart $source 'CloseDesktop(desktop);' 'CloseDesktop(desktop); if (jobFailure != null) throw new InvalidOperationException(jobFailure);'
    Add-Type -TypeDefinition ($source + "`n" + $jobSource) -Language CSharp
}
if (-not ('Kerminal.WebViewProbe.Capture' -as [type])) { Add-Type -TypeDefinition $captureSource -Language CSharp }
$previousTheme = [Environment]::GetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME','Process')
$previousBackground = [Environment]::GetEnvironmentVariable('KERMINAL_WEBVIEW_BACKGROUND','Process')
[Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_BACKGROUND','1','Process')
$failures = [Collections.Generic.List[string]]::new()
try {
    foreach ($theme in @('light','dark','system')) {
        $output = Join-Path $OutputRoot $theme
        $desktop = 'Default'
        [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME',$theme,'Process')
        $process = $null
        $normalExitJob = $null
        try {
            $process = [Kerminal.WebViewProbe.NativeDesktopLauncher]::Start($executable,$repositoryRoot,$output,(Join-Path $OutputRoot "$theme.stdout.log"),(Join-Path $OutputRoot "$theme.stderr.log"),$desktop,1024,720)
            $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
            foreach ($phase in @('ready','menu','dialog')) {
                $markerPath = Join-Path $output "$phase.marker.json"
                while (-not (Test-Path -LiteralPath $markerPath -PathType Leaf)) {
                    if ($process.WaitForExit(0)) { throw "候选提前退出：$theme/$phase" }
                    if ([DateTime]::UtcNow -ge $deadline) { throw "等待截图阶段超时：$theme/$phase" }
                    Start-Sleep -Milliseconds 100
                }
                $marker = Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json
                if ($marker.processId -ne $process.ProcessId -or $marker.phase -cne $phase -or $marker.backgroundSafety.foregroundWindowPid -eq $process.ProcessId -or $marker.backgroundSafety.uiThreadDesktopName -cne $desktop) { throw '截图阶段或后台窗口归属无效' }
                $bmp = Join-Path $output "$phase.bmp"
                $valid = $false
                $capture = $null
                $native = $null
                $captureError = $null
                try {
                    $native = Get-Content -LiteralPath (Join-Path $output "$phase.wgc.json") -Raw | ConvertFrom-Json
                    if (-not $native.captured) { throw "WGC 捕获失败：$($native.error)" }
                    if ($native.mode -cne 'windows-graphics-capture' -or $native.processId -ne $process.ProcessId -or [uint64]$native.parentWindow -ne [uint64]$marker.snapshot.native_parent_id -or $native.file -cne "$phase.bmp") { throw '系统帧来源与当前候选窗口不一致' }
                    $capture = [Kerminal.WebViewProbe.Capture]::Inspect($bmp,($phase -eq 'dialog'))
                    $overlay = $phase -ne 'ready'
                    $valid = $capture.PagePixels -ge 2000 -and $capture.ProofPixels -ge 500 -and (($overlay -and $capture.OverlayPixels -ge 100) -or (-not $overlay -and $capture.OverlayPixels -eq 0))
                    $bitmap = [Drawing.Bitmap]::FromFile($bmp)
                    try { $bitmap.Save((Join-Path $output "$phase.png"),[Drawing.Imaging.ImageFormat]::Png) } finally { $bitmap.Dispose() }
                    if (-not $valid) { $captureError = 'WGC 单帧缺少所需的网页细节或浮层标记' }
                } catch { $captureError = $_.Exception.Message; $valid = $false }
                if (-not $valid) { $failures.Add("$theme/$phase : $captureError") }
                $record = @{processId=$process.ProcessId;phase=$phase;processed=$true;captured=$valid;source='windows-graphics-capture';native=$native;evidence=$capture;error=$captureError}
                [IO.File]::WriteAllText((Join-Path $output "$phase.capture.json"),($record | ConvertTo-Json -Depth 8))
                [IO.File]::WriteAllText((Join-Path $output "$phase.ack.json"),($record | ConvertTo-Json -Depth 8))
            }
            while (-not $process.WaitForExit(1000)) {
                if ([DateTime]::UtcNow -ge $deadline) { throw "候选进程超时：$theme" }
            }
            if ($process.GetExitCode() -ne 0) { throw "候选进程失败：$theme" }
            $report = Get-Content -LiteralPath (Join-Path $output 'report.json') -Raw | ConvertFrom-Json
            if (-not $report.ok -or $report.processId -ne $process.ProcessId -or $report.candidateSha256 -cne $candidateHash -or $report.nativeIsolation.mode -cne 'background-composition' -or $report.nativeIsolation.isolationProven -or $report.nativeIsolation.uiThreadDesktopName -cne $desktop) { throw '探针报告与隔离桌面或候选不一致' }
            $settleDeadline = [DateTime]::UtcNow.AddSeconds(10)
            $normalExitJob = $process.GetJobSnapshot()
            while ($normalExitJob.ActiveProcesses -gt 0 -and [DateTime]::UtcNow -lt $settleDeadline) {
                Start-Sleep -Milliseconds 100
                $normalExitJob = $process.GetJobSnapshot()
            }
            if ($normalExitJob.ActiveProcesses -gt 0) { throw "候选 Job 后代未自然退出：$theme" }
        } catch {
            $failures.Add("$theme : $($_.Exception.Message)")
        } finally {
            if ($null -ne $process) {
                $jobBefore = $null
                $jobAfter = $null
                $candidateExited = $false
                $desktopClosed = $false
                $cleanupError = $null
                try {
                    $jobBefore = $process.GetJobSnapshot()
                    Stop-OwnedProbe $process
                    $jobAfter = $process.GetJobSnapshot()
                    $candidateExited = $process.WaitForExit(0)
                } catch {
                    $cleanupError = $_.Exception.Message
                    $failures.Add("$theme 清理 : $cleanupError")
                }
                try {
                    $process.Dispose()
                    $desktopClosed = $process.DesktopClosed
                    if (-not $desktopClosed) { throw '私有桌面句柄未关闭' }
                } catch {
                    $cleanupError = $_.Exception.Message
                    $failures.Add("$theme 句柄清理 : $cleanupError")
                }
                if (Test-Path -LiteralPath $output -PathType Container) {
                    $cleanup = @{jobScope='candidate-and-inherited-descendants';attachedBeforeResume=$true;normalExitJob=$normalExitJob;beforeForcedCleanup=$jobBefore;afterForcedCleanup=$jobAfter;candidateExited=$candidateExited;desktopClosed=$desktopClosed;error=$cleanupError}
                    [IO.File]::WriteAllText((Join-Path $output 'cleanup.json'),($cleanup | ConvertTo-Json -Depth 6))
                }
            }
        }
    }
    if ($failures.Count) {
        [IO.File]::WriteAllText((Join-Path $OutputRoot 'failures.json'),($failures | ConvertTo-Json))
        throw ('WebView 原生验收未通过：' + ($failures -join '; '))
    }
    if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidateHash) { throw '验收期间候选文件发生变化' }
    Write-Output "WebView 原生验收证据：$OutputRoot"
} finally {
    [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME',$previousTheme,'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_BACKGROUND',$previousBackground,'Process')
}
