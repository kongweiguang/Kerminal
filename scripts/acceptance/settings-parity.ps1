# @author kongweiguang
param(
    [string]$ExecutablePath,

    [switch]$SkipBuild,

    [switch]$McpOnly,

    [switch]$SuggestionsOnly,

    [switch]$WorkbenchOnly,

    [switch]$WorkbenchMotion,

    [switch]$RightRailSplitSwitch,

    [string]$RightRailPowerShell,

    [switch]$SshSuggestions,

    [ValidateSet('debug', 'release')]
    [string]$BuildProfile = 'debug',

    [ValidateRange(800, 3840)]
    [int]$Width = 1440,

    [ValidateRange(600, 2160)]
    [int]$Height = 900,

    [ValidateRange(30, 600)]
    [int]$TimeoutSeconds = 360
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-NoReparseComponent {
    # 清理本轮生成的目录前，从目标一路走到受控根，拒绝任何 reparse point；
    # 只看目标自身不够，父目录被替换成 junction 时递归删除会越界。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $resolvedPath = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd($separators)
    $prefix = $resolvedRoot + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must remain below the owned diagnostics root."
    }
    $cursor = $resolvedPath
    while ($true) {
        if (-not [IO.Directory]::Exists($cursor)) {
            throw "$Label path component is missing: $cursor"
        }
        $attributes = [IO.File]::GetAttributes($cursor)
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "$Label contains a reparse point: $cursor"
        }
        if ($cursor.Equals($resolvedRoot, [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent -or $parent.FullName.Equals($cursor, [StringComparison]::OrdinalIgnoreCase)) {
            throw "$Label escaped its owned diagnostics root."
        }
        $cursor = $parent.FullName
    }
}

function Assert-NoReparseTree {
    # 递归删除前逐项检查 reparse point；只检查路径自身不足以防子项被替换成 junction。
    # 调用方必须先证明路径属于本轮诊断根，这里再做一次所有权与树内检查。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Label
    )

    Assert-PathWithinRoot -Path $Path -Root $Root -Label $Label
    Assert-NoReparseComponent -Path $Path -Root $Root -Label $Label
    $items = @((Get-Item -LiteralPath $Path -Force)) +
        @(Get-ChildItem -LiteralPath $Path -Recurse -Force)
    foreach ($item in $items) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "$Label contains a reparse point: $($item.FullName)"
        }
    }
}

function Assert-NoReparseAncestors {
    # run root 创建前逐级检查已存在的父目录；只检查目标自身会漏掉被替换成 junction 的
    # 上级目录，后续创建与强制中止清理都可能越过诊断根。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $cursor = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    while ($true) {
        if ([IO.Directory]::Exists($cursor)) {
            $attributes = [IO.File]::GetAttributes($cursor)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label contains a reparse point: $cursor"
            }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent -or $parent.FullName.Equals($cursor, [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        $cursor = $parent.FullName
    }
}

function Assert-RightRailOuterIsolation {
    # Add-Type 和 cmdlet 自动加载发生在当前进程；右栏必须由一个已用受控 HOME/APPDATA/TEMP
    # 和可信 PSHOME\Modules 启动的 pwsh 7 承载。这里核对实际环境值而不是信任标记，
    # 直接运行 settings-parity.ps1 时因缺少受控外层环境而失败关闭，其他 scope 不受影响。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$ScriptRoot
    )

    if ($PSVersionTable.PSVersion.Major -ne 7 -or $PSVersionTable.PSEdition -cne 'Core') {
        throw "Right-rail acceptance requires PowerShell 7 Core; Windows PowerShell 5.1 is rejected."
    }
    $outerRoot = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_OUTER_ROOT')
    $trustedModules = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_TRUSTED_MODULES')
    if ([string]::IsNullOrWhiteSpace($outerRoot) -or [string]::IsNullOrWhiteSpace($trustedModules)) {
        throw 'Right-rail acceptance must be started through scripts/acceptance/settings-parity-right-rail.ps1 so HOME/APPDATA/TEMP and PSModulePath are isolated before cmdlet loading.'
    }
    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $repositoryRoot = [IO.Path]::GetFullPath([IO.Path]::Combine($ScriptRoot, '..', '..'))
    $diagnosticsRoot = [IO.Path]::GetFullPath([IO.Path]::Combine($repositoryRoot, 'target', 'diagnostics', 'settings-parity'))
    $outerRoot = [IO.Path]::GetFullPath($outerRoot)
    $trustedModules = [IO.Path]::GetFullPath($trustedModules)
    $prefix = $diagnosticsRoot.TrimEnd($separators) + [IO.Path]::DirectorySeparatorChar
    if (-not ($outerRoot.TrimEnd($separators) + [IO.Path]::DirectorySeparatorChar).StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail outer isolation root escaped the diagnostics root.'
    }
    Assert-NoReparseComponent -Path $outerRoot -Root $diagnosticsRoot -Label 'Right-rail outer isolation root'
    # 不能用 $psHome 命名：它与常量自动变量 $PSHOME 大小写不敏感冲突，赋值会抛
    # “无法覆盖变量 PSHOME”。探针回读侧同样以 ProbeHome 命名避开。
    $runningPsHome = [IO.Path]::GetFullPath($PSHOME)
    $expectedModules = [IO.Path]::GetFullPath([IO.Path]::Combine($runningPsHome, 'Modules'))
    if (-not $trustedModules.Equals($expectedModules, [StringComparison]::OrdinalIgnoreCase)) {
        throw "The right-rail PSModulePath is not the running pwsh's trusted PSHOME\\Modules."
    }
    foreach ($module in @('PSReadLine', 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management')) {
        if (-not [IO.Directory]::Exists([IO.Path]::Combine($trustedModules, $module))) {
            throw "The trusted PowerShell module directory is missing $module."
        }
    }
    # pwsh 启动时会把缺失的默认路径补齐为“受控用户;系统;PSHOME”三段并逐字保留预设
    # 完整序列（外层已实测），因此外层环境必然是三段而不是单一目录。这里对实际序列
    # 做精确白名单：用户段必须等于受控 HOME 派生目录、系统段必须等于已知文件夹的
    # Program Files 派生目录、末段必须等于可信 PSHOME\Modules，条目数与顺序都固定，
    # 真实用户模块路径或任何额外条目都会失败关闭。系统段来自 pwsh 内部推导，与
    # GetFolderPath(ProgramFiles) 同源；MSI 安装同样按该已知文件夹推导。
    $actualModuleEntries = @([Environment]::GetEnvironmentVariable('PSModulePath') -split ';' |
        ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
    # 受控 HOME 派生的用户模块段在下方环境断言前就要参与白名单，这里先定义一次，
    # 后面的环境断言复用同一变量；不能用 $home 命名（只读自动变量 $HOME 大小写
    # 不敏感冲突）。
    $isolationHome = [IO.Path]::Combine($outerRoot, 'home')
    $controlledUserModules = [IO.Path]::GetFullPath([IO.Path]::Combine($isolationHome, 'Documents', 'PowerShell', 'Modules'))
    $systemModulesRoot = [IO.Path]::GetFullPath(
        [IO.Path]::Combine([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles), 'PowerShell', 'Modules'))
    $expectedModuleSequence = [ordered]@{
        user = $controlledUserModules
        system = $systemModulesRoot
        trusted = $expectedModules
    }
    if ($actualModuleEntries.Count -ne 3) {
        throw "The right-rail outer PSModulePath must contain exactly the controlled user, system, and PSHOME module entries; found $($actualModuleEntries.Count)."
    }
    # OrderedDictionary 没有按 key 查下标的方法；键数组与条目数组顺序同源，用数组
    # IndexOf 保持 user/system/trusted 三段与 PSModulePath 实际顺序一一对应。
    # 只断言路径相等不要求目录存在：系统段在 MSIX 安装机器上可能不存在，而 pwsh
    # 仍会把它作为默认段写入；隔离的安全属性是“没有真实用户路径”，由相等性承担。
    $sequenceKeys = @($expectedModuleSequence.Keys)
    for ($entryIndex = 0; $entryIndex -lt $sequenceKeys.Count; $entryIndex++) {
        $entryName = [string]$sequenceKeys[$entryIndex]
        $expectedEntry = [IO.Path]::GetFullPath([string]$expectedModuleSequence[$entryName]).TrimEnd($separators)
        $actualEntry = [IO.Path]::GetFullPath($actualModuleEntries[$entryIndex]).TrimEnd($separators)
        if (-not $actualEntry.Equals($expectedEntry, [StringComparison]::OrdinalIgnoreCase)) {
            throw "The right-rail outer PSModulePath $entryName module entry is not the expected path: $($actualModuleEntries[$entryIndex])"
        }
    }
    # 不能用 $home 命名：它与只读自动变量 $HOME 大小写不敏感冲突，函数作用域内
    # 赋值必败；统一用 IsolationHome 表达受控 HOME 派生路径。
    $expected = [ordered]@{
        USERPROFILE = $isolationHome
        HOME = $isolationHome
        APPDATA = [IO.Path]::Combine($isolationHome, 'AppData', 'Roaming')
        LOCALAPPDATA = [IO.Path]::Combine($isolationHome, 'AppData', 'Local')
        TEMP = [IO.Path]::Combine($outerRoot, 'temp')
        TMP = [IO.Path]::Combine($outerRoot, 'temp')
        TMPDIR = [IO.Path]::Combine($outerRoot, 'temp')
        ProgramData = [IO.Path]::Combine($outerRoot, 'program-data')
        PUBLIC = [IO.Path]::Combine($outerRoot, 'public')
    }
    foreach ($name in $expected.Keys) {
        $actual = [Environment]::GetEnvironmentVariable([string]$name)
        if ([string]::IsNullOrWhiteSpace($actual)) {
            throw "The right-rail outer environment is missing $name."
        }
        $actualPath = [IO.Path]::GetFullPath($actual).TrimEnd($separators)
        $expectedPath = [IO.Path]::GetFullPath([string]$expected[$name]).TrimEnd($separators)
        if (-not $actualPath.Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
            throw "The right-rail outer environment $name does not point to its controlled directory."
        }
        if (-not [IO.Directory]::Exists($expectedPath)) {
            throw "The right-rail outer environment directory is missing: $name"
        }
    }
    $homeDrive = [Environment]::GetEnvironmentVariable('HOMEDRIVE')
    $homePath = [Environment]::GetEnvironmentVariable('HOMEPATH')
    if ([string]::IsNullOrWhiteSpace($homeDrive) -or [string]::IsNullOrWhiteSpace($homePath) -or
        -not $homeDrive.EndsWith(':') -or -not $homePath.StartsWith('\')) {
        throw 'The right-rail outer HOMEDRIVE/HOMEPATH are not in Windows drive form.'
    }
    $combinedHome = [IO.Path]::GetFullPath($homeDrive + $homePath).TrimEnd($separators)
    if (-not $combinedHome.Equals([IO.Path]::GetFullPath($isolationHome).TrimEnd($separators), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail outer HOMEDRIVE/HOMEPATH do not resolve to the controlled HOME.'
    }
}

if (($McpOnly -and $SuggestionsOnly) -or
    ($WorkbenchOnly -and ($McpOnly -or $SuggestionsOnly))) {
    throw '-McpOnly, -SuggestionsOnly and -WorkbenchOnly are mutually exclusive.'
}
if ($SshSuggestions -and -not $SuggestionsOnly) {
    throw '-SshSuggestions requires -SuggestionsOnly and an isolated fixture.'
}
if ($WorkbenchMotion -and (-not $WorkbenchOnly -or $BuildProfile -cne 'release')) {
    throw '-WorkbenchMotion requires -WorkbenchOnly and -BuildProfile release.'
}
if ($RightRailSplitSwitch -and (-not $WorkbenchOnly -or $WorkbenchMotion)) {
    throw '-RightRailSplitSwitch requires -WorkbenchOnly and cannot be combined with -WorkbenchMotion.'
}
# 右栏专项依赖只在 debug_assertions 下编译的 GPUI debug selector；release 在启动后
# 只能以“找不到选择器”失败，因此前置拒绝并把原因写清楚，避免把构建模式误当成功能失败。
if ($RightRailSplitSwitch -and $BuildProfile -cne 'debug') {
    throw '-RightRailSplitSwitch requires -BuildProfile debug because its production-control selectors are compiled only in debug builds.'
}
if (-not [string]::IsNullOrWhiteSpace($RightRailPowerShell) -and -not $RightRailSplitSwitch) {
    throw '-RightRailPowerShell requires -RightRailSplitSwitch.'
}
if ($WorkbenchOnly) {
    $independentScopes = @('KERMINAL_APP_THEME_ACCEPTANCE', 'KERMINAL_SPLIT_DIRECTORY_ACCEPTANCE', 'KERMINAL_COLUMN_REFLOW_ACCEPTANCE')
    if (-not $RightRailSplitSwitch) {
        $independentScopes += 'KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE'
    }
    foreach ($name in $independentScopes) {
        if ([Environment]::GetEnvironmentVariable($name, 'Process') -ceq '1') {
            throw "-WorkbenchOnly cannot run with the independent $name scope enabled."
        }
    }
}

# 右栏必须在任何 cmdlet 自动加载或 Add-Type 之前确认进程环境已经由外层隔离启动器重建；
# 直接运行本脚本时标记缺失，立即失败关闭，其他 scope 不受影响。
if ($RightRailSplitSwitch) {
    Assert-RightRailOuterIsolation -ScriptRoot $PSScriptRoot
}

# 右栏专项只使用 pwsh 7；不改变执行策略，也不回退到 Windows PowerShell 5.1 的会话语义。
$resolvedRightRailPowerShell = $null
if ($RightRailSplitSwitch) {
    $requestedShell = if ([string]::IsNullOrWhiteSpace($RightRailPowerShell)) { 'pwsh.exe' } else { $RightRailPowerShell }
    $shellCommand = Get-Command -Name $requestedShell -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $shellCommand -or [string]::IsNullOrWhiteSpace([string]$shellCommand.Source)) {
        throw "Right-rail split acceptance requires pwsh 7; the requested shell was not found: $requestedShell"
    }
    $resolvedRightRailPowerShell = [IO.Path]::GetFullPath([string]$shellCommand.Source)
    if (-not (Test-Path -LiteralPath $resolvedRightRailPowerShell -PathType Leaf)) {
        throw "Right-rail split PowerShell executable was not found: $resolvedRightRailPowerShell"
    }
    # 这里只解析可执行文件路径；版本探测必须等隔离根就绪后、在清空父环境的最小环境中执行，
    # 不能在父用户环境中启动 pwsh 并读取其模块/配置。
}

$script:RepositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$script:DiagnosticsRoot = [IO.Path]::GetFullPath((Join-Path $script:RepositoryRoot 'target\diagnostics\settings-parity'))
$script:ExpectedPages = @(
    'appearance',
    'terminal',
    'profiles',
    'keywords',
    'suggestions',
    'sftp',
    'external-launch',
    'mcp',
    'sync',
    'desktop',
    'shortcuts',
    'about'
)
$script:ExpectedThemes = @('light', 'dark', 'system')
$script:ExpectedDisclosures = @(
    'sftp-advanced',
    'external-launch-compatibility',
    'mcp-connection-info',
    'desktop-notifications-advanced'
)
$script:ExpectedKeywordLifecycleActions = @(
    'save-whole-word-and-custom-hex-readback',
    'disable-keyword-rule-readback',
    'enable-keyword-rule-readback',
    'delete-keyword-rule-readback',
    'undo-keyword-deletion-within-15-seconds'
)
$script:ExpectedKeywordAdjustmentChecks = @(
    [pscustomobject]@{ Action = 'keyword-scroll-and-preserve-drafts'; File = 'settings-functional-keyword-list-scroll.png' },
    [pscustomobject]@{ Action = 'keyword-search-filter'; File = 'settings-functional-keyword-filter.png' },
    [pscustomobject]@{ Action = 'keyword-limit-and-footer'; File = 'settings-functional-keyword-limit-footer.png' }
)
$script:ExpectedTerminalChecks = @(
    [pscustomobject]@{ Action = 'terminal-assistant-command-routing'; File = 'settings-functional-terminal-assistant-routing.png' },
    [pscustomobject]@{ Action = 'terminal-theme-and-live-palette'; File = 'settings-functional-terminal-theme.png' },
    [pscustomobject]@{ Action = 'terminal-system-font-search'; File = 'settings-functional-terminal-font.png' },
    [pscustomobject]@{ Action = 'terminal-font-size-step-and-bounds'; File = 'settings-functional-terminal-font-size.png' },
    [pscustomobject]@{ Action = 'terminal-line-height-step-and-bounds'; File = 'settings-functional-terminal-line-height.png' }
)
$script:ExpectedShortcutChecks = @(
    [pscustomobject]@{ Action = 'shortcut-capture-and-cancel'; File = 'settings-functional-shortcut-capture.png' },
    [pscustomobject]@{ Action = 'shortcut-custom-binding-live'; File = 'settings-functional-shortcut-live.png' },
    [pscustomobject]@{ Action = 'shortcut-platform-linux-recorder'; File = 'settings-functional-shortcut-linux.png' },
    [pscustomobject]@{ Action = 'shortcut-platform-macos-recorder'; File = 'settings-functional-shortcut-macos.png' }
)
$script:ExpectedSyncActions = @(
    'sync-local-remote-roundtrip',
    'sync-failure-preserves-local',
    'sync-retry-confirms-remote'
)
$script:ExpectedMcpActions = @(
    'mcp-timeout-retry-stops-original-owner',
    'mcp-start-stop-light',
    'mcp-start-stop-dark',
    'mcp-start-stop-system',
    'mcp-lan-binds-all-interfaces'
)
$script:ExpectedProfileActions = @(
    'rescan-profile-readback',
    'create-profile-readback',
    'edit-profile-readback',
    'set-default-profile-readback'
)
$script:ExpectedProfileHostChecks = @(
    [pscustomobject]@{ Action = 'profile-template-not-a-host'; File = $null },
    [pscustomobject]@{ Action = 'profile-create-local-host'; File = 'settings-functional-local-host-created.png' },
    [pscustomobject]@{ Action = 'profile-edit-keeps-host-snapshot'; File = $null },
    [pscustomobject]@{ Action = 'profile-delete-local-host'; File = 'settings-functional-local-host-deleted.png' }
)
$script:NativeDesktopLauncherSource = @'
using System;
using System.ComponentModel;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Text;

namespace Kerminal.SettingsParity
{
    public sealed class NativeDesktopProcess : IDisposable
    {
        private IntPtr _desktopHandle;
        private IntPtr _processHandle;
        private IntPtr _jobHandle;
        private bool _disposed;

        public int ProcessId { get; private set; }
        // FILETIME 来自创建时保留的进程句柄；超时回收前用它排除 PID 已被无关进程复用。
        public long CreationTimeUtcTicks { get; private set; }
        public string DesktopName { get; private set; }
        public bool DesktopClosed { get; private set; }
        public bool JobAssigned { get; private set; }
        public bool JobCleanupVerified { get; private set; }
        public bool JobCleanupForced { get; private set; }
        public uint JobActiveProcessesAtExit { get; private set; }

        // Transfers the launcher's process, desktop, and optional job handles to the bounded owner object.
        internal NativeDesktopProcess(IntPtr desktopHandle, IntPtr processHandle, IntPtr jobHandle, int processId, string desktopName)
        {
            _desktopHandle = desktopHandle;
            _processHandle = processHandle;
            _jobHandle = jobHandle;
            JobAssigned = jobHandle != IntPtr.Zero;
            ProcessId = processId;
            DesktopName = desktopName;
            NativeDesktopLauncher.FileTime creationTime;
            NativeDesktopLauncher.FileTime exitTime;
            NativeDesktopLauncher.FileTime kernelTime;
            NativeDesktopLauncher.FileTime userTime;
            if (!NativeDesktopLauncher.GetProcessTimes(processHandle, out creationTime, out exitTime, out kernelTime, out userTime))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Reading the owned acceptance process creation time failed.");
            long fileTimeTicks = ((long)creationTime.HighDateTime << 32) | creationTime.LowDateTime;
            CreationTimeUtcTicks = DateTime.FromFileTimeUtc(fileTimeTicks).Ticks;
        }

        // Waits on the exact process handle returned by CreateProcessW, avoiding PID-reuse races.
        public bool WaitForExit(uint timeoutMilliseconds)
        {
            if (_disposed || _processHandle == IntPtr.Zero)
                throw new ObjectDisposedException("NativeDesktopProcess");

            uint result = NativeDesktopLauncher.WaitForSingleObject(_processHandle, timeoutMilliseconds);
            if (result == NativeDesktopLauncher.WaitObject0)
                return true;
            if (result == NativeDesktopLauncher.WaitTimeout)
                return false;
            throw new Win32Exception(Marshal.GetLastWin32Error(), "Waiting for the isolated acceptance process failed.");
        }

        // Reads the exit status only after the owned process handle is signaled.
        public uint GetExitCode()
        {
            if (!WaitForExit(0))
                throw new InvalidOperationException("The acceptance process has not exited.");
            uint exitCode;
            if (!NativeDesktopLauncher.GetExitCodeProcess(_processHandle, out exitCode))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Reading the acceptance process exit code failed.");
            return exitCode;
        }

        // Provides a final fallback against the exact owned handle after process-tree cleanup times out.
        public void Terminate(uint exitCode)
        {
            if (_disposed || _processHandle == IntPtr.Zero)
                throw new ObjectDisposedException("NativeDesktopProcess");
            if (!NativeDesktopLauncher.TerminateProcess(_processHandle, exitCode))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Terminating the isolated acceptance process failed.");
        }

        // 父进程退出后给正常关闭一个短窗口；只有计数仍未归零才强制结束整个 Job。
        public bool WaitForJobEmpty(uint timeoutMilliseconds)
        {
            if (_disposed || _jobHandle == IntPtr.Zero)
                throw new ObjectDisposedException("NativeDesktopProcess");
            DateTime deadline = DateTime.UtcNow.AddMilliseconds(timeoutMilliseconds);
            while (true)
            {
                uint active = NativeDesktopLauncher.QueryActiveProcessCount(_jobHandle);
                JobActiveProcessesAtExit = active;
                if (active == 0)
                {
                    JobCleanupVerified = true;
                    return true;
                }
                if (DateTime.UtcNow >= deadline)
                    return false;
                System.Threading.Thread.Sleep(25);
            }
        }

        // 通过创建时绑定的 Job Object 结束整棵树，并在关闭句柄前轮询 active process 计数归零。
        // 只有计数确认为 0 才设置 verified；不能只看父进程退出就声称子孙已回收。
        public void TerminateJobAndWait(uint exitCode, uint timeoutMilliseconds)
        {
            if (_disposed || _jobHandle == IntPtr.Zero)
                throw new ObjectDisposedException("NativeDesktopProcess");
            if (!NativeDesktopLauncher.TerminateJobObject(_jobHandle, exitCode))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Terminating the owned acceptance job failed.");
            JobCleanupForced = true;
            DateTime deadline = DateTime.UtcNow.AddMilliseconds(timeoutMilliseconds);
            while (true)
            {
                uint active = NativeDesktopLauncher.QueryActiveProcessCount(_jobHandle);
                if (active == 0)
                {
                    JobActiveProcessesAtExit = 0;
                    JobCleanupVerified = true;
                    return;
                }
                if (DateTime.UtcNow >= deadline)
                    throw new InvalidOperationException(
                        "The owned acceptance job still has " + active.ToString(CultureInfo.InvariantCulture) +
                        " active process(es) after termination.");
                System.Threading.Thread.Sleep(25);
            }
        }

        // Keeps the private desktop alive until its only child process has exited, then verifies
        // the job is empty before closing owned handles. A non-empty job is terminated and must
        // reach active count zero; otherwise the receipt remains unverified and Dispose fails.
        public void Dispose()
        {
            if (_disposed)
                return;
            if (_processHandle != IntPtr.Zero && !WaitForExit(0))
                throw new InvalidOperationException("The private desktop cannot close while its acceptance process is running.");

            int firstError = 0;
            string jobCleanupError = null;
            if (_jobHandle != IntPtr.Zero)
            {
                try
                {
                    if (!WaitForJobEmpty(2000))
                        TerminateJobAndWait(1, 10000);
                }
                catch (Exception error)
                {
                    jobCleanupError = error.Message;
                }
                if (!NativeDesktopLauncher.CloseHandle(_jobHandle))
                    firstError = Marshal.GetLastWin32Error();
                else
                    _jobHandle = IntPtr.Zero;
            }
            if (_processHandle != IntPtr.Zero)
            {
                if (!NativeDesktopLauncher.CloseHandle(_processHandle))
                    firstError = Marshal.GetLastWin32Error();
                else
                    _processHandle = IntPtr.Zero;
            }
            if (_desktopHandle != IntPtr.Zero)
            {
                DesktopClosed = NativeDesktopLauncher.CloseDesktop(_desktopHandle);
                if (!DesktopClosed && firstError == 0)
                    firstError = Marshal.GetLastWin32Error();
                if (DesktopClosed)
                    _desktopHandle = IntPtr.Zero;
            }
            _disposed = _processHandle == IntPtr.Zero && _desktopHandle == IntPtr.Zero && _jobHandle == IntPtr.Zero;
            if (jobCleanupError != null)
                throw new InvalidOperationException("Could not verify job-object process cleanup: " + jobCleanupError);
            if (firstError != 0)
                throw new Win32Exception(firstError, "Closing the isolated acceptance process or desktop handle failed.");
        }
    }

    public static class NativeDesktopLauncher
    {
        internal const uint WaitObject0 = 0;
        internal const uint WaitTimeout = 0x00000102;
        private const uint GenericRead = 0x80000000;
        private const uint GenericWrite = 0x40000000;
        private const uint FileShareRead = 0x00000001;
        private const uint FileShareWrite = 0x00000002;
        private const uint FileShareDelete = 0x00000004;
        private const uint OpenExisting = 3;
        private const uint CreateAlways = 2;
        private const uint FileAttributeNormal = 0x00000080;
        private const uint DesktopReadObjects = 0x00000001;
        private const uint DesktopCreateWindow = 0x00000002;
        private const uint DesktopEnumerate = 0x00000040;
        private const uint DesktopWriteObjects = 0x00000080;
        private const uint StartfUseStdHandles = 0x00000100;
        private const uint CreateNoWindow = 0x08000000;
        private const uint CreateUnicodeEnvironment = 0x00000400;
        private const uint ExtendedStartupInfoPresent = 0x00080000;
        private const uint CreateSuspended = 0x00000004;
        private const int JobObjectBasicAccountingInformationClass = 1;
        private const int JobObjectExtendedLimitInformationClass = 9;
        private const uint JobObjectLimitKillOnJobClose = 0x00002000;
        private static readonly IntPtr ProcThreadAttributeHandleList = new IntPtr(0x00020002);

        [StructLayout(LayoutKind.Sequential)]
        private struct SecurityAttributes
        {
            public int Length;
            public IntPtr SecurityDescriptor;
            [MarshalAs(UnmanagedType.Bool)] public bool InheritHandle;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct StartupInfo
        {
            public uint Size;
            public IntPtr Reserved;
            public IntPtr Desktop;
            public IntPtr Title;
            public uint X;
            public uint Y;
            public uint XSize;
            public uint YSize;
            public uint XCountChars;
            public uint YCountChars;
            public uint FillAttribute;
            public uint Flags;
            public ushort ShowWindow;
            public ushort Reserved2Size;
            public IntPtr Reserved2;
            public IntPtr StdInput;
            public IntPtr StdOutput;
            public IntPtr StdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct StartupInfoEx
        {
            public StartupInfo StartupInfo;
            public IntPtr AttributeList;
        }

        [StructLayout(LayoutKind.Sequential)]
        internal struct FileTime
        {
            public uint LowDateTime;
            public uint HighDateTime;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JobObjectBasicLimitInformation
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct IoCounters
        {
            public ulong ReadOperationCount;
            public ulong WriteOperationCount;
            public ulong OtherOperationCount;
            public ulong ReadTransferCount;
            public ulong WriteTransferCount;
            public ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JobObjectExtendedLimitInformation
        {
            public JobObjectBasicLimitInformation BasicLimitInformation;
            public IoCounters IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [StructLayout(LayoutKind.Sequential)]
        internal struct JobObjectBasicAccountingInformation
        {
            public long TotalUserTime;
            public long TotalKernelTime;
            public long ThisPeriodTotalUserTime;
            public long ThisPeriodTotalKernelTime;
            public uint TotalPageFaultCount;
            public uint TotalProcesses;
            public uint ActiveProcesses;
            public uint TotalTerminatedProcesses;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct ProcessInformation
        {
            public IntPtr Process;
            public IntPtr Thread;
            public uint ProcessId;
            public uint ThreadId;
        }

        // Creates a private desktop without changing which desktop receives user input.
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateDesktopW")]
        private static extern IntPtr CreateDesktop(string desktopName, IntPtr device, IntPtr devmode, uint flags, uint desiredAccess, IntPtr securityAttributes);

        // Closes only the private desktop handle owned by this acceptance run.
        [DllImport("user32.dll", SetLastError = true, EntryPoint = "CloseDesktop")]
        internal static extern bool CloseDesktop(IntPtr desktop);

        // Opens the three redirected stream targets with an explicit inheritable-handle policy.
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateFileW")]
        private static extern IntPtr CreateFile(string fileName, uint access, uint share, ref SecurityAttributes securityAttributes, uint creation, uint flags, IntPtr template);

        // Starts the app on lpDesktop and confines inheritance to STARTUPINFOEX's handle list.
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateProcessW")]
        private static extern bool CreateProcess(string applicationName, StringBuilder commandLine, IntPtr processAttributes, IntPtr threadAttributes, [MarshalAs(UnmanagedType.Bool)] bool inheritHandles, uint creationFlags, IntPtr environment, string currentDirectory, ref StartupInfoEx startupInfo, out ProcessInformation processInformation);

        // Queries the required native storage for a one-entry extended startup attribute list.
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool InitializeProcThreadAttributeList(IntPtr attributeList, int attributeCount, uint flags, ref IntPtr size);

        // Installs the exact allowlist containing stdin, stdout, and stderr only.
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool UpdateProcThreadAttribute(IntPtr attributeList, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previousValue, IntPtr returnSize);

        // Releases initialized startup attribute storage before returning to PowerShell.
        [DllImport("kernel32.dll")]
        private static extern void DeleteProcThreadAttributeList(IntPtr attributeList);

        // Waits without polling or touching the input desktop.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

        // Reads status from the exact handle whose signaled state was already observed.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

        // Reads the immutable creation time from the retained handle; a later PID lookup can
        // compare against it before any process-tree termination.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool GetProcessTimes(IntPtr process, out FileTime creationTime, out FileTime exitTime, out FileTime kernelTime, out FileTime userTime);

        // Creates an anonymous job so no other process can open and mutate the acceptance job.
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateJobObjectW")]
        private static extern IntPtr CreateJobObject(IntPtr securityAttributes, string name);

        // Sets kill-on-close without breakaway flags; descendants therefore cannot escape the job.
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(IntPtr job, int informationClass, ref JobObjectExtendedLimitInformation information, uint length);

        // Binds the suspended child before it can create descendants.
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        // Resumes the child only after it is already owned by the job.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern uint ResumeThread(IntPtr thread);

        // Terminates every process currently owned by the job; active count is queried separately.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool TerminateJobObject(IntPtr job, uint exitCode);

        // Reads the job's active process count before any handle close.
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool QueryInformationJobObject(IntPtr job, int informationClass, out JobObjectBasicAccountingInformation information, uint length, out uint returnLength);

        internal static uint QueryActiveProcessCount(IntPtr job)
        {
            JobObjectBasicAccountingInformation information;
            uint length = (uint)Marshal.SizeOf(typeof(JobObjectBasicAccountingInformation));
            uint returnLength;
            if (!QueryInformationJobObject(job, JobObjectBasicAccountingInformationClass, out information, length, out returnLength))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Querying the owned acceptance job failed.");
            return information.ActiveProcesses;
        }

        // Releases a process or standard-stream handle acquired by this launcher.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool CloseHandle(IntPtr handle);

        // 恢复 HEAD 版八参数入口：webview/plugin/sftp-follow 等派生消费者仍按八参数调用各自从本脚本
        // 提取的 launcher 副本，缺此重载会在 Add-Type 编译后按“无匹配重载”失败。这里显式委托九参数并
        // 传 useJobObject=false，逐字保持原实现的“无 Job Object”语义；不能默认 true，否则会改变这些
        // 消费者原本的后代回收边界，把兼容修复变成行为变更。
        // 方法头刻意保持单行且不插入换行，因为 plugin-restart-native.ps1 与 webview-native.test.ps1 以
        // 这行旧八参数签名的精确文本作为替换锚点；拆行会让它们的唯一替换点检查失败。
        public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height)
        {
            return Start(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, width, height, false);
        }

        // 保留原 9 参数入口供 settings 与 agent-continuity 共享：命令串是 agent wrapper 的替换锚点，
        // 参数顺序也不变；真正的启动体移到 StartCore，便于探针复用同一 Job/句柄白名单而不复制安全逻辑。
        public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height, bool useJobObject)
        {
            string command = QuoteArgument(executable)
                + " --settings-acceptance --output-dir " + QuoteArgument(outputDirectory)
                + " --width " + width.ToString(CultureInfo.InvariantCulture)
                + " --height " + height.ToString(CultureInfo.InvariantCulture);
            return StartCore(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, useJobObject, command, false);
        }

        // 版本探针只运行受控 pwsh 脚本；显式逐参数引用避免 shell 拼接，并继承调用者已经验证过的
        // 外层隔离环境，不在这里重复重建 HOME/APPDATA，防止检查对象与真实运行环境不一致。
        public static NativeDesktopProcess StartProbe(string executable, string[] arguments, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, bool useJobObject)
        {
            if (arguments == null)
                throw new ArgumentNullException("arguments");
            StringBuilder command = new StringBuilder(QuoteArgument(executable));
            foreach (string argument in arguments)
                command.Append(' ').Append(QuoteArgument(argument));
            return StartCore(executable, workingDirectory, outputDirectory, stdoutPath, stderrPath, desktopName, useJobObject, command.ToString(), true);
        }

        // 统一进程创建路径：普通验收和探针都必须先 suspended 绑定 Job 再恢复，并共享句柄白名单
        // 与私有桌面，避免两套实现产生不同的进程树回收语义。
        private static NativeDesktopProcess StartCore(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, bool useJobObject, string command, bool inheritEnvironment)
        {
            if (String.IsNullOrWhiteSpace(desktopName) || !desktopName.StartsWith("KerminalRail-settings-", StringComparison.Ordinal))
                throw new ArgumentException("The private desktop name is outside the settings acceptance namespace.", "desktopName");

            uint desktopAccess = DesktopReadObjects | DesktopCreateWindow | DesktopEnumerate | DesktopWriteObjects;
            IntPtr desktop = CreateDesktop(desktopName, IntPtr.Zero, IntPtr.Zero, 0, desktopAccess, IntPtr.Zero);
            if (desktop == IntPtr.Zero)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateDesktopW failed.");

            IntPtr stdin = IntPtr.Zero;
            IntPtr stdout = IntPtr.Zero;
            IntPtr stderr = IntPtr.Zero;
            IntPtr attributeList = IntPtr.Zero;
            IntPtr handles = IntPtr.Zero;
            IntPtr desktopNamePointer = IntPtr.Zero;
            IntPtr environment = IntPtr.Zero;
            IntPtr job = IntPtr.Zero;
            ProcessInformation processInformation = new ProcessInformation();
            bool desktopTransferred = false;
            bool jobTransferred = false;
            bool attributeListInitialized = false;
            try
            {
                if (useJobObject)
                {
                    // 进程以 suspended 状态创建，先绑定到不允许 breakaway 的 Job 再恢复，
                    // 避免子进程在绑定前创建 descendant 而逃出回收范围。
                    job = CreateJobObject(IntPtr.Zero, null);
                    if (job == IntPtr.Zero)
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObjectW failed.");
                    JobObjectExtendedLimitInformation limits = new JobObjectExtendedLimitInformation();
                    limits.BasicLimitInformation.LimitFlags = JobObjectLimitKillOnJobClose;
                    uint limitSize = (uint)Marshal.SizeOf(typeof(JobObjectExtendedLimitInformation));
                    if (!SetInformationJobObject(job, JobObjectExtendedLimitInformationClass, ref limits, limitSize))
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "Configuring the owned acceptance job failed.");
                }

                stdin = CreateInheritableFile("NUL", GenericRead, OpenExisting);
                stdout = CreateInheritableFile(stdoutPath, GenericWrite, CreateAlways);
                stderr = CreateInheritableFile(stderrPath, GenericWrite, CreateAlways);

                IntPtr attributeSize = IntPtr.Zero;
                InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref attributeSize);
                if (attributeSize == IntPtr.Zero)
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Could not size the process handle allowlist.");
                attributeList = Marshal.AllocHGlobal(attributeSize);
                if (!InitializeProcThreadAttributeList(attributeList, 1, 0, ref attributeSize))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Could not initialize the process handle allowlist.");
                attributeListInitialized = true;

                handles = Marshal.AllocHGlobal(IntPtr.Size * 3);
                Marshal.WriteIntPtr(handles, 0, stdin);
                Marshal.WriteIntPtr(handles, IntPtr.Size, stdout);
                Marshal.WriteIntPtr(handles, IntPtr.Size * 2, stderr);
                if (!UpdateProcThreadAttribute(attributeList, 0, ProcThreadAttributeHandleList, handles, new IntPtr(IntPtr.Size * 3), IntPtr.Zero, IntPtr.Zero))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Could not restrict inherited process handles.");

                desktopNamePointer = Marshal.StringToHGlobalUni("WinSta0\\" + desktopName);
                StartupInfoEx startupInfo = new StartupInfoEx();
                startupInfo.StartupInfo.Size = (uint)Marshal.SizeOf(typeof(StartupInfoEx));
                startupInfo.StartupInfo.Desktop = desktopNamePointer;
                startupInfo.StartupInfo.Flags = StartfUseStdHandles;
                startupInfo.StartupInfo.StdInput = stdin;
                startupInfo.StartupInfo.StdOutput = stdout;
                startupInfo.StartupInfo.StdError = stderr;
                startupInfo.AttributeList = attributeList;

                ProcessInformation info;
                environment = inheritEnvironment ? IntPtr.Zero : CreateIsolatedGitEnvironment(outputDirectory);
                uint creationFlags = CreateNoWindow | ExtendedStartupInfoPresent;
                if (environment != IntPtr.Zero)
                    creationFlags |= CreateUnicodeEnvironment;
                if (useJobObject)
                    creationFlags |= CreateSuspended;
                if (!CreateProcess(executable, new StringBuilder(command), IntPtr.Zero, IntPtr.Zero, true, creationFlags, environment, workingDirectory, ref startupInfo, out info))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcessW on the private desktop failed.");
                processInformation = info;
                if (useJobObject)
                {
                    if (!AssignProcessToJobObject(job, processInformation.Process))
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "Assigning the isolated process to its job failed.");
                    uint resumeResult = ResumeThread(processInformation.Thread);
                    if (resumeResult == unchecked((uint)-1))
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "Resuming the isolated process failed.");
                }
                if (!CloseHandle(processInformation.Thread))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Closing the isolated process thread handle failed.");
                processInformation.Thread = IntPtr.Zero;

                NativeDesktopProcess result = new NativeDesktopProcess(desktop, processInformation.Process, job, unchecked((int)processInformation.ProcessId), desktopName);
                desktop = IntPtr.Zero;
                processInformation.Process = IntPtr.Zero;
                job = IntPtr.Zero;
                desktopTransferred = true;
                jobTransferred = true;
                return result;
            }
            finally
            {
                if (attributeListInitialized)
                    DeleteProcThreadAttributeList(attributeList);
                if (handles != IntPtr.Zero)
                    Marshal.FreeHGlobal(handles);
                if (desktopNamePointer != IntPtr.Zero)
                    Marshal.FreeHGlobal(desktopNamePointer);
                if (environment != IntPtr.Zero)
                    Marshal.FreeHGlobal(environment);
                CloseQuietly(stdin);
                CloseQuietly(stdout);
                CloseQuietly(stderr);
                if (!desktopTransferred)
                {
                    if (processInformation.Thread != IntPtr.Zero)
                        CloseQuietly(processInformation.Thread);
                    if (processInformation.Process != IntPtr.Zero)
                    {
                        TerminateProcess(processInformation.Process, 1);
                        WaitForSingleObject(processInformation.Process, 5000);
                        CloseQuietly(processInformation.Process);
                    }
                    if (!jobTransferred && job != IntPtr.Zero)
                        CloseQuietly(job);
                    if (desktop != IntPtr.Zero)
                        CloseDesktop(desktop);
                }
            }
        }

        // 隔离 Git 与建议/右栏专项的 Shell 历史到子进程；正常用户配置和历史不得收到夹具命令。
        // 右栏专项不复制父用户环境，而是用固定白名单重建；KnownFolder API 仍可能绕过环境变量，
        // 因此只声称环境变量与路径隔离，不声称全局系统元数据完全隔离。
        private static IntPtr CreateIsolatedGitEnvironment(string outputDirectory)
        {
            string scope = Environment.GetEnvironmentVariable("KERMINAL_SETTINGS_ACCEPTANCE_SCOPE");
            // 只有右栏专项进入严格白名单；agent-continuity 会清空 scope，继续走原有
            // 继承环境 + Git 隔离分支，不能把右栏策略误套到共享 launcher 的其他调用方。
            if (scope == "right-rail-split")
                return CreateRightRailIsolatedEnvironment(outputDirectory);
            var variables = new System.Collections.Generic.SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            foreach (System.Collections.DictionaryEntry entry in Environment.GetEnvironmentVariables())
            {
                string key = (string)entry.Key;
                if (!key.StartsWith("GIT_", StringComparison.OrdinalIgnoreCase))
                    variables[key] = (string)entry.Value;
            }
            variables["GIT_CONFIG_GLOBAL"] = System.IO.Path.Combine(outputDirectory, "config", ".acceptance-sync", "global.gitconfig");
            variables["GIT_CONFIG_NOSYSTEM"] = "1";
            variables["GIT_CONFIG_SYSTEM"] = "NUL";
            variables["GIT_ALLOW_PROTOCOL"] = "file";
            variables["GIT_TERMINAL_PROMPT"] = "0";
            if (scope == "suggestions")
            {
                var profileRoot = System.IO.Path.Combine(outputDirectory, "config");
                variables["USERPROFILE"] = profileRoot;
                variables["APPDATA"] = System.IO.Path.Combine(profileRoot, "AppData", "Roaming");
                variables["LOCALAPPDATA"] = System.IO.Path.Combine(profileRoot, "AppData", "Local");
            }
            return CreateEnvironmentBlock(variables);
        }

        // 右栏子进程只接收系统运行所需变量和 run root 内的模拟用户/KnownFolder 路径；
        // 认证代理、用户模块、真实 HOME/APPDATA 和 KERMINAL_CONFIG_ROOT 都不会被继承。
        private static IntPtr CreateRightRailIsolatedEnvironment(string outputDirectory)
        {
            var variables = new System.Collections.Generic.SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            string systemRoot = Environment.GetEnvironmentVariable("SystemRoot");
            string systemDrive = Environment.GetEnvironmentVariable("SystemDrive");
            string windowsDir = Environment.GetEnvironmentVariable("WINDIR");
            if (String.IsNullOrWhiteSpace(systemRoot) || String.IsNullOrWhiteSpace(systemDrive) ||
                String.IsNullOrWhiteSpace(windowsDir))
                throw new InvalidOperationException("The Windows system environment is incomplete.");

            variables["SystemRoot"] = systemRoot;
            variables["SystemDrive"] = systemDrive;
            variables["WINDIR"] = windowsDir;
            variables["ComSpec"] = System.IO.Path.Combine(systemRoot, "System32", "cmd.exe");
            variables["PATHEXT"] = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
            // PATH 不加入 WindowsPowerShell\v1.0：右栏只允许显式 pwsh 7，
            // 避免任何组件按名字回退到 5.1。
            variables["PATH"] = String.Join(";", new[]
            {
                System.IO.Path.Combine(systemRoot, "System32"),
                systemRoot,
                System.IO.Path.Combine(systemRoot, "System32", "Wbem")
            });
            foreach (var name in new[] { "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER", "PROCESSOR_LEVEL", "PROCESSOR_REVISION", "OS" })
            {
                string value = Environment.GetEnvironmentVariable(name);
                if (!String.IsNullOrWhiteSpace(value))
                    variables[name] = value;
            }
            variables["USERNAME"] = "acceptance";
            variables["USERDOMAIN"] = "KERMINAL_ACCEPTANCE";
            variables["COMPUTERNAME"] = "KERMINAL-ACCEPTANCE";

            string home = System.IO.Path.Combine(outputDirectory, "home");
            string appData = System.IO.Path.Combine(home, "AppData", "Roaming");
            string localAppData = System.IO.Path.Combine(home, "AppData", "Local");
            string tempRoot = System.IO.Path.Combine(outputDirectory, "temp");
            // 只使用本次显式 pwsh 可执行文件自带的 Modules；父进程用户模块路径绝不继承。
            // 该值由外层隔离启动和版本探针实际验证后写入，缺失或未验证时在此失败关闭。
            string trustedModules = Environment.GetEnvironmentVariable("KERMINAL_RIGHT_RAIL_TRUSTED_MODULES");
            string probeHistoryPath = Environment.GetEnvironmentVariable("KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY");
            string moduleProbeVerified = Environment.GetEnvironmentVariable("KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED");
            string probeAppData = Environment.GetEnvironmentVariable("APPDATA");
            if (String.IsNullOrWhiteSpace(trustedModules) ||
                String.IsNullOrWhiteSpace(probeHistoryPath) ||
                String.IsNullOrWhiteSpace(probeAppData) ||
                moduleProbeVerified != "1")
                throw new InvalidOperationException("The right-rail trusted PowerShell module probe is missing.");
            trustedModules = System.IO.Path.GetFullPath(trustedModules);
            probeHistoryPath = System.IO.Path.GetFullPath(probeHistoryPath);
            probeAppData = System.IO.Path.GetFullPath(probeAppData);
            if (!System.IO.Directory.Exists(trustedModules))
                throw new InvalidOperationException("The right-rail trusted PowerShell module root does not exist.");
            foreach (string module in new[] { "PSReadLine", "Microsoft.PowerShell.Utility", "Microsoft.PowerShell.Management" })
            {
                if (!System.IO.Directory.Exists(System.IO.Path.Combine(trustedModules, module)))
                    throw new InvalidOperationException("The right-rail trusted PowerShell module root is incomplete: " + module);
            }
            // 探针在 outer APPDATA 中已证明 PSReadLine 的历史路径；子进程使用 run root 的
            // 对应 APPDATA，必须把同一个相对历史文件映射过去，不能把 outer 绝对路径带进 run root。
            string probeHistoryRoot = System.IO.Path.GetFullPath(System.IO.Path.Combine(probeAppData, "Microsoft", "Windows", "PowerShell", "PSReadLine"));
            if (!probeHistoryPath.StartsWith(probeHistoryRoot + System.IO.Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase) ||
                !String.Equals(System.IO.Path.GetFileName(probeHistoryPath), "ConsoleHost_history.txt", StringComparison.OrdinalIgnoreCase))
                throw new InvalidOperationException("The right-rail PSReadLine history path escaped the isolated outer APPDATA.");
            string historyPath = System.IO.Path.Combine(appData, "Microsoft", "Windows", "PowerShell", "PSReadLine", "ConsoleHost_history.txt");
            string programData = System.IO.Path.Combine(outputDirectory, "program-data");
            string publicRoot = System.IO.Path.Combine(outputDirectory, "public");
            string configRoot = System.IO.Path.Combine(outputDirectory, "config");
            // 这里不能预建 run root：kerminal 的 --output-dir 入口要求末级目录不存在。
            // 目录由子进程在创建 output root 后、DesktopComposition/PTY 之前建立并逐项校验。
            string homeRoot = System.IO.Path.GetPathRoot(home);
            if (String.IsNullOrWhiteSpace(homeRoot))
                throw new InvalidOperationException("The isolated HOME has no drive root.");
            variables["USERPROFILE"] = home;
            variables["HOME"] = home;
            variables["HOMEDRIVE"] = homeRoot.TrimEnd('\\');
            variables["HOMEPATH"] = home.Substring(homeRoot.Length - 1);
            variables["APPDATA"] = appData;
            variables["LOCALAPPDATA"] = localAppData;
            variables["TEMP"] = tempRoot;
            variables["TMP"] = tempRoot;
            variables["TMPDIR"] = tempRoot;
            variables["PSModulePath"] = trustedModules;
            variables["ProgramData"] = programData;
            variables["PUBLIC"] = publicRoot;
            variables["KERMINAL_CONFIG_ROOT"] = configRoot;
            variables["KERMINAL_SETTINGS_ACCEPTANCE_SCOPE"] = "right-rail-split";
            variables["KERMINAL_WORKBENCH_ACCEPTANCE"] = "1";
            variables["KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE"] = "1";
            variables["KERMINAL_RIGHT_RAIL_TRUSTED_MODULES"] = trustedModules;
            variables["KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY"] = historyPath;
            variables["KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED"] = "1";
            string acceptanceShell = Environment.GetEnvironmentVariable("KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL");
            if (String.IsNullOrWhiteSpace(acceptanceShell))
                throw new InvalidOperationException("The right-rail acceptance shell is missing.");
            variables["KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL"] = acceptanceShell;
            variables["GIT_CONFIG_GLOBAL"] = System.IO.Path.Combine(configRoot, ".acceptance-sync", "global.gitconfig");
            variables["GIT_CONFIG_NOSYSTEM"] = "1";
            variables["GIT_CONFIG_SYSTEM"] = "NUL";
            variables["GIT_ALLOW_PROTOCOL"] = "file";
            variables["GIT_TERMINAL_PROMPT"] = "0";
            return CreateEnvironmentBlock(variables);
        }

        // 只序列化白名单字典，末尾保留 CreateProcessW 要求的双 NUL。
        private static IntPtr CreateEnvironmentBlock(System.Collections.Generic.SortedDictionary<string, string> variables)
        {
            var block = new StringBuilder();
            foreach (var entry in variables)
                block.Append(entry.Key).Append('=').Append(entry.Value).Append('\0');
            block.Append('\0');
            return Marshal.StringToHGlobalUni(block.ToString());
        }

        // Marks each stream handle inheritable; CreateProcessW still receives the explicit three-handle allowlist.
        private static IntPtr CreateInheritableFile(string path, uint access, uint creation)
        {
            SecurityAttributes securityAttributes = new SecurityAttributes();
            securityAttributes.Length = Marshal.SizeOf(typeof(SecurityAttributes));
            securityAttributes.InheritHandle = true;
            IntPtr handle = CreateFile(path, access, FileShareRead | FileShareWrite | FileShareDelete, ref securityAttributes, creation, FileAttributeNormal, IntPtr.Zero);
            if (handle == IntPtr.Zero || handle == new IntPtr(-1))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateFileW for redirected standard streams failed.");
            return handle;
        }

        // Applies Windows command-line escaping so spaces, quotes, and trailing slashes cannot alter arguments.
        private static string QuoteArgument(string value)
        {
            StringBuilder quoted = new StringBuilder("\"");
            int backslashes = 0;
            foreach (char character in value)
            {
                if (character == '\\')
                {
                    backslashes++;
                }
                else if (character == '\"')
                {
                    quoted.Append('\\', backslashes * 2 + 1);
                    quoted.Append('\"');
                    backslashes = 0;
                }
                else
                {
                    quoted.Append('\\', backslashes);
                    quoted.Append(character);
                    backslashes = 0;
                }
            }
            quoted.Append('\\', backslashes * 2);
            quoted.Append('\"');
            return quoted.ToString();
        }

        // Closes optional resources during partial-launch cleanup without masking the original Win32 error.
        private static void CloseQuietly(IntPtr handle)
        {
            if (handle != IntPtr.Zero && handle != new IntPtr(-1))
                CloseHandle(handle);
        }

        // Terminates only the process represented by this owned native handle.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool TerminateProcess(IntPtr process, uint exitCode);
    }
}
'@

function Assert-PathWithinRoot {
    # Canonicalizes both paths before accepting generated report paths or config cleanup targets.
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $resolvedPath = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd($separators)
    $prefix = $resolvedRoot + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must remain below the dedicated settings parity run directory."
    }
}

function Assert-CaptureRecord {
    # 截图必须与本轮 native 元数据一致；调用方可额外固定物理视口，防止 1×1 帧通过。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Capture,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [uint32]$ExpectedWidth = 0,
        [uint32]$ExpectedHeight = 0
    )

    $fileName = [string]$Capture.file
    if ([string]::IsNullOrWhiteSpace($fileName) -or [IO.Path]::GetFileName($fileName) -cne $fileName) {
        throw "Capture path is not a file name: $fileName"
    }
    if ([string]$Capture.pixelSha256 -notmatch '^[0-9a-f]{64}$') {
        throw "Capture pixel digest is invalid: $fileName"
    }

    $capturePath = Join-Path $RunRoot $fileName
    Assert-PathWithinRoot -Path $capturePath -Root $RunRoot -Label 'Screenshot'
    if (-not (Test-Path -LiteralPath $capturePath -PathType Leaf)) {
        throw "Screenshot is missing: $fileName"
    }
    $captureItem = Get-Item -LiteralPath $capturePath -Force
    if ($captureItem.Length -lt 24 -or $captureItem.Length -gt 134217728) {
        throw "Screenshot size is outside the supported bounds: $fileName"
    }

    $bytes = [IO.File]::ReadAllBytes($capturePath)
    $signature = [byte[]]@(137, 80, 78, 71, 13, 10, 26, 10)
    for ($index = 0; $index -lt $signature.Length; $index++) {
        if ($bytes[$index] -ne $signature[$index]) {
            throw "Screenshot is not a PNG: $fileName"
        }
    }
    if ([Text.Encoding]::ASCII.GetString($bytes, 12, 4) -cne 'IHDR') {
        throw "Screenshot has no PNG IHDR chunk: $fileName"
    }
    $pngWidth = [uint64]$bytes[16] * 16777216 + [uint64]$bytes[17] * 65536 + [uint64]$bytes[18] * 256 + [uint64]$bytes[19]
    $pngHeight = [uint64]$bytes[20] * 16777216 + [uint64]$bytes[21] * 65536 + [uint64]$bytes[22] * 256 + [uint64]$bytes[23]
    if ($pngWidth -ne [uint64]$Capture.width -or $pngHeight -ne [uint64]$Capture.height) {
        throw "Screenshot dimensions differ from the report: $fileName"
    }
    if (($ExpectedWidth -gt 0 -and $pngWidth -ne $ExpectedWidth) -or
        ($ExpectedHeight -gt 0 -and $pngHeight -ne $ExpectedHeight)) {
        throw "Screenshot dimensions differ from the native viewport: $fileName"
    }
}

function Assert-WorkbenchNativeEvidence {
    # 工作台专项共用隔离桌面、窗口尺寸和配置根的验收边界，不依赖报告自称处于后台。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ExpectedWidth,
        [Parameter(Mandatory = $true)][int]$ExpectedHeight,
        [switch]$VisibleMotion
    )

    $isolation = $Report.nativeIsolation
    $threadDesktop = [string]$isolation.uiThreadDesktopName
    $inputDesktop = [string]$isolation.inputDesktopName
    $expectedCaptureMode = if ($VisibleMotion) { 'private-visible-native-window' } else { 'hidden-native-window' }
    if ($Report.captureMode -cne $expectedCaptureMode -or
        [bool]$Report.windowVisible -ne [bool]$VisibleMotion -or -not $isolation.platformSupported -or
        -not $isolation.isolationProven -or $threadDesktop -cne $DesktopName -or
        -not $threadDesktop.StartsWith('KerminalRail-settings-', [StringComparison]::Ordinal) -or
        [string]::IsNullOrWhiteSpace($inputDesktop) -or
        $threadDesktop.Equals($inputDesktop, [StringComparison]::OrdinalIgnoreCase) -or
        [uint64]$isolation.processId -ne [uint64]$ProcessId) {
        throw 'The workbench report did not prove its owned native desktop and expected visibility.'
    }
    if ([double]$Report.windowWidth -ne $ExpectedWidth -or
        [double]$Report.windowHeight -ne $ExpectedHeight) {
        throw "The workbench viewport dimensions are invalid; expected ${ExpectedWidth}x${ExpectedHeight}."
    }
    $expectedConfig = [IO.Path]::GetFullPath((Join-Path $RunRoot 'config'))
    if (-not [IO.Path]::GetFullPath([string]$Report.configDirectory).Equals(
        $expectedConfig, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The workbench report used a different configuration root.'
    }
}

function Assert-WorkbenchMotionReport {
    # 只接受有真实平台 present 差分和足够动画样本的报告；屏幕扫描与硬件呈现仍不在此测量口径。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ExpectedWidth,
        [Parameter(Mandatory = $true)][int]$ExpectedHeight
    )
    Assert-WorkbenchNativeEvidence -Report $Report -RunRoot $RunRoot -ProcessId $ProcessId `
        -DesktopName $DesktopName -ExpectedWidth $ExpectedWidth -ExpectedHeight $ExpectedHeight -VisibleMotion
    if ($Report.schemaVersion -ne 1 -or
        $Report.measurementBoundary -cne 'gpui-platform-present-submission-not-screen-scanout' -or
        $Report.status -cne 'passed' -or -not $Report.windowActive -or
        -not $Report.nativeWindowVisible -or
        [uint64]$Report.motionWindowHandle -eq 0 -or
        [uint64]$Report.motionWindowHandle -ne [uint64]$Report.nativeIsolation.getActiveWindowHandle -or
        [uint64]$Report.motionWindowHandle -ne [uint64]$Report.nativeFocusWindowHandle -or
        $Report.warmupToggles -ne 4 -or $Report.toggleCycles -ne 20 -or
        $Report.completedTransitions -ne 40 -or $Report.minimumAnimationSamples -ne 100 -or
        $Report.initialSidebarVisible -ne $Report.finalSidebarVisible -or
        -not $Report.sessionAndPanePreserved -or $Report.frame.interval -cne 'since_baseline') {
        throw 'The workbench motion report is incomplete or did not preserve the workspace.'
    }
    $scale = [double]$Report.scaleFactor
    $refresh = [uint64]$Report.refreshIntervalNs
    $target = [uint64]$Report.targetP95Ns
    $animation = $Report.frame.gpuiAnimatedPresentInterval
    $bucketCount = ($animation.buckets | Measure-Object -Property sampleCount -Sum).Sum
    if (-not [double]::IsFinite($scale) -or $scale -le 0 -or $refresh -eq 0 -or
        $target -ne ($refresh + [uint64][Math]::Floor($refresh / 2)) -or
        $animation.unit -cne 'nanoseconds' -or [uint64]$animation.sampleCount -lt 100 -or
        [uint64]$bucketCount -ne [uint64]$animation.sampleCount -or
        [uint64]$animation.p95 -eq 0 -or [uint64]$animation.p95 -gt $target -or
        [string]::IsNullOrWhiteSpace([string]$Report.uiFontFamily) -or
        [string]::IsNullOrWhiteSpace([string]$Report.terminalFontPreference)) {
        throw 'The workbench motion samples, timing budget, or environment are invalid.'
    }
}

function Assert-WorkbenchAcceptanceReport {
    # 工作台和设置报告是不同范围；逐项回读真实菜单、焦点、拖拽与会话证据，不能仅凭进程退出判通过。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ExpectedWidth,
        [Parameter(Mandatory = $true)][int]$ExpectedHeight
    )
    Assert-WorkbenchNativeEvidence -Report $Report -RunRoot $RunRoot -ProcessId $ProcessId `
        -DesktopName $DesktopName -ExpectedWidth $ExpectedWidth -ExpectedHeight $ExpectedHeight
    if ($Report.schemaVersion -ne 4 -or
        $Report.captureBackend -cne 'gpui-root-click-and-state-readback') {
        throw 'The workbench report schema or backend is unsupported.'
    }

    $expectedChecks = [ordered]@{
        hostSelection = 'select-remote-host-without-details'
        localProfileSelection = 'select-local-profile-without-details'
        hostSelectionPreservedAgentTarget = 'host-selection-preserves-agent-target'
        profileSelectionPreservedAgentTarget = 'profile-selection-preserves-agent-target'
        defaultProfileRetry = 'default-profile-failure-retry'
        profileResolutionFailureRecovery = 'default-profile-resolution-failure-recovery'
        profileEnterOpensOneSession = 'enter-opens-local-profile'
        repeatedProfileTabs = 'ten-repeated-profile-independent-tabs'
        splitThenPlus = 'plus-after-split'
        closingNewTabPreservesOtherSessions = 'close-new-tab-preserves-other-sessions'
    }
    foreach ($name in $expectedChecks.Keys) {
        $check = $Report.$name
        if ($null -eq $check -or -not $check.passed -or
            $check.action -cne $expectedChecks[$name] -or
            [string]::IsNullOrWhiteSpace([string]$check.observed)) {
            throw "The workbench check is missing or failed: $name"
        }
    }

    $expectedMoves = @(
        @{ source = 'acceptance-host'; destination = $null },
        @{ source = 'acceptance-ungrouped-host'; destination = 'acceptance-drop-group' },
        @{ source = 'acceptance-default'; destination = 'acceptance-drop-group' },
        @{ source = 'acceptance-ungrouped-host'; destination = $null }
    )
    $expectedTargets = @('group', 'host', 'profile', 'ungrouped-host', 'sftp-copy-ssh', 'background')
    foreach ($theme in $script:ExpectedThemes) {
        $dragPath = Join-Path $RunRoot "sidebar-drag-$theme.json"
        $menuPath = Join-Path $RunRoot "sidebar-menus-$theme.json"
        if (-not (Test-Path -LiteralPath $dragPath -PathType Leaf) -or
            -not (Test-Path -LiteralPath $menuPath -PathType Leaf)) {
            throw "The workbench report is missing sidebar evidence for theme $theme."
        }
        $drag = Get-Content -LiteralPath $dragPath -Raw | ConvertFrom-Json
        $scale = [double]$drag.scaleFactor
        if ($drag.theme -cne $theme -or -not [double]::IsFinite($scale) -or $scale -le 0 -or
            @($drag.steps).Count -ne $expectedMoves.Count -or
            $drag.escape.activeAfterEscape -or $drag.escape.previewVisibleAfterEscape -or
            $drag.escape.committed -or -not $drag.escape.groupUnchanged -or
            -not $drag.escape.revisionUnchanged -or
            $drag.terminalCountBefore -ne 1 -or $drag.terminalCountAfter -ne 1) {
            throw "The workbench drag evidence is incomplete for theme $theme."
        }
        for ($index = 0; $index -lt $expectedMoves.Count; $index++) {
            $step = $drag.steps[$index]
            $expected = $expectedMoves[$index]
            if ($step.source -cne $expected.source -or $step.destination -cne $expected.destination -or
                [string]$step.beforeRevision -notmatch '^[0-9a-f]{64}$' -or
                [string]$step.afterRevision -notmatch '^[0-9a-f]{64}$' -or
                $step.beforeRevision -ceq $step.afterRevision) {
                throw "The workbench drag move is invalid for theme $theme at index $index."
            }
        }

        $physicalWidth = [uint32][Math]::Round($ExpectedWidth * $scale, [MidpointRounding]::AwayFromZero)
        $physicalHeight = [uint32][Math]::Round($ExpectedHeight * $scale, [MidpointRounding]::AwayFromZero)
        $menus = @(Get-Content -LiteralPath $menuPath -Raw | ConvertFrom-Json)
        $remainingTargets = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($target in $expectedTargets) { [void]$remainingTargets.Add($target) }
        if ($menus.Count -ne $expectedTargets.Count) {
            throw "The workbench menu evidence is incomplete for theme $theme."
        }
        foreach ($menu in $menus) {
            if (-not $remainingTargets.Remove([string]$menu.target) -or $menu.menuCount -ne 1 -or
                -not $menu.escapeRestoredFocus -or -not $menu.actionOpenedEditor -or
                -not $menu.editorCancelled -or @($menu.labels).Count -eq 0) {
                throw "The workbench menu or focus check failed for theme $theme."
            }
            if ($menu.menuCapture.file -cne "sidebar-menu-$theme-$($menu.target).png" -or
                $menu.actionCapture.file -cne "sidebar-action-$theme-$($menu.target).png") {
                throw "The workbench menu capture name is invalid for theme $theme."
            }
            Assert-CaptureRecord -Capture $menu.menuCapture -RunRoot $RunRoot -ExpectedWidth $physicalWidth -ExpectedHeight $physicalHeight
            Assert-CaptureRecord -Capture $menu.actionCapture -RunRoot $RunRoot -ExpectedWidth $physicalWidth -ExpectedHeight $physicalHeight
        }
    }

    $overflowPath = Join-Path $RunRoot 'sidebar-drag-overflow.json'
    if (-not (Test-Path -LiteralPath $overflowPath -PathType Leaf)) {
        throw 'The workbench overflow drag evidence is missing.'
    }
    $overflow = Get-Content -LiteralPath $overflowPath -Raw | ConvertFrom-Json
    if (-not $overflow.edgeScrollReachedTarget -or $overflow.overflowGroupCount -ne 24 -or
        $overflow.destination -cne 'acceptance-overflow-23' -or
        $overflow.restoredGroup -cne 'acceptance-pinned-group' -or
        $overflow.terminalCountBefore -ne 1 -or $overflow.terminalCountAfter -ne 1) {
        throw 'The workbench overflow drag did not reach and restore its target.'
    }
}

function Assert-RightRailRegions {
    # 固定区域只比较真实稳定的导航、标题栏和工具栏几何；目标文字内容不在此处冒充固定证据。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Regions,
        [Parameter(Mandatory = $true)]$Reference,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $regions = @($Regions)
    $expectedRegions = @($Reference)
    if ($regions.Count -ne $expectedRegions.Count -or $regions.Count -ne 5) {
        throw "Right-rail fixed region count is invalid: $Label"
    }
    for ($index = 0; $index -lt $regions.Count; $index++) {
        $region = $regions[$index]
        $expected = $expectedRegions[$index]
        if ([string]$region.key -cne [string]$expected.key -or
            [string]$region.selector -cne [string]$expected.selector -or
            [string]$region.pixelSha256 -notmatch '^[0-9a-f]{64}$') {
            throw "Right-rail fixed region identity is invalid: $Label/$index"
        }
        foreach ($field in @('x', 'y', 'width', 'height')) {
            $value = [double]$region.bounds.$field
            $expectedValue = [double]$expected.bounds.$field
            if (-not [double]::IsFinite($value) -or -not [double]::IsFinite($expectedValue)) {
                throw "Right-rail fixed region $field is not finite: $Label/$index"
            }
            if ([Math]::Abs($value - $expectedValue) -gt 0.6) {
                throw "Right-rail fixed region $field changed: $Label/$index"
            }
        }
        if ([double]$region.bounds.width -le 0 -or [double]$region.bounds.height -le 0) {
            throw "Right-rail fixed region is empty: $Label/$index"
        }
    }
}

function Get-RightRailRegion {
    # 按稳定 key 取唯一区域，避免依赖数组下标把导航、标题栏和日志列表的证据互换。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Frame,
        [Parameter(Mandatory = $true)][string]$Key
    )

    $found = @(@($Frame.regions) | Where-Object { [string]$_.key -ceq $Key })
    if ($found.Count -ne 1) {
        throw "Right-rail fixed region is missing or duplicated: $Key"
    }
    return $found[0]
}

function Assert-RightRailFrame {
    # requestedFocusPaneId 只是请求记录，不能当真实焦点；activePaneId 才是渲染时的实际活动 pane。
    # 固定区域几何和截图必须与物理视口一致。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Frame,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string]$FocusPane,
        [ValidateSet('from', 'to', 'either')][string]$ExpectedActivePane = 'to',
        [Parameter(Mandatory = $true)][string]$FromPane,
        [Parameter(Mandatory = $true)][string]$ToPane,
        [ValidateSet('true', 'false', 'either')][string]$ExpectedARow = 'either',
        [ValidateSet('true', 'false', 'either')][string]$ExpectedBRow = 'either',
        [ValidateSet('true', 'false', 'either')][string]$ExpectedARowPresent = 'either',
        [ValidateSet('true', 'false', 'either')][string]$ExpectedBRowPresent = 'either',
        [Parameter(Mandatory = $true)]$ReferenceRegions,
        [Parameter(Mandatory = $true)][uint32]$ExpectedWidth,
        [Parameter(Mandatory = $true)][uint32]$ExpectedHeight,
        [Parameter(Mandatory = $true)]$CaptureNames
    )

    if ([string]$Frame.requestedFocusPaneId -cne $FocusPane) {
        throw "Right-rail frame requested pane is invalid: $Label"
    }
    $expectedActivePane = switch ($ExpectedActivePane) {
        'to' { $ToPane }
        'from' { $FromPane }
        default { $null }
    }
    if ($null -ne $expectedActivePane -and [string]$Frame.activePaneId -cne $expectedActivePane) {
        throw "Right-rail frame active pane is invalid: $Label"
    }
    if ($ExpectedActivePane -eq 'either' -and
        ([string]::IsNullOrWhiteSpace([string]$Frame.activePaneId) -or
            [string]$Frame.activePaneId -cnotin @($FromPane, $ToPane))) {
        throw "Right-rail first-sample active pane is neither the source nor target pane: $Label"
    }
    if ($null -eq $Frame.PSObject.Properties['aMarkerRowVisibleInCurrentPage'] -or
        $null -eq $Frame.PSObject.Properties['bMarkerRowVisibleInCurrentPage'] -or
        $null -eq $Frame.PSObject.Properties['aMarkerRowPresentInCurrentPage'] -or
        $null -eq $Frame.PSObject.Properties['bMarkerRowPresentInCurrentPage']) {
        throw "Right-rail frame is missing current-page row scope fields: $Label"
    }
    if ($ExpectedARow -ne 'either' -and
        [bool]$Frame.aMarkerRowVisibleInCurrentPage -ne ($ExpectedARow -ceq 'true')) {
        throw "Right-rail pane A current-page history visibility is invalid: $Label"
    }
    if ($ExpectedBRow -ne 'either' -and
        [bool]$Frame.bMarkerRowVisibleInCurrentPage -ne ($ExpectedBRow -ceq 'true')) {
        throw "Right-rail pane B current-page history visibility is invalid: $Label"
    }
    if ($ExpectedARowPresent -ne 'either' -and
        [bool]$Frame.aMarkerRowPresentInCurrentPage -ne ($ExpectedARowPresent -ceq 'true')) {
        throw "Right-rail pane A current-page history presence is invalid: $Label"
    }
    if ($ExpectedBRowPresent -ne 'either' -and
        [bool]$Frame.bMarkerRowPresentInCurrentPage -ne ($ExpectedBRowPresent -ceq 'true')) {
        throw "Right-rail pane B current-page history presence is invalid: $Label"
    }
    # 纯黑、全透明或均匀空白截图也会产生稳定哈希；逐帧核对整帧与日志列表的
    # 颜色数量和亮度跨度，避免把无内容渲染当成目标数据有效采样。
    # @author kongweiguang
    if ($null -eq $Frame.PSObject.Properties['pixelContent']) {
        throw "Right-rail frame pixel-content evidence is missing: $Label"
    }
    $pixelContent = @($Frame.pixelContent)
    if ($pixelContent.Count -ne 2) {
        throw "Right-rail frame pixel-content evidence is incomplete: $Label"
    }
    foreach ($expected in @(
            [pscustomobject]@{ key = 'frame'; minColors = 8; minLumaRange = 8 },
            [pscustomobject]@{ key = 'logsList'; minColors = 4; minLumaRange = 4 })) {
        $found = @($pixelContent | Where-Object { [string]$_.key -ceq $expected.key })
        if ($found.Count -ne 1) {
            throw "Right-rail pixel-content evidence is missing or duplicated: $Label/$($expected.key)"
        }
        $content = $found[0]
        $distinct = [uint32]$content.distinctRgbColors
        $lumaRange = [int]$content.lumaMax - [int]$content.lumaMin
        if (-not [bool]$content.contentVerified -or
            [bool]$content.fullyTransparent -or
            [uint64]$content.sampledPixels -eq 0 -or
            [uint32]$content.requiredMinDistinctRgbColors -ne [uint32]$expected.minColors -or
            [uint32]$content.requiredMinLumaRange -ne [uint32]$expected.minLumaRange -or
            $distinct -lt [uint32]$expected.minColors -or
            $lumaRange -lt [int]$expected.minLumaRange -or
            [int]$content.lumaMin -gt [int]$content.lumaMax -or
            [uint32]$content.physicalBounds.right -le [uint32]$content.physicalBounds.left -or
            [uint32]$content.physicalBounds.bottom -le [uint32]$content.physicalBounds.top) {
            throw "Right-rail pixel-content gate failed: $Label/$($expected.key)"
        }
    }
    Assert-RightRailRegions -Regions $Frame.regions -Reference $ReferenceRegions -Label $Label
    Assert-CaptureRecord -Capture $Frame.capture -RunRoot $RunRoot `
        -ExpectedWidth $ExpectedWidth -ExpectedHeight $ExpectedHeight
    $fileName = [string]$Frame.capture.file
    if (-not $CaptureNames.Add($fileName)) {
        throw "Duplicate right-rail capture: $fileName"
    }
}

function Assert-RightRailTransition {
    # 首张采样只证明点击后的即时绘制，允许活动 pane 尚未切换；过渡帧和稳定帧才要求
    # 目标焦点、目标数据以及至少一张不同于首张和稳定帧的中间态图像。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Transition,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string]$ThemeName,
        [Parameter(Mandatory = $true)][string]$Direction,
        [Parameter(Mandatory = $true)][string]$ExpectedRow,
        [Parameter(Mandatory = $true)][string]$ForbiddenRow,
        [Parameter(Mandatory = $true)][string]$FromPane,
        [Parameter(Mandatory = $true)][string]$ToPane,
        [Parameter(Mandatory = $true)]$ReferenceRegions,
        [Parameter(Mandatory = $true)]$SourceFrame,
        [Parameter(Mandatory = $true)][uint32]$ExpectedWidth,
        [Parameter(Mandatory = $true)][uint32]$ExpectedHeight,
        [Parameter(Mandatory = $true)]$CaptureNames
    )

    if ([string]$Transition.fromPaneId -cne $FromPane -or
        [string]$Transition.toPaneId -cne $ToPane -or
        [string]$Transition.expectedRow -cne $ExpectedRow -or
        [string]$Transition.forbiddenRow -cne $ForbiddenRow -or
        -not [bool]$Transition.dataTransitionPassed -or
        -not [bool]$Transition.expectedRowVisibleInCurrentPage -or
        -not [bool]$Transition.forbiddenRowAbsentFromCurrentPage -or
        -not [bool]$Transition.fixedGeometryStable -or
        -not [bool]$Transition.targetHashChanged) {
        throw "Right-rail transition metadata is invalid: $Label"
    }
    # 毫秒来自 as_millis()，真实耗时不足 1ms 时会截断为 0，不能拿 >=1 当作“确实测到”。
    # 以微秒字段的存在、顺序和与总耗时的相等关系为证，并记录计时起点与精度。
    $expectedTimingOrigin = 'Instant::now immediately before synthetic click dispatch, after pre-click selector lookup and viewport validation; first-sample duration includes click dispatch, one explicit draw/clear, render_to_image, region hashing, PNG save, and row checks; transition/settled durations use the same click origin and include each frame''s settle wait, capture work, and final copy'
    if ($null -eq $Transition.PSObject.Properties['elapsedMicros'] -or
        $null -eq $Transition.elapsedMicros -or
        $null -eq $Transition.PSObject.Properties['timingOrigin'] -or
        [string]$Transition.timingOrigin -cne $expectedTimingOrigin -or
        $null -eq $Transition.PSObject.Properties['timingPrecision'] -or
        [string]$Transition.timingPrecision -cne 'microseconds') {
        throw "Right-rail transition is missing microsecond timing provenance: $Label"
    }
    $transitionElapsedMicros = [uint64]$Transition.elapsedMicros
    if ($null -ne $Transition.PSObject.Properties['elapsedMs'] -and
        $null -ne $Transition.elapsedMs -and
        [uint64]$Transition.elapsedMs -ne [uint64][Math]::Floor([double]$transitionElapsedMicros / 1000)) {
        throw "Right-rail transition millisecond duration disagrees with its microsecond measurement: $Label"
    }
    $expectedFirstSampleCoverage = 'single-draw-after-click-dispatch-in-same-window-update; renderer-first-present-and-first-gpui-draw-not-proven'
    if ([string]$Transition.firstSampleCoverage -cne $expectedFirstSampleCoverage) {
        throw "Right-rail first-sample coverage claim is invalid: $Label"
    }

    $firstSample = $Transition.firstSample
    $transitionFrames = @($Transition.transitionFrames)
    $settledFrame = $Transition.settledFrame
    if ($null -eq $firstSample -or $null -eq $settledFrame -or
        $transitionFrames.Count -lt 1 -or $transitionFrames.Count -gt 24) {
        throw "Right-rail transition frame set is incomplete: $Label"
    }
    if ([string]$firstSample.capture.file -cne "right-rail-$ThemeName-$Direction-first-sample.png" -or
        [string]$settledFrame.capture.file -cne "right-rail-$ThemeName-$Direction-settled.png") {
        throw "Right-rail transition capture names are invalid: $Label"
    }
    if ($null -eq $firstSample.PSObject.Properties['sampleElapsedMicros'] -or
        $null -eq $firstSample.sampleElapsedMicros -or
        [uint64]$firstSample.sampleElapsedMicros -gt $transitionElapsedMicros) {
        throw "Right-rail first sample is missing its microsecond measurement or is outside the transition interval: $Label"
    }
    if ($null -ne $firstSample.PSObject.Properties['sampleElapsedMs'] -and
        $null -ne $firstSample.sampleElapsedMs -and
        [uint64]$firstSample.sampleElapsedMs -ne [uint64][Math]::Floor([double]$firstSample.sampleElapsedMicros / 1000)) {
        throw "Right-rail first sample millisecond timing disagrees with its microsecond measurement: $Label"
    }
    if ($null -eq $settledFrame.PSObject.Properties['sampleElapsedMicros'] -or
        $null -eq $settledFrame.sampleElapsedMicros -or
        [uint64]$settledFrame.sampleElapsedMicros -ne $transitionElapsedMicros) {
        throw "Right-rail settled frame timing does not match the reported transition duration: $Label"
    }
    if ($null -ne $settledFrame.PSObject.Properties['sampleElapsedMs'] -and
        $null -ne $settledFrame.sampleElapsedMs -and
        [uint64]$settledFrame.sampleElapsedMs -ne [uint64][Math]::Floor([double]$settledFrame.sampleElapsedMicros / 1000)) {
        throw "Right-rail settled frame millisecond timing disagrees with its microsecond measurement: $Label"
    }
    $transitionPattern = '^right-rail-' + [regex]::Escape($ThemeName) + '-' +
        [regex]::Escape($Direction) + '-transition-[0-9]{2}\.png$'
    $previousTransitionElapsedMicros = [uint64]$firstSample.sampleElapsedMicros
    foreach ($frame in $transitionFrames) {
        if ([string]$frame.capture.file -notmatch $transitionPattern) {
            throw "Right-rail transition capture name is invalid: $Label"
        }
        if ($null -eq $frame.PSObject.Properties['sampleElapsedMicros'] -or
            $null -eq $frame.sampleElapsedMicros) {
            throw "Right-rail transition frame is missing microsecond timing: $Label"
        }
        if ($null -ne $frame.PSObject.Properties['sampleElapsedMs'] -and
            $null -ne $frame.sampleElapsedMs -and
            [uint64]$frame.sampleElapsedMs -ne [uint64][Math]::Floor([double]$frame.sampleElapsedMicros / 1000)) {
            throw "Right-rail transition frame millisecond timing disagrees with its microsecond measurement: $Label"
        }
        $frameElapsedMicros = [uint64]$frame.sampleElapsedMicros
        if ($frameElapsedMicros -lt $previousTransitionElapsedMicros) {
            throw "Right-rail transition frame timing is not monotonic: $Label"
        }
        $previousTransitionElapsedMicros = $frameElapsedMicros
    }

    # 首张采样发生在 click 派发后的同一次 update 内，active pane 可能仍是来源 pane；
    # 因此只要求焦点已指向目标，数据判断全部留给稳定帧。
    Assert-RightRailFrame -Frame $firstSample -RunRoot $RunRoot -Label "$Label/first-sample" `
        -FocusPane $ToPane -ExpectedActivePane 'either' -FromPane $FromPane -ToPane $ToPane `
        -ReferenceRegions $ReferenceRegions -ExpectedWidth $ExpectedWidth `
        -ExpectedHeight $ExpectedHeight -CaptureNames $CaptureNames
    for ($index = 0; $index -lt $transitionFrames.Count; $index++) {
        Assert-RightRailFrame -Frame $transitionFrames[$index] -RunRoot $RunRoot `
            -Label "$Label/transition-$index" -FocusPane $ToPane -ExpectedActivePane 'to' `
            -FromPane $FromPane -ToPane $ToPane `
            -ReferenceRegions $ReferenceRegions -ExpectedWidth $ExpectedWidth `
            -ExpectedHeight $ExpectedHeight -CaptureNames $CaptureNames
    }
    $expectedA = if ($ExpectedRow -ceq 'a') { 'true' } else { 'false' }
    $expectedB = if ($ExpectedRow -ceq 'b') { 'true' } else { 'false' }
    Assert-RightRailFrame -Frame $settledFrame -RunRoot $RunRoot -Label "$Label/settled" `
        -FocusPane $ToPane -ExpectedActivePane 'to' -FromPane $FromPane -ToPane $ToPane `
        -ExpectedARow $expectedA -ExpectedBRow $expectedB `
        -ExpectedARowPresent $expectedA -ExpectedBRowPresent $expectedB `
        -ReferenceRegions $ReferenceRegions -ExpectedWidth $ExpectedWidth `
        -ExpectedHeight $ExpectedHeight -CaptureNames $CaptureNames

    $sourceListHash = [string](Get-RightRailRegion -Frame $SourceFrame -Key 'logsList').pixelSha256
    $firstListHash = [string](Get-RightRailRegion -Frame $firstSample -Key 'logsList').pixelSha256
    $settledListHash = [string](Get-RightRailRegion -Frame $settledFrame -Key 'logsList').pixelSha256
    if ($sourceListHash -ceq $settledListHash) {
        throw "Right-rail transition did not change the logs list: $Label"
    }
    # 端点不同仍可能只是空白加载态或布局变化；中间帧必须同时证明活动目标 pane、
    # 新目标行可见/存在、旧目标行不可见/不存在，且图像不同于首张和稳定帧。
    $observedIntermediateCount = 0
    $observedFirstIntermediateElapsedMicros = $null
    foreach ($frame in $transitionFrames) {
        $expectedVisible = if ($ExpectedRow -ceq 'a') {
            [bool]$frame.aMarkerRowVisibleInCurrentPage
        } else {
            [bool]$frame.bMarkerRowVisibleInCurrentPage
        }
        $expectedPresent = if ($ExpectedRow -ceq 'a') {
            [bool]$frame.aMarkerRowPresentInCurrentPage
        } else {
            [bool]$frame.bMarkerRowPresentInCurrentPage
        }
        $forbiddenVisible = if ($ForbiddenRow -ceq 'a') {
            [bool]$frame.aMarkerRowVisibleInCurrentPage
        } else {
            [bool]$frame.bMarkerRowVisibleInCurrentPage
        }
        $forbiddenPresent = if ($ForbiddenRow -ceq 'a') {
            [bool]$frame.aMarkerRowPresentInCurrentPage
        } else {
            [bool]$frame.bMarkerRowPresentInCurrentPage
        }
        if (-not $expectedVisible -or -not $expectedPresent -or
            $forbiddenVisible -or $forbiddenPresent -or
            [string]$frame.activePaneId -cne $ToPane) {
            continue
        }
        $frameHash = [string](Get-RightRailRegion -Frame $frame -Key 'logsList').pixelSha256
        if ($frameHash -cne $firstListHash -and $frameHash -cne $settledListHash) {
            $observedIntermediateCount += 1
            if ($null -eq $observedFirstIntermediateElapsedMicros) {
                $observedFirstIntermediateElapsedMicros = [uint64]$frame.sampleElapsedMicros
            }
        }
    }
    if (-not [bool]$Transition.intermediateStateObserved -or
        $observedIntermediateCount -lt 1 -or
        [int]$Transition.intermediateFrameCount -ne $observedIntermediateCount -or
        [string]$Transition.intermediateClaim -cne 'target-data-visible-old-data-absent-intermediate-sample-not-animation' -or
        [bool]$Transition.opacityObserved -or
        [bool]$Transition.fadeDurationVerified -or
        [string]$Transition.limitation -cne 'target-data intermediate samples only; normal-window present, opacity, running state, and 140ms fade duration are not instrumented or proven; hash difference is not an animation measurement' -or
        $null -eq $Transition.PSObject.Properties['firstIntermediateElapsedMicros'] -or
        $null -eq $Transition.firstIntermediateElapsedMicros -or
        [uint64]$Transition.firstIntermediateElapsedMicros -ne $observedFirstIntermediateElapsedMicros) {
        throw "Right-rail intermediate target-data evidence is incomplete or claims an unproven fade: $Label"
    }
    # 只把跨切换仍然固定的导航栏和来源工具栏做像素级比较；工作区标签栏会反映活动 pane，
    # 其几何仍按固定区域核对，但不把合法的活动态高亮当成布局漂移。
    foreach ($key in @('navigationRail', 'rightRailToolbar')) {
        $sourceHash = [string](Get-RightRailRegion -Frame $SourceFrame -Key $key).pixelSha256
        $settledHash = [string](Get-RightRailRegion -Frame $settledFrame -Key $key).pixelSha256
        if ($sourceHash -cne $settledHash) {
            throw "Right-rail fixed region changed pixels during transition: $Label/$key"
        }
    }
}

function Assert-RightRailSplitCleanup {
    # 右栏验收不逐个显式关闭 pane，只能依赖组合根 shutdown；回执必须证明关闭前后
    # 会话表从 A/B 两条变为空，并记录 explicitSessionClose=false，避免把 shutdown 冒充显式回收。
    # 成功运行还核对精确会话 ID，确保回收的是本轮的 A/B，而不是碰巧清空其他会话。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [ValidateSet('success', 'failure')][string]$ExpectedUiResult,
        $Report = $null
    )

    $cleanupPath = Join-Path $RunRoot 'right-rail-split-switch-cleanup.json'
    if (-not (Test-Path -LiteralPath $cleanupPath -PathType Leaf)) {
        throw 'The right-rail split cleanup receipt is missing.'
    }
    $cleanup = Get-Content -LiteralPath $cleanupPath -Raw | ConvertFrom-Json
    if ($cleanup.schemaVersion -ne 1 -or $cleanup.scope -cne 'right-rail-split' -or
        [string]$cleanup.uiResult -cne $ExpectedUiResult -or
        -not [bool]$cleanup.shutdownSucceeded -or
        [bool]$cleanup.explicitSessionClose -or
        [string]$cleanup.ptyCleanupSource -cne 'composition.shutdown_until:SessionService::close_all_until') {
        throw 'The right-rail split cleanup receipt schema or shutdown claim is invalid.'
    }
    if ($null -ne $cleanup.shutdownError -or
        $null -ne $cleanup.sessionsBeforeError -or
        $null -ne $cleanup.sessionsAfterError) {
        throw 'The right-rail split cleanup receipt contains a shutdown or session-list error.'
    }
    if ($ExpectedUiResult -ceq 'success' -and $null -ne $cleanup.uiError) {
        throw 'The successful right-rail split run reported a UI error in its cleanup receipt.'
    }
    if ($ExpectedUiResult -ceq 'failure' -and
        [string]::IsNullOrWhiteSpace([string]$cleanup.uiError)) {
        throw 'The failed right-rail split run did not record its UI error in the cleanup receipt.'
    }

    $sessionsBefore = @($cleanup.sessionsBefore | ForEach-Object { [string]$_ } | Sort-Object -Unique)
    $sessionsAfter = @($cleanup.sessionsAfter | ForEach-Object { [string]$_ })
    if ($sessionsAfter.Count -ne 0) {
        throw 'The right-rail split cleanup receipt still lists sessions after shutdown.'
    }
    if ($sessionsBefore.Count -eq 0) {
        if ([bool]$cleanup.ptyReclaimed) {
            throw 'The right-rail split cleanup receipt claims reclamation without observing a session.'
        }
    } elseif (-not [bool]$cleanup.ptyReclaimed) {
        throw 'The right-rail split cleanup receipt observed sessions but did not prove their reclamation.'
    }
    if ($ExpectedUiResult -ceq 'success' -and $sessionsBefore.Count -lt 2) {
        throw 'The successful right-rail split cleanup receipt did not observe both A/B sessions before shutdown.'
    }
    if ($null -ne $Report) {
        $expectedSessions = @(
            [string]$Report.profileA.sessionId,
            [string]$Report.profileB.sessionId
        ) | Sort-Object -Unique
        if (($expectedSessions -join "`n") -cne ($sessionsBefore -join "`n")) {
            throw 'The right-rail split cleanup receipt does not match the reported A/B session IDs.'
        }
    }
}

function Assert-RightRailSplitSwitchReport {
    # 右栏分屏切换必须由私有桌面隐藏窗口证明，逐主题核对 A/B 目标数据、固定区域几何和双方向切换。
    # 目标文字与悬停允许变化，A 的历史条目和动作不能出现在 B 的稳定结果中。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ExpectedWidth,
        [Parameter(Mandatory = $true)][int]$ExpectedHeight,
        [Parameter(Mandatory = $true)][string]$ExpectedPowerShell
    )

    Assert-WorkbenchNativeEvidence -Report $Report -RunRoot $RunRoot -ProcessId $ProcessId `
        -DesktopName $DesktopName -ExpectedWidth $ExpectedWidth -ExpectedHeight $ExpectedHeight
    if ($Report.schemaVersion -ne 1 -or
        $Report.captureBackend -cne 'gpui-root-pane-focus-split-and-logs-readback' -or
        $Report.inputSource -cne 'gpui-key-and-mouse-events-on-private-desktop' -or
        [bool]$Report.windowVisible -or
        -not [bool]$Report.profileMarkersDistinct -or
        -not [bool]$Report.paneSessionsDistinct -or
        -not [bool]$Report.workspacePreserved) {
        throw 'The right-rail split report schema, backend, or final state is invalid.'
    }
    if ($Report.status -cne 'passed') {
        $targetDataLimitation = if ($null -ne $Report.PSObject.Properties['targetDataObservation']) {
            [string]$Report.targetDataObservation.limitation
        } else {
            ''
        }
        if ([string]::IsNullOrWhiteSpace($targetDataLimitation)) {
            $targetDataLimitation = 'no target-data observation limitation was reported'
        }
        throw "The right-rail split report status is '$($Report.status)'; target-data intermediate evidence is insufficient: $targetDataLimitation"
    }
    if (-not [bool]$Report.debugBuild) {
        throw 'The right-rail split report was not produced by a debug build with production debug selectors.'
    }
    $expectedRowEvidenceScope = 'rendered-logs-page-selectors; not-a-full-history-or-backing-store-enumeration'
    if ([string]$Report.rowEvidenceScope -cne $expectedRowEvidenceScope -or
        [bool]$Report.systemAppearanceVerified) {
        throw 'The right-rail split row evidence scope or system-appearance claim is invalid.'
    }
    $expectedConfigDirectory = [IO.Path]::GetFullPath((Join-Path $RunRoot 'config')).TrimEnd('\', '/')
    $observedConfigDirectory = [IO.Path]::GetFullPath([string]$Report.configDirectory).TrimEnd('\', '/')
    $expectedTempDirectory = [IO.Path]::GetFullPath((Join-Path $RunRoot 'temp')).TrimEnd('\', '/')
    $observedTempDirectory = [IO.Path]::GetFullPath([string]$Report.tempDirectory).TrimEnd('\', '/')
    if (-not $observedConfigDirectory.Equals($expectedConfigDirectory, [StringComparison]::OrdinalIgnoreCase) -or
        -not $observedTempDirectory.Equals($expectedTempDirectory, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail split config or TEMP/TMP directory escaped the exclusive run root.'
    }
    # 隐藏窗口在已证明的私有桌面上仍可能被报告为 active；这里不把 active 当成
    # 正常窗口 present，只要求报告显式声明隐藏、手动 draw 和未验证 present。
    # @author kongweiguang
    $expectedStatusScope = 'right-rail-target-data-switch-and-environment; not-animation-not-window-present-not-file-sandbox'
    $expectedObservationBoundary = 'hidden-window; window is hidden and may be OS-inactive; acceptance-active override enabled; manual draw_acceptance_frame/window.draw+clear; window active state is reported separately and is not a normal-window present proof; target-data intermediate samples only; normal-window present, opacity, running state, and 140ms fade are not proven; KnownFolder paths are not redirected and the process is not a file sandbox'
    if (-not [bool]$Report.windowHiddenVerified -or
        $null -eq $Report.PSObject.Properties['windowActive'] -or
        -not [bool]$Report.acceptanceActiveOverride -or
        -not [bool]$Report.manualDrawUsed -or
        [bool]$Report.normalWindowPresentVerified -or
        [string]$Report.statusScope -cne $expectedStatusScope -or
        [string]$Report.observationBoundary -cne $expectedObservationBoundary) {
        throw 'The right-rail split report does not declare its hidden-window observation boundary.'
    }
    $expectedTargetDataClaim = 'target-data-visible-old-data-absent-intermediate-sample-not-animation'
    $expectedTargetDataBoundary = 'hidden-window-manual-draw; window is hidden and may be OS-inactive; its active state is reported separately and is not a normal-window present proof; intermediate target-data samples only; normal-window present, opacity, running state, and 140ms fade are not instrumented or proven; hash difference is not an animation measurement'
    # 报告状态已经是 passed，因此限制文案必须是“观察到中间态”的版本；出现
    # no-intermediate 文案说明方向计数与限制自相矛盾。
    $expectedTargetDataLimitation = 'target-data intermediate samples only; normal-window present, opacity, running state, and 140ms fade duration are not instrumented or proven; hash difference is not an animation measurement'
    if ($null -eq $Report.PSObject.Properties['targetDataObservation'] -or
        -not [bool]$Report.targetDataObservation.observed -or
        [int]$Report.targetDataObservation.observedDirections -ne [int]$Report.targetDataObservation.totalDirections -or
        [int]$Report.targetDataObservation.totalDirections -ne 6 -or
        [string]$Report.targetDataObservation.claim -cne $expectedTargetDataClaim -or
        [bool]$Report.targetDataObservation.opacityObserved -or
        [bool]$Report.targetDataObservation.fadeDurationVerified -or
        [bool]$Report.targetDataObservation.windowPresentVerified -or
        [string]$Report.targetDataObservation.measurementBoundary -cne $expectedTargetDataBoundary -or
        [string]$Report.targetDataObservation.limitation -cne $expectedTargetDataLimitation) {
        throw 'The right-rail split target-data observation is incomplete or claims an unproven animation or window present.'
    }
    $expectedHome = Join-Path $RunRoot 'home'
    $trustedModules = [IO.Path]::GetFullPath((Join-Path $PSHOME 'Modules'))
    $expectedHistoryRoot = [IO.Path]::GetFullPath((Join-Path $expectedHome 'AppData\Roaming\Microsoft\Windows\PowerShell\PSReadLine'))
    $expectedEnvironment = [ordered]@{
        # 原生应用子进程是严格白名单；外层 pwsh 只重写受控值，其余继承变量未清除。
        scope = 'native-child-process-environment-whitelist; outer-pwsh-rewrites-controlled-values-but-does-not-scrub-inherited-variables; known-folder-api-not-covered'
        home = $expectedHome
        appdata = Join-Path $expectedHome 'AppData\Roaming'
        localappdata = Join-Path $expectedHome 'AppData\Local'
        temp = Join-Path $RunRoot 'temp'
        psModulePath = $trustedModules
        configRoot = Join-Path $RunRoot 'config'
        programData = Join-Path $RunRoot 'program-data'
        public = Join-Path $RunRoot 'public'
    }
    $environment = $Report.environmentIsolation
    $expectedModuleProbeScope = 'outer-isolated-pwsh-probe-only; child shell inherits trusted modules and is not separately probed'
    $expectedHistoryScope = 'outer-isolated-pwsh-probe-plus-run-root-env-mapping; child shell actual path is inferred, not probed'
    if ($null -eq $environment -or
        [string]$environment.scope -cne $expectedEnvironment.scope -or
        -not [bool]$environment.validatedBeforeComposition -or
        -not [bool]$environment.moduleProbeVerified -or
        [string]$environment.moduleProbeScope -cne $expectedModuleProbeScope -or
        [string]$environment.psreadlineHistoryScope -cne $expectedHistoryScope -or
        [bool]$environment.psreadlineHistoryChildActualVerified -or
        [bool]$environment.knownFolderApiCovered) {
        throw 'The right-rail split process-environment isolation evidence is incomplete.'
    }
    foreach ($name in @('home', 'appdata', 'localappdata', 'temp', 'psModulePath', 'configRoot', 'programData', 'public')) {
        $actual = [IO.Path]::GetFullPath([string]$environment.$name).TrimEnd('\', '/')
        $expected = [IO.Path]::GetFullPath([string]$expectedEnvironment[$name]).TrimEnd('\', '/')
        if (-not $actual.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
            throw "The right-rail split isolated environment path is invalid: $name"
        }
    }
    # PSReadLine 历史路径只是外层探针加 run-root 环境映射的期望值；子 shell 实际路径
    # 没有回读，因此只核对它落在隔离 APPDATA 下，并把“子进程已实测”固定为 false。
    $reportedHistory = [IO.Path]::GetFullPath([string]$environment.psreadlineHistoryExpectedPath).TrimEnd('\', '/')
    $historyPrefix = $expectedHistoryRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $reportedHistory.StartsWith($historyPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail split PSReadLine history path escaped the isolated APPDATA.'
    }
    $knownFolder = $Report.knownFolderEvidence
    $expectedContentGuard = 'checkpoints only: initial active tool must be None or Logs and final active tool must be Logs; transient activation and other components'' KnownFolder access are not proven'
    $expectedTrustedModuleCommandsScope = 'outer-isolated-pwsh-probe-only; child shell inherits trusted modules but is not separately probed'
    if ($null -eq $knownFolder -or
        [bool]$knownFolder.redirected -or
        [string]$knownFolder.contentAccessGuard -cne $expectedContentGuard -or
        -not [bool]$knownFolder.initialActiveToolChecked -or
        -not [bool]$knownFolder.finalActiveToolIsLogs -or
        [bool]$knownFolder.transientActivationVerified -or
        -not [bool]$knownFolder.psreadlineHistoryExpectedUnderIsolatedAppdata -or
        [bool]$knownFolder.psreadlineHistoryChildActualVerified -or
        -not [bool]$knownFolder.trustedModuleCommandsVerified -or
        [string]$knownFolder.trustedModuleCommandsScope -cne $expectedTrustedModuleCommandsScope -or
        [string]([IO.Path]::GetFullPath([string]$knownFolder.psreadlineHistoryExpectedPath).TrimEnd('\', '/')) -cne $reportedHistory -or
        [string]([IO.Path]::GetFullPath([string]$knownFolder.trustedModules).TrimEnd('\', '/')) -cne [IO.Path]::GetFullPath($trustedModules).TrimEnd('\', '/')) {
        throw 'The right-rail split KnownFolder and trusted-module evidence is incomplete or overclaims isolation.'
    }
    $reportedShell = [IO.Path]::GetFullPath([string]$Report.localProfileShell)
    $expectedShell = [IO.Path]::GetFullPath($ExpectedPowerShell)
    if (-not $reportedShell.Equals($expectedShell, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail split report used a different PowerShell profile shell.'
    }
    $scale = [double]$Report.scaleFactor
    if (-not [double]::IsFinite($scale) -or $scale -le 0) {
        throw 'The right-rail split scale factor is invalid.'
    }
    $physicalWidth = [uint32][Math]::Round($ExpectedWidth * $scale, [MidpointRounding]::AwayFromZero)
    $physicalHeight = [uint32][Math]::Round($ExpectedHeight * $scale, [MidpointRounding]::AwayFromZero)

    $fixedRegions = @($Report.fixedRegions)
    $expectedFixedRegions = @(
        [pscustomobject]@{ key = 'navigationRail'; selector = 'workbench-tool-rail' },
        [pscustomobject]@{ key = 'workspaceTabs'; selector = 'workspace-tabs-view' },
        [pscustomobject]@{ key = 'rightRailCard'; selector = 'rr-logs-card' },
        [pscustomobject]@{ key = 'rightRailToolbar'; selector = 'rr-logs-source' },
        [pscustomobject]@{ key = 'logsList'; selector = 'rr-logs-list' }
    )
    if ($fixedRegions.Count -ne $expectedFixedRegions.Count) {
        throw 'The right-rail fixed region specification is incomplete.'
    }
    for ($index = 0; $index -lt $expectedFixedRegions.Count; $index++) {
        if ([string]$fixedRegions[$index].key -cne [string]$expectedFixedRegions[$index].key -or
            [string]$fixedRegions[$index].selector -cne [string]$expectedFixedRegions[$index].selector) {
            throw 'The right-rail fixed region specification changed.'
        }
    }

    $profileA = $Report.profileA
    $profileB = $Report.profileB
    if ($null -eq $profileA -or $null -eq $profileB -or
        [string]$profileA.profileId -cne 'acceptance-default' -or
        [string]$profileA.profileName -cne 'Acceptance Local Profile' -or
        [string]$profileB.profileId -cne 'acceptance-right-rail-split-b' -or
        [string]$profileB.profileName -cne 'Acceptance Right Rail Split B' -or
        [string]$profileB.splitTargetKey -cne 'local:acceptance-right-rail-split-b' -or
        [string]$profileA.markerCommand -cne 'echo KERMINAL_RIGHT_RAIL_SPLIT_A' -or
        [string]$profileB.markerCommand -cne 'echo KERMINAL_RIGHT_RAIL_SPLIT_B') {
        throw 'The right-rail split profile identity or marker command is invalid.'
    }
    foreach ($profile in @($profileA, $profileB)) {
        foreach ($name in @('paneId', 'sessionId', 'historyEntryId', 'cwd')) {
            if ([string]::IsNullOrWhiteSpace([string]$profile.$name)) {
                throw "The right-rail split profile field is empty: $name"
            }
        }
    }
    if ([string]$profileA.paneId -ceq [string]$profileB.paneId -or
        [string]$profileA.sessionId -ceq [string]$profileB.sessionId -or
        [string]$profileA.historyEntryId -ceq [string]$profileB.historyEntryId -or
        [string]$profileA.cwd -ceq [string]$profileB.cwd) {
        throw 'The right-rail split panes, sessions, history entries, or working directories are not distinct.'
    }
    # B 的启动目录是隔离配置根内的专属目录；回读到它才能排除静默回退到默认 Profile。
    $expectedProfileBCwd = [IO.Path]::GetFullPath((Join-Path $RunRoot 'config\right-rail-split-b-cwd')).TrimEnd('\', '/')
    $observedProfileBCwd = [IO.Path]::GetFullPath([string]$profileB.cwd).TrimEnd('\', '/')
    if (-not $observedProfileBCwd.Equals(
        $expectedProfileBCwd, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail split profile B did not start in its configured working directory.'
    }

    $themes = @($Report.themes)
    if ($themes.Count -ne $script:ExpectedThemes.Count) {
        throw 'The right-rail split report does not contain all three themes.'
    }
    $captureNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    for ($themeIndex = 0; $themeIndex -lt $themes.Count; $themeIndex++) {
        $theme = $themes[$themeIndex]
        $themeName = $script:ExpectedThemes[$themeIndex]
        if ($null -eq $theme -or [string]$theme.theme -cne $themeName -or
            -not [bool]$theme.fixedGeometryStable -or
            -not [bool]$theme.targetHashesDistinct) {
            throw "The right-rail split theme evidence is invalid: $themeName"
        }
        $effectiveTheme = [string]$theme.effectiveTheme
        if ($effectiveTheme -cnotin @('light', 'dark')) {
            throw "The right-rail split effective theme is invalid: $themeName"
        }
        $expectedThemeClaim = if ($themeName -ceq 'system') { 'mode-only' } else { 'palette-verified' }
        if ([string]$theme.themeClaim -cne $expectedThemeClaim -or
            [bool]$theme.systemFollowVerified) {
            throw "The right-rail split theme claim is invalid: $themeName"
        }
        $baseline = $theme.aBaseline
        if ($null -eq $baseline -or
            [string]$baseline.capture.file -cne "right-rail-$themeName-a-baseline.png") {
            throw "The right-rail split baseline frame is missing: $themeName"
        }
        $referenceRegions = @($baseline.regions)
        Assert-RightRailFrame -Frame $baseline -RunRoot $RunRoot -Label "$themeName/baseline" `
            -FocusPane ([string]$profileA.paneId) -ExpectedActivePane 'from' `
            -FromPane ([string]$profileA.paneId) -ToPane ([string]$profileB.paneId) `
            -ExpectedARow 'true' -ExpectedBRow 'false' `
            -ExpectedARowPresent 'true' -ExpectedBRowPresent 'false' `
            -ReferenceRegions $referenceRegions -ExpectedWidth $physicalWidth `
            -ExpectedHeight $physicalHeight -CaptureNames $captureNames

        Assert-RightRailTransition -Transition $theme.aToB -RunRoot $RunRoot `
            -Label "$themeName/a-to-b" -ThemeName $themeName -Direction 'a-to-b' `
            -ExpectedRow 'b' -ForbiddenRow 'a' -FromPane ([string]$profileA.paneId) `
            -ToPane ([string]$profileB.paneId) -ReferenceRegions $referenceRegions `
            -SourceFrame $baseline -ExpectedWidth $physicalWidth `
            -ExpectedHeight $physicalHeight -CaptureNames $captureNames
        Assert-RightRailTransition -Transition $theme.bToA -RunRoot $RunRoot `
            -Label "$themeName/b-to-a" -ThemeName $themeName -Direction 'b-to-a' `
            -ExpectedRow 'a' -ForbiddenRow 'b' -FromPane ([string]$profileB.paneId) `
            -ToPane ([string]$profileA.paneId) -ReferenceRegions $referenceRegions `
            -SourceFrame $theme.aToB.settledFrame -ExpectedWidth $physicalWidth `
            -ExpectedHeight $physicalHeight -CaptureNames $captureNames
    }
    # 三个主题各需基线、两个方向的首张采样与稳定帧，以及至少一张过渡采样；少于 21 张
    # 说明某个方向的中间态或数据证据被合并/省略。
    if ($captureNames.Count -lt 21) {
        throw 'The right-rail split report has too few independent frame captures.'
    }
}

function Assert-SettingsParityReport {
    # 命令提示专项同时核对交互、原生截图和真实采样延迟，避免功能通过掩盖性能目标未达成。
    # 同步验收必须有本地 bare 仓库的成功、失败保留和恢复回读，页面截图不替代写入证据。
    # 核对页面、操作回读、截图清单及请求视口，避免默认尺寸掩盖窄窗验收未生效。
    # 密集规则的确认框截图属于焦点流程的固定证据，必须登记并保留严格的完整清单校验。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ExpectedWidth,
        [Parameter(Mandatory = $true)][int]$ExpectedHeight,
        [switch]$McpOnly,
        [switch]$SuggestionsOnly,
        [switch]$SshSuggestions
    )

    if ($Report.schemaVersion -ne 6 -or $Report.captureMode -cne 'hidden_native_window') {
        throw 'Settings parity report schema or capture mode is unsupported.'
    }
    if ($Report.captureBackend -cne 'gpui_window_render_to_image') {
        throw 'Settings parity report was not produced by the GPUI native renderer.'
    }
    $nativeIsolation = $Report.nativeIsolation
    $threadDesktopName = [string]$nativeIsolation.uiThreadDesktopName
    $inputDesktopName = [string]$nativeIsolation.inputDesktopName
    if ($Report.windowVisible -or
        -not $nativeIsolation.platformSupported -or
        -not $nativeIsolation.isolationProven -or
        $threadDesktopName -cne $DesktopName -or
        -not $threadDesktopName.StartsWith('KerminalRail-settings-', [StringComparison]::Ordinal) -or
        [string]::IsNullOrWhiteSpace($inputDesktopName) -or
        $threadDesktopName.Equals($inputDesktopName, [StringComparison]::OrdinalIgnoreCase) -or
        [uint64]$nativeIsolation.processId -ne [uint64]$ProcessId) {
        throw 'The acceptance window was not proven hidden on its owned private desktop.'
    }
    if ([double]$Report.windowWidth -ne $ExpectedWidth -or
        [double]$Report.windowHeight -ne $ExpectedHeight) {
        throw "The native viewport dimensions are invalid; expected ${ExpectedWidth}x${ExpectedHeight}."
    }
    if ($SuggestionsOnly) {
        # 终端专项没有打开设置弹窗；仍严格检查所有操作、主题和本轮截图。
        $expectedChecks = @('history-recorded', 'simulated-ime-chinese-commit-once-and-cancel',
            'ghost-at-cursor', 'native-tab-recovers', 'cursor-move-recovers', 'right-append-same-prompt',
            'menu-inside-pane', 'enter-append-same-prompt', 'escape-suppresses',
            'mouse-dismiss-keeps-input', 'mouse-append-same-prompt', 'mouse-accept-keeps-input')
        $expectedSuggestionCaptures = 6
        if ($SshSuggestions) {
            $expectedSuggestionCaptures = 8
            $expectedChecks += 'ssh-docker-prefix-suggestions'
            $expectedChecks += 'ssh-next-prompt-recovers'
            if ($Report.target -cne 'isolated-ssh' -or $Report.shell -cne 'Bash') {
                throw 'The suggestion report did not use the isolated SSH Bash fixture.'
            }
        } elseif ($Report.target -cne 'local') {
            throw 'The local suggestion report used an unexpected target.'
        }
        if ($Report.scope -cne 'suggestions' -or
            ($Report.themes -join ',') -cne 'light,dark,system' -or
            ($Report.checks -join ',') -cne ($expectedChecks -join ',') -or
            @($Report.screenshots).Count -ne $expectedSuggestionCaptures) {
            throw 'The suggestion-only report is incomplete.'
        }
        if (@($Report.warmCandidateLatency.samplesMs).Count -ne 20 -or
            [double]$Report.warmCandidateLatency.p95Ms -le 0) {
            throw 'The suggestion latency samples are incomplete.'
        }
        if (-not $SshSuggestions -and (
            [double]$Report.warmCandidateLatency.targetMs -ne 200 -or
            -not $Report.warmCandidateLatency.meetsTarget -or
            [double]$Report.warmCandidateLatency.p95Ms -gt 200)) {
            throw 'The suggestion warm candidate latency did not meet the 200 ms P95 target.'
        }
        $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($capture in $Report.screenshots) {
            if (-not $names.Add([string]$capture.file)) {
                throw "Duplicate suggestion capture: $($capture.file)"
            }
            Assert-CaptureRecord -Capture $capture -RunRoot $RunRoot
        }
        foreach ($theme in $script:ExpectedThemes) {
            foreach ($kind in @('ghost', 'menu')) {
                if (-not $names.Contains("suggestions-$kind-$theme.png")) {
                    throw "The suggestion report is missing $kind for theme $theme."
                }
            }
        }
        if ($SshSuggestions) {
            foreach ($file in @('suggestions-docker-ghost.png', 'suggestions-docker-menu.png')) {
                if (-not $names.Contains($file)) {
                    throw "The SSH suggestion report is missing the actual Docker prefix capture: $file."
                }
            }
        }
        return
    }
    if ($Report.settingsModalGeneration -lt 1) {
        throw 'The settings modal lifecycle is invalid.'
    }
    if ($McpOnly) {
        # 专项仍核对相同的隐藏桌面与尺寸证据，不能用局部截图冒充完整设置验收。
        if ($Report.scope -cne 'mcp' -or $Report.themes.Count -ne 3 -or
            ($Report.themes -join ',') -cne 'light,dark,system' -or
            $Report.mcpChecks.Count -ne $script:ExpectedMcpActions.Count -or
            $Report.draftChecks.Count -ne 5) {
            throw 'The MCP-only report is incomplete.'
        }
        $expectedActions = @($script:ExpectedMcpActions) + @(
            'mcp-occupied-port-keeps-retry',
            'mcp-retry-reads-latest-and-copies-live-json',
            'mcp-invalid-draft-allows-stop',
            'mcp-invalid-port-prevents-start',
            'mcp-corrected-draft-starts'
        )
        $checks = @($Report.mcpChecks) + @($Report.draftChecks)
        for ($index = 0; $index -lt $expectedActions.Count; $index++) {
            if (-not $checks[$index].passed -or $checks[$index].action -cne $expectedActions[$index]) {
                throw "MCP native check failed or is missing: $($expectedActions[$index])"
            }
        }
        foreach ($capture in $Report.screenshots) {
            Assert-CaptureRecord -Capture $capture -RunRoot $RunRoot
        }
        foreach ($theme in $script:ExpectedThemes) {
            if (@($Report.screenshots.file) -cnotcontains "settings-mcp-compact-$theme.png") {
                throw "The MCP report is missing theme $theme."
            }
        }
        return
    }
    if ($Report.themes.Count -ne $script:ExpectedThemes.Count) {
        throw 'The report does not contain all three theme captures.'
    }

    $expectedCaptureNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $languageCheck = $Report.languageEscapeCheck
    if ($null -eq $languageCheck -or
        -not $languageCheck.passed -or
        $languageCheck.action -cne 'escape-select-then-modal-and-restore-focus' -or
        @('system', 'zh-CN', 'en-US') -cnotcontains [string]$languageCheck.expectedLanguage -or
        $languageCheck.expectedLanguage -cne $languageCheck.observedLanguage -or
        -not $languageCheck.firstEscapeClosedSelect -or
        -not $languageCheck.firstEscapePreservedModal -or
        -not $languageCheck.firstEscapePreservedLanguage -or
        -not $languageCheck.secondEscapeClosedModal -or
        -not $languageCheck.focusReturnedToTrigger -or
        -not $languageCheck.modalReopened) {
        throw 'The language Select Escape, modal Escape, or focus restoration workflow failed.'
    }
    if ([string]$languageCheck.dropdownOpenCapture.file -cne 'settings-functional-language-select-open.png' -or
        [string]$languageCheck.dropdownClosedCapture.file -cne 'settings-functional-language-select-dismissed.png') {
        throw 'The language Select screenshots are not associated with their expected popup states.'
    }
    foreach ($capture in @($languageCheck.dropdownOpenCapture, $languageCheck.dropdownClosedCapture)) {
        if ($null -eq $capture) {
            throw 'The language Select Escape workflow is missing a native screenshot.'
        }
        $null = $expectedCaptureNames.Add([string]$capture.file)
        Assert-CaptureRecord -Capture $capture -RunRoot $RunRoot
    }

    foreach ($theme in $Report.themes) {
        $themeName = [string]$theme.requestedMode
        if ($script:ExpectedThemes -cnotcontains $themeName -or $theme.persistedMode -cne $themeName) {
            throw "Theme selection was not confirmed by persisted readback: $themeName"
        }
        if ($theme.pages.Count -ne $script:ExpectedPages.Count) {
            throw "Theme $themeName does not contain every settings page."
        }
        foreach ($page in $theme.pages) {
            $pageId = [string]$page.pageId
            if ($script:ExpectedPages -cnotcontains $pageId -or $page.observedPageId -cne $pageId) {
                throw "Theme $themeName contains an unexpected or unselected page: $pageId"
            }
            if (-not $page.settingsReady -or $page.navigationSelector -cne "preferences-nav-$pageId") {
                throw "Settings page is not ready or its selector changed: $pageId"
            }
            $null = $expectedCaptureNames.Add([string]$page.capture.file)
            Assert-CaptureRecord -Capture $page.capture -RunRoot $RunRoot
            if ($null -eq $page.bottomCapture) {
                throw "Theme $themeName page $pageId has no bottom viewport capture."
            }
            $null = $expectedCaptureNames.Add([string]$page.bottomCapture.file)
            Assert-CaptureRecord -Capture $page.bottomCapture -RunRoot $RunRoot
        }
    }

    if ($Report.densityChecks.Count -ne 2) {
        throw 'The report does not contain both reversible density changes.'
    }
    foreach ($check in $Report.densityChecks) {
        if (-not $check.passed -or $check.expected -cne $check.observed) {
            throw "Density setting did not survive persisted readback: $($check.expected)"
        }
        if ($null -eq $check.capture) {
            throw "Density setting has no native screenshot: $($check.expected)"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    if ($Report.disclosureChecks.Count -ne $script:ExpectedDisclosures.Count) {
        throw 'The report does not contain every expanded settings disclosure.'
    }
    $reportedDisclosures = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($check in $Report.disclosureChecks) {
        if (-not $check.passed -or
            $check.action -cne 'capture-expanded-disclosure' -or
            $check.expected -cne $check.observed -or
            $check.expected -notmatch '^(.+):expanded$') {
            throw 'An expanded settings disclosure did not match its expected state.'
        }
        $disclosureId = [string]$Matches[1]
        if ($script:ExpectedDisclosures -cnotcontains $disclosureId -or
            -not $reportedDisclosures.Add($disclosureId)) {
            throw "The report contains an unexpected or duplicate disclosure: $disclosureId"
        }
        if ($null -eq $check.capture -or
            [string]$check.capture.file -cne "settings-disclosure-system-$disclosureId.png") {
            throw "Disclosure capture does not match its state: $disclosureId"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    if (-not $Report.keywordRuleCheck.passed -or
        $Report.keywordRuleCheck.action -cne 'save-keyword-and-cancel-unsaved-edit' -or
        $Report.keywordRuleCheck.expected -cne $Report.keywordRuleCheck.observed -or
        $null -eq $Report.keywordRuleCheck.capture) {
        throw 'The keyword save/cancel workflow did not produce a matching persisted readback and capture.'
    }
    $null = $expectedCaptureNames.Add([string]$Report.keywordRuleCheck.capture.file)
    Assert-CaptureRecord -Capture $Report.keywordRuleCheck.capture -RunRoot $RunRoot

    $keywordLifecycleProperty = $Report.PSObject.Properties['keywordLifecycleChecks']
    if ($null -eq $keywordLifecycleProperty -or
        $null -eq $keywordLifecycleProperty.Value -or
        $keywordLifecycleProperty.Value.Count -ne $script:ExpectedKeywordLifecycleActions.Count) {
        throw 'The report does not contain the complete keyword lifecycle readbacks.'
    }
    for ($index = 0; $index -lt $script:ExpectedKeywordLifecycleActions.Count; $index++) {
        $check = $keywordLifecycleProperty.Value[$index]
        $expectedAction = $script:ExpectedKeywordLifecycleActions[$index]
        if (-not $check.passed -or
            $check.action -cne $expectedAction -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or
            $check.expected -cne $check.observed) {
            throw "Keyword lifecycle readback failed or was out of order: $expectedAction"
        }

        $elapsedProperty = $check.PSObject.Properties['elapsedMs']
        if ($expectedAction -eq 'undo-keyword-deletion-within-15-seconds') {
            if ($null -eq $elapsedProperty -or
                $null -eq $elapsedProperty.Value -or
                [uint64]$elapsedProperty.Value -ge 15000) {
                throw 'Keyword undo did not complete and read back within 15 seconds.'
            }
        } elseif ($null -ne $elapsedProperty -and $null -ne $elapsedProperty.Value) {
            throw "Unexpected elapsed-time value on keyword lifecycle action: $expectedAction"
        }
    }

    # 真实终端配置、PTy 色盘和输入控件均需有本次运行的截图与持久化回读。
    $terminalChecks = $Report.PSObject.Properties['terminalChecks']
    if ($null -eq $terminalChecks -or @($terminalChecks.Value).Count -ne $script:ExpectedTerminalChecks.Count) {
        throw 'The report does not contain the complete terminal settings checks.'
    }
    for ($index = 0; $index -lt $script:ExpectedTerminalChecks.Count; $index++) {
        $check = $terminalChecks.Value[$index]
        $expected = $script:ExpectedTerminalChecks[$index]
        if (-not $check.passed -or $check.action -cne $expected.Action -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or
            $check.expected -cne $check.observed -or $null -eq $check.capture -or
            [string]$check.capture.file -cne $expected.File) {
            throw "Terminal settings check failed: $($expected.Action)"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    # 快捷键回执同时验证录制取消和应用后新旧键的不同结果。
    $shortcutChecks = $Report.PSObject.Properties['shortcutChecks']
    if ($null -eq $shortcutChecks -or @($shortcutChecks.Value).Count -ne $script:ExpectedShortcutChecks.Count) {
        throw 'The report does not contain the complete shortcut capture checks.'
    }
    for ($index = 0; $index -lt $script:ExpectedShortcutChecks.Count; $index++) {
        $check = $shortcutChecks.Value[$index]
        $expected = $script:ExpectedShortcutChecks[$index]
        if (-not $check.passed -or $check.action -cne $expected.Action -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or
            $check.expected -cne $check.observed -or $null -eq $check.capture -or
            [string]$check.capture.file -cne $expected.File) {
            throw "Shortcut capture check failed: $($expected.Action)"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    # 每项都必须同时有操作结果和属于当前运行的原生截图，缺任何一项都不能计为通过。
    $keywordAdjustments = $Report.PSObject.Properties['keywordAdjustmentChecks']
    if ($null -eq $keywordAdjustments -or @($keywordAdjustments.Value).Count -ne $script:ExpectedKeywordAdjustmentChecks.Count) {
        throw 'The report does not contain the complete keyword layout and draft checks.'
    }
    for ($index = 0; $index -lt $script:ExpectedKeywordAdjustmentChecks.Count; $index++) {
        $check = $keywordAdjustments.Value[$index]
        $expected = $script:ExpectedKeywordAdjustmentChecks[$index]
        if (-not $check.passed -or $check.action -cne $expected.Action -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or
            $check.expected -cne $check.observed -or $null -eq $check.capture -or
            [string]$check.capture.file -cne $expected.File) {
            throw "Keyword layout or draft check failed: $($expected.Action)"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    $syncChecks = $Report.PSObject.Properties['syncChecks']
    if ($null -eq $syncChecks -or @($syncChecks.Value).Count -ne $script:ExpectedSyncActions.Count) {
        throw 'The report does not contain the complete local Git sync lifecycle.'
    }
    for ($index = 0; $index -lt $script:ExpectedSyncActions.Count; $index++) {
        $check = $syncChecks.Value[$index]
        $action = $script:ExpectedSyncActions[$index]
        if (-not $check.passed -or $check.action -cne $action -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or
            $check.expected -cne $check.observed -or $null -eq $check.capture -or
            [string]$check.capture.file -cne "settings-functional-$action.png") {
            throw "Local Git sync lifecycle failed: $action"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    # 真实 listener 与原生按钮必须共同到达终态，不能只凭生成的截图宣称恢复成功。
    $mcpChecks = $Report.PSObject.Properties['mcpChecks']
    if ($null -eq $mcpChecks -or @($mcpChecks.Value).Count -ne $script:ExpectedMcpActions.Count) {
        throw 'The report does not contain the complete MCP lifecycle.'
    }
    for ($index = 0; $index -lt $script:ExpectedMcpActions.Count; $index++) {
        $check = $mcpChecks.Value[$index]
        $action = $script:ExpectedMcpActions[$index]
        if (-not $check.passed -or $check.action -cne $action -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or
            $check.expected -cne $check.observed -or $null -eq $check.capture) {
            throw "MCP lifecycle failed: $action"
        }
        $null = $expectedCaptureNames.Add([string]$check.capture.file)
        Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
    }

    $profileChecks = $Report.PSObject.Properties['profileChecks']
    if ($null -eq $profileChecks -or @($profileChecks.Value).Count -ne $script:ExpectedProfileActions.Count) {
        throw 'The report does not contain the complete Profile lifecycle.'
    }
    for ($index = 0; $index -lt $script:ExpectedProfileActions.Count; $index++) {
        $check = $profileChecks.Value[$index]
        if (-not $check.passed -or $check.action -cne $script:ExpectedProfileActions[$index] -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or $check.expected -cne $check.observed) {
            throw "Profile lifecycle failed: $($script:ExpectedProfileActions[$index])"
        }
    }

    # 模板和本地主机的独立身份、快照与删除都必须有本轮回执，关键界面另保留截图。
    $profileHostChecks = $Report.PSObject.Properties['profileHostChecks']
    if ($null -eq $profileHostChecks -or @($profileHostChecks.Value).Count -ne $script:ExpectedProfileHostChecks.Count) {
        throw 'The report does not contain the complete Profile-to-local-host lifecycle.'
    }
    for ($index = 0; $index -lt $script:ExpectedProfileHostChecks.Count; $index++) {
        $check = $profileHostChecks.Value[$index]
        $expected = $script:ExpectedProfileHostChecks[$index]
        if (-not $check.passed -or $check.action -cne $expected.Action -or
            [string]::IsNullOrWhiteSpace([string]$check.expected) -or $check.expected -cne $check.observed) {
            throw "Profile-to-local-host lifecycle failed: $($expected.Action)"
        }
        if ($null -ne $expected.File) {
            if ($null -eq $check.capture -or [string]$check.capture.file -cne $expected.File) {
                throw "Profile-to-local-host capture missing: $($expected.Action)"
            }
            $null = $expectedCaptureNames.Add([string]$check.capture.file)
            Assert-CaptureRecord -Capture $check.capture -RunRoot $RunRoot
        } elseif ($null -ne $check.capture) {
            throw "Unexpected Profile-to-local-host capture: $($expected.Action)"
        }
    }

    foreach ($fileName in @(
        'settings-opened.png',
        'settings-functional-keyword-close-confirmation.png',
        'settings-functional-keyword-colors.png',
        'settings-functional-keyword-color-picker.png',
        'settings-functional-keyword-summary-light.png',
        'settings-functional-keyword-summary-dark.png',
        'settings-functional-keyword-summary-system.png',
        'settings-functional-profile.png',
        'settings-functional-profile-rescan.png'
    )) {
        $null = $expectedCaptureNames.Add($fileName)
    }

    $reportedCaptureNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($capture in $Report.screenshots) {
        $fileName = [string]$capture.file
        if (-not $reportedCaptureNames.Add($fileName)) {
            throw "The report screenshot index contains a duplicate capture: $fileName"
        }
        Assert-CaptureRecord -Capture $capture -RunRoot $RunRoot
    }
    if ($reportedCaptureNames.Count -ne $expectedCaptureNames.Count) {
        throw 'The report screenshot index contains unexpected or missing captures.'
    }
    foreach ($fileName in $expectedCaptureNames) {
        if (-not $reportedCaptureNames.Contains($fileName)) {
            throw "The report screenshot index is missing $fileName."
        }
    }
    foreach ($disclosureId in $script:ExpectedDisclosures) {
        if (-not $reportedDisclosures.Contains($disclosureId)) {
            throw "The report is missing expanded disclosure $disclosureId."
        }
    }
}

# 将请求逻辑尺寸传给隔离的 native child，同时保留原生退出状态以便准确识别崩溃。
# @author kongweiguang
function Invoke-HiddenAcceptanceProcess {
    # Creates one private WinSta0 desktop and carries the requested viewport into the owned child without switching input desktops.
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$OutputDirectory,
        [Parameter(Mandatory = $true)][string]$StdoutPath,
        [Parameter(Mandatory = $true)][string]$StderrPath,
        [Parameter(Mandatory = $true)][int]$TimeoutMilliseconds,
        [Parameter(Mandatory = $true)][int]$Width,
        [Parameter(Mandatory = $true)][int]$Height,
        [switch]$VerifyOwnedProcessIdentity,
        [switch]$UseJobObject
    )

    if (-not [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([Runtime.InteropServices.OSPlatform]::Windows)) {
        throw 'Settings parity native desktop isolation is available only on Windows.'
    }
    if (-not ('Kerminal.SettingsParity.NativeDesktopLauncher' -as [type])) {
        Add-Type -TypeDefinition $script:NativeDesktopLauncherSource -Language CSharp
    }

    $desktopName = 'KerminalRail-settings-{0}' -f [Guid]::NewGuid().ToString('N')
    $nativeProcess = $null
    $timedOut = $false
    $result = $null
    try {
        $nativeProcess = [Kerminal.SettingsParity.NativeDesktopLauncher]::Start(
            [IO.Path]::GetFullPath($Executable),
            [IO.Path]::GetFullPath($WorkingDirectory),
            [IO.Path]::GetFullPath($OutputDirectory),
            [IO.Path]::GetFullPath($StdoutPath),
            [IO.Path]::GetFullPath($StderrPath),
            $desktopName,
            [uint32]$Width,
            [uint32]$Height,
            [bool]$UseJobObject
        )
        $timedOut = -not $nativeProcess.WaitForExit([uint32]$TimeoutMilliseconds)
        if ($timedOut) {
            Stop-OwnedAcceptanceProcessTree -NativeProcess $nativeProcess `
                -VerifyOwnedProcessIdentity:$VerifyOwnedProcessIdentity
        }
        $exitCode = $nativeProcess.GetExitCode()
        $exitCodeHex = '0x{0:X8}' -f $exitCode
        $result = [pscustomobject]@{
            ExitCode = $exitCode
            ExitCodeHex = $exitCodeHex
            TimedOut = $timedOut
            ProcessId = $nativeProcess.ProcessId
            DesktopName = $nativeProcess.DesktopName
        }
    } finally {
        if ($null -ne $nativeProcess) {
            if (-not $nativeProcess.WaitForExit(0)) {
                Stop-OwnedAcceptanceProcessTree -NativeProcess $nativeProcess `
                -VerifyOwnedProcessIdentity:$VerifyOwnedProcessIdentity
            }
            try {
                $nativeProcess.Dispose()
            } catch {
                $nativeProcess.Dispose()
            }
        }
    }
    $result | Add-Member -NotePropertyName DesktopClosed -NotePropertyValue $nativeProcess.DesktopClosed
    $result | Add-Member -NotePropertyName JobAssigned -NotePropertyValue ([bool]$nativeProcess.JobAssigned)
    $result | Add-Member -NotePropertyName JobCleanupVerified -NotePropertyValue ([bool]$nativeProcess.JobCleanupVerified)
    $result | Add-Member -NotePropertyName JobCleanupForced -NotePropertyValue ([bool]$nativeProcess.JobCleanupForced)
    $result | Add-Member -NotePropertyName JobActiveProcessesAtExit -NotePropertyValue ([uint32]$nativeProcess.JobActiveProcessesAtExit)
    return $result
}

function Stop-OwnedAcceptanceProcessTree {
    # 超时回收优先使用创建时绑定的 Job Object，并验证 active process 计数归零。
    # 只有在没有 Job 的旧范围才保留按 PID 的清理；右栏若 Job 缺失则退回创建时间核对，
    # 绝不因父进程恰好退出就把“未证明子孙回收”当成成功。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)]$NativeProcess,
        [switch]$VerifyOwnedProcessIdentity
    )

    $process = $null
    $treeCleanupError = $null
    if ([bool]$NativeProcess.JobAssigned) {
        # Job 在进程恢复前绑定；超时回收不再按 PID 枚举，避免父进程已退出后误杀复用的 PID。
        # TerminateJobObject 必须让 active process 计数归零才返回，否则这里 fail-closed。
        try {
            $NativeProcess.TerminateJobAndWait(1, 10000)
        } catch {
            throw "Could not prove owned job-object process cleanup: $($_.Exception.Message)"
        }
        if (-not $NativeProcess.WaitForExit(10000)) {
            throw 'The settings acceptance process remained alive after owned job cleanup.'
        }
        return
    }
    try {
        $process = [Diagnostics.Process]::GetProcessById($NativeProcess.ProcessId)
    } catch [ArgumentException] {
        $process = $null
    }
    if ($null -ne $process) {
        try {
            if (-not $process.HasExited) {
                if ($VerifyOwnedProcessIdentity) {
                    # Process 对象在 GetProcessById 时已持有该 PID 对应的进程句柄；创建时间
                    # 匹配后 Kill 作用于这个已绑定的对象，不会重新解析到后续复用的 PID。
                    $expectedCreationTicks = [long]$NativeProcess.CreationTimeUtcTicks
                    $actualCreationTicks = $process.StartTime.ToUniversalTime().Ticks
                    if ($expectedCreationTicks -le 0 -or $actualCreationTicks -ne $expectedCreationTicks) {
                        $treeCleanupError = "PID $($NativeProcess.ProcessId) no longer matches the owned process creation time; refusing process-tree termination."
                    }
                }
                if ($null -eq $treeCleanupError) {
                    try {
                        $process.Kill($true)
                    } catch {
                        if (-not $process.HasExited) {
                            $treeCleanupError = $_.Exception.Message
                        }
                    }
                }
            }
        } catch {
            $treeCleanupError = $_.Exception.Message
        } finally {
            $process.Dispose()
        }
    }
    if (-not $NativeProcess.WaitForExit(10000)) {
        try {
            $NativeProcess.Terminate(1)
        } catch {
            $reason = if ($treeCleanupError) { "; process-tree cleanup also failed: $treeCleanupError" } else { '' }
            throw "Could not terminate the owned acceptance process: $($_.Exception.Message)$reason"
        }
        if (-not $NativeProcess.WaitForExit(10000)) {
            throw 'The settings acceptance process remained alive after owned-process cleanup.'
        }
    }
    if ($treeCleanupError) {
        throw "Could not prove owned process-tree cleanup: $treeCleanupError"
    }
}

function Remove-RightRailIsolatedEnvironment {
    # 右栏子进程会在 run root 内建立 home/temp/program-data/public。删除前必须确认
    # 目标仍在本次 run root 下，且路径及其所有子项都不是 reparse point，避免递归删除越界。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunRoot
    )

    $removed = $true
    foreach ($name in @('home', 'temp', 'program-data', 'public')) {
        $path = Join-Path $RunRoot $name
        if (-not (Test-Path -LiteralPath $path)) {
            continue
        }
        Assert-PathWithinRoot -Path $path -Root $RunRoot -Label "Right-rail environment $name"
        Assert-NoReparseComponent -Path $path -Root $RunRoot -Label "Right-rail environment $name"
        $items = @((Get-Item -LiteralPath $path -Force)) +
            @(Get-ChildItem -LiteralPath $path -Recurse -Force)
        foreach ($item in $items) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing right-rail environment cleanup through a reparse point: $($item.FullName)"
            }
        }
        [IO.Directory]::Delete($path, $true)
        if (Test-Path -LiteralPath $path) {
            $removed = $false
        }
    }
    return $removed
}

function Remove-IsolatedConfigDirectory {
    # Removes only the config child created for this fresh run after the application has fully exited.
    param(
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][string]$ConfigDirectory
    )

    if (-not (Test-Path -LiteralPath $ConfigDirectory)) {
        return
    }
    Assert-PathWithinRoot -Path $ConfigDirectory -Root $RunRoot -Label 'Config cleanup'
    $expected = [IO.Path]::GetFullPath((Join-Path $RunRoot 'config'))
    if (-not [IO.Path]::GetFullPath($ConfigDirectory).Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to remove a path other than the run-local config directory.'
    }

    $items = @((Get-Item -LiteralPath $ConfigDirectory -Force)) + @(Get-ChildItem -LiteralPath $ConfigDirectory -Recurse -Force)
    foreach ($item in $items) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing config cleanup through a reparse point: $($item.FullName)"
        }
    }
    Remove-Item -LiteralPath $ConfigDirectory -Recurse -Force
}

function Move-AcceptanceLogs {
    # Moves only the two runner-owned stream files into the immutable run evidence directory.
    param(
        [Parameter(Mandatory = $true)][string]$RunnerLogDirectory,
        [Parameter(Mandatory = $true)][string]$RunRoot
    )

    Assert-PathWithinRoot -Path $RunnerLogDirectory -Root $script:DiagnosticsRoot -Label 'Runner logs'
    Assert-PathWithinRoot -Path $RunRoot -Root $script:DiagnosticsRoot -Label 'Run evidence'
    if (-not (Test-Path -LiteralPath $RunRoot -PathType Container)) {
        [void][IO.Directory]::CreateDirectory($RunRoot)
    }

    foreach ($fileName in @('stdout.log', 'stderr.log')) {
        $sourcePath = Join-Path $RunnerLogDirectory $fileName
        if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
            continue
        }
        $destinationPath = Join-Path $RunRoot $fileName
        if (Test-Path -LiteralPath $destinationPath) {
            throw "Refusing to replace existing acceptance log: $destinationPath"
        }
        [IO.File]::Move($sourcePath, $destinationPath)
    }

    $remaining = @(Get-ChildItem -LiteralPath $RunnerLogDirectory -Force)
    if ($remaining.Count -eq 0) {
        [IO.Directory]::Delete($RunnerLogDirectory, $false)
    } else {
        throw "Unexpected files remain in the isolated runner log directory: $RunnerLogDirectory"
    }
}

function Assert-RightRailPowerShellVersionInIsolation {
    # 探针必须在已经由外层隔离启动器重建的环境中运行；不复制父用户环境，也不在用户
    # APPDATA 或用户模块目录中启动。探针通过共享 launcher 的 Job Object 有界回收，
    # 只有 active process 计数归零才继续；模块、历史和命令解析都从探针实际输出回读。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string]$ProbeRoot
    )

    if (-not ('Kerminal.SettingsParity.NativeDesktopLauncher' -as [type])) {
        throw 'The shared native launcher must be compiled before the isolated PowerShell probe starts.'
    }
    if (Test-Path -LiteralPath $ProbeRoot) {
        throw "Refusing to reuse an existing right-rail PowerShell probe root: $ProbeRoot"
    }
    $trustedModules = [IO.Path]::GetFullPath((Join-Path $PSHOME 'Modules'))
    $outerRoot = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_OUTER_ROOT')
    if ([string]::IsNullOrWhiteSpace($outerRoot)) {
        throw 'The right-rail PowerShell probe requires the isolated outer root marker.'
    }
    $outerRoot = [IO.Path]::GetFullPath($outerRoot)
    # 探针的 PSModulePath 白名单需要受控 HOME 派生的用户模块段；它与外层启动器写入
    # 的是同一路径，这里从隔离根重新推导而不是新造路径，防止两处各自演化。
    # 变量名不能占用 $probeHome：函数后半段用同名变量承载探针回读的 PSHOME 并做校验。
    $probeIsolatedHome = [IO.Path]::Combine($outerRoot, 'home')
    $appData = [Environment]::GetEnvironmentVariable('APPDATA')
    if ([string]::IsNullOrWhiteSpace($appData)) {
        throw 'The right-rail PowerShell probe requires an isolated APPDATA.'
    }
    $appData = [IO.Path]::GetFullPath($appData)
    $historyRoot = [IO.Path]::GetFullPath((Join-Path $appData 'Microsoft\Windows\PowerShell\PSReadLine'))
    $outerPrefix = $outerRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $historyRoot.StartsWith($outerPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The right-rail PowerShell probe APPDATA escaped the isolated outer root.'
    }
    # 探针目录由内层创建，创建前先确认目标仍在诊断根下且父链没有 reparse point；
    # 探针本身也会启动子进程，清理路径不可信时不能继续。
    Assert-PathWithinRoot -Path $ProbeRoot -Root $script:DiagnosticsRoot -Label 'Right-rail PowerShell probe root'
    Assert-NoReparseAncestors -Path $ProbeRoot -Label 'Right-rail PowerShell probe root'
    [void][IO.Directory]::CreateDirectory($ProbeRoot)
    [void][IO.Directory]::CreateDirectory($historyRoot)
    $stdoutPath = Join-Path $ProbeRoot 'stdout.log'
    $stderrPath = Join-Path $ProbeRoot 'stderr.log'

    # 这里只输出可校验的键值，不执行 profile；PSReadLine 的 HistorySavePath 必须来自
    # 实际加载的模块，命令解析也必须指向本次 pwsh 自带的 Modules。
    $probeScript = @'
$ErrorActionPreference = 'Stop'
$psReadLine = Get-Module -ListAvailable -Name PSReadLine | Select-Object -First 1
$utility = Get-Module -ListAvailable -Name Microsoft.PowerShell.Utility | Select-Object -First 1
$management = Get-Module -ListAvailable -Name Microsoft.PowerShell.Management | Select-Object -First 1
$readLineCommand = Get-Command -Name Get-PSReadLineKeyHandler -ErrorAction Stop
$setLineCommand = Get-Command -Name Set-PSReadLineKeyHandler -ErrorAction Stop
$optionCommand = Get-Command -Name Get-PSReadLineOption -ErrorAction Stop
'KERMINAL_PROBE_major=' + [string]$PSVersionTable.PSVersion.Major
'KERMINAL_PROBE_edition=' + [string]$PSVersionTable.PSEdition
'KERMINAL_PROBE_psHome=' + [string]$PSHOME
'KERMINAL_PROBE_psModulePath=' + [string]$env:PSModulePath
'KERMINAL_PROBE_history=' + [string](Get-PSReadLineOption).HistorySavePath
'KERMINAL_PROBE_psreadlineModule=' + [string]$psReadLine.ModuleBase
'KERMINAL_PROBE_utilityModule=' + [string]$utility.ModuleBase
'KERMINAL_PROBE_managementModule=' + [string]$management.ModuleBase
'KERMINAL_PROBE_readLineCommandModule=' + [string]$readLineCommand.Module.Path
'KERMINAL_PROBE_setLineCommandModule=' + [string]$setLineCommand.Module.Path
'KERMINAL_PROBE_optionCommandModule=' + [string]$optionCommand.Module.Path
'@

    $nativeProcess = $null
    $timedOut = $false
    $probeRootRemoved = $false
    $jobCleanupVerified = $false
    $evidence = $null
    try {
        $desktopName = 'KerminalRail-settings-{0}' -f [Guid]::NewGuid().ToString('N')
        $nativeProcess = [Kerminal.SettingsParity.NativeDesktopLauncher]::StartProbe(
            [IO.Path]::GetFullPath($Executable),
            [string[]]@('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $probeScript),
            [IO.Path]::GetFullPath($ProbeRoot),
            [IO.Path]::GetFullPath($ProbeRoot),
            [IO.Path]::GetFullPath($stdoutPath),
            [IO.Path]::GetFullPath($stderrPath),
            $desktopName,
            $true
        )
        $timedOut = -not $nativeProcess.WaitForExit(15000)
        if ($timedOut) {
            Stop-OwnedAcceptanceProcessTree -NativeProcess $nativeProcess -VerifyOwnedProcessIdentity
        }
        $exitCode = $nativeProcess.GetExitCode()
        $jobCleanupVerified = $nativeProcess.WaitForJobEmpty(5000)
        if (-not $jobCleanupVerified) {
            # 探针本身不应创建子进程；如果 Job 仍有活动进程，终止整棵树并失败关闭，
            # 不把“父进程已退出”当成进程树已经回收。
            $nativeProcess.TerminateJobAndWait(1, 10000)
            throw 'The isolated PowerShell probe left descendant processes in its owned job.'
        }
        if ($timedOut) {
            throw 'The isolated PowerShell version probe timed out and its owned process tree was terminated.'
        }
        $stdout = if (Test-Path -LiteralPath $stdoutPath -PathType Leaf) {
            [IO.File]::ReadAllText($stdoutPath)
        } else {
            throw 'The isolated PowerShell probe produced no stdout.'
        }
        $stderr = if (Test-Path -LiteralPath $stderrPath -PathType Leaf) {
            [IO.File]::ReadAllText($stderrPath)
        } else {
            ''
        }
        if ($exitCode -ne 0) {
            $detail = if ([string]::IsNullOrWhiteSpace($stderr)) { $stdout } else { $stderr }
            throw "The isolated PowerShell probe failed with exit code ${exitCode}: $($detail.Trim())"
        }

        $values = @{}
        foreach ($line in ($stdout -split "`r?`n")) {
            $trimmed = $line.Trim()
            if (-not $trimmed.StartsWith('KERMINAL_PROBE_', [StringComparison]::Ordinal)) {
                continue
            }
            $rest = $trimmed.Substring('KERMINAL_PROBE_'.Length)
            $separator = $rest.IndexOf('=')
            if ($separator -lt 1) {
                throw "The isolated PowerShell probe emitted a malformed line: $trimmed"
            }
            $key = $rest.Substring(0, $separator)
            $value = $rest.Substring($separator + 1)
            if ($values.ContainsKey($key)) {
                throw "The isolated PowerShell probe emitted a duplicate key: $key"
            }
            $values[$key] = $value
        }
        foreach ($name in @('major', 'edition', 'psHome', 'psModulePath', 'history', 'psreadlineModule', 'utilityModule', 'managementModule', 'readLineCommandModule', 'setLineCommandModule', 'optionCommandModule')) {
            if (-not $values.ContainsKey($name) -or [string]::IsNullOrWhiteSpace([string]$values[$name])) {
                throw "The isolated PowerShell probe is missing $name."
            }
        }
        $major = 0
        if (-not [int]::TryParse([string]$values['major'], [ref]$major) -or $major -ne 7 -or
            [string]$values['edition'] -cne 'Core') {
            throw "The isolated PowerShell probe requires 7 (Core); major=$($values['major']) edition=$($values['edition'])"
        }
        $probeHome = [IO.Path]::GetFullPath([string]$values['psHome']).TrimEnd('\', '/')
        if (-not $probeHome.Equals([IO.Path]::GetFullPath($PSHOME).TrimEnd('\', '/'), [StringComparison]::OrdinalIgnoreCase)) {
            throw 'The isolated PowerShell probe ran a different PSHOME than the launching pwsh.'
        }
        $moduleEntries = @([string]$values['psModulePath'] -split ';' |
            ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
        # 探针与外层共享同一进程环境，pwsh 对预设完整序列逐字保留；断言实际探测值与
        # 外层白名单完全一致，而不是重新假设单条目。真实用户模块路径不在白名单内，
        # 出现即失败关闭；受控用户段与系统段的目录存在性在下方一并核对。
        $controlledUserModules = [IO.Path]::GetFullPath([IO.Path]::Combine($probeIsolatedHome, 'Documents', 'PowerShell', 'Modules'))
        $systemModulesRoot = [IO.Path]::GetFullPath(
            [IO.Path]::Combine([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles), 'PowerShell', 'Modules'))
        $expectedProbeSequence = @($controlledUserModules, $systemModulesRoot, $trustedModules)
        # 只断言路径相等不要求目录存在：系统段在 MSIX 安装机器上可能不存在，pwsh 仍会
        # 把它写入默认序列；受信任段的真实存在性由 PSHOME\Modules 白名单检查承担。
        if ($moduleEntries.Count -ne $expectedProbeSequence.Count) {
            throw "The isolated PowerShell probe PSModulePath must match the controlled user/system/PSHOME sequence; found $($moduleEntries.Count) entries."
        }
        for ($entryIndex = 0; $entryIndex -lt $expectedProbeSequence.Count; $entryIndex++) {
            $expectedEntry = [IO.Path]::GetFullPath($expectedProbeSequence[$entryIndex]).TrimEnd('\', '/')
            $actualEntry = [IO.Path]::GetFullPath($moduleEntries[$entryIndex]).TrimEnd('\', '/')
            if (-not $actualEntry.Equals($expectedEntry, [StringComparison]::OrdinalIgnoreCase)) {
                throw "The isolated PowerShell probe PSModulePath entry $entryIndex is not the expected controlled path: $($moduleEntries[$entryIndex])"
            }
        }
        $history = [IO.Path]::GetFullPath([string]$values['history']).TrimEnd('\', '/')
        $historyPrefix = $historyRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
        if (-not $history.StartsWith($historyPrefix, [StringComparison]::OrdinalIgnoreCase) -or
            [IO.Path]::GetFileName($history) -cne 'ConsoleHost_history.txt') {
            throw "The isolated PowerShell probe resolved PSReadLine history outside its isolated APPDATA: $history"
        }
        $modulePrefix = $trustedModules.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
        foreach ($name in @('psreadlineModule', 'utilityModule', 'managementModule', 'readLineCommandModule', 'setLineCommandModule', 'optionCommandModule')) {
            $modulePath = [IO.Path]::GetFullPath([string]$values[$name]).TrimEnd('\', '/')
            if (-not $modulePath.StartsWith($modulePrefix, [StringComparison]::OrdinalIgnoreCase)) {
                throw "The isolated PowerShell probe resolved $name outside the trusted PSHOME\Modules: $modulePath"
            }
        }
        $evidence = [pscustomobject]@{
            PsVersionMajor = $major
            PsEdition = [string]$values['edition']
            PsHome = $probeHome
            # PsModulePath 语义是“受信任的单一 PSHOME\Modules”；探针实际序列是三段，
            # 其他两段只用于启动隔离白名单，向下传递的必须是末段可信目录。
            PsModulePath = $trustedModules
            PsReadLineHistoryPath = $history
            PsReadLineModulePath = [string]$values['psreadlineModule']
            UtilityModulePath = [string]$values['utilityModule']
            ManagementModulePath = [string]$values['managementModule']
            ReadLineCommandModulePath = [string]$values['readLineCommandModule']
            SetLineCommandModulePath = [string]$values['setLineCommandModule']
            OptionCommandModulePath = [string]$values['optionCommandModule']
            TrustedModuleCommandsVerified = $true
            JobAssigned = [bool]$nativeProcess.JobAssigned
            JobCleanupVerified = $jobCleanupVerified
            JobActiveProcessesAtExit = [uint32]$nativeProcess.JobActiveProcessesAtExit
            ProbeRootRemoved = $false
        }
    } finally {
        if ($null -ne $nativeProcess) {
            if (-not $nativeProcess.WaitForExit(0)) {
                Stop-OwnedAcceptanceProcessTree -NativeProcess $nativeProcess -VerifyOwnedProcessIdentity
            }
            $nativeProcess.Dispose()
        }
        # 只删除本轮创建、且从诊断根到目标都没有 reparse point 的探针目录。
        if (Test-Path -LiteralPath $ProbeRoot) {
            Assert-NoReparseTree -Path $ProbeRoot -Root $script:DiagnosticsRoot -Label 'Right-rail PowerShell probe root'
            [IO.Directory]::Delete($ProbeRoot, $true)
            $probeRootRemoved = $true
        }
    }
    if ($null -ne $evidence) {
        $evidence.ProbeRootRemoved = $probeRootRemoved
    }
    return $evidence
}

if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
    $ExecutablePath = Join-Path $script:RepositoryRoot "target\$BuildProfile\kerminal.exe"
} else {
    $ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
}

if (-not $SkipBuild) {
    $features = if ($WorkbenchMotion) { 'right-rail-acceptance,acceptance-metrics' } else { 'right-rail-acceptance' }
    $buildArguments = @('build', '--locked', '--manifest-path', (Join-Path $script:RepositoryRoot 'Cargo.toml'), '-p', 'kerminal-desktop', '--features', $features)
    if ($BuildProfile -ceq 'release') { $buildArguments += '--release' }
    & cargo @buildArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE."
    }
}
if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
    throw "Settings acceptance executable was not found: $ExecutablePath"
}
$ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
$motionEnvironment = $null
if ($WorkbenchMotion) {
    $motionEnvironment = [ordered]@{
        executableSha256 = (Get-FileHash -LiteralPath $ExecutablePath -Algorithm SHA256).Hash.ToLowerInvariant()
        buildProfile = $BuildProfile
        buildProfileSource = $(if ($SkipBuild) { 'operator-declared' } else { 'cargo-invocation' })
        operatingSystem = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber
        processors = @(Get-CimInstance Win32_Processor | Select-Object Name)
        graphicsAdapters = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion)
        renderer = 'unknown; graphicsAdapters lists available adapters only'
    }
    if ($motionEnvironment.processors.Count -eq 0 -or $motionEnvironment.graphicsAdapters.Count -eq 0) {
        throw 'Motion acceptance requires a recorded CPU and graphics-adapter inventory.'
    }
}

[void][IO.Directory]::CreateDirectory($script:DiagnosticsRoot)
# 右栏外层 wrapper 需要在强制中止时清理本轮的临时环境，因此由它生成 run 名并通过
# 环境变量传入；内层只接受严格格式，避免外部值把 run root 指向诊断根之外。
$outerRunName = if ($RightRailSplitSwitch) {
    [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_RUN_NAME', 'Process')
} else {
    $null
}
if ($RightRailSplitSwitch) {
    if ([string]::IsNullOrWhiteSpace($outerRunName) -or
        $outerRunName -notmatch '^run-[0-9]{8}-[0-9]{9}-[0-9a-f]{32}$') {
        throw 'Right-rail acceptance requires a wrapper-generated KERMINAL_RIGHT_RAIL_RUN_NAME in the form run-yyyyMMdd-HHmmssfff-<32 hex>.'
    }
    $runName = $outerRunName
} else {
    $runName = 'run-{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmssfff'), [Guid]::NewGuid().ToString('N')
}
$runRoot = Join-Path $script:DiagnosticsRoot $runName
$runnerLogDirectory = Join-Path $script:DiagnosticsRoot ".runner-$runName"
$runnerStdoutPath = Join-Path $runnerLogDirectory 'stdout.log'
$runnerStderrPath = Join-Path $runnerLogDirectory 'stderr.log'
$configDirectory = Join-Path $runRoot 'config'
if (Test-Path -LiteralPath $runRoot) {
    throw "Refusing to reuse an existing run directory: $runRoot"
}
if (Test-Path -LiteralPath $runnerLogDirectory) {
    throw "Refusing to reuse an existing runner log directory: $runnerLogDirectory"
}
Assert-PathWithinRoot -Path $runnerLogDirectory -Root $script:DiagnosticsRoot -Label 'Runner log directory'
Assert-PathWithinRoot -Path $runRoot -Root $script:DiagnosticsRoot -Label 'Run root'
if ($RightRailSplitSwitch) {
    # run root 尚未创建，不能对它做树检查；先确认诊断根到盘符之间没有已存在的 reparse
    # 组件，否则后续创建和强制中止清理都可能落到用户目录。
    Assert-NoReparseAncestors -Path $script:DiagnosticsRoot -Label 'Right-rail diagnostics root'
}

$result = $null
$processEvidence = $null
$rightRailProbeEvidence = $null
$processEvidencePath = Join-Path $runRoot 'process-result.json'
$acceptancePassed = $false
$acceptanceFailure = $null
$cleanupFailure = $null
$evidenceFailure = $null
$logMoveFailure = $null
$configDirectoryRemoved = $false
$rightRailEnvironmentRemoved = $false
$previousAcceptanceScope = [Environment]::GetEnvironmentVariable('KERMINAL_SETTINGS_ACCEPTANCE_SCOPE', 'Process')
$previousSshSuggestions = [Environment]::GetEnvironmentVariable('KERMINAL_SUGGESTIONS_ACCEPTANCE_SSH', 'Process')
$previousWorkbenchAcceptance = [Environment]::GetEnvironmentVariable('KERMINAL_WORKBENCH_ACCEPTANCE', 'Process')
$previousWorkbenchMotion = [Environment]::GetEnvironmentVariable('KERMINAL_WORKBENCH_MOTION_ACCEPTANCE', 'Process')
$previousRightRailSplitSwitch = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE', 'Process')
$previousWorkbenchPowerShell = [Environment]::GetEnvironmentVariable('KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL', 'Process')
$previousRightRailTrustedModules = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_TRUSTED_MODULES', 'Process')
$previousRightRailPsReadLineHistory = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY', 'Process')
$previousRightRailModuleProbeVerified = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED', 'Process')
$previousRightRailRunName = [Environment]::GetEnvironmentVariable('KERMINAL_RIGHT_RAIL_RUN_NAME', 'Process')
try {
    [Environment]::SetEnvironmentVariable('KERMINAL_SETTINGS_ACCEPTANCE_SCOPE', $(if ($RightRailSplitSwitch) { 'right-rail-split' } elseif ($McpOnly) { 'mcp' } elseif ($SuggestionsOnly) { 'suggestions' } else { $null }), 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_SUGGESTIONS_ACCEPTANCE_SSH', $(if ($SshSuggestions) { '1' } else { $null }), 'Process')
    if ($WorkbenchOnly) {
        [Environment]::SetEnvironmentVariable('KERMINAL_WORKBENCH_ACCEPTANCE', '1', 'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_WORKBENCH_MOTION_ACCEPTANCE', $(if ($WorkbenchMotion) { '1' } else { $null }), 'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE', $(if ($RightRailSplitSwitch) { '1' } else { $null }), 'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL', $(if ($RightRailSplitSwitch) { $resolvedRightRailPowerShell } else { $previousWorkbenchPowerShell }), 'Process')
    }
    [void][IO.Directory]::CreateDirectory($runnerLogDirectory)
    if ($RightRailSplitSwitch) {
        $rightRailProbeEvidence = Assert-RightRailPowerShellVersionInIsolation `
            -Executable $resolvedRightRailPowerShell `
            -ProbeRoot (Join-Path $runnerLogDirectory 'pwsh-probe')
        # 探针已经证明这些路径来自本次 pwsh 的可信 Modules 和隔离 APPDATA；子进程白名单
        # 只复制验证结果，不再使用 output/modules 之类的空目录假装隔离。
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_TRUSTED_MODULES', $rightRailProbeEvidence.PsModulePath, 'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY', $rightRailProbeEvidence.PsReadLineHistoryPath, 'Process')
        [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED', '1', 'Process')
    }
    $result = Invoke-HiddenAcceptanceProcess `
        -Executable $ExecutablePath `
        -WorkingDirectory $script:RepositoryRoot `
        -OutputDirectory $runRoot `
        -StdoutPath $runnerStdoutPath `
        -StderrPath $runnerStderrPath `
        -TimeoutMilliseconds ($TimeoutSeconds * 1000) `
        -Width $Width `
        -Height $Height `
        -VerifyOwnedProcessIdentity:$RightRailSplitSwitch `
        -UseJobObject:$RightRailSplitSwitch

    if (-not (Test-Path -LiteralPath $runRoot -PathType Container)) {
        [void][IO.Directory]::CreateDirectory($runRoot)
    }
    if ($RightRailSplitSwitch) {
        # 子进程退出后、写入任何证据前确认本轮 run root 树没有 reparse point；
        # 后续清理和证据回读都依赖这个所有权前提。
        Assert-NoReparseTree -Path $runRoot -Root $script:DiagnosticsRoot -Label 'Right-rail run root'
    }
    Assert-PathWithinRoot -Path $processEvidencePath -Root $runRoot -Label 'Process evidence'
    $processEvidence = [ordered]@{
        schemaVersion = 1
        processId = $result.ProcessId
        exitCode = $result.ExitCode
        exitCodeHex = $result.ExitCodeHex
        timedOut = $result.TimedOut
        processExited = $true
        desktopName = $result.DesktopName
        privateDesktopClosed = $result.DesktopClosed
        configDirectoryRemoved = $false
        stdoutFile = 'stdout.log'
        stderrFile = 'stderr.log'
        rightRailSplitCleanup = if ($RightRailSplitSwitch) {
            [ordered]@{
                file = 'right-rail-split-switch-cleanup.json'
                verified = $false
                timedOut = $result.TimedOut
                jobObjectAssigned = [bool]$result.JobAssigned
                jobCleanupVerified = [bool]$result.JobCleanupVerified
                jobCleanupForced = [bool]$result.JobCleanupForced
                jobActiveProcessesAtExit = [uint32]$result.JobActiveProcessesAtExit
                forcedProcessTreeStop = [bool]($result.TimedOut -and $result.JobCleanupVerified)
                limitation = if ($result.TimedOut -and -not $result.JobCleanupVerified) {
                    'process timed out; shutdown cleanup receipt unavailable and job-object process cleanup was not verified'
                } elseif ($result.TimedOut) {
                    'process timed out; shutdown cleanup receipt unavailable, but job-object active process count reached zero'
                } else {
                    $null
                }
            }
        } else {
            $null
        }
        rightRailProbe = if ($RightRailSplitSwitch -and $null -ne $rightRailProbeEvidence) {
            [ordered]@{
                file = 'pwsh-probe'
                # 探针在独立 Job 中运行，只证明外层 pwsh 的版本、模块与历史路径；
                # 内层子 shell 是否读到同一历史文件没有回读，报告不得把它说成已验证。
                scope = 'outer-isolated-pwsh-probe-only; child shell history path is inferred from run-root environment mapping, not probed'
                psVersionMajor = $rightRailProbeEvidence.PsVersionMajor
                psEdition = $rightRailProbeEvidence.PsEdition
                psHome = $rightRailProbeEvidence.PsHome
                psModulePath = $rightRailProbeEvidence.PsModulePath
                psReadLineHistoryPath = $rightRailProbeEvidence.PsReadLineHistoryPath
                trustedModuleCommandsVerified = [bool]$rightRailProbeEvidence.TrustedModuleCommandsVerified
                jobObjectAssigned = [bool]$rightRailProbeEvidence.JobAssigned
                jobCleanupVerified = [bool]$rightRailProbeEvidence.JobCleanupVerified
                jobActiveProcessesAtExit = [uint32]$rightRailProbeEvidence.JobActiveProcessesAtExit
                probeRootRemoved = [bool]$rightRailProbeEvidence.ProbeRootRemoved
            }
        } else {
            $null
        }
    }
    $processEvidenceJson = ConvertTo-Json -InputObject $processEvidence -Depth 3
    [IO.File]::WriteAllText(
        $processEvidencePath,
        $processEvidenceJson + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false)
    )
    if ($RightRailSplitSwitch) {
        # 父进程退出不等于子孙回收；只有 Job active process 计数归零才算证明。
        # 正常退出时若仍需强制结束 Job，说明 graceful cleanup 未完成，不能当作通过。
        if (-not [bool]$result.JobAssigned -or
            -not [bool]$result.JobCleanupVerified -or
            [uint32]$result.JobActiveProcessesAtExit -ne 0) {
            throw "The right-rail process tree did not prove an empty owned job (assigned=$($result.JobAssigned), verified=$($result.JobCleanupVerified), active=$($result.JobActiveProcessesAtExit))."
        }
        if (-not $result.TimedOut -and [bool]$result.JobCleanupForced) {
            throw 'The right-rail process tree required forced job cleanup after a normal exit; graceful process cleanup is not proven.'
        }
    }
    if ($WorkbenchMotion) {
        $afterHash = (Get-FileHash -LiteralPath $ExecutablePath -Algorithm SHA256).Hash.ToLowerInvariant()
        $motionEnvironment['executableUnchanged'] = $afterHash -ceq $motionEnvironment.executableSha256
        [IO.File]::WriteAllText(
            (Join-Path $runRoot 'workbench-motion-environment.json'),
            ((ConvertTo-Json -InputObject $motionEnvironment -Depth 5) + [Environment]::NewLine),
            [Text.UTF8Encoding]::new($false)
        )
        if (-not $motionEnvironment.executableUnchanged) {
            throw 'The motion acceptance executable changed during sampling.'
        }
    }

    if (-not $result.DesktopClosed) {
        throw "The settings acceptance private desktop was not closed after native exit status $($result.ExitCodeHex) ($($result.ExitCode)). See process-result.json and stderr.log in $runRoot."
    }
    if ($result.TimedOut) {
        $timeoutCleanup = if ($RightRailSplitSwitch) {
            ' The right-rail cleanup receipt is unavailable because shutdown did not finish;'
        } else {
            ''
        }
        throw "Settings parity process exceeded $TimeoutSeconds seconds and was stopped with status $($result.ExitCodeHex) ($($result.ExitCode)).$timeoutCleanup See process-result.json and stderr.log in $runRoot."
    }
    if ($result.ExitCode -ne 0) {
        $cleanupDetail = ''
        if ($RightRailSplitSwitch) {
            try {
                Assert-RightRailSplitCleanup -RunRoot $runRoot -ExpectedUiResult 'failure'
                $processEvidence.rightRailSplitCleanup.verified = $true
            } catch {
                $processEvidence.rightRailSplitCleanup.limitation = $_.Exception.Message
                $cleanupDetail = " Right-rail cleanup evidence also failed: $($_.Exception.Message)"
            }
        }
        throw "Settings parity process failed with native exit status $($result.ExitCodeHex) ($($result.ExitCode)).$cleanupDetail See process-result.json and stderr.log in $runRoot."
    }

    $reportPath = Join-Path $runRoot $(if ($WorkbenchMotion) { 'workbench-motion-report.json' } elseif ($RightRailSplitSwitch) { 'right-rail-split-switch-report.json' } elseif ($WorkbenchOnly) { 'workbench-report.json' } else { 'report.json' })
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
        throw "Settings parity process exited without a report: $runRoot"
    }
    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    if ($WorkbenchMotion) {
        Assert-WorkbenchMotionReport `
            -Report $report `
            -RunRoot $runRoot `
            -ProcessId $result.ProcessId `
            -DesktopName $result.DesktopName `
            -ExpectedWidth $Width `
            -ExpectedHeight $Height
    } elseif ($RightRailSplitSwitch) {
        Assert-RightRailSplitSwitchReport `
            -Report $report `
            -RunRoot $runRoot `
            -ProcessId $result.ProcessId `
            -DesktopName $result.DesktopName `
            -ExpectedWidth $Width `
            -ExpectedHeight $Height `
            -ExpectedPowerShell $resolvedRightRailPowerShell
        try {
            Assert-RightRailSplitCleanup -RunRoot $runRoot -ExpectedUiResult 'success' -Report $report
            $processEvidence.rightRailSplitCleanup.verified = $true
        } catch {
            $processEvidence.rightRailSplitCleanup.limitation = $_.Exception.Message
            throw
        }
    } elseif ($WorkbenchOnly) {
        Assert-WorkbenchAcceptanceReport `
            -Report $report `
            -RunRoot $runRoot `
            -ProcessId $result.ProcessId `
            -DesktopName $result.DesktopName `
            -ExpectedWidth $Width `
            -ExpectedHeight $Height
    } else {
        Assert-SettingsParityReport `
            -Report $report `
            -RunRoot $runRoot `
            -ProcessId $result.ProcessId `
            -DesktopName $result.DesktopName `
            -ExpectedWidth $Width `
            -ExpectedHeight $Height `
            -McpOnly:$McpOnly `
            -SuggestionsOnly:$SuggestionsOnly `
            -SshSuggestions:$SshSuggestions
    }
    $acceptancePassed = $true
} catch {
    $acceptanceFailure = $_
    if ($RightRailSplitSwitch -and $null -ne $processEvidence -and
        -not $processEvidence.rightRailSplitCleanup.verified -and
        $null -eq $processEvidence.rightRailSplitCleanup.limitation) {
        $processEvidence.rightRailSplitCleanup.limitation = $_.Exception.Message
    }
} finally {
    [Environment]::SetEnvironmentVariable('KERMINAL_SETTINGS_ACCEPTANCE_SCOPE', $previousAcceptanceScope, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_SUGGESTIONS_ACCEPTANCE_SSH', $previousSshSuggestions, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_WORKBENCH_ACCEPTANCE', $previousWorkbenchAcceptance, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_WORKBENCH_MOTION_ACCEPTANCE', $previousWorkbenchMotion, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE', $previousRightRailSplitSwitch, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL', $previousWorkbenchPowerShell, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_TRUSTED_MODULES', $previousRightRailTrustedModules, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY', $previousRightRailPsReadLineHistory, 'Process')
    [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED', $previousRightRailModuleProbeVerified, 'Process')
    # run 名属于外层 wrapper 的本轮身份；恢复原值，避免同一进程里后续 scope 误用旧名。
    [Environment]::SetEnvironmentVariable('KERMINAL_RIGHT_RAIL_RUN_NAME', $previousRightRailRunName, 'Process')
    try {
        if (Test-Path -LiteralPath $runRoot) {
            Remove-IsolatedConfigDirectory -RunRoot $runRoot -ConfigDirectory $configDirectory
        }
        $configDirectoryRemoved = -not (Test-Path -LiteralPath $configDirectory)
        if (-not $configDirectoryRemoved) {
            throw 'The isolated configuration directory still exists after cleanup.'
        }
    } catch {
        $cleanupFailure = $_
        $configDirectoryRemoved = $false
    }
    if ($RightRailSplitSwitch) {
        if (-not (Test-Path -LiteralPath $runRoot)) {
            # run root 未建立时没有本轮环境目录需要删除；后续仍会因缺少报告/证据失败。
            $rightRailEnvironmentRemoved = $true
        } else {
            try {
                $rightRailEnvironmentRemoved = Remove-RightRailIsolatedEnvironment -RunRoot $runRoot
                if (-not $rightRailEnvironmentRemoved) {
                    throw 'The isolated right-rail environment directories still exist after cleanup.'
                }
            } catch {
                if ($null -eq $cleanupFailure) {
                    $cleanupFailure = $_
                } else {
                    $cleanupFailure = [InvalidOperationException]::new(
                        $cleanupFailure.Exception.Message + '; ' + $_.Exception.Message)
                }
                $rightRailEnvironmentRemoved = $false
            }
        }
    }

    if ($null -ne $processEvidence) {
        $processEvidence['configDirectoryRemoved'] = $configDirectoryRemoved
        if ($RightRailSplitSwitch) {
            $processEvidence['rightRailEnvironmentRemoved'] = $rightRailEnvironmentRemoved
        }
        try {
            $processEvidenceJson = ConvertTo-Json -InputObject $processEvidence -Depth 3
            [IO.File]::WriteAllText(
                $processEvidencePath,
                $processEvidenceJson + [Environment]::NewLine,
                [Text.UTF8Encoding]::new($false)
            )
            $processEvidenceReadback = Get-Content -LiteralPath $processEvidencePath -Raw | ConvertFrom-Json
            $removalProperty = $processEvidenceReadback.PSObject.Properties['configDirectoryRemoved']
            if ($null -eq $removalProperty -or
                [bool]$removalProperty.Value -ne $configDirectoryRemoved -or
                ($configDirectoryRemoved -and (Test-Path -LiteralPath $configDirectory))) {
                throw 'Process evidence did not confirm the isolated configuration cleanup readback.'
            }
            if ($RightRailSplitSwitch) {
                $environmentRemovalProperty = $processEvidenceReadback.PSObject.Properties['rightRailEnvironmentRemoved']
                if ($null -eq $environmentRemovalProperty -or
                    [bool]$environmentRemovalProperty.Value -ne $rightRailEnvironmentRemoved) {
                    throw 'Process evidence did not confirm the isolated right-rail environment cleanup readback.'
                }
                foreach ($name in @('home', 'temp', 'program-data', 'public')) {
                    if (Test-Path -LiteralPath (Join-Path $runRoot $name)) {
                        throw "The right-rail environment directory still exists after cleanup: $name"
                    }
                }
            }
        } catch {
            $evidenceFailure = $_
        }
    }

    if (Test-Path -LiteralPath $runnerLogDirectory -PathType Container) {
        try {
            Move-AcceptanceLogs -RunnerLogDirectory $runnerLogDirectory -RunRoot $runRoot
        } catch {
            $logMoveFailure = $_
        }
    }
}

$finalizationFailures = @()
if ($null -ne $cleanupFailure) {
    $finalizationFailures += "Config cleanup failed: $($cleanupFailure.Exception.Message)"
}
if ($null -ne $evidenceFailure) {
    $finalizationFailures += "Process evidence writeback failed: $($evidenceFailure.Exception.Message)"
}
if ($null -ne $logMoveFailure) {
    $finalizationFailures += "Runner log preservation failed: $($logMoveFailure.Exception.Message)"
}
if ($null -ne $acceptanceFailure) {
    if ($finalizationFailures.Count -gt 0) {
        throw "Settings acceptance failed: $($acceptanceFailure.Exception.Message). Finalization failures: $($finalizationFailures -join '; ')"
    }
    throw $acceptanceFailure
}
if ($finalizationFailures.Count -gt 0) {
    throw ($finalizationFailures -join '; ')
}
if (-not $acceptancePassed -or
    $null -eq $processEvidence -or
    -not $processEvidence['configDirectoryRemoved']) {
    throw 'Settings parity acceptance did not produce a verified cleanup receipt.'
}
Write-Output "$(if ($WorkbenchMotion) { 'Workbench motion' } elseif ($RightRailSplitSwitch) { 'Right rail split switch' } elseif ($WorkbenchOnly) { 'Workbench' } else { 'Settings parity' }) acceptance passed."
Write-Output "Evidence: $runRoot"
if ($RightRailSplitSwitch) {
    Write-Output 'Theme note: system mode is mode-only; following the operating-system appearance was not verified.'
    # 这里只声明目标数据的中间采样事实；哈希差异不能当作动画、正常窗口 present 或
    # 140ms 淡入的测量结果。
    Write-Output 'Target-data note: all six directions required a hidden-window intermediate sample with the active target pane, new row visible/present, and old row absent; normal-window present, opacity, running state, and the 140ms fade remain unproven.'
}
