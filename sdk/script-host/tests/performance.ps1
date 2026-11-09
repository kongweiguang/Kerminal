# @author kongweiguang
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [int]$Processes = 20,
    [int]$RoundTrips = 100
)

$ErrorActionPreference = 'Stop'

function Read-ProtocolFrame {
    <# 对输出设等待上限，让宿主卡死时采样明确失败。 #>
    param([System.Diagnostics.Process]$Process)
    $read = $Process.StandardOutput.ReadLineAsync()
    if (-not $read.Wait(5000)) { throw 'plugin host did not respond within five seconds' }
    if ($null -eq $read.Result) { throw 'plugin host closed stdout unexpectedly' }
    return ($read.Result | ConvertFrom-Json)
}

function Get-Median {
    <# 保留原始样本并另外计算中位数，避免单次调度抖动掩盖分布。 #>
    param([double[]]$Values)
    $ordered = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($ordered.Count / 2)
    if (($ordered.Count % 2) -eq 1) { return $ordered[$middle] }
    return ($ordered[$middle - 1] + $ordered[$middle]) / 2
}

$fixture = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../../tests/fixtures/plugins/scripts/p0'))
$binaryPath = [IO.Path]::GetFullPath($Binary)
$startSamples = [Collections.Generic.List[double]]::new()
$memorySamples = [Collections.Generic.List[long]]::new()
$rpcSamples = [Collections.Generic.List[double]]::new()
$dataDir = Join-Path ([IO.Path]::GetTempPath()) ("kerminal-script-host-perf-" + [guid]::NewGuid().ToString())
[IO.Directory]::CreateDirectory($dataDir) | Out-Null
$initialize = @{jsonrpc='2.0'; id='h:1'; method='initialize'; params=@{protocol=1; host_api='1.0'; plugin=@{id='acme.notes'; version='1.2.0'}; locale='zh-CN'; theme='dark'; data_dir=$dataDir; generation=1}} | ConvertTo-Json -Depth 5 -Compress
$initialized = '{"jsonrpc":"2.0","method":"initialized"}'
$ping = '{"jsonrpc":"2.0","id":"h:2","method":"$/ping","params":{}}'
$shutdown = '{"jsonrpc":"2.0","id":"h:3","method":"shutdown","params":{}}'

try {
for ($index = 0; $index -lt $Processes; $index++) {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $binaryPath
    $startInfo.ArgumentList.Add($fixture)
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        if (-not $process.Start()) { throw 'failed to start plugin host' }
        $process.StandardInput.WriteLine($initialize)
        $reply = Read-ProtocolFrame $process
        $startSamples.Add($clock.Elapsed.TotalMilliseconds)
        if ($reply.id -ne 'h:1' -or $reply.result.plugin.id -ne 'acme.notes') { throw 'invalid handshake' }
        $process.StandardInput.WriteLine($initialized)
        $process.Refresh()
        $memorySamples.Add($process.WorkingSet64)
        for ($request = 0; $request -lt $RoundTrips; $request++) {
            $clock.Restart()
            $process.StandardInput.WriteLine($ping)
            $reply = Read-ProtocolFrame $process
            $rpcSamples.Add($clock.Elapsed.TotalMilliseconds)
            if ($reply.id -ne 'h:2' -or $null -eq $reply.result) { throw 'invalid ping response' }
        }
        $process.StandardInput.WriteLine($shutdown)
        $reply = Read-ProtocolFrame $process
        if ($reply.id -ne 'h:3' -or -not $process.WaitForExit(5000) -or $process.ExitCode -ne 0) {
            throw 'plugin host did not shut down cleanly'
        }
    } finally {
        if (-not $process.HasExited) { $process.Kill() }
        $process.Dispose()
    }
}

} finally {
    # 该目录由本次采样新建且没有插件存储操作，只删除空目录。
    [IO.Directory]::Delete($dataDir)
}

# WMI 不可用时读取 Windows 的处理器注册表描述，保留同样的硬件上下文。
try {
    $cpu = Get-CimInstance Win32_Processor -ErrorAction Stop | Select-Object -First 1 -ExpandProperty Name
} catch {
    $cpu = [Microsoft.Win32.Registry]::GetValue('HKEY_LOCAL_MACHINE\HARDWARE\DESCRIPTION\System\CentralProcessor\0', 'ProcessorNameString', 'unknown')
}
[ordered]@{
    binary = $binaryPath
    binary_sha256 = (Get-FileHash -LiteralPath $binaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
    sampled_at_utc = [DateTime]::UtcNow.ToString('O')
    file_cache_cleared = $false
    startup_scope = 'new process through initialize response; operating-system file cache retained'
    cpu = $cpu
    os = [Environment]::OSVersion.VersionString
    process_count = $Processes
    round_trips_per_process = $RoundTrips
    startup_ms = @($startSamples)
    working_set_bytes = @($memorySamples)
    rpc_ms = @($rpcSamples)
    startup_median_ms = Get-Median $startSamples.ToArray()
    startup_max_ms = ($startSamples | Measure-Object -Maximum).Maximum
    working_set_max_bytes = ($memorySamples | Measure-Object -Maximum).Maximum
    rpc_median_ms = Get-Median $rpcSamples.ToArray()
} | ConvertTo-Json -Depth 5
