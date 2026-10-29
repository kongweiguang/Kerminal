# @author kongweiguang
param(
    [string]$ExecutablePath,

    [switch]$SkipBuild,

    [ValidateRange(800, 3840)]
    [int]$Width = 1440,

    [ValidateRange(600, 2160)]
    [int]$Height = 900,

    [ValidateRange(30, 600)]
    [int]$TimeoutSeconds = 360
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:RepositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$script:DiagnosticsRoot = [IO.Path]::GetFullPath((Join-Path $script:RepositoryRoot 'target\diagnostics\settings-parity'))
$script:ExpectedPages = @(
    'appearance',
    'terminal',
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
    'terminal-renderer',
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
$script:ExpectedSyncActions = @(
    'sync-local-remote-roundtrip',
    'sync-failure-preserves-local',
    'sync-retry-confirms-remote'
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
        private bool _disposed;

        public int ProcessId { get; private set; }
        public string DesktopName { get; private set; }
        public bool DesktopClosed { get; private set; }

        // Transfers the launcher's process and desktop handles to the bounded owner object.
        internal NativeDesktopProcess(IntPtr desktopHandle, IntPtr processHandle, int processId, string desktopName)
        {
            _desktopHandle = desktopHandle;
            _processHandle = processHandle;
            ProcessId = processId;
            DesktopName = desktopName;
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

        // Keeps the private desktop alive until its only child process has exited, then closes both owned handles.
        public void Dispose()
        {
            if (_disposed)
                return;
            if (_processHandle != IntPtr.Zero && !WaitForExit(0))
                throw new InvalidOperationException("The private desktop cannot close while its acceptance process is running.");

            int firstError = 0;
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
            _disposed = _processHandle == IntPtr.Zero && _desktopHandle == IntPtr.Zero;
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

        // Releases a process or standard-stream handle acquired by this launcher.
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool CloseHandle(IntPtr handle);

        // Keeps the acceptance child on its private desktop and forwards one validated logical viewport to native capture.
        public static NativeDesktopProcess Start(string executable, string workingDirectory, string outputDirectory, string stdoutPath, string stderrPath, string desktopName, uint width, uint height)
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
            ProcessInformation processInformation = new ProcessInformation();
            bool desktopTransferred = false;
            bool attributeListInitialized = false;
            try
            {
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

                string command = QuoteArgument(executable)
                    + " --settings-acceptance --output-dir " + QuoteArgument(outputDirectory)
                    + " --width " + width.ToString(CultureInfo.InvariantCulture)
                    + " --height " + height.ToString(CultureInfo.InvariantCulture);
                ProcessInformation info;
                environment = CreateIsolatedGitEnvironment(outputDirectory);
                if (!CreateProcess(executable, new StringBuilder(command), IntPtr.Zero, IntPtr.Zero, true, CreateNoWindow | ExtendedStartupInfoPresent | CreateUnicodeEnvironment, environment, workingDirectory, ref startupInfo, out info))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcessW on the private desktop failed.");
                processInformation = info;
                if (!CloseHandle(processInformation.Thread))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Closing the isolated process thread handle failed.");
                processInformation.Thread = IntPtr.Zero;

                NativeDesktopProcess result = new NativeDesktopProcess(desktop, processInformation.Process, unchecked((int)processInformation.ProcessId), desktopName);
                desktop = IntPtr.Zero;
                processInformation.Process = IntPtr.Zero;
                desktopTransferred = true;
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
                    if (desktop != IntPtr.Zero)
                        CloseDesktop(desktop);
                }
            }
        }

        // Isolates production Git's inherited configuration in the child only, preventing user rewrites, hooks, or network transports from affecting local fixtures.
        private static IntPtr CreateIsolatedGitEnvironment(string outputDirectory)
        {
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
    # Checks PNG file identity and IHDR dimensions against the native renderer's pixel metadata.
    param(
        [Parameter(Mandatory = $true)]$Capture,
        [Parameter(Mandatory = $true)][string]$RunRoot
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
}

function Assert-SettingsParityReport {
    # 同步验收必须有本地 bare 仓库的成功、失败保留和恢复回读，页面截图不替代写入证据。
    # 核对页面、操作回读、截图清单及请求视口，避免默认尺寸掩盖窄窗验收未生效。
    param(
        [Parameter(Mandatory = $true)]$Report,
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][string]$DesktopName,
        [Parameter(Mandatory = $true)][int]$ExpectedWidth,
        [Parameter(Mandatory = $true)][int]$ExpectedHeight
    )

    if ($Report.schemaVersion -ne 4 -or $Report.captureMode -cne 'hidden_native_window') {
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
    if ($Report.settingsModalGeneration -lt 1 -or
        [double]$Report.windowWidth -ne $ExpectedWidth -or
        [double]$Report.windowHeight -ne $ExpectedHeight) {
        throw "The settings modal lifecycle or native viewport dimensions are invalid; expected ${ExpectedWidth}x${ExpectedHeight}."
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

    foreach ($fileName in @(
        'settings-opened.png',
        'settings-functional-keyword-colors.png',
        'settings-functional-keyword-color-picker.png'
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
        [Parameter(Mandatory = $true)][int]$Height
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
            [uint32]$Height
        )
        $timedOut = -not $nativeProcess.WaitForExit([uint32]$TimeoutMilliseconds)
        if ($timedOut) {
            Stop-OwnedAcceptanceProcessTree -NativeProcess $nativeProcess
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
                Stop-OwnedAcceptanceProcessTree -NativeProcess $nativeProcess
            }
            try {
                $nativeProcess.Dispose()
            } catch {
                $nativeProcess.Dispose()
            }
        }
    }
    $result | Add-Member -NotePropertyName DesktopClosed -NotePropertyValue $nativeProcess.DesktopClosed
    return $result
}

function Stop-OwnedAcceptanceProcessTree {
    # Uses the exact child PID returned by CreateProcessW and never enumerates or terminates unrelated processes.
    param([Parameter(Mandatory = $true)]$NativeProcess)

    $process = $null
    try {
        $process = [Diagnostics.Process]::GetProcessById($NativeProcess.ProcessId)
    } catch [ArgumentException] {
        $process = $null
    }
    $treeCleanupError = $null
    if ($null -ne $process) {
        try {
            if (-not $process.HasExited) {
                try {
                    $process.Kill($true)
                } catch {
                    if (-not $process.HasExited) {
                        $treeCleanupError = $_.Exception.Message
                    }
                }
            }
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
    } elseif ($treeCleanupError) {
        throw "Could not prove owned process-tree cleanup: $treeCleanupError"
    }
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

if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
    $ExecutablePath = Join-Path $script:RepositoryRoot 'target\debug\kerminal.exe'
} else {
    $ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
}

if (-not $SkipBuild) {
    & cargo build --manifest-path (Join-Path $script:RepositoryRoot 'Cargo.toml') -p kerminal-desktop --features right-rail-acceptance
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE."
    }
}
if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
    throw "Settings acceptance executable was not found: $ExecutablePath"
}
$ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)

[void][IO.Directory]::CreateDirectory($script:DiagnosticsRoot)
$runName = 'run-{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmssfff'), [Guid]::NewGuid().ToString('N')
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

$result = $null
$processEvidence = $null
$processEvidencePath = Join-Path $runRoot 'process-result.json'
$acceptancePassed = $false
$acceptanceFailure = $null
$cleanupFailure = $null
$evidenceFailure = $null
$logMoveFailure = $null
$configDirectoryRemoved = $false
try {
    [void][IO.Directory]::CreateDirectory($runnerLogDirectory)
    $result = Invoke-HiddenAcceptanceProcess `
        -Executable $ExecutablePath `
        -WorkingDirectory $script:RepositoryRoot `
        -OutputDirectory $runRoot `
        -StdoutPath $runnerStdoutPath `
        -StderrPath $runnerStderrPath `
        -TimeoutMilliseconds ($TimeoutSeconds * 1000) `
        -Width $Width `
        -Height $Height

    if (-not (Test-Path -LiteralPath $runRoot -PathType Container)) {
        [void][IO.Directory]::CreateDirectory($runRoot)
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
    }
    $processEvidenceJson = ConvertTo-Json -InputObject $processEvidence -Depth 3
    [IO.File]::WriteAllText(
        $processEvidencePath,
        $processEvidenceJson + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false)
    )

    if (-not $result.DesktopClosed) {
        throw "The settings acceptance private desktop was not closed after native exit status $($result.ExitCodeHex) ($($result.ExitCode)). See process-result.json and stderr.log in $runRoot."
    }
    if ($result.TimedOut) {
        throw "Settings parity process exceeded $TimeoutSeconds seconds and was stopped with status $($result.ExitCodeHex) ($($result.ExitCode)). See process-result.json and stderr.log in $runRoot."
    }
    if ($result.ExitCode -ne 0) {
        throw "Settings parity process failed with native exit status $($result.ExitCodeHex) ($($result.ExitCode)). See process-result.json and stderr.log in $runRoot."
    }

    $reportPath = Join-Path $runRoot 'report.json'
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
        throw "Settings parity process exited without a report: $runRoot"
    }
    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    Assert-SettingsParityReport `
        -Report $report `
        -RunRoot $runRoot `
        -ProcessId $result.ProcessId `
        -DesktopName $result.DesktopName `
        -ExpectedWidth $Width `
        -ExpectedHeight $Height
    $acceptancePassed = $true
} catch {
    $acceptanceFailure = $_
} finally {
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

    if ($null -ne $processEvidence) {
        $processEvidence['configDirectoryRemoved'] = $configDirectoryRemoved
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
Write-Output "Settings parity acceptance passed."
Write-Output "Evidence: $runRoot"
