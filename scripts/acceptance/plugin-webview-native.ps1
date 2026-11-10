# @author kongweiguang
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$PluginPath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('light','dark','system','all')][string]$Theme='all',
    [ValidateRange(60,900)][int]$TimeoutSeconds=300
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Get-PluginStrictBoolean {
    # ConvertFrom-Json 的正常布尔值只能是 bool；字符串和数字不能被成功判定隐式转换。
    # @author kongweiguang
    param($Value,[string]$Field)
    if($Value -isnot [bool]){throw "Invalid boolean field: $Field"}
    return [bool]$Value
}

function Assert-PluginBooleanIs {
    # 期望值也必须精确匹配，避免 "false" 之类的非空字符串被当成 true。
    # @author kongweiguang
    param($Value,[string]$Field,[bool]$Expected)
    $actual=Get-PluginStrictBoolean -Value $Value -Field $Field
    if($actual -ne $Expected){throw "Unexpected boolean field: $Field"}
}

function Get-PluginStrictInteger {
    # 只接受 ConvertFrom-Json 产生的 Int32/Int64；拒绝数字字符串、浮点、NaN、缺失和越界。
    # @author kongweiguang
    param($Value,[string]$Field,[long]$Minimum=0,[long]$Maximum=[long]::MaxValue)
    if($Value -isnot [int] -and $Value -isnot [long]){throw "Invalid integer field: $Field"}
    $number=[long]$Value
    if($number -lt $Minimum -or $number -gt $Maximum){throw "Out of range integer field: $Field"}
    return $number
}

function Get-PluginStrictProcessId {
    # 共享启动器用 C# int 保存候选 PID；高 32 位 PID 已在其启动校验中失败，这里不扩大范围掩盖该限制。
    # @author kongweiguang
    param($Value,[string]$Field)
    return Get-PluginStrictInteger -Value $Value -Field $Field -Minimum 1 -Maximum ([long][int]::MaxValue)
}

function Get-PluginJsonObject {
    # 工作流证据必须是显式对象；缺失字段不能按 null 或 false 通过成功判定。
    # @author kongweiguang
    param($Value,[string]$Field,[string[]]$Required=@())
    if($Value -isnot [pscustomobject]){throw "Invalid object field: $Field"}
    foreach($name in $Required){
        if($null -eq $Value.PSObject.Properties[$name]){throw "Missing object field: ${Field}.${name}"}
    }
    return $Value
}

function Get-PluginJsonObjectOrNull {
    # 部分状态字段允许 null；类型错误仍必须立即失败，不能把字符串或数组当作状态对象。
    # @author kongweiguang
    param($Value,[string]$Field,[string[]]$Required=@())
    if($null -eq $Value){return $null}
    return Get-PluginJsonObject -Value $Value -Field $Field -Required $Required
}

function Get-PluginJsonArray {
    # JSON 数组必须是真正的数组；空数组与单元素数组都要保留对象形状，不能因 PowerShell 展开而丢失。
    # @author kongweiguang
    param($Value,[string]$Field,[int]$Minimum=0)
    if($Value -isnot [array]){throw "Invalid array field: $Field"}
    $items=@($Value)
    if($items.Count -lt $Minimum){throw "Too few array items: $Field"}
    return ,$items
}

function Get-PluginJsonString {
    # 字符串字段不允许数字或布尔值隐式转换；可空字段仍需显式 null。
    # @author kongweiguang
    param($Value,[string]$Field,[switch]$AllowNull,[switch]$NonEmpty)
    if($null -eq $Value){
        if($AllowNull){return $null}
        throw "Invalid string field: $Field"
    }
    if($Value -isnot [string]){throw "Invalid string field: $Field"}
    if($NonEmpty -and [string]::IsNullOrWhiteSpace($Value)){throw "Empty string field: $Field"}
    return $Value
}

function Get-PluginJsonNumber {
    # 几何/缩放是 JSON 浮点；只接受有限数值，字符串、布尔、NaN、Infinity 和越界均拒绝。
    # @author kongweiguang
    param($Value,[string]$Field,[double]$Minimum=[double]::NegativeInfinity,[double]$Maximum=[double]::PositiveInfinity)
    if($Value -isnot [int] -and $Value -isnot [long] -and $Value -isnot [double] -and $Value -isnot [decimal] -and $Value -isnot [single]){throw "Invalid number field: $Field"}
    $number=[double]$Value
    if([double]::IsNaN($number) -or [double]::IsInfinity($number)){throw "Invalid number field: $Field"}
    if($number -lt $Minimum -or $number -gt $Maximum){throw "Out of range number field: $Field"}
    return $number
}

function Get-PluginJsonIntegerOrNull {
    # 可空计数只接受原生 Int32/Int64 或 null；数字字符串和浮点在进入比较前就被拒绝。
    # @author kongweiguang
    param($Value,[string]$Field,[long]$Minimum=0)
    if($null -eq $Value){return $null}
    return Get-PluginStrictInteger -Value $Value -Field $Field -Minimum $Minimum
}

function Get-PluginJobActiveProcesses {
    # Job 计数来自受控 C# launcher 的 uint，不是 JSON；仍需拒绝 null 或字符串参与关闭判定。
    # @author kongweiguang
    param([Parameter(Mandatory)]$Process,[string]$Stage)
    $snapshot=$Process.GetJobSnapshot()
    if($null -eq $snapshot){throw "Missing Job snapshot: $Stage"}
    if($snapshot.ActiveProcesses -isnot [uint32]){throw "Invalid Job active process count: $Stage"}
    return [uint32]$snapshot.ActiveProcesses
}

function Replace-PluginLauncherPart {
    # 每个变换点必须唯一，源启动器变更时拒绝运行而不是猜测桌面或 Job 语义。
    # @author kongweiguang
    param([string]$Source,[string]$Old,[string]$New)
    $first=$Source.IndexOf($Old,[StringComparison]::Ordinal)
    if($first -lt 0 -or $Source.IndexOf($Old,$first+$Old.Length,[StringComparison]::Ordinal) -ge 0){throw "Plugin launcher replacement changed: $Old"}
    return $Source.Remove($first,$Old.Length).Insert($first,$New)
}

function Get-PluginPrivateDesktopSource {
    # 只在原样保留 CreateDesktop、显式桌面和悬挂 Job 装配时注入碰撞预检。
    # @author kongweiguang
    param([string]$Source,[string]$JobSource)
    foreach($required in @('EntryPoint = "CreateDesktopW"','if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))','startupInfo.StartupInfo.Desktop = desktopNamePointer;','Marshal.StringToHGlobalUni("WinSta0\\" + desktopName)','ProbeJob','GetJobSnapshot')){
        if(-not $Source.Contains($required)){throw "Private desktop or suspended Job guard missing: $required"}
    }
    if(-not $JobSource.Contains('JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE') -or -not $JobSource.Contains('ProbeJob')){throw 'The inherited Job ownership contract changed.'}
    foreach($forbidden in @('OpenDesktop','SwitchDesktop','SendInput')){
        if($Source.Contains($forbidden) -or $JobSource.Contains($forbidden)){throw "Unsafe launcher operation: $forbidden"}
    }
    $marker='// Closes only the private desktop handle owned by this acceptance run.'
    $preflight=@'
        // 桌面名由本轮密码学随机 GUID 生成；已有对象或无法确认不存在时均拒绝启动。
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "OpenDesktopW")]
        private static extern IntPtr OpenPluginDesktop(string name, uint flags, bool inherit, uint access);
        // 只接受不存在对象的明确错误；拒绝访问等不确定结果不能当成新桌面。
        private static void EnsureNewPluginDesktop(string name)
        {
            IntPtr existing = OpenPluginDesktop(name, 0, false, DesktopReadObjects);
            int error = Marshal.GetLastWin32Error();
            if (existing != IntPtr.Zero)
            {
                if (!CloseDesktop(existing)) throw new Win32Exception(Marshal.GetLastWin32Error(), "Could not close the collision check desktop.");
                throw new InvalidOperationException("The acceptance desktop already exists.");
            }
            if (error != 2) throw new Win32Exception(error, "Cannot establish that the acceptance desktop is new.");
        }

'@
    $Source=Replace-PluginLauncherPart $Source $marker ($preflight+$marker)
    $create='IntPtr desktop = CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero);'
    return Replace-PluginLauncherPart $Source $create ('EnsureNewPluginDesktop(desktopName); '+$create)
}

function Get-PluginWebViewLauncherScript {
    # 根据 AST 只剔除 P0 的四个 Default 变换，保留其余完整 Job 代码和严格替换检查。
    # @author kongweiguang
    param([string]$LauncherSource,[string]$LauncherDirectory)
    $tokens=$null; $errors=$null
    $ast=[Management.Automation.Language.Parser]::ParseInput($LauncherSource,[ref]$tokens,[ref]$errors)
    if($errors.Count){throw 'Cannot parse the existing WebView launcher.'}
    $parts=[Collections.Generic.List[string]]::new()
    foreach($name in @('Get-WebViewLauncherSource','Replace-WebViewLauncherPart','Stop-OwnedProbe')){
        $matches=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name},$true))
        if($matches.Count -ne 1){throw "Launcher function is missing or ambiguous: $name"}
        $parts.Add($matches[0].Extent.Text)
    }
    $jobs=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$jobSource'},$true))
    $launch=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.StartsWith("if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher'",[StringComparison]::Ordinal)},$true))
    if($jobs.Count -ne 1 -or $launch.Count -ne 1 -or $launch[0].Clauses.Count -ne 1 -or $null -ne $launch[0].ElseClause){throw 'The complete Job launcher cannot be identified.'}
    $removals=@(
        '$source = Replace-WebViewLauncherPart $source ''if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))'' ''if (!String.Equals(desktopName, "Default", StringComparison.OrdinalIgnoreCase))''',
        '$source = Replace-WebViewLauncherPart $source ''EntryPoint = "CreateDesktopW"'' ''EntryPoint = "OpenDesktopW"''',
        '$source = Replace-WebViewLauncherPart $source ''private static extern IntPtr CreateDesktop(string desktopName, IntPtr device, IntPtr devmode, uint flags, uint desiredAccess, IntPtr securityAttributes);'' ''private static extern IntPtr OpenDesktop(string desktopName, uint flags, bool inherit, uint desiredAccess);''',
        '$source = Replace-WebViewLauncherPart $source ''CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero)'' ''OpenDesktop(desktopName, 0, false, desktopAccess)'''
    )
    $launchText=$launch[0].Extent.Text
    foreach($expected in $removals){
        $matches=@($launch[0].Clauses[0].Item2.Statements | Where-Object {$_ -is [Management.Automation.Language.AssignmentStatementAst] -and $_.Extent.Text -ceq $expected})
        if($matches.Count -ne 1){throw 'The Default desktop transformations changed or became ambiguous.'}
        $launchText=Replace-PluginLauncherPart $launchText $expected ''
    }
    foreach($forbidden in @('OpenDesktop','SwitchDesktop','SendInput','String.Equals(desktopName, "Default"')){
        if($launchText.Contains($forbidden)){throw "Unexpected desktop transformation remains: $forbidden"}
    }
    $add='Add-Type -TypeDefinition ($source + "`n" + $jobSource) -Language CSharp'
    $launchText=Replace-PluginLauncherPart $launchText $add ('$source = Get-PluginPrivateDesktopSource -Source $source -JobSource $jobSource'+"`n    "+$add)
    $parts.Add($jobs[0].Extent.Text)
    $parts.Add($launchText)
    $source=$parts -join "`n"
    $source=$source.Replace('$PSScriptRoot',("'"+$LauncherDirectory.Replace("'","''")+"'"))
    $old='--webview-probe --output '
    $new='--plugin-webview-acceptance --dev-plugin " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_PLUGIN_WEBVIEW_DIRECTORY")) + " --output-dir '
    $source=Replace-PluginLauncherPart $source $old $new
    return $source.Replace('Kerminal.WebViewProbe','Kerminal.PluginWebViewAcceptance')
}

function Initialize-PluginWebViewLauncher {
    # 只执行经结构校验的声明和原有 Job 装配，不运行 P0 验收或改变其语义。
    # @author kongweiguang
    if('Kerminal.PluginWebViewAcceptance.NativeDesktopLauncher' -as [type]){throw 'A fresh PowerShell process is required to compile the validated launcher.'}
    $path=Join-Path $PSScriptRoot 'webview-native.ps1'
    $source=Get-PluginWebViewLauncherScript -LauncherSource ([IO.File]::ReadAllText($path)) -LauncherDirectory $PSScriptRoot
    . ([scriptblock]::Create($source))
}

function Initialize-PluginIsolationProbe {
    # 脚本从内核独立读取候选线程和输入桌面，不把候选自报当成跨进程证明。
    # @author kongweiguang
    if('Kerminal.PluginWebViewAcceptance.Isolation' -as [type]){throw 'A fresh PowerShell process is required for the isolation probe.'}
    $source=@'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
namespace Kerminal.PluginWebViewAcceptance {
    public sealed class IsolationSnapshot {
        public uint ProcessId, ThreadId;
        public string ThreadDesktopName, InputDesktopName, WindowStationName;
    }
    public sealed class WindowOwner { public uint ProcessId, ThreadId; }
    public sealed class PagePixels { public int Width, Height; public long Body, Proof; }
    public static class Isolation {
        [DllImport("user32.dll", SetLastError=true)] static extern IntPtr GetThreadDesktop(uint id);
        [DllImport("user32.dll", SetLastError=true)] static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
        [DllImport("user32.dll", SetLastError=true)] static extern IntPtr GetProcessWindowStation();
        [DllImport("user32.dll", SetLastError=true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
        [DllImport("user32.dll", SetLastError=true)] static extern bool CloseDesktop(IntPtr desktop);
        [DllImport("user32.dll", EntryPoint="GetUserObjectInformationW", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool GetUserObjectInformation(IntPtr handle, int index, StringBuilder value, uint length, out uint needed);
        [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenThread(uint access, bool inherit, uint id);
        [DllImport("kernel32.dll", SetLastError=true)] static extern uint GetProcessIdOfThread(IntPtr thread);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr handle);
        // 固定上限和非空校验避免未知桌面名称被归一化成安全值。
        static string Name(IntPtr handle) {
            if(handle==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(),"Missing user object");
            var buffer=new StringBuilder(4096); uint needed;
            if(!GetUserObjectInformation(handle,2,buffer,8192,out needed)) throw new Win32Exception(Marshal.GetLastWin32Error(),"GetUserObjectInformationW failed");
            string result=buffer.ToString();
            if(String.IsNullOrWhiteSpace(result) || needed>8192) throw new InvalidOperationException("Invalid user object name");
            return result;
        }
        // 输入桌面句柄只用于读取名称，始终关闭且不进行桌面切换。
        public static string InputDesktop() {
            IntPtr desktop=OpenInputDesktop(0,false,0x0001);
            if(desktop==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(),"OpenInputDesktop failed");
            try { return Name(desktop); }
            finally { if(!CloseDesktop(desktop)) throw new Win32Exception(Marshal.GetLastWin32Error(),"CloseDesktop failed"); }
        }
        // 只读取候选父窗的归属，不能以不同进程或线程的系统帧代替本轮窗口。
        public static WindowOwner Owner(ulong window) {
            uint pid; uint tid=GetWindowThreadProcessId(new IntPtr(unchecked((long)window)),out pid);
            if(tid==0 || pid==0) throw new Win32Exception(Marshal.GetLastWin32Error(),"GetWindowThreadProcessId failed");
            return new WindowOwner { ProcessId=pid, ThreadId=tid };
        }
        // 独立检查同一 BMP 的像素，网页标记只证明页面内容，不能冒充正式设置遮罩。
        public static PagePixels InspectPage(byte[] bytes) {
            if(bytes==null || bytes.Length<54 || bytes[0]!=0x42 || bytes[1]!=0x4d ||
               BitConverter.ToInt32(bytes,14)!=40 || BitConverter.ToInt16(bytes,28)!=32 || BitConverter.ToInt32(bytes,30)!=0)
                throw new InvalidOperationException("Invalid 32-bit BMP");
            int offset=BitConverter.ToInt32(bytes,10), width=BitConverter.ToInt32(bytes,18), rawHeight=BitConverter.ToInt32(bytes,22);
            if(offset<54 || width<=0 || width>8192 || rawHeight==0 || rawHeight < -8192 || rawHeight>8192)
                throw new InvalidOperationException("Invalid BMP dimensions");
            int height=Math.Abs(rawHeight); long end=(long)offset+(long)width*height*4;
            if(end>bytes.LongLength) throw new InvalidOperationException("Incomplete BMP pixels");
            long body=0,proof=0;
            for(long i=offset;i<end;i+=4) {
                int at=(int)i;
                bool isBody=Math.Abs(bytes[at]-86)<=3 && Math.Abs(bytes[at+1]-52)<=3 && Math.Abs(bytes[at+2]-18)<=3;
                if(isBody) body++;
                else if(Math.Abs(bytes[at]-120)<=3 && Math.Abs(bytes[at+1]-207)<=3 && Math.Abs(bytes[at+2]-37)<=3) proof++;
            }
            return new PagePixels { Width=width, Height=height, Body=body, Proof=proof };
        }
        // 持有线程句柄至 PID、实际桌面和输入桌面均读完，避免线程号复用影响同次采样。
        public static IsolationSnapshot Capture(uint threadId) {
            IntPtr thread=OpenThread(0x0800,false,threadId);
            if(thread==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(),"OpenThread failed");
            try {
                uint pid=GetProcessIdOfThread(thread);
                if(pid==0) throw new Win32Exception(Marshal.GetLastWin32Error(),"GetProcessIdOfThread failed");
                return new IsolationSnapshot { ProcessId=pid, ThreadId=threadId, ThreadDesktopName=Name(GetThreadDesktop(threadId)), InputDesktopName=InputDesktop(), WindowStationName=Name(GetProcessWindowStation()) };
            } finally { if(!CloseHandle(thread)) throw new Win32Exception(Marshal.GetLastWin32Error(),"CloseHandle failed"); }
        }
    }
}
'@
    Add-Type -TypeDefinition $source -Language CSharp
}

function Assert-PluginIsolationEvidence {
    # 不同阶段都核对脚本的 Win32 采样与候选原始基线；缺字段和类型混淆必须失败。
    # @author kongweiguang
    param($Evidence,$Native,[string]$Stage,[int]$ProcessId,[string]$Desktop,[string]$InputDesktop,[string]$Candidate,[uint32]$BaselineThreadId)
    if($null -eq $Evidence -or $null -eq $Evidence.nativeIsolation){throw 'Isolation evidence is missing.'}
    $isolation=$Evidence.nativeIsolation
    $evidenceProcess=Get-PluginStrictProcessId -Value $Evidence.processId -Field 'evidence.processId'
    $isolationProcess=Get-PluginStrictProcessId -Value $isolation.processId -Field 'isolation.processId'
    $isolationThread=Get-PluginStrictInteger -Value $isolation.uiThreadId -Field 'isolation.uiThreadId' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
    $platformSupported=Get-PluginStrictBoolean -Value $isolation.platformSupported -Field 'isolation.platformSupported'
    $isolationProven=Get-PluginStrictBoolean -Value $isolation.isolationProven -Field 'isolation.isolationProven'
    if($Evidence.stage -isnot [string] -or $Evidence.stage -cne $Stage -or
       $evidenceProcess -ne $ProcessId -or
       $Evidence.candidateSha256 -isnot [string] -or $Evidence.candidateSha256 -cne $Candidate -or
       $platformSupported -ne $true -or $isolationProven -ne $true -or
       $isolationProcess -ne $ProcessId -or
       $isolation.uiThreadDesktopName -isnot [string] -or $isolation.uiThreadDesktopName -cne $Desktop -or
       $isolation.inputDesktopName -isnot [string] -or [string]::IsNullOrWhiteSpace($isolation.inputDesktopName) -or
       $isolation.inputDesktopName -cne $InputDesktop -or $Desktop.Equals($InputDesktop,[StringComparison]::OrdinalIgnoreCase) -or
       $Native.ProcessId -ne $ProcessId -or $Native.ThreadId -ne $isolationThread -or
       $Native.ThreadDesktopName -cne $Desktop -or $Native.InputDesktopName -cne $InputDesktop -or
       $Native.WindowStationName -cne 'WinSta0' -or ($BaselineThreadId -ne 0 -and $isolationThread -ne $BaselineThreadId)){
        throw "Private desktop, input desktop, thread, PID or candidate mismatch: $Stage"
    }
}

function Assert-PluginLiveCandidate {
    # 使用启动器持有的进程号回读实际映像路径及摘要；退出或身份变化不得使用旧回执。
    # @author kongweiguang
    param([int]$ProcessId,[string]$Executable,[string]$Candidate)
    $live=Get-Process -Id $ProcessId -ErrorAction Stop
    if([string]::IsNullOrWhiteSpace($live.Path) -or -not [IO.Path]::GetFullPath($live.Path).Equals($Executable,[StringComparison]::OrdinalIgnoreCase) -or
       (Get-FileHash -LiteralPath $live.Path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Candidate){throw 'The live candidate image does not match the launched executable.'}
}

function Get-PluginHostPlatform {
    # Sidecar 平台必须由实际运行时 OS/架构推导；未知组合不能接受候选自报值。
    # @author kongweiguang
    $arch=[System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
    if([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)){
        if($arch -eq [System.Runtime.InteropServices.Architecture]::X64){return 'windows-x86_64'}
        if($arch -eq [System.Runtime.InteropServices.Architecture]::Arm64){return 'windows-aarch64'}
    }
    if([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Linux)){
        if($arch -eq [System.Runtime.InteropServices.Architecture]::X64){return 'linux-x86_64'}
        if($arch -eq [System.Runtime.InteropServices.Architecture]::Arm64){return 'linux-aarch64'}
    }
    if([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)){
        if($arch -eq [System.Runtime.InteropServices.Architecture]::X64){return 'macos-x86_64'}
        if($arch -eq [System.Runtime.InteropServices.Architecture]::Arm64){return 'macos-aarch64'}
    }
    throw 'Unsupported plugin host platform.'
}

function Get-PluginPackageFiles {
    # 独立枚举插件根实际文件集合，拒绝重解析点，供与候选冻结清点交叉核对。
    # @author kongweiguang
    param([string]$Root)
    $rootPath=[IO.Path]::GetFullPath($Root)
    $rootAttributes=[IO.File]::GetAttributes($rootPath)
    if(($rootAttributes -band [IO.FileAttributes]::Directory) -eq 0 -or ($rootAttributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'Invalid plugin package root.'}
    $files=[Collections.Generic.List[object]]::new()
    $pending=[Collections.Generic.Stack[string]]::new()
    $pending.Push($rootPath)
    while($pending.Count -gt 0){
        $directory=$pending.Pop()
        foreach($entry in [IO.Directory]::EnumerateFileSystemEntries($directory)){
            $attributes=[IO.File]::GetAttributes($entry)
            if(($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'Plugin package contains a reparse point.'}
            if(($attributes -band [IO.FileAttributes]::Directory) -ne 0){$pending.Push([string]$entry);continue}
            $relative=[IO.Path]::GetRelativePath($rootPath,[string]$entry)
            if([string]::IsNullOrWhiteSpace($relative) -or $relative -ceq '..' -or $relative.StartsWith('..'+[IO.Path]::DirectorySeparatorChar,[StringComparison]::Ordinal)){throw 'Plugin package path escapes its root.'}
            [void]$files.Add([pscustomobject]@{path=$relative.Replace([IO.Path]::DirectorySeparatorChar,'/');size=([IO.FileInfo]::new($entry)).Length})
        }
    }
    return $files.ToArray()
}

function Assert-PluginPackageFiles {
    # 冻结清点必须覆盖实际插件目录的每个文件及大小；只比较候选资源数量不能代替此检查。
    # @author kongweiguang
    param($Reported,$Actual)
    $reported=@($Reported); $actual=@($Actual)
    if($reported.Count -le 0 -or $reported.Count -ne $actual.Count){throw 'Frozen package inventory is absent or incomplete.'}
    $sizes=[Collections.Generic.Dictionary[string,uint64]]::new([StringComparer]::Ordinal)
    foreach($file in $reported){
        if($file.path -isnot [string] -or [string]::IsNullOrWhiteSpace($file.path) -or $sizes.ContainsKey($file.path)){throw 'Invalid frozen package inventory entry.'}
        $size=Get-PluginStrictInteger -Value $file.size -Field 'packageFiles.size' -Minimum 0
        $sizes.Add($file.path,[uint64]$size)
    }
    foreach($file in $actual){
        if($file.path -isnot [string] -or -not $sizes.ContainsKey($file.path) -or $sizes[$file.path] -ne [uint64]$file.size){throw 'Actual plugin package differs from its frozen inventory.'}
        [void]$sizes.Remove($file.path)
    }
    if($sizes.Count -ne 0){throw 'Frozen package inventory contains files missing from the actual package.'}
}

function Test-PluginWebViewResource {
    # 与宿主 WebView 资源白名单保持一致；不接受任意扩展名或路径回退。
    # @author kongweiguang
    param([string]$Path)
    return [bool]($Path -match '\.(?:html|htm|js|mjs|css|json|png|jpg|jpeg|gif|webp|ico|svg|woff|woff2|ttf|otf)$')
}

function Assert-PluginResourceCompleteness {
    # 从实际包清点和清单声明推导期望资源集合，避免候选资源列表自证完整。
    # @author kongweiguang
    param($Manifest,$PackageFiles,$Resources)
    $files=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($file in @($PackageFiles)){
        if($file.path -isnot [string] -or -not $files.Add($file.path)){throw 'Invalid actual package inventory for resource completeness.'}
    }
    $viewsProperty=$Manifest.PSObject.Properties['views']
    if($null -eq $viewsProperty -or $viewsProperty.Value -isnot [pscustomobject]){throw 'Plugin manifest has no unambiguous views declaration.'}
    $webview=$false
    foreach($view in $viewsProperty.Value.PSObject.Properties){
        $kind=$view.Value.PSObject.Properties['kind']
        if($null -eq $kind -or $kind.Value -isnot [string]){throw 'Plugin view declaration is invalid.'}
        if($kind.Value -ceq 'native'){throw 'Native view resource completeness is not independently proven.'}
        if($kind.Value -ceq 'webview'){$webview=$true}
    }
    if(-not $webview){throw 'Plugin manifest does not declare a WebView resource set.'}
    $expected=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($path in $files){if(Test-PluginWebViewResource -Path $path){[void]$expected.Add($path)}}
    $icon=$Manifest.PSObject.Properties['icon']
    if($null -ne $icon -and $icon.Value -is [string] -and $files.Contains($icon.Value)){[void]$expected.Add($icon.Value)}
    $contributes=$Manifest.PSObject.Properties['contributes']
    if($null -ne $contributes -and $contributes.Value -is [pscustomobject]){
        $tools=$contributes.Value.PSObject.Properties['tools']
        if($null -ne $tools){
            foreach($tool in @($tools.Value)){
                if($tool -isnot [pscustomobject]){throw 'Plugin tool declaration is invalid.'}
                $toolIcon=$tool.PSObject.Properties['icon']
                if($null -ne $toolIcon -and $toolIcon.Value -is [string] -and $files.Contains($toolIcon.Value)){[void]$expected.Add($toolIcon.Value)}
            }
        }
    }
    $reported=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($resource in @($Resources)){
        if($resource.path -isnot [string] -or -not $reported.Add($resource.path)){throw 'Invalid frozen plugin resource set.'}
    }
    if($expected.Count -ne $reported.Count){throw 'Frozen plugin resource set does not match the actual package declarations.'}
    foreach($path in $expected){if(-not $reported.Contains($path)){throw 'Frozen plugin resource set is missing a declared WebView resource.'}}
}

function Resolve-PluginEvidenceFile {
    # 只读取插件根内普通文件，拒绝相对路径别名及任何重解析点。
    # @author kongweiguang
    param([string]$Root,[string]$Relative)
    if([string]::IsNullOrWhiteSpace($Relative) -or [IO.Path]::IsPathRooted($Relative) -or $Relative -match '[\\:\x00-\x1f]' ){throw 'Invalid plugin evidence path.'}
    $segments=$Relative.Split('/')
    foreach($segment in $segments){
        if([string]::IsNullOrWhiteSpace($segment) -or $segment -ceq '.' -or $segment -ceq '..' -or $segment.EndsWith('.') -or $segment.EndsWith(' ')){throw 'Invalid plugin evidence path segment.'}
    }
    $current=$Root
    for($i=0;$i -lt $segments.Length;$i++){
        $current=Join-Path $current $segments[$i]
        $attributes=[IO.File]::GetAttributes($current)
        if(($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'Plugin evidence path contains a reparse point.'}
        $isDirectory=($attributes -band [IO.FileAttributes]::Directory) -ne 0
        if(($i -lt $segments.Length-1 -and -not $isDirectory) -or ($i -eq $segments.Length-1 -and $isDirectory)){throw 'Plugin evidence path has the wrong file type.'}
    }
    return $current
}

function Assert-PluginIdentityData {
    # 用冻结资源摘要重算插件身份，并独立核对实际包清点、资源完整集合、宿主平台和 Sidecar 文件证据。
    # @author kongweiguang
    param($Identity,$Manifest,$ManifestHash,$Observations,$PackageFiles,[string]$HostPlatform,[string]$SidecarHash,[int]$ProcessId,[string]$Theme,[string]$Candidate,$Baseline)
    $schema=Get-PluginStrictInteger -Value $Identity.schemaVersion -Field 'identity.schemaVersion' -Minimum 1 -Maximum 1
    $identityProcess=Get-PluginStrictProcessId -Value $Identity.processId -Field 'identity.processId'
    $manifestVerified=Get-PluginStrictBoolean -Value $Identity.manifestSemanticsVerified -Field 'identity.manifestSemanticsVerified'
    $resourceComplete=Get-PluginStrictBoolean -Value $Identity.resourceSetComplete -Field 'identity.resourceSetComplete'
    $sidecarProcessBound=Get-PluginStrictBoolean -Value $Identity.sidecarProcessBound -Field 'identity.sidecarProcessBound'
    if($schema -ne 1 -or $Identity.algorithm -isnot [string] -or $Identity.algorithm -cne 'kerminal-webview-plugin-v1' -or
       $identityProcess -ne $ProcessId -or $Identity.theme -isnot [string] -or $Identity.theme -cne $Theme -or
       $Identity.candidateSha256 -isnot [string] -or $Identity.candidateSha256 -cne $Candidate -or
       $Identity.pluginId -isnot [string] -or $Identity.pluginId -cne 'kerminal.file-search' -or $Identity.pluginId -cne $Manifest.id -or
       $Identity.pluginVersion -isnot [string] -or $Identity.pluginVersion -cne $Manifest.version -or $Manifest.runtime.kind -cne 'sidecar' -or
       $Identity.manifestSha256 -isnot [string] -or $Identity.manifestSha256 -cne $ManifestHash -or
       $Identity.manifestSource -isnot [string] -or $Identity.manifestSource -cne 'prepared.resources' -or
       $manifestVerified -ne $true -or $resourceComplete -ne $true -or
       $Identity.resourceSetDerivation -isnot [string] -or $Identity.resourceSetDerivation -cne 'package-files+manifest' -or
       $Identity.pluginSha256 -isnot [string] -or $Identity.pluginSha256 -cnotmatch '^[0-9a-f]{64}$' -or
       $Identity.sidecarSha256 -isnot [string] -or $Identity.sidecarSha256 -cnotmatch '^[0-9a-f]{64}$' -or $Identity.sidecarSha256 -cne $SidecarHash -or
       $Identity.sidecarEvidence -isnot [string] -or $Identity.sidecarEvidence -cne 'package-file-only' -or
       $sidecarProcessBound -ne $false -or
       $Identity.sidecarPlatform -isnot [string] -or $Identity.sidecarPlatform -cne $HostPlatform -or
       $Identity.sidecarMain -isnot [string] -or [string]::IsNullOrWhiteSpace($Identity.sidecarMain)){
        throw 'Plugin or Sidecar identity is invalid.'
    }
    $mapping=$Manifest.runtime.main.PSObject.Properties[[string]$Identity.sidecarPlatform]
    if($null -eq $mapping -or $mapping.Value -isnot [string] -or $mapping.Value -cne $Identity.sidecarMain){throw 'Sidecar identity does not match runtime.main.'}
    if(-not [BitConverter]::IsLittleEndian){throw 'Plugin digest requires little-endian integer encoding.'}
    $packageSeen=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($file in @($PackageFiles)){
        if($file.path -isnot [string] -or -not $packageSeen.Add($file.path)){throw 'Actual package inventory is invalid.'}
    }
    $resources=@($Identity.resources)
    if($resources.Count -eq 0 -or $resources.Count -ne $Observations.Count){throw 'Frozen plugin resource evidence is incomplete.'}
    $manifestResources=@($resources | Where-Object {$_.path -ceq 'kerminal-plugin.json'})
    if($manifestResources.Count -ne 1 -or $manifestResources[0].sha256 -cne $Identity.manifestSha256){throw 'Frozen manifest resource is missing or mismatched.'}
    $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $hash=[Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    try {
        $hash.AppendData([Text.Encoding]::UTF8.GetBytes("kerminal-webview-plugin-v1`0"))
        $hash.AppendData([Convert]::FromHexString($Identity.manifestSha256))
        foreach($resource in $resources){
            if($resource.path -isnot [string] -or -not $seen.Add($resource.path) -or -not $packageSeen.Contains($resource.path) -or
               $resource.sha256 -isnot [string] -or $resource.sha256 -cnotmatch '^[0-9a-f]{64}$' -or -not $Observations.ContainsKey($resource.path)) {throw 'Invalid frozen plugin resource evidence.'}
            $resourceSize=Get-PluginStrictInteger -Value $resource.size -Field 'identity.resources.size' -Minimum 0
            $actual=$Observations[$resource.path]
            if($actual.size -ne $resourceSize -or $actual.sha256 -cne $resource.sha256){throw 'Frozen plugin resource differs from its package file.'}
            $name=[Text.Encoding]::UTF8.GetBytes($resource.path)
            $hash.AppendData([BitConverter]::GetBytes([uint64]$name.Length))
            $hash.AppendData($name)
            $hash.AppendData([BitConverter]::GetBytes([uint64]$resourceSize))
            $hash.AppendData([Convert]::FromHexString($resource.sha256))
        }
        $calculated=[Convert]::ToHexString($hash.GetHashAndReset()).ToLowerInvariant()
    } finally {$hash.Dispose()}
    if($calculated -cne $Identity.pluginSha256){throw 'Frozen plugin digest is incomplete or mismatched.'}
    if($null -ne $Baseline){
        foreach($field in @('pluginId','pluginVersion','manifestSha256','manifestSource','resourceSetDerivation','pluginSha256','sidecarPlatform','sidecarMain','sidecarSha256','sidecarEvidence')){
            if($Identity.$field -cne $Baseline.$field){throw "Plugin or Sidecar changed across acceptance checkpoints: $field"}
        }
        foreach($field in @('manifestSemanticsVerified','resourceSetComplete','sidecarProcessBound')){
            $left=Get-PluginStrictBoolean -Value $Identity.$field -Field "identity.$field"
            $right=Get-PluginStrictBoolean -Value $Baseline.$field -Field "baseline.$field"
            if($left -ne $right){throw "Plugin or Sidecar changed across acceptance checkpoints: $field"}
        }
    }
}

function Read-PluginIdentity {
    # 先独立枚举实际包文件，再从冻结清单/资源摘要核对身份；磁盘清单只作为时点一致性复核。
    # @author kongweiguang
    param([string]$Output,[string]$PluginRoot,[int]$ProcessId,[string]$Theme,[string]$Candidate,$Baseline)
    $identity=Get-Content -LiteralPath (Join-Path $Output 'plugin-identity.json') -Raw | ConvertFrom-Json
    $hostPlatform=Get-PluginHostPlatform
    $actualFiles=Get-PluginPackageFiles -Root $PluginRoot
    Assert-PluginPackageFiles -Reported $identity.packageFiles -Actual $actualFiles
    $manifestPath=Resolve-PluginEvidenceFile -Root $PluginRoot -Relative 'kerminal-plugin.json'
    $manifest=Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    $manifestHash=(Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $observations=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
    foreach($resource in @($identity.resources)){
        if($resource.path -isnot [string] -or $observations.ContainsKey($resource.path)){throw 'Plugin resource path is missing or duplicated.'}
        $path=Resolve-PluginEvidenceFile -Root $PluginRoot -Relative $resource.path
        $item=Get-Item -LiteralPath $path -Force
        $observations.Add($resource.path,[pscustomobject]@{size=$item.Length;sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()})
    }
    Assert-PluginResourceCompleteness -Manifest $manifest -PackageFiles $actualFiles -Resources $identity.resources
    $sidecar=Resolve-PluginEvidenceFile -Root $PluginRoot -Relative $identity.sidecarMain
    $sidecarHash=(Get-FileHash -LiteralPath $sidecar -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-PluginIdentityData -Identity $identity -Manifest $manifest -ManifestHash $manifestHash -Observations $observations -PackageFiles $actualFiles -HostPlatform $hostPlatform -SidecarHash $sidecarHash -ProcessId $ProcessId -Theme $Theme -Candidate $Candidate -Baseline $Baseline
    return $identity
}

function Wait-PluginIsolationCheckpoint {
    # 双端逐阶段握手，候选退出、超时或证据不一致时保留文件并拒绝进入下一阶段。
    # @author kongweiguang
    param($Process,[string]$Output,[string]$Stage,[string]$Desktop,[string]$InputDesktop,[string]$Candidate,[string]$Executable,[string]$PluginRoot,[string]$Theme,$PluginBaseline,[uint32]$BaselineThreadId,[uint64]$ExpectedParent,[DateTime]$Deadline)
    $path=Join-Path $Output "isolation-$Stage.json"
    while(-not (Test-Path -LiteralPath $path -PathType Leaf)){
        if($Process.WaitForExit(0)){
            $stderr=Join-Path (Split-Path -Parent $Output) ((Split-Path -Leaf $Output)+'.stderr.log')
            throw "Candidate exited before isolation checkpoint $Stage; exitCode=$($Process.GetExitCode()); stderr=$stderr; evidence=$Output"
        }
        if([DateTime]::UtcNow -ge $Deadline){throw "Timed out waiting for isolation checkpoint: $Stage"}
        Start-Sleep -Milliseconds 70
    }
    $evidence=Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
    if($null -eq $evidence.nativeIsolation){throw "Missing native isolation: $Stage"}
    $evidenceProcess=Get-PluginStrictProcessId -Value $evidence.processId -Field 'evidence.processId'
    $evidenceThread=Get-PluginStrictInteger -Value $evidence.nativeIsolation.uiThreadId -Field 'evidence.uiThreadId' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
    if($evidenceProcess -ne $Process.ProcessId){throw "Invalid candidate thread: $Stage"}
    Assert-PluginLiveCandidate -ProcessId $Process.ProcessId -Executable $Executable -Candidate $Candidate
    if($env:KERMINAL_WEBVIEW_OFFSCREEN_ONLY -ceq '1'){
        # 离屏模式只核对候选自报的隔离与三主题状态；脚本不再读取候选线程桌面。
        $native=[pscustomobject]@{ThreadId=$evidenceThread}
    } else {
        $native=[Kerminal.PluginWebViewAcceptance.Isolation]::Capture([uint32]$evidenceThread)
        Assert-PluginIsolationEvidence -Evidence $evidence -Native $native -Stage $Stage -ProcessId $Process.ProcessId -Desktop $Desktop -InputDesktop $InputDesktop -Candidate $Candidate -BaselineThreadId $BaselineThreadId
    }
    if($Stage -ceq 'start' -or $Stage -ceq 'complete' -or $Stage.EndsWith('-after',[StringComparison]::Ordinal)){
        $identity=Read-PluginIdentity -Output $Output -PluginRoot $PluginRoot -ProcessId $Process.ProcessId -Theme $Theme -Candidate $Candidate -Baseline $PluginBaseline
        $evidence | Add-Member -NotePropertyName pluginIdentity -NotePropertyValue $identity
    }
    if($Stage.EndsWith('-after',[StringComparison]::Ordinal)){
        $phase=$Stage.Substring(0,$Stage.Length-6)
        $capture=Get-Content -LiteralPath (Join-Path $Output "$phase.wgc.json") -Raw | ConvertFrom-Json
        $captured=Get-PluginStrictBoolean -Value $capture.captured -Field "capture.$phase.captured"
        if($captured){
            $captureProcess=Get-PluginStrictProcessId -Value $capture.processId -Field "capture.$phase.processId"
            $parent=Get-PluginStrictInteger -Value $capture.parentWindow -Field "capture.$phase.parentWindow" -Minimum 1
            if($capture.mode -isnot [string] -or $capture.mode -cne 'windows-graphics-capture' -or $captureProcess -ne $Process.ProcessId -or ($ExpectedParent -ne 0 -and $parent -ne $ExpectedParent)){throw "Invalid system frame origin: $phase"}
            $owner=[Kerminal.PluginWebViewAcceptance.Isolation]::Owner([uint64]$parent)
            if($owner.ProcessId -ne $Process.ProcessId -or $owner.ThreadId -ne $native.ThreadId){throw "System frame parent is not the candidate UI window: $phase"}
            if($phase -ceq 'search'){Assert-PluginWebViewFrame -Output $Output -Phase $phase -ProcessId $Process.ProcessId -ParentWindow ([uint64]$parent)}
        }
    }
    $ack=[ordered]@{stage=$Stage;processId=$Process.ProcessId;candidateSha256=$Candidate;desktopName=$Desktop;inputDesktopName=$InputDesktop;uiThreadId=$native.ThreadId}
    $temporary=Join-Path $Output "isolation-$Stage.ack.tmp"
    $destination=Join-Path $Output "isolation-$Stage.ack.json"
    [IO.File]::WriteAllText($temporary,($ack | ConvertTo-Json -Compress),[Text.UTF8Encoding]::new($false))
    [IO.File]::Move($temporary,$destination)
    return $evidence
}

function Stop-PluginOwnedProcess {
    # Job 查询失败仍尝试只终止本轮 Job，并分别记录查询、终止、等待与句柄释放的所有错误。
    # @author kongweiguang
    param([Parameter(Mandatory)]$Process)
    $errors=[Collections.Generic.List[Exception]]::new()
    $active=$null
    try {$active=Get-PluginJobActiveProcesses -Process $Process -Stage 'cleanup-query'} catch {$errors.Add($_.Exception)}
    if($null -eq $active -or $active -gt 0){
        try {$Process.TerminateJob(1)} catch {$errors.Add($_.Exception)}
    }
    try {
        if(-not $Process.WaitForExit(10000)){throw 'The owned candidate did not terminate.'}
        $deadline=[DateTime]::UtcNow.AddSeconds(10)
        while((Get-PluginJobActiveProcesses -Process $Process -Stage 'cleanup-wait') -gt 0 -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
        if((Get-PluginJobActiveProcesses -Process $Process -Stage 'cleanup-final') -gt 0){throw 'The owned Job still has live descendants.'}
    } catch {$errors.Add($_.Exception)}
    try {$Process.Dispose()} catch {$errors.Add($_.Exception)}
    if($errors.Count -eq 1){throw $errors[0]}
    if($errors.Count -gt 1){throw [AggregateException]::new('Owned candidate cleanup had multiple failures.',$errors)}
}

function Assert-PluginOcclusionProof {
    # 无真实正样本时通知和设置均不可放行；合法预览退化也只标记证据不足而非产品故障。
    # @author kongweiguang
    param([ValidateSet('notification','dialog')][string]$Phase)
    throw "The $Phase overlay and page were not proven in the same system frame; fallback can be valid and original evidence is retained."
}

function Assert-PluginWebViewFrame {
    # 系统帧只证明其捕获来源；没有可靠的正式设置同帧识别器时始终拒绝 dialog。
    # @author kongweiguang
    param([string]$Output,[string]$Phase,[int]$ProcessId,[uint64]$ParentWindow)
    $capture=Get-Content -LiteralPath (Join-Path $Output "$Phase.wgc.json") -Raw | ConvertFrom-Json
    $captured=Get-PluginStrictBoolean -Value $capture.captured -Field "capture.$Phase.captured"
    $captureProcess=Get-PluginStrictProcessId -Value $capture.processId -Field "capture.$Phase.processId"
    $parent=Get-PluginStrictInteger -Value $capture.parentWindow -Field "capture.$Phase.parentWindow" -Minimum 1
    $width=Get-PluginStrictInteger -Value $capture.width -Field "capture.$Phase.width" -Minimum 1 -Maximum 8192
    $height=Get-PluginStrictInteger -Value $capture.height -Field "capture.$Phase.height" -Minimum 1 -Maximum 8192
    $fixture=$capture.fixturePixels
    if($null -eq $fixture){throw "Missing fixture pixels: $Phase"}
    $fixtureBody=Get-PluginStrictInteger -Value $fixture.body -Field "capture.$Phase.fixturePixels.body" -Minimum 0
    $fixtureProof=Get-PluginStrictInteger -Value $fixture.proof -Field "capture.$Phase.fixturePixels.proof" -Minimum 0
    if(-not $captured -or $capture.mode -isnot [string] -or $capture.mode -cne 'windows-graphics-capture' -or
       $captureProcess -ne $ProcessId -or $parent -ne $ParentWindow){ throw "Invalid WGC origin: $Phase" }
    $bmp=Join-Path $Output "$Phase.bmp"
    $bytes=[IO.File]::ReadAllBytes($bmp)
    if ($bytes.Length -lt 54 -or $bytes[0] -ne 0x42 -or $bytes[1] -ne 0x4d -or [BitConverter]::ToInt32($bytes,18) -ne $width -or [Math]::Abs([BitConverter]::ToInt32($bytes,22)) -ne $height) { throw "Incomplete system frame: $Phase" }
    if ($Phase -ceq 'search') {
        $pixels=[Kerminal.PluginWebViewAcceptance.Isolation]::InspectPage($bytes)
        if($pixels.Width -ne $width -or $pixels.Height -ne $height -or $pixels.Body -ne $fixtureBody -or $pixels.Proof -ne $fixtureProof -or $pixels.Body -lt 2000 -or $pixels.Proof -lt 500){throw 'The system BMP did not independently confirm the injected page proof marker.'}
    }
    if ($Phase -in @('notification','dialog')) { Assert-PluginOcclusionProof -Phase $Phase }
    $image=[Drawing.Bitmap]::FromFile($bmp)
    try { $image.Save((Join-Path $Output "$Phase.png"),[Drawing.Imaging.ImageFormat]::Png) } finally { $image.Dispose() }
}

function Assert-PluginWebViewBounds {
    # 页面矩形必须完整；缺失边界会让“仍落在启动窗口内”的状态无法证明。
    # @author kongweiguang
    param($Bounds,[string]$Field)
    $Bounds=Get-PluginJsonObject -Value $Bounds -Field $Field -Required @('x','y','width','height')
    [void](Get-PluginStrictInteger -Value $Bounds.x -Field "${Field}.x" -Minimum ([long][int]::MinValue) -Maximum ([long][int]::MaxValue))
    [void](Get-PluginStrictInteger -Value $Bounds.y -Field "${Field}.y" -Minimum ([long][int]::MinValue) -Maximum ([long][int]::MaxValue))
    [void](Get-PluginStrictInteger -Value $Bounds.width -Field "${Field}.width" -Minimum 0 -Maximum ([long][uint32]::MaxValue))
    [void](Get-PluginStrictInteger -Value $Bounds.height -Field "${Field}.height" -Minimum 0 -Maximum ([long][uint32]::MaxValue))
}

function Assert-PluginRendererSnapshot {
    # renderer/native 是候选自报的 GPUI 快照；这里校验完整类型、成功状态、父窗锚点及跨快照身份，
    # 不把它当作独立于候选的 DOM 或窗口证明。
    # @author kongweiguang
    param($Renderer,[string]$Field,[uint64]$ExpectedParent,[switch]$CheckVisible,[bool]$ExpectedVisible,[switch]$CheckBackdropPending,[bool]$ExpectedBackdropPending)
    $Renderer=Get-PluginJsonObject -Value $Renderer -Field $Field -Required @('instance','pageEpoch','processGeneration','pageReady','opened','failure','error','dom','native')
    $instance=Get-PluginJsonString -Value $Renderer.instance -Field "${Field}.instance" -NonEmpty
    $pageEpoch=Get-PluginStrictInteger -Value $Renderer.pageEpoch -Field "${Field}.pageEpoch" -Minimum 0
    [void](Get-PluginStrictInteger -Value $Renderer.processGeneration -Field "${Field}.processGeneration" -Minimum 0)
    Assert-PluginBooleanIs -Value $Renderer.pageReady -Field "${Field}.pageReady" -Expected $true
    Assert-PluginBooleanIs -Value $Renderer.opened -Field "${Field}.opened" -Expected $true
    if($null -ne $Renderer.failure){throw "Unexpected renderer failure: ${Field}.failure"}
    if($null -ne $Renderer.error){throw "Unexpected renderer error: ${Field}.error"}
    [void](Get-PluginJsonObjectOrNull -Value $Renderer.dom -Field "${Field}.dom")
    $native=Get-PluginJsonObject -Value $Renderer.native -Field "${Field}.native" -Required @('instance_id','native_id','native_parent_id','visible','engine_visible','loaded','bounds','clip','scale_factor','message_count','parent_position_notifications','failure','backdrop_visible','backdrop_pending','backdrop_failure','backdrop_captures','backdrop_capture_ms','backdrop_slow_fallback')
    [void](Get-PluginJsonString -Value $native.instance_id -Field "${Field}.native.instance_id" -NonEmpty)
    $nativeId=Get-PluginJsonString -Value $native.native_id -Field "${Field}.native.native_id" -NonEmpty
    $parent=Get-PluginStrictInteger -Value $native.native_parent_id -Field "${Field}.native.native_parent_id" -Minimum 1
    if([uint64]$parent -ne $ExpectedParent){throw "Renderer parent window mismatch: ${Field}.native.native_parent_id"}
    $visible=Get-PluginStrictBoolean -Value $native.visible -Field "${Field}.native.visible"
    if($CheckVisible -and $visible -ne $ExpectedVisible){throw "Unexpected renderer visibility: ${Field}.native.visible"}
    [void](Get-PluginStrictBoolean -Value $native.engine_visible -Field "${Field}.native.engine_visible")
    Assert-PluginBooleanIs -Value $native.loaded -Field "${Field}.native.loaded" -Expected $true
    Assert-PluginWebViewBounds -Bounds $native.bounds -Field "${Field}.native.bounds"
    if($null -ne $native.clip){Assert-PluginWebViewBounds -Bounds $native.clip -Field "${Field}.native.clip"}
    [void](Get-PluginJsonNumber -Value $native.scale_factor -Field "${Field}.native.scale_factor" -Minimum ([double]::Epsilon) -Maximum 10.0)
    [void](Get-PluginStrictInteger -Value $native.message_count -Field "${Field}.native.message_count" -Minimum 0)
    [void](Get-PluginStrictInteger -Value $native.parent_position_notifications -Field "${Field}.native.parent_position_notifications" -Minimum 0)
    [void](Get-PluginStrictBoolean -Value $native.backdrop_visible -Field "${Field}.native.backdrop_visible")
    $backdropPending=Get-PluginStrictBoolean -Value $native.backdrop_pending -Field "${Field}.native.backdrop_pending"
    if($CheckBackdropPending -and $backdropPending -ne $ExpectedBackdropPending){throw "Unexpected backdrop pending state: ${Field}.native.backdrop_pending"}
    if($null -ne $native.failure){throw "Unexpected native failure: ${Field}.native.failure"}
    [void](Get-PluginJsonString -Value $native.backdrop_failure -Field "${Field}.native.backdrop_failure" -AllowNull)
    [void](Get-PluginStrictInteger -Value $native.backdrop_captures -Field "${Field}.native.backdrop_captures" -Minimum 0)
    $samples=Get-PluginJsonArray -Value $native.backdrop_capture_ms -Field "${Field}.native.backdrop_capture_ms"
    for($index=0;$index -lt $samples.Count;$index++){[void](Get-PluginJsonNumber -Value $samples[$index] -Field "${Field}.native.backdrop_capture_ms[$index]" -Minimum 0)}
    [void](Get-PluginStrictBoolean -Value $native.backdrop_slow_fallback -Field "${Field}.native.backdrop_slow_fallback")
    return [pscustomobject]@{Instance=$instance;PageEpoch=$pageEpoch;NativeId=$nativeId;Visible=$visible}
}

function Assert-PluginRootSnapshot {
    # root 快照必须包含真实工作区字段和准确的标签集合；活动标签的可见性/偏好模态状态按阶段核对。
    # 这些值仍是候选自报，只与脚本独立观察到的父窗和夹具路径关联，不冒充独立快照。
    # @author kongweiguang
    param($Snapshot,[string]$Field,[string]$ExpectedActiveTab,[bool]$ExpectedPreferencesOpen,[string[]]$ExpectedTabIds,[uint64]$ExpectedParent,[switch]$CheckActiveVisible,[bool]$ExpectedActiveVisible,[switch]$CheckActiveBackdropPending,[bool]$ExpectedActiveBackdropPending,[switch]$EmptyWorkspace)
    $Snapshot=Get-PluginJsonObject -Value $Snapshot -Field $Field -Required @('tabs','activeTabId','instanceCount','centralContent','status','workspaceReady','connectionsModalOpen','pendingHostOpen','selectedPluginHostId','navigationGeneration','preferencesOpen','settingsReady','registeredContent','windowActive')
    $expected=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($id in @($ExpectedTabIds)){if(-not $expected.Add([string]$id)){throw "Duplicate expected tab identity: $Field"}}
    if($EmptyWorkspace){
        if($expected.Count -ne 0){throw "Unexpected expected tab identities: $Field"}
    } elseif($expected.Count -eq 0){throw "Missing expected tab identities: $Field"}
    $minimum=if($EmptyWorkspace){0}else{1}
    $tabs=Get-PluginJsonArray -Value $Snapshot.tabs -Field "${Field}.tabs" -Minimum $minimum
    $actual=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $activeTab=$null
    $activeRenderer=$null
    foreach($tab in $tabs){
        $tab=Get-PluginJsonObject -Value $tab -Field "${Field}.tab" -Required @('tabId','contentId','savedVersion','savedState','phase','liveVersion','liveState','viewId','renderer')
        $tabId=Get-PluginJsonString -Value $tab.tabId -Field "${Field}.tab.tabId" -NonEmpty
        if(-not $actual.Add($tabId)){throw "Duplicate tab identity: ${Field}.tab.tabId"}
        $contentId=Get-PluginJsonString -Value $tab.contentId -Field "${Field}.tab.contentId" -NonEmpty
        if($contentId -cne 'kerminal.file-search/search-tab'){throw "Unexpected content identity: ${Field}.tab.contentId"}
        [void](Get-PluginStrictInteger -Value $tab.savedVersion -Field "${Field}.tab.savedVersion" -Minimum 0)
        [void](Get-PluginJsonObject -Value $tab.savedState -Field "${Field}.tab.savedState")
        [void](Get-PluginJsonString -Value $tab.phase -Field "${Field}.tab.phase" -NonEmpty)
        [void](Get-PluginStrictInteger -Value $tab.liveVersion -Field "${Field}.tab.liveVersion" -Minimum 0)
        [void](Get-PluginJsonObject -Value $tab.liveState -Field "${Field}.tab.liveState")
        [void](Get-PluginJsonString -Value $tab.viewId -Field "${Field}.tab.viewId" -NonEmpty)
        $isActive=$tabId -ceq $ExpectedActiveTab
        $renderer=Assert-PluginRendererSnapshot -Renderer $tab.renderer -Field "${Field}.tab.renderer" -ExpectedParent $ExpectedParent -CheckVisible:($isActive -and $CheckActiveVisible) -ExpectedVisible $ExpectedActiveVisible -CheckBackdropPending:($isActive -and $CheckActiveBackdropPending) -ExpectedBackdropPending $ExpectedActiveBackdropPending
        if($isActive){$activeTab=$tabId; $activeRenderer=$renderer}
    }
    if($EmptyWorkspace){
        if($actual.Count -ne 0){throw "Unexpected tab set: ${Field}.tabs"}
        if($null -ne $Snapshot.activeTabId){throw "Unexpected active tab: ${Field}.activeTabId"}
        Assert-PluginBooleanIs -Value $Snapshot.centralContent -Field "${Field}.centralContent" -Expected $false
    } else {
        if($null -eq $activeTab){throw "Active tab is missing: ${Field}.activeTabId"}
        $declaredActive=Get-PluginJsonString -Value $Snapshot.activeTabId -Field "${Field}.activeTabId" -NonEmpty
        if($declaredActive -cne $activeTab){throw "Active tab identity mismatch: ${Field}.activeTabId"}
        if($actual.Count -ne $expected.Count -or @($expected | Where-Object {-not $actual.Contains($_)}).Count -ne 0){throw "Unexpected tab set: ${Field}.tabs"}
        Assert-PluginBooleanIs -Value $Snapshot.centralContent -Field "${Field}.centralContent" -Expected $true
    }
    [void](Get-PluginStrictInteger -Value $Snapshot.instanceCount -Field "${Field}.instanceCount" -Minimum 0)
    [void](Get-PluginJsonString -Value $Snapshot.status -Field "${Field}.status" -AllowNull)
    Assert-PluginBooleanIs -Value $Snapshot.workspaceReady -Field "${Field}.workspaceReady" -Expected $true
    Assert-PluginBooleanIs -Value $Snapshot.connectionsModalOpen -Field "${Field}.connectionsModalOpen" -Expected $false
    Assert-PluginBooleanIs -Value $Snapshot.pendingHostOpen -Field "${Field}.pendingHostOpen" -Expected $false
    [void](Get-PluginJsonString -Value $Snapshot.selectedPluginHostId -Field "${Field}.selectedPluginHostId" -AllowNull)
    [void](Get-PluginStrictInteger -Value $Snapshot.navigationGeneration -Field "${Field}.navigationGeneration" -Minimum 0)
    Assert-PluginBooleanIs -Value $Snapshot.preferencesOpen -Field "${Field}.preferencesOpen" -Expected $ExpectedPreferencesOpen
    Assert-PluginBooleanIs -Value $Snapshot.settingsReady -Field "${Field}.settingsReady" -Expected $true
    Assert-PluginBooleanIs -Value $Snapshot.registeredContent -Field "${Field}.registeredContent" -Expected $true
    [void](Get-PluginStrictBoolean -Value $Snapshot.windowActive -Field "${Field}.windowActive")
    return [pscustomobject]@{ActiveTab=$activeTab;Renderer=$activeRenderer}
}

function Assert-PluginDom {
    # DOM 回执仍是候选页面自报；脚本校验完整形状、工作流状态，并把搜索路径集合与磁盘夹具独立对照。
    # 页面证明的像素只由同一系统 BMP 的搜索标记提供，不能由这里的文字回执替代。
    # @author kongweiguang
    param($Dom,[string]$Field,[string]$FixtureRoot,[string[]]$ExpectedSearchPaths,[ValidateSet('search','cancelled','events','draft')][string]$Kind)
    $Dom=Get-PluginJsonObject -Value $Dom -Field $Field -Required @('token','root','query','status','busy','rows','trace','subscribed','subscribeError','events')
    [void](Get-PluginJsonString -Value $Dom.token -Field "${Field}.token" -NonEmpty)
    $root=Get-PluginJsonString -Value $Dom.root -Field "${Field}.root" -NonEmpty
    $query=Get-PluginJsonString -Value $Dom.query -Field "${Field}.query" -NonEmpty
    [void](Get-PluginJsonString -Value $Dom.status -Field "${Field}.status" -AllowNull)
    Assert-PluginBooleanIs -Value $Dom.busy -Field "${Field}.busy" -Expected $false
    $rows=Get-PluginJsonArray -Value $Dom.rows -Field "${Field}.rows"
    foreach($row in $rows){[void](Get-PluginJsonString -Value $row -Field "${Field}.rows" -NonEmpty)}
    $trace=Get-PluginJsonArray -Value $Dom.trace -Field "${Field}.trace"
    foreach($event in $trace){
        $event=Get-PluginJsonObject -Value $event -Field "${Field}.trace" -Required @('rows','busy')
        [void](Get-PluginStrictInteger -Value $event.rows -Field "${Field}.trace.rows" -Minimum 0)
        [void](Get-PluginStrictBoolean -Value $event.busy -Field "${Field}.trace.busy")
    }
    $subscribed=Get-PluginStrictBoolean -Value $Dom.subscribed -Field "${Field}.subscribed"
    [void](Get-PluginJsonString -Value $Dom.subscribeError -Field "${Field}.subscribeError" -AllowNull)
    $events=Get-PluginJsonArray -Value $Dom.events -Field "${Field}.events"
    foreach($event in $events){
        $event=Get-PluginJsonObject -Value $event -Field "${Field}.events" -Required @('name','payload')
        [void](Get-PluginJsonString -Value $event.name -Field "${Field}.events.name" -NonEmpty)
        [void](Get-PluginJsonObject -Value $event.payload -Field "${Field}.events.payload")
    }
    if(-not [IO.Path]::IsPathRooted($root)){throw "DOM root is not absolute: ${Field}.root"}
    $actualRoot=[IO.Path]::GetFullPath($root)
    $expectedRoot=[IO.Path]::GetFullPath($FixtureRoot)
    if(-not $actualRoot.Equals($expectedRoot,[StringComparison]::OrdinalIgnoreCase)){throw "DOM root mismatch: ${Field}.root"}
    switch($Kind){
        'search' {
            if($query -cne 'needle'){throw "Unexpected search query: ${Field}.query"}
            $expected=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            foreach($path in @($ExpectedSearchPaths)){if(-not $expected.Add([IO.Path]::GetFullPath([string]$path))){throw "Duplicate expected search path: ${Field}.rows"}}
            if($expected.Count -eq 0 -or $rows.Count -ne $expected.Count){throw "Search row count mismatch: ${Field}.rows"}
            foreach($row in $rows){if(-not $expected.Remove([IO.Path]::GetFullPath($row))){throw "Unexpected search row: ${Field}.rows"}}
            if($expected.Count -ne 0){throw "Missing search row: ${Field}.rows"}
            $streaming=$false
            foreach($event in $trace){if($event.busy -eq $true -and $event.rows -gt 0 -and $event.rows -lt $rows.Count){$streaming=$true}}
            if(-not $streaming){throw "Missing streaming search trace: ${Field}.trace"}
        }
        'cancelled' {
            if($query -cne 'no-such-needle-for-cancel'){throw "Unexpected cancelled query: ${Field}.query"}
            if($Dom.status -isnot [string] -or (-not $Dom.status.Contains('取消') -and -not $Dom.status.Contains('cancelled'))){throw "Missing cancelled status: ${Field}.status"}
        }
        'events' {
            Assert-PluginBooleanIs -Value $subscribed -Field "${Field}.subscribed" -Expected $true
            if($null -ne $Dom.subscribeError){throw "Unexpected subscribe error: ${Field}.subscribeError"}
            $eventTab=$null
            foreach($event in $events){
                if($event.name -ceq 'tab.changed'){
                    $candidateTab=Get-PluginJsonString -Value $event.payload.tab -Field "${Field}.events.payload.tab" -NonEmpty
                    if($null -ne $eventTab -and $eventTab -cne $candidateTab){throw "Conflicting tab.changed identities: ${Field}.events"}
                    $eventTab=$candidateTab
                }
            }
            if($null -eq $eventTab){throw "Missing tab.changed event: ${Field}.events"}
            return $eventTab
        }
        'draft' {
            if($query -cne 'no-such-needle-for-cancel'){throw "Unexpected draft query: ${Field}.query"}
        }
    }
}

function Assert-PluginWorkflowReport {
    # 报告中的 UI 快照与 DOM 均为候选自报；本函数只校验完整类型/状态和可外部锚定的父窗、
    # 夹具路径/文件集合、启动视口。没有独立读取 GPUI 状态或 DOM 的部分仍保持未证明。
    # 工作流按候选真实顺序使用同一标签：首个标签完成搜索、通知、设置、重载和关闭；
    # 第二标签只用于产生 tab.changed，关闭资源完成后方可进入通知阶段。
    # @author kongweiguang
    param($Report,[uint64]$ExpectedParent,[string]$FixtureRoot,[string[]]$ExpectedSearchPaths,[double]$ViewportWidth,[double]$ViewportHeight)
    $schema=Get-PluginStrictInteger -Value $Report.schemaVersion -Field 'report.schemaVersion' -Minimum 1 -Maximum 1
    if($schema -ne 1){throw 'Unexpected acceptance report schema.'}
    $viewport=Get-PluginJsonArray -Value $Report.viewport -Field 'report.viewport'
    if($viewport.Count -ne 2){throw 'Invalid viewport shape: report.viewport'}
    $actualViewportWidth=Get-PluginJsonNumber -Value $viewport[0] -Field 'report.viewport[0]' -Minimum 0
    $actualViewportHeight=Get-PluginJsonNumber -Value $viewport[1] -Field 'report.viewport[1]' -Minimum 0
    # PowerShell 变量名不区分大小写；实际值不能复用参数名，否则比较会退化成同值自比。
    if([Math]::Abs($actualViewportWidth-$ViewportWidth) -gt 32 -or [Math]::Abs($actualViewportHeight-$ViewportHeight) -gt 32){throw 'Viewport does not match the launched logical size: report.viewport'}
    [void](Get-PluginJsonNumber -Value $Report.scaleFactor -Field 'report.scaleFactor' -Minimum ([double]::Epsilon) -Maximum 10.0)
    Assert-PluginBooleanIs -Value $Report.nativeImeTested -Field 'report.nativeImeTested' -Expected $false
    Assert-PluginBooleanIs -Value $Report.dpiSwitchTested -Field 'report.dpiSwitchTested' -Expected $false
    Assert-PluginBooleanIs -Value $Report.sleepResumeTested -Field 'report.sleepResumeTested' -Expected $false
    [void](Get-PluginJsonString -Value $Report.inputMode -Field 'report.inputMode' -NonEmpty)
    $readyObject=Get-PluginJsonObject -Value $Report.ready -Field 'report.ready'
    $firstTab=Get-PluginJsonString -Value $readyObject.activeTabId -Field 'report.ready.activeTabId' -NonEmpty
    $ready=Assert-PluginRootSnapshot -Snapshot $readyObject -Field 'report.ready' -ExpectedActiveTab $firstTab -ExpectedPreferencesOpen $false -ExpectedTabIds @($firstTab) -ExpectedParent $ExpectedParent -CheckActiveVisible -ExpectedActiveVisible $true -CheckActiveBackdropPending -ExpectedActiveBackdropPending $false
    [void](Assert-PluginDom -Dom $Report.searched -Field 'report.searched' -FixtureRoot $FixtureRoot -ExpectedSearchPaths $ExpectedSearchPaths -Kind 'search')
    [void](Assert-PluginDom -Dom $Report.cancelled -Field 'report.cancelled' -FixtureRoot $FixtureRoot -ExpectedSearchPaths $ExpectedSearchPaths -Kind 'cancelled')
    $secondTab=Assert-PluginDom -Dom $Report.pageEvents -Field 'report.pageEvents' -FixtureRoot $FixtureRoot -ExpectedSearchPaths $ExpectedSearchPaths -Kind 'events'
    if($secondTab -ceq $firstTab){throw 'Page event did not identify a distinct second tab: report.pageEvents.events'}
    $notificationHidden=Assert-PluginRootSnapshot -Snapshot $Report.notificationOccluded -Field 'report.notificationOccluded' -ExpectedActiveTab $firstTab -ExpectedPreferencesOpen $false -ExpectedTabIds @($firstTab) -ExpectedParent $ExpectedParent -CheckActiveVisible -ExpectedActiveVisible $false -CheckActiveBackdropPending -ExpectedActiveBackdropPending $false
    $notificationVisible=Assert-PluginRootSnapshot -Snapshot $Report.notificationRestored -Field 'report.notificationRestored' -ExpectedActiveTab $firstTab -ExpectedPreferencesOpen $false -ExpectedTabIds @($firstTab) -ExpectedParent $ExpectedParent -CheckActiveVisible -ExpectedActiveVisible $true
    $occluded=Assert-PluginRootSnapshot -Snapshot $Report.occluded -Field 'report.occluded' -ExpectedActiveTab $firstTab -ExpectedPreferencesOpen $true -ExpectedTabIds @($firstTab) -ExpectedParent $ExpectedParent -CheckActiveVisible -ExpectedActiveVisible $false -CheckActiveBackdropPending -ExpectedActiveBackdropPending $false
    $restored=Assert-PluginRootSnapshot -Snapshot $Report.restored -Field 'report.restored' -ExpectedActiveTab $firstTab -ExpectedPreferencesOpen $false -ExpectedTabIds @($firstTab) -ExpectedParent $ExpectedParent -CheckActiveVisible -ExpectedActiveVisible $true
    $reloaded=Assert-PluginRootSnapshot -Snapshot $Report.reloaded -Field 'report.reloaded' -ExpectedActiveTab $firstTab -ExpectedPreferencesOpen $false -ExpectedTabIds @($firstTab) -ExpectedParent $ExpectedParent
    [void](Assert-PluginDom -Dom $Report.draftAfterOcclusion -Field 'report.draftAfterOcclusion' -FixtureRoot $FixtureRoot -ExpectedSearchPaths $ExpectedSearchPaths -Kind 'draft')
    $closed=Assert-PluginRootSnapshot -Snapshot $Report.closed -Field 'report.closed' -ExpectedActiveTab '' -ExpectedPreferencesOpen $false -ExpectedTabIds @() -ExpectedParent $ExpectedParent -EmptyWorkspace
    foreach($summary in @($ready,$notificationHidden,$notificationVisible,$occluded,$restored)){
        if($summary.Renderer.Instance -cne $ready.Renderer.Instance){throw 'First tab page instance changed before the explicit reload.'}
        if($summary.Renderer.NativeId -cne $ready.Renderer.NativeId){throw 'First tab native identity changed before the explicit reload.'}
        if($summary.Renderer.PageEpoch -ne $ready.Renderer.PageEpoch){throw 'First tab page epoch changed before the explicit reload.'}
    }
    if($reloaded.Renderer.Instance -cne $restored.Renderer.Instance){throw 'Reload replaced the page instance: report.reloaded'}
    if($reloaded.Renderer.PageEpoch -le $restored.Renderer.PageEpoch){throw 'Reload did not advance the page epoch: report.reloaded'}
    if($null -ne $closed.Renderer){throw 'Closed workspace unexpectedly retained an active renderer: report.closed'}
}

function Assert-PluginAcceptanceReport {
    # 最终成功判定只接受原生 JSON 布尔/整数；字符串 true/false 或数字字符串都不能通过。
    # @author kongweiguang
    param($Report,[int]$ProcessId,[string]$Candidate,[string]$Theme,[string]$Desktop,[string]$InputDesktop,[int]$ThreadId,$PluginBaseline,[string]$HostPlatform,[uint64]$ExpectedParent,[string]$FixtureRoot,[string[]]$ExpectedSearchPaths,[double]$ViewportWidth,[double]$ViewportHeight)
    if($null -eq $Report -or $null -eq $Report.nativeIsolation -or $null -eq $PluginBaseline){throw 'Acceptance report or baseline is missing.'}
    Assert-PluginBooleanIs -Value $Report.resourcesShutdownComplete -Field 'report.resourcesShutdownComplete' -Expected $true
    Assert-PluginBooleanIs -Value $Report.runtimeShutdownObserved -Field 'report.runtimeShutdownObserved' -Expected $false
    Assert-PluginBooleanIs -Value $Report.quitReceipt -Field 'report.quitReceipt' -Expected $true
    Assert-PluginBooleanIs -Value $Report.nativeIsolation.platformSupported -Field 'report.nativeIsolation.platformSupported' -Expected $true
    Assert-PluginBooleanIs -Value $Report.nativeIsolation.isolationProven -Field 'report.nativeIsolation.isolationProven' -Expected $true
    $reportProcess=Get-PluginStrictProcessId -Value $Report.processId -Field 'report.processId'
    $isolationProcess=Get-PluginStrictProcessId -Value $Report.nativeIsolation.processId -Field 'report.nativeIsolation.processId'
    $isolationThread=Get-PluginStrictInteger -Value $Report.nativeIsolation.uiThreadId -Field 'report.nativeIsolation.uiThreadId' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
    if($Report.shutdownScope -isnot [string] -or $Report.shutdownScope -cne 'composition-resources-only' -or
       $Report.sidecarRuntimeBinding -isnot [string] -or $Report.sidecarRuntimeBinding -cne 'not-proven' -or
       $reportProcess -ne $ProcessId -or
       $Report.candidateSha256 -isnot [string] -or $Report.candidateSha256 -cne $Candidate -or
       $Report.theme -isnot [string] -or $Report.theme -cne $Theme -or
       $Report.mode -isnot [string] -or $Report.mode -cne 'private-desktop-composition' -or
       $isolationProcess -ne $ProcessId -or $isolationThread -ne $ThreadId -or
       $Report.nativeIsolation.uiThreadDesktopName -isnot [string] -or $Report.nativeIsolation.uiThreadDesktopName -cne $Desktop -or
       $Report.nativeIsolation.inputDesktopName -isnot [string] -or $Report.nativeIsolation.inputDesktopName -cne $InputDesktop -or
       $Report.pluginSha256 -isnot [string] -or $Report.pluginSha256 -cnotmatch '^[0-9a-f]{64}$' -or
       $Report.sidecarSha256 -isnot [string] -or $Report.sidecarSha256 -cnotmatch '^[0-9a-f]{64}$' -or
       $Report.pluginSha256 -cne $PluginBaseline.pluginSha256 -or $Report.sidecarSha256 -cne $PluginBaseline.sidecarSha256 -or
       $Report.sidecarMain -isnot [string] -or $Report.sidecarMain -cne $PluginBaseline.sidecarMain -or
       $Report.sidecarPlatform -isnot [string] -or $Report.sidecarPlatform -cne $PluginBaseline.sidecarPlatform -or
       $Report.sidecarPlatform -cne $HostPlatform){
        throw 'Candidate, plugin, Sidecar, private desktop or shutdown evidence mismatch.'
    }
    Assert-PluginWorkflowReport -Report $Report -ExpectedParent $ExpectedParent -FixtureRoot $FixtureRoot -ExpectedSearchPaths $ExpectedSearchPaths -ViewportWidth $ViewportWidth -ViewportHeight $ViewportHeight
}

$executable=(Resolve-Path -LiteralPath $ExecutablePath).Path
$plugin=(Resolve-Path -LiteralPath $PluginPath).Path
$repositoryRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$base=[IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $base) { throw 'The output root must not exist.' }
[void][IO.Directory]::CreateDirectory($base)
$candidate=(Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
$hostPlatform=Get-PluginHostPlatform
$previous=@{}
$names=@('KERMINAL_PLUGIN_WEBVIEW_DIRECTORY','KERMINAL_WEBVIEW_PROBE_THEME','KERMINAL_WEBVIEW_BACKGROUND','KERMINAL_PLUGIN_WEBVIEW_DESKTOP','KERMINAL_PLUGIN_WEBVIEW_CANDIDATE_SHA256')
foreach($name in $names){$previous[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
$process=$null
$failure=$null
$receipts=@()
$pluginBaseline=$null
try {
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_WEBVIEW_DIRECTORY',$plugin,'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_BACKGROUND','0','Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_WEBVIEW_CANDIDATE_SHA256',$candidate,'Process')
    Initialize-PluginWebViewLauncher
    Initialize-PluginIsolationProbe
    foreach($currentTheme in $(if($Theme -ceq 'all'){@('light','dark','system')}else{@($Theme)})) {
        $output=Join-Path $base $currentTheme
        $desktop='KerminalRail-settings-plugin-webview-'+[guid]::NewGuid().ToString('N')
        $inputDesktop=[Kerminal.PluginWebViewAcceptance.Isolation]::InputDesktop()
        if([string]::IsNullOrWhiteSpace($inputDesktop) -or $desktop.Equals($inputDesktop,[StringComparison]::OrdinalIgnoreCase)){throw 'The fresh acceptance desktop is not independent of input.'}
        [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME',$currentTheme,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_WEBVIEW_DESKTOP',$desktop,'Process')
        $deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        $viewportWidth=1024
        $viewportHeight=720
        $process=[Kerminal.PluginWebViewAcceptance.NativeDesktopLauncher]::Start($executable,$repositoryRoot,$output,(Join-Path $base "$currentTheme.stdout.log"),(Join-Path $base "$currentTheme.stderr.log"),$desktop,$viewportWidth,$viewportHeight)
        if($process.ProcessId -le 0 -or $process.DesktopName -cne $desktop){throw 'The launch handle does not belong to this private desktop.'}
        $threadId=[uint32]0
        $parent=[uint64]0
        foreach($stage in @('start','search-before','search-after','notification-before','notification-after','dialog-before','dialog-after','complete')){
            $evidence=Wait-PluginIsolationCheckpoint -Process $process -Output $output -Stage $stage -Desktop $desktop -InputDesktop $inputDesktop -Candidate $candidate -Executable $executable -PluginRoot $plugin -Theme $currentTheme -PluginBaseline $pluginBaseline -BaselineThreadId $threadId -ExpectedParent $parent -Deadline $deadline
            if($stage -ceq 'start'){
                $threadId=Get-PluginStrictInteger -Value $evidence.nativeIsolation.uiThreadId -Field 'evidence.uiThreadId' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
                if($null -eq $pluginBaseline){$pluginBaseline=$evidence.pluginIdentity}
            }
            if($stage -ceq 'search-after'){$parent=Get-PluginStrictInteger -Value (Get-Content -LiteralPath (Join-Path $output 'search.wgc.json') -Raw | ConvertFrom-Json).parentWindow -Field 'capture.search.parentWindow' -Minimum 1}
        }
        $remaining=[Math]::Max(0,[Math]::Min([double][uint32]::MaxValue,($deadline-[DateTime]::UtcNow).TotalMilliseconds))
        if(-not $process.WaitForExit([uint32]$remaining)){throw "WebView plugin acceptance timed out: $currentTheme"}
        if($process.GetExitCode() -ne 0){
            $stderrPath=Join-Path $base "$currentTheme.stderr.log"
            $stderrText=if(Test-Path -LiteralPath $stderrPath -PathType Leaf){[IO.File]::ReadAllText($stderrPath)}else{''}
            if($stderrText.Contains('网页遮挡系统同帧证据未证明')){
                throw "P3 is hard-blocked by unproven same-system-frame overlay evidence; this may be valid preview degradation, not a product failure. evidence=$output; stderr=$stderrPath"
            }
            throw "WebView plugin acceptance failed: $currentTheme; stderr=$stderrPath"
        }
        $report=Get-Content -LiteralPath (Join-Path $output 'report.json') -Raw|ConvertFrom-Json
        $offscreenOnly=$env:KERMINAL_WEBVIEW_OFFSCREEN_ONLY -ceq '1'
        if($offscreenOnly){
            # 私有桌面 WGC 不再是成功门禁；父窗来自候选报告中活动标签的真实原生子窗身份。
            $activeTab=@($report.ready.tabs | Where-Object { $_.tabId -eq $report.ready.activeTabId }) | Select-Object -First 1
            if($null -eq $activeTab){throw 'Offscreen report has no active tab.'}
            $parent=Get-PluginStrictInteger -Value $activeTab.renderer.native.native_parent_id -Field 'report.ready.active.renderer.native.native_parent_id' -Minimum 1
        } else {
            $searchCapture=Get-Content -LiteralPath (Join-Path $output 'search.wgc.json') -Raw|ConvertFrom-Json
            $parent=Get-PluginStrictInteger -Value $searchCapture.parentWindow -Field 'capture.search.parentWindow' -Minimum 1
        }
        $fixtureRoot=Join-Path $output 'search-fixture'
        $expectedSearchPaths=@([IO.Directory]::GetFiles($fixtureRoot,'needle-*.txt') | ForEach-Object {[IO.Path]::GetFullPath($_)})
        if($expectedSearchPaths.Count -eq 0){throw 'The search fixture does not contain the expected files.'}
        Assert-PluginAcceptanceReport -Report $report -ProcessId $process.ProcessId -Candidate $candidate -Theme $currentTheme -Desktop $desktop -InputDesktop $inputDesktop -ThreadId $threadId -PluginBaseline $pluginBaseline -HostPlatform $hostPlatform -ExpectedParent ([uint64]$parent) -FixtureRoot $fixtureRoot -ExpectedSearchPaths $expectedSearchPaths -ViewportWidth $viewportWidth -ViewportHeight $viewportHeight
        [void](Read-PluginIdentity -Output $output -PluginRoot $plugin -ProcessId $process.ProcessId -Theme $currentTheme -Candidate $candidate -Baseline $pluginBaseline)
        if($offscreenOnly){
            foreach($phase in @('search','notification','dialog')){
                $png=Join-Path $output "$phase.png"
                if(-not (Test-Path -LiteralPath $png -PathType Leaf)){throw "Missing GPUI offscreen frame: $phase"}
                $bytes=[IO.File]::ReadAllBytes($png)
                if($bytes.Length -lt 33 -or $bytes[0] -ne 137 -or $bytes[1] -ne 80 -or $bytes[2] -ne 78 -or $bytes[3] -ne 71){throw "Invalid GPUI offscreen PNG: $phase"}
                $width=([int]$bytes[16] -shl 24) -bor ([int]$bytes[17] -shl 16) -bor ([int]$bytes[18] -shl 8) -bor [int]$bytes[19]
                $height=([int]$bytes[20] -shl 24) -bor ([int]$bytes[21] -shl 16) -bor ([int]$bytes[22] -shl 8) -bor [int]$bytes[23]
                if($width -lt 100 -or $height -lt 100){throw "GPUI offscreen frame is empty: $phase"}
            }
        } else {
            foreach($phase in @('search','notification','dialog')){Assert-PluginWebViewFrame -Output $output -Phase $phase -ProcessId $process.ProcessId -ParentWindow ([uint64]$parent)}
        }
        $deadline=[DateTime]::UtcNow.AddSeconds(10)
        while((Get-PluginJobActiveProcesses -Process $process -Stage 'normal-wait') -gt 0 -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
        $jobActive=Get-PluginJobActiveProcesses -Process $process -Stage 'normal-final'
        if($jobActive -ne 0){throw 'The candidate left running descendants after normal shutdown.'}
        $processId=$process.ProcessId
        $process.Dispose()
        Assert-PluginBooleanIs -Value $process.DesktopClosed -Field 'desktop.closed' -Expected $true
        $process=$null
        $receipts+=[pscustomobject]@{Theme=$currentTheme;ProcessId=$processId;DesktopName=$desktop;InputDesktopName=$inputDesktop;UiThreadId=$threadId;Report="$currentTheme/report.json";NormalExitJob=$jobActive;DesktopHandleClosed=$true}
    }
    if((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidate){throw 'The candidate changed during acceptance.'}
    [pscustomobject]@{SchemaVersion=1;CandidateSha256=$candidate;PluginSha256=$pluginBaseline.pluginSha256;SidecarSha256=$pluginBaseline.sidecarSha256;SidecarMain=$pluginBaseline.sidecarMain;SidecarPlatform=$pluginBaseline.sidecarPlatform;HostPlatform=$hostPlatform;SidecarRuntimeBinding='not-proven';SidecarEvidence='package-file-only';PackageFileCount=@($pluginBaseline.packageFiles).Count;ResourceCount=@($pluginBaseline.resources).Count;Runs=$receipts;Mode='private-desktop-composition';WorkflowEvidence='candidate-self-report';WorkflowAnchors='wgc-parent-window+fixture-paths+launch-viewport';UiWorkflowIndependentlyProven=$false;NativeImeTested=$false;DpiSwitchTested=$false;SleepResumeTested=$false;RuntimeShutdownObserved=$false}|ConvertTo-Json -Depth 8|Set-Content -LiteralPath (Join-Path $base 'acceptance-report.json') -Encoding utf8
}
catch {
    $failure=$_
    throw
}
finally {
    try {
        try {
            if($null -ne $process){Stop-PluginOwnedProcess -Process $process}
        } catch {
            if($null -eq $failure){throw}
            throw [AggregateException]::new('Acceptance and owned-resource cleanup both failed.',[Exception[]]@($failure.Exception,$_.Exception))
        }
    } finally {
        foreach($name in $names){[Environment]::SetEnvironmentVariable($name,$previous[$name],'Process')}
    }
}
