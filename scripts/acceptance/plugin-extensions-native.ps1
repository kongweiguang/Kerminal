# @author kongweiguang
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$LocalPackagePath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('light','dark','system','all')][string]$Theme='all',
    [string]$PythonPath='python',
    [ValidateRange(60,900)][int]$TimeoutSeconds=300
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Get-ExtensionsStrictBoolean {
    # ConvertFrom-Json 的正常布尔值只能是 bool；字符串和数字不能经隐式转换冒充成功。
    # @author kongweiguang
    param($Value,[string]$Field)
    if($Value -isnot [bool]){throw "Invalid boolean field: $Field"}
    return [bool]$Value
}

function Assert-ExtensionsBoolean {
    # 期望值同样精确匹配，避免 "false" 之类的非空字符串被当成 true。
    # @author kongweiguang
    param($Value,[string]$Field,[bool]$Expected)
    $actual=Get-ExtensionsStrictBoolean -Value $Value -Field $Field
    if($actual -ne $Expected){throw "Unexpected boolean field: $Field"}
}

function Get-ExtensionsStrictInteger {
    # 只接受 ConvertFrom-Json 产生的 Int32/Int64；拒绝数字字符串、浮点、NaN、缺失和越界。
    # @author kongweiguang
    param($Value,[string]$Field,[long]$Minimum=0,[long]$Maximum=[long]::MaxValue)
    if($Value -isnot [int] -and $Value -isnot [long]){throw "Invalid integer field: $Field"}
    $number=[long]$Value
    if($number -lt $Minimum -or $number -gt $Maximum){throw "Out of range integer field: $Field"}
    return $number
}

function Get-ExtensionsStrictNumber {
    # 视口与缩放因子是 JSON 浮点数；仅接受真实数值类型，字符串和布尔值必须拒绝。
    # @author kongweiguang
    param($Value,[string]$Field)
    if($Value -is [bool] -or ($Value -isnot [int] -and $Value -isnot [long] -and $Value -isnot [double] -and $Value -isnot [single] -and $Value -isnot [decimal])){
        throw "Invalid number field: $Field"
    }
    $number=[double]$Value
    if([double]::IsNaN($number) -or [double]::IsInfinity($number)){throw "Invalid number field: $Field"}
    return $number
}

function Get-ExtensionsStrictString {
    # 身份、主题和关闭范围必须是实际非空字符串，不能让 null 或数组参与比较。
    # @author kongweiguang
    param($Value,[string]$Field)
    if($Value -isnot [string] -or [string]::IsNullOrWhiteSpace($Value)){throw "Invalid string field: $Field"}
    return $Value
}

function Assert-ExtensionsString {
    # 关键枚举必须逐字匹配，避免大小写或空白别名被当成同一证据。
    # @author kongweiguang
    param($Value,[string]$Field,[string]$Expected)
    $actual=Get-ExtensionsStrictString -Value $Value -Field $Field
    if($actual -cne $Expected){throw "Unexpected string field: $Field"}
    return $actual
}

function Assert-ExtensionsDigest {
    # 摘要统一为小写十六进制，避免大小写差异或短字符串被当成候选身份。
    # @author kongweiguang
    param($Value,[string]$Field)
    $actual=Get-ExtensionsStrictString -Value $Value -Field $Field
    if($actual -cnotmatch '^[0-9a-f]{64}$'){throw "Invalid digest field: $Field"}
    return $actual
}

function Get-ExtensionsStringSet {
    # JSON 数组必须是唯一字符串集合；重复项会让“数量相等”掩盖身份替换。
    # @author kongweiguang
    param($Value,[string]$Field)
    if($Value -isnot [System.Collections.IList]){throw "Invalid string array field: $Field"}
    $items=[Collections.Generic.List[string]]::new()
    foreach($item in $Value){
        if($item -isnot [string] -or [string]::IsNullOrWhiteSpace($item)){throw "Invalid string array field: $Field"}
        if($items.Contains($item)){throw "Duplicate string array entry: $Field"}
        $items.Add($item)
    }
    return $items.ToArray()
}

function Get-ExtensionsSnapshotPage {
    # 页面状态嵌在快照的 page 字段；缺失或类型错误必须显式失败而不是静默跳过。
    # @author kongweiguang
    param($Snapshot,[string]$Field)
    if($null -eq $Snapshot -or $Snapshot -isnot [pscustomobject]){throw "Missing snapshot: $Field"}
    $property=$Snapshot.PSObject.Properties['page']
    if($null -eq $property -or $property.Value -isnot [pscustomobject]){throw "Missing page snapshot: $Field"}
    return $property.Value
}

function Get-ExtensionsJobActiveProcesses {
    # Job 计数来自受控 C# launcher 的 uint；拒绝 null、字符串或浮点参与关闭判定。
    # @author kongweiguang
    param([Parameter(Mandatory)]$Process,[string]$Stage)
    $snapshot=$Process.GetJobSnapshot()
    if($null -eq $snapshot){throw "Missing Job snapshot: $Stage"}
    if($snapshot.ActiveProcesses -isnot [uint32]){throw "Invalid Job active process count: $Stage"}
    return [uint32]$snapshot.ActiveProcesses
}

function Get-ExtensionsFrameDimensions {
    # 逻辑视口固定来自本轮窗口；截图像素尺寸必须是视口乘缩放因子后的实际绘制目标。
    # @author kongweiguang
    param($Viewport,$Scale,[string]$Stage)
    if($Viewport -isnot [System.Collections.IList] -or $Viewport.Count -ne 2){throw "Invalid viewport: $Stage"}
    $width=Get-ExtensionsStrictNumber -Value $Viewport[0] -Field 'viewport.width'
    $height=Get-ExtensionsStrictNumber -Value $Viewport[1] -Field 'viewport.height'
    if([Math]::Abs($width-1200.0) -gt 0.5 -or [Math]::Abs($height-800.0) -gt 0.5){throw "Unexpected logical viewport: $Stage"}
    $scale=Get-ExtensionsStrictNumber -Value $Scale -Field 'scaleFactor'
    if($scale -le 0){throw "Invalid scale factor: $Stage"}
    return [pscustomobject]@{
        Width=[int][Math]::Round($width*$scale,[MidpointRounding]::AwayFromZero)
        Height=[int][Math]::Round($height*$scale,[MidpointRounding]::AwayFromZero)
        ViewportWidth=$width
        ViewportHeight=$height
        Scale=$scale
    }
}

function Assert-ExtensionsFrameDimensions {
    # PNG 结构检查只证明文件与绘制尺寸；没有同帧特征像素时不得延伸为主题或系统合成证据。
    # @author kongweiguang
    param($Evidence,$Expected,[string]$Phase)
    # InspectPng 返回 C# PngEvidence；不能用 pscustomobject 类型判断把真实回执误判成缺失。
    if($null -eq $Evidence -or $null -eq $Evidence.PSObject.Properties['Width'] -or $null -eq $Evidence.PSObject.Properties['Height'] -or $null -eq $Evidence.PSObject.Properties['IdatBytes'] -or $null -eq $Evidence.PSObject.Properties['Chunks'] -or $null -eq $Evidence.PSObject.Properties['Sha256']){throw "Missing PNG evidence: $Phase"}
    if($Evidence.Width -isnot [int] -or $Evidence.Height -isnot [int] -or ($Evidence.IdatBytes -isnot [int] -and $Evidence.IdatBytes -isnot [long]) -or $Evidence.Chunks -isnot [int]){throw "Invalid PNG evidence types: $Phase"}
    if($Evidence.Width -ne $Expected.Width -or $Evidence.Height -ne $Expected.Height){throw "Screenshot dimensions do not match the reported viewport: $Phase"}
    if([long]$Evidence.IdatBytes -le 0 -or [int]$Evidence.Chunks -lt 3){throw "Screenshot has no complete image stream: $Phase"}
    if($Evidence.Sha256 -isnot [string] -or $Evidence.Sha256 -cnotmatch '^[0-9a-f]{64}$'){throw "Invalid screenshot digest: $Phase"}
}

function Initialize-ExtensionsNativeEvidence {
    # PNG 解析和输入桌面读取都在本进程完成，不依赖候选自报；只读桌面句柄并始终关闭。
    # @author kongweiguang
    if('Kerminal.PluginExtensionsAcceptance.NativeEvidence' -as [type]){return}
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;

namespace Kerminal.PluginExtensionsAcceptance {
    public sealed class PngEvidence {
        public int Width, Height, BitDepth, ColorType;
        public long IdatBytes;
        public int Chunks;
        public string Sha256;
    }
    public static class NativeEvidence {
        [DllImport("user32.dll", SetLastError=true)]
        static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
        [DllImport("user32.dll", SetLastError=true)]
        static extern bool CloseDesktop(IntPtr desktop);
        [DllImport("user32.dll", EntryPoint="GetUserObjectInformationW", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool GetUserObjectInformation(IntPtr handle, int index, StringBuilder value, uint length, out uint needed);

        // 输入桌面只读取名称且立即关闭句柄，不切换桌面、不发送输入。
        public static string InputDesktop() {
            IntPtr desktop=OpenInputDesktop(0,false,0x0001);
            if(desktop==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(),"OpenInputDesktop failed");
            try { return Name(desktop); }
            finally { if(!CloseDesktop(desktop)) throw new Win32Exception(Marshal.GetLastWin32Error(),"CloseDesktop failed"); }
        }
        static string Name(IntPtr handle) {
            StringBuilder buffer=new StringBuilder(4096);
            uint needed;
            if(!GetUserObjectInformation(handle,2,buffer,8192,out needed)) throw new Win32Exception(Marshal.GetLastWin32Error(),"GetUserObjectInformationW failed");
            string result=buffer.ToString();
            if(String.IsNullOrWhiteSpace(result) || needed>8192) throw new InvalidOperationException("Invalid input desktop name");
            return result;
        }

        static readonly uint[] CrcTable=BuildCrcTable();
        static uint[] BuildCrcTable() {
            uint[] table=new uint[256];
            for(uint n=0;n<256;n++){
                uint c=n;
                for(int k=0;k<8;k++) c=((c&1)!=0)?(0xEDB88320u^(c>>1)):(c>>1);
                table[n]=c;
            }
            return table;
        }
        static uint Crc(byte[] bytes,int offset,int length) {
            uint crc=0xFFFFFFFFu;
            for(int i=offset;i<offset+length;i++) crc=unchecked(CrcTable[(int)((crc^bytes[i])&0xFF)]^(crc>>8));
            return crc^0xFFFFFFFFu;
        }
        static uint BigUInt32(byte[] bytes,int offset) {
            return ((uint)bytes[offset]<<24)|((uint)bytes[offset+1]<<16)|((uint)bytes[offset+2]<<8)|bytes[offset+3];
        }
        // 逐块校验签名、CRC、IHDR 尺寸、IDAT 和 IEND，拒绝头部伪造或截断图片。
        public static PngEvidence InspectPng(byte[] bytes) {
            if(bytes==null || bytes.Length<57) throw new InvalidDataException("PNG is too short.");
            byte[] signature=new byte[]{137,80,78,71,13,10,26,10};
            for(int i=0;i<signature.Length;i++) if(bytes[i]!=signature[i]) throw new InvalidDataException("PNG signature is invalid.");
            PngEvidence evidence=new PngEvidence();
            int offset=8; bool first=true; bool ended=false; bool idat=false;
            while(offset<bytes.Length){
                if(offset+12>bytes.Length) throw new InvalidDataException("PNG chunk header is incomplete.");
                uint length=BigUInt32(bytes,offset);
                if(length>0x7FFFFFFFu) throw new InvalidDataException("PNG chunk length is invalid.");
                long next=(long)offset+12L+(long)length;
                if(next>bytes.Length) throw new InvalidDataException("PNG chunk length is invalid.");
                string type=Encoding.ASCII.GetString(bytes,offset+4,4);
                uint expected=Crc(bytes,offset+4,4+(int)length);
                uint actual=BigUInt32(bytes,offset+8+(int)length);
                if(expected!=actual) throw new InvalidDataException("PNG chunk CRC is invalid: "+type);
                evidence.Chunks++;
                if(first){
                    if(type!="IHDR" || length!=13) throw new InvalidDataException("PNG must start with a 13-byte IHDR.");
                    evidence.Width=(int)BigUInt32(bytes,offset+8);
                    evidence.Height=(int)BigUInt32(bytes,offset+12);
                    evidence.BitDepth=bytes[offset+16];
                    evidence.ColorType=bytes[offset+17];
                    if(evidence.Width<=0 || evidence.Height<=0 || evidence.Width>16384 || evidence.Height>16384) throw new InvalidDataException("PNG dimensions are invalid.");
                    first=false;
                } else if(type=="IHDR") {
                    throw new InvalidDataException("PNG has a duplicate IHDR.");
                }
                if(type=="IDAT"){ idat=true; evidence.IdatBytes+=(long)length; }
                else if(type=="IEND"){
                    if(length!=0) throw new InvalidDataException("PNG IEND is not empty.");
                    ended=true; offset=(int)next; break;
                }
                offset=(int)next;
            }
            if(first) throw new InvalidDataException("PNG IHDR is missing.");
            if(!idat || evidence.IdatBytes<=0) throw new InvalidDataException("PNG has no image data.");
            if(!ended) throw new InvalidDataException("PNG IEND is missing.");
            if(offset!=bytes.Length) throw new InvalidDataException("PNG has trailing bytes after IEND.");
            using(SHA256 sha=SHA256.Create()) {
                evidence.Sha256=BitConverter.ToString(sha.ComputeHash(bytes)).Replace("-","").ToLowerInvariant();
            }
            return evidence;
        }
    }
}
'@ -Language CSharp
}

function Read-ExtensionsPngEvidence {
    # 只读取本轮输出目录中的普通文件；所有截图都必须经过完整 PNG 块解析。
    # @author kongweiguang
    param([Parameter(Mandatory)][string]$Path)
    if(-not (Test-Path -LiteralPath $Path -PathType Leaf)){throw "Missing native screenshot: $Path"}
    $bytes=[IO.File]::ReadAllBytes($Path)
    return [Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InspectPng($bytes)
}

function Assert-ExtensionsAcceptanceReport {
    # 启动器只接受原生 JSON 类型并逐项复核 UI 回执、隔离、安装集合、关闭范围和帧证据边界。
    # @author kongweiguang
    param($Report,[int]$ProcessId,[string]$Candidate,[string]$PackageHash,[string]$Theme,[string]$Desktop,[string]$InputDesktop)
    if($null -eq $Report -or $Report -isnot [pscustomobject]){throw 'Acceptance report is missing.'}
    [void](Get-ExtensionsStrictInteger -Value $Report.schemaVersion -Field 'report.schemaVersion' -Minimum 1 -Maximum 1)
    [void](Assert-ExtensionsString -Value $Report.mode -Field 'report.mode' -Expected 'private-desktop-composition')
    [void](Assert-ExtensionsString -Value $Report.theme -Field 'report.theme' -Expected $Theme)
    [void](Assert-ExtensionsString -Value $Report.desktopName -Field 'report.desktopName' -Expected $Desktop)
    [void](Assert-ExtensionsString -Value $Report.shutdownScope -Field 'report.shutdownScope' -Expected 'composition-resources-only')
    [void](Assert-ExtensionsString -Value $Report.isolationScope -Field 'report.isolationScope' -Expected 'candidate-win32-thread-snapshot')
    Assert-ExtensionsBoolean -Value $Report.quitReceipt -Field 'report.quitReceipt' -Expected $true
    Assert-ExtensionsBoolean -Value $Report.resourcesShutdownComplete -Field 'report.resourcesShutdownComplete' -Expected $true
    Assert-ExtensionsBoolean -Value $Report.runtimeShutdownObserved -Field 'report.runtimeShutdownObserved' -Expected $false

    $reportProcess=Get-ExtensionsStrictInteger -Value $Report.processId -Field 'report.processId' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
    if($reportProcess -ne $ProcessId){throw 'Acceptance report belongs to another process.'}
    if((Assert-ExtensionsDigest -Value $Report.candidateSha256 -Field 'report.candidateSha256') -cne $Candidate){throw 'Acceptance report belongs to another candidate.'}
    if((Assert-ExtensionsDigest -Value $Report.localPackageSha256 -Field 'report.localPackageSha256') -cne $PackageHash){throw 'Acceptance report used another local package.'}

    $isolation=$Report.nativeIsolation
    if($null -eq $isolation -or $isolation -isnot [pscustomobject]){throw 'Native isolation evidence is missing.'}
    Assert-ExtensionsBoolean -Value $isolation.platformSupported -Field 'isolation.platformSupported' -Expected $true
    Assert-ExtensionsBoolean -Value $isolation.isolationProven -Field 'isolation.isolationProven' -Expected $true
    $isolationProcess=Get-ExtensionsStrictInteger -Value $isolation.processId -Field 'isolation.processId' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
    [void](Get-ExtensionsStrictInteger -Value $isolation.uiThreadId -Field 'isolation.uiThreadId' -Minimum 1 -Maximum ([long][uint32]::MaxValue))
    $isolationDesktop=Assert-ExtensionsString -Value $isolation.uiThreadDesktopName -Field 'isolation.uiThreadDesktopName' -Expected $Desktop
    $isolationInput=Get-ExtensionsStrictString -Value $isolation.inputDesktopName -Field 'isolation.inputDesktopName'
    if($isolationProcess -ne $ProcessId){throw 'Native isolation snapshot belongs to another process.'}
    if($isolationInput -cne $InputDesktop){throw 'Native isolation input desktop does not match the launcher sample.'}
    if($isolationDesktop.Equals($isolationInput,[StringComparison]::OrdinalIgnoreCase)){throw 'Candidate UI thread is on the input desktop.'}
    $foregroundProperty=$isolation.PSObject.Properties['foregroundWindowPid']
    if($null -ne $foregroundProperty -and $null -ne $foregroundProperty.Value){
        $foreground=Get-ExtensionsStrictInteger -Value $foregroundProperty.Value -Field 'isolation.foregroundWindowPid' -Minimum 1 -Maximum ([long][uint32]::MaxValue)
        if($foreground -eq $ProcessId){throw 'Candidate occupied the input desktop foreground.'}
    }

    $dimensions=Get-ExtensionsFrameDimensions -Viewport $Report.viewport -Scale $Report.scaleFactor -Stage 'report'
    $frame=$Report.frameEvidence
    if($null -eq $frame -or $frame -isnot [pscustomobject]){throw 'Frame evidence is missing.'}
    [void](Assert-ExtensionsString -Value $frame.source -Field 'frame.source' -Expected 'gpui-render-to-image')
    [void](Assert-ExtensionsString -Value $frame.validation -Field 'frame.validation' -Expected 'png-structure-crc-and-dimensions')
    Assert-ExtensionsBoolean -Value $frame.featurePixelsChecked -Field 'frame.featurePixelsChecked' -Expected $false
    [void](Assert-ExtensionsString -Value $frame.systemComposition -Field 'frame.systemComposition' -Expected 'not-observed')

    $market=Get-ExtensionsSnapshotPage -Snapshot $Report.market -Field 'market'
    [void](Get-ExtensionsStrictInteger -Value $market.catalog_entries -Field 'market.catalog_entries' -Minimum 2 -Maximum 2)
    Assert-ExtensionsBoolean -Value $market.busy -Field 'market.busy' -Expected $false
    Assert-ExtensionsBoolean -Value $market.offline -Field 'market.offline' -Expected $false
    if($null -ne $market.error){throw 'Market page reported an error.'}

    $selection=Get-ExtensionsSnapshotPage -Snapshot $Report.selection -Field 'selection'
    [void](Assert-ExtensionsString -Value $selection.selected -Field 'selection.selected' -Expected 'acme.market-two')

    $catalogConfirmation=Get-ExtensionsSnapshotPage -Snapshot $Report.catalogConfirmation -Field 'catalogConfirmation'
    [void](Assert-ExtensionsString -Value $catalogConfirmation.confirmation_kind -Field 'catalogConfirmation.confirmation_kind' -Expected 'install')
    [void](Assert-ExtensionsString -Value $catalogConfirmation.confirmation_id -Field 'catalogConfirmation.confirmation_id' -Expected 'acme.market-two')

    $catalogInstalled=Get-ExtensionsSnapshotPage -Snapshot $Report.catalogInstalled -Field 'catalogInstalled'
    [void](Get-ExtensionsStrictInteger -Value $catalogInstalled.pending -Field 'catalogInstalled.pending' -Minimum 1 -Maximum 1)
    Assert-ExtensionsBoolean -Value $catalogInstalled.busy -Field 'catalogInstalled.busy' -Expected $false
    Assert-ExtensionsBoolean -Value $catalogInstalled.confirmation_visible -Field 'catalogInstalled.confirmation_visible' -Expected $false
    Assert-ExtensionsBoolean -Value $catalogInstalled.pending_restart -Field 'catalogInstalled.pending_restart' -Expected $true

    $confirmation=Get-ExtensionsSnapshotPage -Snapshot $Report.confirmation -Field 'confirmation'
    Assert-ExtensionsBoolean -Value $confirmation.confirmation_visible -Field 'confirmation.confirmation_visible' -Expected $true
    Assert-ExtensionsBoolean -Value $confirmation.busy -Field 'confirmation.busy' -Expected $false
    [void](Assert-ExtensionsString -Value $confirmation.confirmation_kind -Field 'confirmation.confirmation_kind' -Expected 'file')
    [void](Assert-ExtensionsString -Value $confirmation.confirmation_id -Field 'confirmation.confirmation_id' -Expected 'acme.file-search')
    $permissions=Get-ExtensionsStringSet -Value $confirmation.confirmation_permissions -Field 'confirmation.confirmation_permissions'
    if($permissions -notcontains 'full-access'){throw 'Real package confirmation did not declare full-access.'}

    $installed=Get-ExtensionsSnapshotPage -Snapshot $Report.installed -Field 'installed'
    [void](Get-ExtensionsStrictInteger -Value $installed.pending -Field 'installed.pending' -Minimum 2 -Maximum 2)
    Assert-ExtensionsBoolean -Value $installed.busy -Field 'installed.busy' -Expected $false
    Assert-ExtensionsBoolean -Value $installed.confirmation_visible -Field 'installed.confirmation_visible' -Expected $false
    Assert-ExtensionsBoolean -Value $installed.pending_restart -Field 'installed.pending_restart' -Expected $true
    if($null -ne $installed.error){throw 'Extensions page reported an error after installation.'}

    $pendingIds=Get-ExtensionsStringSet -Value $Report.pendingPluginIds -Field 'report.pendingPluginIds'
    # 权威状态按字典序排序；集合比较只看唯一身份，不依赖安装发生顺序。
    $pendingSorted=@($pendingIds | Sort-Object -CaseSensitive)
    $expectedSorted=@(@('acme.file-search','acme.market-two') | Sort-Object -CaseSensitive)
    if($pendingIds.Count -ne 2 -or ($pendingSorted -join "`n") -cne ($expectedSorted -join "`n")){throw 'Authoritative pending plugin set is not exactly the two expected installs.'}
    $pendingCount=Get-ExtensionsStrictInteger -Value $Report.pendingCountReadBack -Field 'report.pendingCountReadBack' -Minimum 2 -Maximum 2
    if($pendingCount -ne $pendingIds.Count){throw 'Pending count does not match the authoritative plugin identities.'}
    return $dimensions
}

function Stop-ExtensionsOwnedProcess {
    # Job 查询失败仍尝试终止本轮 Job，并分别保留查询、终止、等待、桌面和句柄释放错误。
    # @author kongweiguang
    param([Parameter(Mandatory)]$Process)
    $errors=[Collections.Generic.List[Exception]]::new()
    $active=$null
    try {$active=Get-ExtensionsJobActiveProcesses -Process $Process -Stage 'cleanup-query'} catch {$errors.Add($_.Exception)}
    if($null -eq $active -or $active -gt 0){
        try {$Process.TerminateJob(1)} catch {$errors.Add($_.Exception)}
    }
    try {
        if(-not $Process.WaitForExit(10000)){throw 'Owned candidate did not terminate.'}
        $deadline=[DateTime]::UtcNow.AddSeconds(10)
        while((Get-ExtensionsJobActiveProcesses -Process $Process -Stage 'cleanup-wait') -gt 0 -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
        if((Get-ExtensionsJobActiveProcesses -Process $Process -Stage 'cleanup-final') -gt 0){throw 'Owned Job has live descendants.'}
    } catch {$errors.Add($_.Exception)}
    try {$Process.Dispose()} catch {$errors.Add($_.Exception)}
    try {
        if(-not $Process.DesktopClosed){throw 'Owned private desktop handle did not close.'}
    } catch {$errors.Add($_.Exception)}
    if($errors.Count -eq 1){throw $errors[0]}
    if($errors.Count -gt 1){throw [AggregateException]::new('Owned extensions candidate cleanup had multiple failures.',$errors.ToArray())}
}

function Stop-ExtensionsFixture {
    # 夹具不在候选 Job 内，必须独立停止、等待、回读日志和释放；所有失败都要保留。
    # @author kongweiguang
    param([Parameter(Mandatory)]$Fixture,$StdoutTask,$StderrTask,[string]$Base)
    $errors=[Collections.Generic.List[Exception]]::new()
    $hasExited=$false
    try {$hasExited=[bool]$Fixture.HasExited} catch {}
    if(-not $hasExited){
        try {$Fixture.Kill()} catch {$errors.Add($_.Exception)}
    }
    $exited=$false
    try {
        $exited=[bool]$Fixture.WaitForExit(10000)
        if(-not $exited){throw 'Owned fixture did not stop.'}
    } catch {$errors.Add($_.Exception)}
    if($exited){
        try {
            if($null -ne $StdoutTask){$StdoutTask.GetAwaiter().GetResult()|Set-Content -LiteralPath (Join-Path $Base 'fixture.stdout.log') -Encoding utf8}
        } catch {$errors.Add($_.Exception)}
        try {
            if($null -ne $StderrTask){$StderrTask.GetAwaiter().GetResult()|Set-Content -LiteralPath (Join-Path $Base 'fixture.stderr.log') -Encoding utf8}
        } catch {$errors.Add($_.Exception)}
    }
    try {$Fixture.Dispose()} catch {$errors.Add($_.Exception)}
    if($errors.Count -eq 1){throw $errors[0]}
    if($errors.Count -gt 1){throw [AggregateException]::new('Owned extensions fixture cleanup had multiple failures.',$errors.ToArray())}
}

function Initialize-ExtensionsLauncher {
    # 共用已校验的后台启动器与首线程挂起Job绑定，差异仅限命令和独立类型名。
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
    $old='--webview-probe --output '
    $new='--plugin-extensions-acceptance --local-package " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_PLUGIN_EXTENSIONS_PACKAGE")) + " --output-dir '
    if($source.Split($old).Count -ne 2){throw 'Native launcher command changed.'}
    $source=$source.Replace($old,$new).Replace('Kerminal.WebViewProbe','Kerminal.PluginExtensionsAcceptance')
    # 本验收用GPUI私有桌面帧，不需要WGC；编译前恢复桌面创建和隐藏启动，保留原Job绑定。
    $compile='Add-Type -TypeDefinition ($source + "`n" + $jobSource) -Language CSharp'
    $private=@'
    $source = Replace-WebViewLauncherPart $source 'if (!String.Equals(desktopName, "Default", StringComparison.OrdinalIgnoreCase))' 'if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))'
    $source = Replace-WebViewLauncherPart $source 'EntryPoint = "OpenDesktopW"' 'EntryPoint = "CreateDesktopW"'
    $source = Replace-WebViewLauncherPart $source 'private static extern IntPtr OpenDesktop(string desktopName, uint flags, bool inherit, uint desiredAccess);' 'private static extern IntPtr CreateDesktop(string desktopName, IntPtr device, IntPtr devmode, uint flags, uint desiredAccess, IntPtr securityAttributes);'
    $source = Replace-WebViewLauncherPart $source 'OpenDesktop(desktopName, 0, false, desktopAccess)' 'CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero)'
    $source = Replace-WebViewLauncherPart $source 'startupInfo.StartupInfo.Flags = StartfUseStdHandles | 1; startupInfo.StartupInfo.ShowWindow = 4;' 'startupInfo.StartupInfo.Flags = StartfUseStdHandles;'
'@
    if($source.Split($compile).Count -ne 2){throw 'Native launcher compile point changed.'}
    $source=$source.Replace($compile,($private+"`n"+$compile))
    . ([scriptblock]::Create($source))
}

$executable=(Resolve-Path -LiteralPath $ExecutablePath).Path
$package=(Resolve-Path -LiteralPath $LocalPackagePath).Path
$base=[IO.Path]::GetFullPath($OutputDirectory)
$repositoryRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
if(Test-Path -LiteralPath $base){throw 'Output root must not exist.'}
[void][IO.Directory]::CreateDirectory($base)
$candidate=Assert-ExtensionsDigest -Value ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()) -Field 'candidate'
$packageHash=Assert-ExtensionsDigest -Value ((Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant()) -Field 'localPackage'
$names=@('KERMINAL_PLUGIN_EXTENSIONS_PACKAGE','KERMINAL_PLUGIN_EXTENSIONS_DESKTOP','KERMINAL_PLUGIN_CATALOG_URL','KERMINAL_PLUGIN_CATALOG_PUBKEY','KERMINAL_WEBVIEW_PROBE_THEME','KERMINAL_WEBVIEW_BACKGROUND')
$previous=@{}
foreach($name in $names){$previous[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
$process=$null; $fixture=$null; $fixtureOutput=$null; $fixtureError=$null; $receipts=@(); $failure=$null
try {
    $start=[Diagnostics.ProcessStartInfo]::new()
    $start.FileName=$PythonPath
    $start.UseShellExecute=$false
    $start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true
    $start.RedirectStandardError=$true
    $start.ArgumentList.Add((Join-Path $PSScriptRoot 'plugin-extensions-fixture.py'))
    $start.ArgumentList.Add('--ready')
    $start.ArgumentList.Add((Join-Path $base 'fixture-ready.json'))
    $fixture=[Diagnostics.Process]::Start($start)
    $fixtureOutput=$fixture.StandardOutput.ReadToEndAsync()
    $fixtureError=$fixture.StandardError.ReadToEndAsync()
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    while(-not (Test-Path -LiteralPath (Join-Path $base 'fixture-ready.json'))){
        if($fixture.HasExited){throw "Signed fixture exited: $($fixtureError.GetAwaiter().GetResult())"}
        if([DateTime]::UtcNow -gt $deadline){throw 'Signed fixture startup timed out.'}
        Start-Sleep -Milliseconds 100
    }
    $ready=Get-Content -LiteralPath (Join-Path $base 'fixture-ready.json') -Raw|ConvertFrom-Json
    $catalogUrl=Get-ExtensionsStrictString -Value $ready.url -Field 'fixture.url'
    $catalogKey=Get-ExtensionsStrictString -Value $ready.key -Field 'fixture.key'
    $catalogHash=Assert-ExtensionsDigest -Value $ready.catalogSha256 -Field 'fixture.catalogSha256'
    if($catalogUrl -cnotmatch '^http://127\.0\.0\.1:\d{1,5}/catalog\.json$'){throw 'Signed fixture URL is not a loopback catalog.'}
    try {[void][Convert]::FromBase64String($catalogKey)} catch {throw 'Signed fixture public key is not base64.'}
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_EXTENSIONS_PACKAGE',$package,'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_CATALOG_URL',$catalogUrl,'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_CATALOG_PUBKEY',$catalogKey,'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_BACKGROUND','0','Process')
    Initialize-ExtensionsLauncher
    Initialize-ExtensionsNativeEvidence
    foreach($currentTheme in $(if($Theme -ceq 'all'){@('light','dark','system')}else{@($Theme)})){
        $output=Join-Path $base $currentTheme
        if(Test-Path -LiteralPath $output){throw "Per-theme output already exists: $currentTheme"}
        $desktop='KerminalRail-settings-'+[Guid]::NewGuid().ToString('N')
        # 每轮启动前重新采样输入桌面，避免用户中途切换桌面后把合法回执误判成候选隔离失败。
        $inputDesktop=[Kerminal.PluginExtensionsAcceptance.NativeEvidence]::InputDesktop()
        if([string]::IsNullOrWhiteSpace($inputDesktop) -or $desktop.Equals($inputDesktop,[StringComparison]::OrdinalIgnoreCase)){throw 'The fresh acceptance desktop is not independent of input.'}
        [Environment]::SetEnvironmentVariable('KERMINAL_WEBVIEW_PROBE_THEME',$currentTheme,'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_EXTENSIONS_DESKTOP',$desktop,'Process')
        $stderrPath=Join-Path $base "$currentTheme.stderr.log"
        $process=[Kerminal.PluginExtensionsAcceptance.NativeDesktopLauncher]::Start($executable,$repositoryRoot,$output,(Join-Path $base "$currentTheme.stdout.log"),$stderrPath,$desktop,1200,800)
        if($process.ProcessId -le 0 -or $process.DesktopName -cne $desktop){throw 'The launch handle does not belong to this private desktop.'}
        if(-not $process.WaitForExit([uint32]($TimeoutSeconds*1000))){throw "Extensions acceptance timed out: $currentTheme"}
        $exitCode=$process.GetExitCode()
        if($exitCode -ne 0){throw "Extensions acceptance failed: $currentTheme; exitCode=$exitCode; stderr=$stderrPath"}
        $reportPath=Join-Path $output 'report.json'
        if(-not (Test-Path -LiteralPath $reportPath -PathType Leaf)){throw "Extensions acceptance did not write report.json: $currentTheme; stderr=$stderrPath"}
        $report=Get-Content -LiteralPath $reportPath -Raw|ConvertFrom-Json
        $dimensions=Assert-ExtensionsAcceptanceReport -Report $report -ProcessId $process.ProcessId -Candidate $candidate -PackageHash $packageHash -Theme $currentTheme -Desktop $desktop -InputDesktop $inputDesktop
        $frames=@()
        foreach($phase in @('catalog','selection','catalog-permissions','permissions','pending-restart')){
            $png=Read-ExtensionsPngEvidence -Path (Join-Path $output "$phase.png")
            Assert-ExtensionsFrameDimensions -Evidence $png -Expected $dimensions -Phase $phase
            $frames+=[pscustomobject]@{Phase=$phase;Width=$png.Width;Height=$png.Height;IdatBytes=$png.IdatBytes;Sha256=$png.Sha256}
        }
        $deadline=[DateTime]::UtcNow.AddSeconds(10)
        while((Get-ExtensionsJobActiveProcesses -Process $process -Stage 'normal-wait') -gt 0 -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
        $jobActive=Get-ExtensionsJobActiveProcesses -Process $process -Stage 'normal-final'
        if($jobActive -ne 0){throw 'Candidate left live Job members after normal shutdown.'}
        $processId=$process.ProcessId
        $process.Dispose()
        if(-not $process.DesktopClosed){throw 'Owned private desktop handle did not close.'}
        $process=$null
        $receipts+=[pscustomobject]@{
            Theme=$currentTheme;ProcessId=$processId;Report="$currentTheme/report.json";ExitCode=$exitCode
            NormalExitJobMembers=$jobActive;JobScope='owned-kill-on-close-job-members-only';NonJobProcessesProven=$false
            DesktopName=$desktop;InputDesktopName=$inputDesktop;DesktopHandleClosed=$true;DesktopHandleScope='candidate-owned-handle-only'
            FrameDimensions=$dimensions;Frames=$frames
        }
    }
    if((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $candidate -or (Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant() -cne $packageHash){throw 'Candidate or package changed during acceptance.'}
    [pscustomobject]@{
        SchemaVersion=1;CandidateSha256=$candidate;LocalPackageSha256=$packageHash;CatalogSha256=$catalogHash
        Runs=$receipts
        IsolationEvidence='candidate-win32-thread-snapshot';IndependentDesktopResample=$false
        FrameEvidenceValidation='png-structure-crc-and-dimensions';FrameFeaturePixelsChecked=$false;SystemComposition='not-observed'
        ShutdownScope='composition-resources-only';RuntimeShutdownObserved=$false
        JobScope='owned-kill-on-close-job-members-only';NonJobProcessesProven=$false;DesktopHandleScope='candidate-owned-handle-only'
        FixtureCleanupScope='finally-owned-process-handle';RunnerForcedKillProven=$false
        NativeImeTested=$false;DpiSwitchTested=$false;SleepResumeTested=$false
    }|ConvertTo-Json -Depth 8|Set-Content -LiteralPath (Join-Path $base 'acceptance-report.json') -Encoding utf8
}
catch {
    $failure=$_
    throw
}
finally {
    $cleanupErrors=[Collections.Generic.List[Exception]]::new()
    if($null -ne $process){
        try {Stop-ExtensionsOwnedProcess -Process $process} catch {$cleanupErrors.Add($_.Exception)}
    }
    if($null -ne $fixture){
        try {Stop-ExtensionsFixture -Fixture $fixture -StdoutTask $fixtureOutput -StderrTask $fixtureError -Base $base} catch {$cleanupErrors.Add($_.Exception)}
    }
    foreach($name in $names){
        try {[Environment]::SetEnvironmentVariable($name,$previous[$name],'Process')} catch {$cleanupErrors.Add($_.Exception)}
    }
    if($cleanupErrors.Count -gt 0){
        $all=[Collections.Generic.List[Exception]]::new()
        if($null -ne $failure){$all.Add($failure.Exception)}
        foreach($error in $cleanupErrors){
            if($error -is [AggregateException]){
                foreach($inner in $error.Flatten().InnerExceptions){$all.Add($inner)}
            } else {$all.Add($error)}
        }
        if($all.Count -eq 1){throw $all[0]}
        $message=if($null -ne $failure){'Acceptance and owned-resource cleanup both failed.'}else{'Owned-resource cleanup failed after acceptance.'}
        throw [AggregateException]::new($message,$all.ToArray())
    }
}
