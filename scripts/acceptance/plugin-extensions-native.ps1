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

function Get-ExtensionsProperty {
    # 缺失字段与 null 必须区分；StrictMode 直接访问缺失属性无法把错误定位到调用方字段名。
    # 返回值用一元逗号包一层：PowerShell 函数输出会枚举数组，空数组直接 return 会变成 $null，
    # 使 batch_items 等合法空集合被严格类型检查误判；逗号包装同时保留单项数组、标量和 null 语义。
    # @author kongweiguang
    param($Object,[string]$Name,[string]$Field)
    if($null -eq $Object -or $Object -isnot [pscustomobject]){throw "Invalid object field: $Field"}
    $property=$Object.PSObject.Properties[$Name]
    if($null -eq $property){throw "Missing field: $Field"}
    return ,$property.Value
}

function Get-ExtensionsGroups {
    # 统一列表的组顺序、计数和折叠事实必须来自真实快照；重复、未知或乱序组都不能通过。
    # @author kongweiguang
    param($Page,[string]$Field)
    $value=Get-ExtensionsProperty -Object $Page -Name 'groups' -Field "$Field.groups"
    if($value -isnot [System.Collections.IList]){throw "Invalid group array field: $Field.groups"}
    $rank=[Collections.Generic.Dictionary[string,int]]::new([StringComparer]::Ordinal)
    $rank.Add('installed',0); $rank.Add('builtin',1); $rank.Add('pending',2); $rank.Add('market',3)
    $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $last=-1
    $groups=[Collections.Generic.List[object]]::new()
    foreach($group in $value){
        if($null -eq $group -or $group -isnot [pscustomobject]){throw "Invalid group entry: $Field.groups"}
        $kind=Get-ExtensionsStrictString -Value (Get-ExtensionsProperty -Object $group -Name 'kind' -Field "$Field.groups.kind") -Field "$Field.groups.kind"
        if(-not $rank.ContainsKey($kind)){throw "Unknown group kind: $Field.groups.kind"}
        if(-not $seen.Add($kind)){throw "Duplicate group kind: $Field.groups.kind"}
        $count=Get-ExtensionsStrictInteger -Value (Get-ExtensionsProperty -Object $group -Name 'count' -Field "$Field.groups.count") -Field "$Field.groups.count" -Minimum 1
        $collapsed=Get-ExtensionsStrictBoolean -Value (Get-ExtensionsProperty -Object $group -Name 'collapsed' -Field "$Field.groups.collapsed") -Field "$Field.groups.collapsed"
        $current=$rank[$kind]
        if($current -le $last){throw "Group order is not canonical: $Field.groups.kind"}
        $last=$current
        $groups.Add([pscustomobject]@{Kind=$kind;Count=$count;Collapsed=$collapsed})
    }
    if($groups.Count -eq 0){throw "Empty group array: $Field.groups"}
    return $groups.ToArray()
}

function Get-ExtensionsGroup {
    # 按稳定 kind 查找组；缺失返回 null，由调用方决定该组是否必须存在。
    # @author kongweiguang
    param($Groups,[string]$Kind)
    foreach($group in $Groups){
        if($group.Kind -ceq $Kind){return $group}
    }
    return $null
}

function Assert-ExtensionsUnifiedPage {
    # 统一列表快照必须同时证明分组去重、内置折叠事实和批量范围；不能用返回态或 pending 数量代替。
    # @author kongweiguang
    param($Snapshot,[string]$Field,[bool]$BuiltinCollapsed,[bool]$BackToList=$true)
    $page=Get-ExtensionsSnapshotPage -Snapshot $Snapshot -Field $Field
    [void](Get-ExtensionsStrictInteger -Value (Get-ExtensionsProperty -Object $page -Name 'catalog_entries' -Field "$Field.catalog_entries") -Field "$Field.catalog_entries" -Minimum 2 -Maximum 2)
    [void](Get-ExtensionsStrictInteger -Value (Get-ExtensionsProperty -Object $page -Name 'pending' -Field "$Field.pending") -Field "$Field.pending" -Minimum 2 -Maximum 2)
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'offline' -Field "$Field.offline") -Field "$Field.offline" -Expected $false
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'busy' -Field "$Field.busy") -Field "$Field.busy" -Expected $false
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'confirmation_visible' -Field "$Field.confirmation_visible") -Field "$Field.confirmation_visible" -Expected $false
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'pending_restart' -Field "$Field.pending_restart") -Field "$Field.pending_restart" -Expected $true
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'back_to_list' -Field "$Field.back_to_list") -Field "$Field.back_to_list" -Expected $BackToList
    foreach($name in @('error','catalog_error','state_error','readme_error')){
        if($null -ne (Get-ExtensionsProperty -Object $page -Name $name -Field "$Field.$name")){throw "Unified list reported $name`: $Field"}
    }
    [void](Get-ExtensionsStrictInteger -Value (Get-ExtensionsProperty -Object $page -Name 'update_candidates' -Field "$Field.update_candidates") -Field "$Field.update_candidates" -Minimum 0 -Maximum 0)
    $batchItems=@(Get-ExtensionsStringSet -Value (Get-ExtensionsProperty -Object $page -Name 'batch_items' -Field "$Field.batch_items") -Field "$Field.batch_items")
    if($batchItems.Count -ne 0){throw "Unexpected batch items in unified list: $Field.batch_items"}
    if($null -ne (Get-ExtensionsProperty -Object $page -Name 'batch_progress' -Field "$Field.batch_progress")){throw "Unexpected batch progress in unified list: $Field.batch_progress"}
    $groups=@(Get-ExtensionsGroups -Page $page -Field $Field)
    if($groups.Count -ne 3){throw "Unified list must contain exactly builtin, pending and market groups: $Field.groups"}
    if($null -ne (Get-ExtensionsGroup -Groups $groups -Kind 'installed')){throw "Current fixture has no installed records; installed group must be absent: $Field.groups"}
    $builtin=Get-ExtensionsGroup -Groups $groups -Kind 'builtin'
    if($null -eq $builtin){throw "Builtin group is missing: $Field.groups"}
    if($builtin.Collapsed -ne $BuiltinCollapsed){throw "Builtin group collapsed fact does not match the phase: $Field"}
    $pending=Get-ExtensionsGroup -Groups $groups -Kind 'pending'
    if($null -eq $pending -or $pending.Count -ne 2){throw "Pending group must contain the two submitted installs: $Field.groups"}
    $market=Get-ExtensionsGroup -Groups $groups -Kind 'market'
    if($null -eq $market -or $market.Count -ne 1){throw "Market group must contain only the remaining catalog entry: $Field.groups"}
    $installed=Get-ExtensionsStrictInteger -Value (Get-ExtensionsProperty -Object $page -Name 'installed' -Field "$Field.installed") -Field "$Field.installed" -Minimum 1
    if($installed -ne $builtin.Count){throw "Installed count must equal the builtin group when no external records exist: $Field.installed"}
    return $page
}

function Assert-ExtensionsSourcePage {
    # 来源场景的三个快照必须证明弹层真实开关、保存无错误且来源仍是夹具地址；
    # 缺失字段不能默认当成 false/null，否则缺少 UI 证据也会假通过。
    # @author kongweiguang
    param($Snapshot,[string]$Field,[string]$Url,[bool]$DialogOpen,[bool]$Saving)
    $page=Get-ExtensionsSnapshotPage -Snapshot $Snapshot -Field $Field
    [void](Assert-ExtensionsString -Value (Get-ExtensionsProperty -Object $page -Name 'source_url' -Field "$Field.source_url") -Field "$Field.source_url" -Expected $Url)
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'source_dialog_open' -Field "$Field.source_dialog_open") -Field "$Field.source_dialog_open" -Expected $DialogOpen
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $page -Name 'source_saving' -Field "$Field.source_saving") -Field "$Field.source_saving" -Expected $Saving
    if($null -ne (Get-ExtensionsProperty -Object $page -Name 'source_error' -Field "$Field.source_error")){throw "Source dialog reported an error: $Field.source_error"}
    return $page
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
    # 逻辑视口由各阶段真实窗口尺寸给出；截图像素尺寸必须是该视口乘缩放因子后的实际绘制目标。
    # 窄窗阶段不能沿用宽窗 1200x800，否则会把错误尺寸的截图当成通过。
    # @author kongweiguang
    param($Viewport,$Scale,[string]$Stage,[double]$ExpectedWidth=1200.0,[double]$ExpectedHeight=800.0)
    if($Viewport -isnot [System.Collections.IList] -or $Viewport.Count -ne 2){throw "Invalid viewport: $Stage"}
    $width=Get-ExtensionsStrictNumber -Value $Viewport[0] -Field 'viewport.width'
    $height=Get-ExtensionsStrictNumber -Value $Viewport[1] -Field 'viewport.height'
    if([Math]::Abs($width-$ExpectedWidth) -gt 0.5 -or [Math]::Abs($height-$ExpectedHeight) -gt 0.5){throw "Unexpected logical viewport: $Stage"}
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
    # 键盘证据必须与当前候选一致：列表焦点由一次指针点击建立，随后是真实 Home/End/方向键。
    [void](Assert-ExtensionsString -Value $Report.inputMode -Field 'report.inputMode' -Expected 'synthetic GPUI pointer dispatch to focus the extension list plus real Home/End/arrow keystrokes; production file-picker result entry')

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

    # 来源保存证据必须来自真实页头点击与设置回读：配置、运行源和启动器注入地址
    # 三者相同，且弹层前/打开/保存后三个快照都保留正确的来源与错误状态。
    $source=$Report.source
    if($null -eq $source -or $source -isnot [pscustomobject]){throw 'Source save evidence is missing.'}
    [void](Assert-ExtensionsString -Value $source.entry -Field 'source.entry' -Expected 'real header button opens the window dialog; unchanged backfilled fixture URL is saved through the settings CAS and load_settings read-back')
    [void](Assert-ExtensionsString -Value $source.screenshot -Field 'source.screenshot' -Expected 'source-dialog.png')
    $configUrl=Get-ExtensionsStrictString -Value $source.configUrl -Field 'source.configUrl'
    $runtimeUrl=Get-ExtensionsStrictString -Value $source.runtimeUrl -Field 'source.runtimeUrl'
    $expectedUrl=Get-ExtensionsStrictString -Value $source.expectedUrl -Field 'source.expectedUrl'
    foreach($url in @($configUrl,$runtimeUrl,$expectedUrl)){
        if($url -cnotmatch '^http://127\.0\.0\.1:\d{1,5}/catalog\.json$'){throw 'Source URL is not the loopback fixture catalog: source'}
    }
    if($configUrl -cne $runtimeUrl -or $runtimeUrl -cne $expectedUrl){throw 'Source config, runtime and expected URLs disagree: source'}
    Assert-ExtensionsBoolean -Value $source.configMatchesRuntime -Field 'source.configMatchesRuntime' -Expected $true
    [void](Assert-ExtensionsSourcePage -Snapshot $source.before -Field 'source.before' -Url $expectedUrl -DialogOpen $false -Saving $false)
    [void](Assert-ExtensionsSourcePage -Snapshot $source.dialog -Field 'source.dialog' -Url $expectedUrl -DialogOpen $true -Saving $false)
    $sourceSaved=Assert-ExtensionsSourcePage -Snapshot $source.saved -Field 'source.saved' -Url $expectedUrl -DialogOpen $false -Saving $false
    [void](Get-ExtensionsStrictInteger -Value (Get-ExtensionsProperty -Object $sourceSaved -Name 'catalog_entries' -Field 'source.saved.catalog_entries') -Field 'source.saved.catalog_entries' -Minimum 2 -Maximum 2)
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $sourceSaved -Name 'offline' -Field 'source.saved.offline') -Field 'source.saved.offline' -Expected $false
    Assert-ExtensionsBoolean -Value (Get-ExtensionsProperty -Object $sourceSaved -Name 'busy' -Field 'source.saved.busy') -Field 'source.saved.busy' -Expected $false

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

    # 统一列表证据必须来自真实分组快照：内置组默认收起、展开后为 false、再次收起为 true；
    # 当前夹具没有已安装外部记录，因此 installed 组缺失、候选数为 0，批量更新未原生覆盖。
    if($null -ne $Report.PSObject.Properties['installedTab']){throw 'The report still contains installedTab evidence.'}
    $unified=$Report.unifiedList
    if($null -eq $unified -or $unified -isnot [pscustomobject]){throw 'Unified list evidence is missing.'}
    [void](Assert-ExtensionsString -Value $unified.entry -Field 'unifiedList.entry' -Expected 'real unified list; builtin group header click toggles expansion')
    $unifiedShots=Get-ExtensionsStringSet -Value $unified.screenshots -Field 'unifiedList.screenshots'
    $expectedUnifiedShots=@('unified-list.png','builtin-expanded.png','builtin-collapsed.png')
    if($unifiedShots.Count -ne $expectedUnifiedShots.Count){throw 'Invalid unified screenshots: unifiedList.screenshots'}
    foreach($shot in $expectedUnifiedShots){if($unifiedShots -cnotcontains $shot){throw 'Invalid unified screenshots: unifiedList.screenshots'}}
    Assert-ExtensionsBoolean -Value $unified.installedGroupObserved -Field 'unifiedList.installedGroupObserved' -Expected $false
    [void](Get-ExtensionsStrictInteger -Value $unified.updateCandidates -Field 'unifiedList.updateCandidates' -Minimum 0 -Maximum 0)
    Assert-ExtensionsBoolean -Value $unified.batchUpdateNativeCovered -Field 'unifiedList.batchUpdateNativeCovered' -Expected $false
    [void](Get-ExtensionsStrictString -Value $unified.batchUpdateNotCoveredReason -Field 'unifiedList.batchUpdateNotCoveredReason')
    [void](Assert-ExtensionsUnifiedPage -Snapshot $unified.collapsed -Field 'unifiedList.collapsed' -BuiltinCollapsed $true)
    [void](Assert-ExtensionsUnifiedPage -Snapshot $unified.expanded -Field 'unifiedList.expanded' -BuiltinCollapsed $false)
    [void](Assert-ExtensionsUnifiedPage -Snapshot $unified.collapsedAgain -Field 'unifiedList.collapsedAgain' -BuiltinCollapsed $true)

    $narrow=$Report.narrow
    if($null -eq $narrow -or $narrow -isnot [pscustomobject]){throw 'Narrow evidence is missing.'}
    [void](Get-ExtensionsFrameDimensions -Viewport $narrow.viewport -Scale $Report.scaleFactor -Stage 'narrow' -ExpectedWidth 700.0 -ExpectedHeight 800.0)
    $narrowShots=Get-ExtensionsStringSet -Value $narrow.screenshots -Field 'narrow.screenshots'
    $expectedNarrowShots=@('narrow-list.png','narrow-detail.png','narrow-list-returned.png')
    if($narrowShots.Count -ne $expectedNarrowShots.Count){throw 'Invalid narrow screenshots: narrow.screenshots'}
    foreach($shot in $expectedNarrowShots){if($narrowShots -cnotcontains $shot){throw 'Invalid narrow screenshots: narrow.screenshots'}}
    $narrowList=Assert-ExtensionsUnifiedPage -Snapshot $narrow.list -Field 'narrow.list' -BuiltinCollapsed $true
    $narrowDetail=Assert-ExtensionsUnifiedPage -Snapshot $narrow.detail -Field 'narrow.detail' -BuiltinCollapsed $true -BackToList $false
    [void](Assert-ExtensionsString -Value $narrowDetail.selected -Field 'narrow.detail.selected' -Expected 'acme.market-one')
    $narrowReturned=Assert-ExtensionsUnifiedPage -Snapshot $narrow.returned -Field 'narrow.returned' -BuiltinCollapsed $true
    foreach($page in @($narrowList,$narrowDetail,$narrowReturned)){
        $width=Get-ExtensionsStrictNumber -Value $page.content_width -Field 'narrow.content_width'
        if($width -ge 700.0){throw 'Narrow evidence did not report a single-column content width: narrow.content_width'}
    }
    if($narrow.dpi200PercentTested -isnot [bool] -or $narrow.dpi200PercentTested){throw 'Narrow DPI claim is invalid: narrow.dpi200PercentTested'}

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

function Assert-ExtensionsFixtureLogs {
    # 真实网络日志必须证明只读取 JSON 目录；任何 minisig 请求都说明客户端仍在验签，
    # 与当前“目录只读 JSON”的契约冲突，不能把夹具流程当成通过。
    # @author kongweiguang
    param([Parameter(Mandatory)][string]$Base)
    $catalogRequests=0
    foreach($name in @('fixture.stdout.log','fixture.stderr.log')){
        $path=Join-Path $Base $name
        if(-not (Test-Path -LiteralPath $path -PathType Leaf)){throw "Missing fixture log: $name"}
        $text=[IO.File]::ReadAllText($path)
        if($text -match '(?i)minisig'){throw "Fixture log contains a signature request: $name"}
        $catalogRequests+=([regex]::Matches($text,'fixture GET /catalog\.json(?:\s|$)')).Count
    }
    if($catalogRequests -lt 1){throw 'Fixture logs contain no catalog JSON request.'}
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
$names=@('KERMINAL_PLUGIN_EXTENSIONS_PACKAGE','KERMINAL_PLUGIN_EXTENSIONS_DESKTOP','KERMINAL_PLUGIN_CATALOG_URL','KERMINAL_WEBVIEW_PROBE_THEME','KERMINAL_WEBVIEW_BACKGROUND')
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
        if($fixture.HasExited){throw "Fixture exited: $($fixtureError.GetAwaiter().GetResult())"}
        if([DateTime]::UtcNow -gt $deadline){throw 'Fixture startup timed out.'}
        Start-Sleep -Milliseconds 100
    }
    $ready=Get-Content -LiteralPath (Join-Path $base 'fixture-ready.json') -Raw|ConvertFrom-Json
    $catalogUrl=Get-ExtensionsStrictString -Value $ready.url -Field 'fixture.url'
    $catalogHash=Assert-ExtensionsDigest -Value $ready.catalogSha256 -Field 'fixture.catalogSha256'
    if($catalogUrl -cnotmatch '^http://127\.0\.0\.1:\d{1,5}/catalog\.json$'){throw 'Fixture URL is not a loopback catalog.'}
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_EXTENSIONS_PACKAGE',$package,'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_PLUGIN_CATALOG_URL',$catalogUrl,'Process')
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
        # 宽窗与窄窗截图分别按各自真实逻辑视口计算像素尺寸；不能用一个 1200x800
        # 的期望值盖过所有阶段，否则窄窗证据会以错误尺寸通过。
        $narrowDimensions=Get-ExtensionsFrameDimensions -Viewport $report.narrow.viewport -Scale $report.scaleFactor -Stage 'narrow' -ExpectedWidth 700.0 -ExpectedHeight 800.0
        $phasePlan=@(
            @{Phase='catalog';Dimensions=$dimensions},
            @{Phase='source-dialog';Dimensions=$dimensions},
            @{Phase='selection';Dimensions=$dimensions},
            @{Phase='catalog-permissions';Dimensions=$dimensions},
            @{Phase='permissions';Dimensions=$dimensions},
            @{Phase='pending-restart';Dimensions=$dimensions},
            @{Phase='unified-list';Dimensions=$dimensions},
            @{Phase='builtin-expanded';Dimensions=$dimensions},
            @{Phase='builtin-collapsed';Dimensions=$dimensions},
            @{Phase='narrow-list';Dimensions=$narrowDimensions},
            @{Phase='narrow-detail';Dimensions=$narrowDimensions},
            @{Phase='narrow-list-returned';Dimensions=$narrowDimensions}
        )
        foreach($phase in $phasePlan){
            $png=Read-ExtensionsPngEvidence -Path (Join-Path $output "$($phase.Phase).png")
            Assert-ExtensionsFrameDimensions -Evidence $png -Expected $phase.Dimensions -Phase $phase.Phase
            $frames+=[pscustomobject]@{Phase=$phase.Phase;Width=$png.Width;Height=$png.Height;ViewportWidth=$phase.Dimensions.ViewportWidth;ViewportHeight=$phase.Dimensions.ViewportHeight;IdatBytes=$png.IdatBytes;Sha256=$png.Sha256}
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
            FrameDimensions=$dimensions;NarrowFrameDimensions=$narrowDimensions;Frames=$frames
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
    # 只有验收本身成功且夹具确实启动过时才要求网络日志；失败路径优先保留原始原因。
    if($null -eq $failure -and $null -ne $fixture){
        try {Assert-ExtensionsFixtureLogs -Base $base} catch {$cleanupErrors.Add($_.Exception)}
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
