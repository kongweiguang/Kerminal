# @author kongweiguang
param(
    [ValidateSet('all','owner','suspended-owner','package-timeout','startup-failure','setup-failure','cleanup-failure')][string]$Case='all',
    [string]$Scratch
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'

function Get-TestFunctionSource {
    # 只抽取函数声明而不执行验收脚本主体，避免测试启动候选、插件或真实窗口。
    # @author kongweiguang
    param([string]$Path,[string]$Name)
    $tokens=$null; $errors=$null
    $ast=[Management.Automation.Language.Parser]::ParseFile($Path,[ref]$tokens,[ref]$errors)
    if($errors.Count){throw "Cannot parse $Path : $($errors -join '; ')"}
    $matches=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $Name},$true))
    if($matches.Count -ne 1){throw "Function missing or ambiguous: $Path / $Name"}
    return $matches[0].Extent.Text
}

function Import-TestInitializer {
    # 把动态函数的脚本目录绑定到真实来源；不改写函数内用于二次 AST 拼接的字面量。
    # @author kongweiguang
    param([string]$File,[string]$Name,[switch]$SyntheticCommand,[switch]$PauseBeforeResume,[switch]$ForceJobSetupFailure,[switch]$InjectCleanupFailures)
    $source=Get-TestFunctionSource -Path (Join-Path $PSScriptRoot $File) -Name $Name
    $quoted="'"+$PSScriptRoot.Replace("'","''")+"'"
    $tokens=$null;$errors=$null
    $parsed=[Management.Automation.Language.Parser]::ParseInput($source,[ref]$tokens,[ref]$errors)
    if($errors.Count){throw 'Cannot parse the extracted initializer.'}
    $roots=@($parsed.FindAll({param($node) $node -is [Management.Automation.Language.VariableExpressionAst] -and $node.VariablePath.UserPath -ceq 'PSScriptRoot'},$true) | Sort-Object {$_.Extent.StartOffset} -Descending)
    foreach($root in $roots){$source=$source.Remove($root.Extent.StartOffset,$root.Extent.Text.Length).Insert($root.Extent.StartOffset,('('+$quoted+')'))}
    if($SyntheticCommand){
        $old='$packageCommand=''+ (packageInput == null ? "" : " package --dir " + QuoteArgument(packageInput) + " --out " + QuoteArgument(packageOutput))'''
        $new='$packageCommand=''+ " -NoLogo -NoProfile -NonInteractive -EncodedCommand " + QuoteArgument(Environment.GetEnvironmentVariable("KERMINAL_ACCEPTANCE_JOB_TEST_COMMAND"))'''
        if($source.Split([string[]]@($old),[StringSplitOptions]::None).Count -ne 2){throw 'Synthetic test command injection is ambiguous.'}
        $source=$source.Replace($old,$new)
    }
    if($ForceJobSetupFailure){
        # 仅替换内存中的 Job 信息类，检查完整启动路径在 CreateProcess 前拒绝失败配置。
        $old='$parts.Add($jobs[0].Extent.Text); $parts.Add($launch[0].Extent.Text)'
        $new='$parts.Add($jobs[0].Extent.Text.Replace(''SetInformationJobObject(job,9,ref limits,'',''SetInformationJobObject(job,-1,ref limits,'')); $parts.Add($launch[0].Extent.Text)'
        if($source.Split([string[]]@($old),[StringSplitOptions]::None).Count -ne 2){throw 'Job setup failure injection is ambiguous.'}
        $source=$source.Replace($old,$new)
    }
    if($InjectCleanupFailures){
        # 测试副本仍先执行真实终止和关闭，再把结果伪装为失败，既覆盖诊断又不泄漏测试资源。
        $old='$parts.Add($jobs[0].Extent.Text); $parts.Add($launch[0].Extent.Text)'
        $new='$parts.Add($jobs[0].Extent.Text.Replace(''if(!TerminateJobObject(job,1))'',''if(!TerminateJobObject(job,1) || true)'').Replace(''if(!CloseHandle(job)) errors.Add'',''if(!CloseHandle(job) || true) errors.Add'')); $parts.Add($launch[0].Extent.Text)'
        if($source.Split([string[]]@($old),[StringSplitOptions]::None).Count -ne 2){throw 'Job cleanup injection is ambiguous.'}
        $source=$source.Replace($old,$new)
        $anchor='    $source = Replace-WebViewLauncherPart $source ''public JobSnapshot GetJobSnapshot()'
        if($source.Split([string[]]@($anchor),[StringSplitOptions]::None).Count -ne 2){throw 'Launcher cleanup injection is ambiguous.'}
        $inject=@'
    $source = Replace-WebViewLauncherPart $source 'if (desktop != IntPtr.Zero && !CloseDesktop(desktop))' 'if (desktop != IntPtr.Zero && (!CloseDesktop(desktop) || true))'
    $source = Replace-WebViewLauncherPart $source 'if (!CloseHandle(handle)) errors.Add' 'if (!CloseHandle(handle) || description == "标准输出") errors.Add'
    $source = Replace-WebViewLauncherPart $source 'processInformation = info; if (ProbeJob.ResumeThread(processInformation.Thread) == uint.MaxValue)' 'processInformation = info; var marker = Environment.GetEnvironmentVariable("KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER"); if (marker != null) { using (var p = System.Diagnostics.Process.GetProcessById(unchecked((int)processInformation.ProcessId))) System.IO.File.WriteAllText(marker, p.Id.ToString() + ":" + p.StartTime.ToUniversalTime().Ticks.ToString()); } if (Environment.GetEnvironmentVariable("KERMINAL_ACCEPTANCE_JOB_TEST_AFTER_CREATE") == "1") throw new InvalidOperationException("测试：首线程恢复前启动失败"); if (ProbeJob.ResumeThread(processInformation.Thread) == uint.MaxValue)'
'@
        $source=$source.Replace($anchor,($inject+"`n"+$anchor))
    }
    if($PauseBeforeResume){
        # 只在测试编译产物中暂停首线程恢复，验证创建与归属之间没有可遗留的窗口。
        $anchor='    $source = Replace-WebViewLauncherPart $source ''public JobSnapshot GetJobSnapshot()'
        if($source.Split([string[]]@($anchor),[StringSplitOptions]::None).Count -ne 2){throw 'Resume pause injection is ambiguous.'}
        $inject=@'
    $source = Replace-WebViewLauncherPart $source 'processInformation = info; if (ProbeJob.ResumeThread(processInformation.Thread) == uint.MaxValue)' 'processInformation = info; var marker = Environment.GetEnvironmentVariable("KERMINAL_ACCEPTANCE_JOB_TEST_MARKER"); System.IO.File.WriteAllText(marker + ".tmp", processInformation.ProcessId.ToString()); System.IO.File.Move(marker + ".tmp", marker); System.Threading.Thread.Sleep(25000); if (ProbeJob.ResumeThread(processInformation.Thread) == uint.MaxValue)'
'@
        $source=$source.Replace($anchor,($inject+"`n"+$anchor))
    }
    if($File -ceq 'plugin-webview-native.ps1'){
        foreach($helper in @('Replace-PluginLauncherPart','Get-PluginPrivateDesktopSource','Get-PluginWebViewLauncherScript')){
            . ([scriptblock]::Create((Get-TestFunctionSource -Path (Join-Path $PSScriptRoot $File) -Name $helper)))
        }
    }
    . ([scriptblock]::Create($source))
    & $Name
}

function Import-TestWebViewLauncher {
    # 直接编译原始 WebView 装配块，与其他消费者相同地保留唯一替换点检查。
    # @author kongweiguang
    $path=Join-Path $PSScriptRoot 'webview-native.ps1'
    $tokens=$null; $errors=$null
    $ast=[Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors)
    if($errors.Count){throw 'Cannot parse shared WebView launcher.'}
    $parts=[Collections.Generic.List[string]]::new()
    foreach($name in @('Get-WebViewLauncherSource','Replace-WebViewLauncherPart')){$parts.Add((Get-TestFunctionSource -Path $path -Name $name))}
    $jobs=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$jobSource'},$true))
    $launch=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.StartsWith("if (-not ('Kerminal.WebViewProbe.NativeDesktopLauncher'",[StringComparison]::Ordinal)},$true))
    if($jobs.Count -ne 1 -or $launch.Count -ne 1){throw 'Shared WebView launcher blocks are ambiguous.'}
    $parts.Add($jobs[0].Extent.Text); $parts.Add($launch[0].Extent.Text)
    $quoted="'"+$PSScriptRoot.Replace("'","''")+"'"
    $source=($parts -join "`n").Replace('$PSScriptRoot',$quoted)
    . ([scriptblock]::Create($source))
}

function Test-JobSetupFailure {
    # 只在内存中注入无效信息类，重复检查真实失败路径没有泄漏新建 Job 句柄。
    # @author kongweiguang
    $path=Join-Path $PSScriptRoot 'webview-native.ps1'
    $tokens=$null; $errors=$null
    $ast=[Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors)
    if($errors.Count){throw 'Cannot parse Job source.'}
    $jobs=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -ceq '$jobSource'},$true))
    if($jobs.Count -ne 1){throw 'Job source is ambiguous.'}
    $source=$jobs[0].Right.Expression.Value.Replace('Kerminal.WebViewProbe','Kerminal.JobSetupFailureTest')
    $old='SetInformationJobObject(job,9,ref limits,'
    if($source.Split([string[]]@($old),[StringSplitOptions]::None).Count -ne 2){throw 'Job setup injection is ambiguous.'}
    $source=$source.Replace($old,'SetInformationJobObject(job,-1,ref limits,')
    $source+=@'

namespace Kerminal.JobSetupFailureTest {
    public static class FailureProbe {
        // 意外配置成功时也回收 Job，测试不得因自身断言而造成泄漏。
        public static void Run() { var job=ProbeJob.Create(); var errors=new System.Collections.Generic.List<System.Exception>(); ProbeJob.Abort(job,errors); if(errors.Count != 0) throw new System.AggregateException(errors); throw new System.InvalidOperationException("invalid setup unexpectedly succeeded"); }
    }
}
'@
    Add-Type -TypeDefinition $source -Language CSharp
    for($i=0;$i -le 40;$i++){
        if($i -eq 1){$self=[Diagnostics.Process]::GetCurrentProcess();$self.Refresh();$before=$self.HandleCount}
        try {[Kerminal.JobSetupFailureTest.FailureProbe]::Run();throw 'Invalid Job configuration succeeded.'}
        catch {if($_.Exception.GetBaseException() -isnot [ComponentModel.Win32Exception]){throw}}
    }
    $self.Refresh();$after=$self.HandleCount;$self.Dispose()
    if($after-$before -gt 3){throw "Job setup leaked handles: before=$before after=$after"}
    Write-Output 'PASS: failing Job configuration is closed and rejected'
}

function Test-PackageEntrypoint {
    # 只编译本地短命假 CLI，覆盖真实参数引用、日志及非零退出；不运行正式插件打包。
    # @author kongweiguang
    param([string]$Directory)
    $compiler=Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
    if(-not (Test-Path -LiteralPath $compiler -PathType Leaf)){throw 'The local C# test compiler is unavailable.'}
    $sourcePath=Join-Path $Directory 'fake-cli.cs'
    $exe=Join-Path $Directory 'fake cli.exe'
    $source=@'
using System;
using System.IO;
public static class FakeCli {
    // 严格匹配输入和输出，确保带空格路径没有被 Windows 命令行拆分。
    public static int Main(string[] args) {
        if (args.Length == 1 && args[0] == "--sleep-child") { System.Threading.Thread.Sleep(25000); return 0; }
        if (args.Length != 5 || args[0] != "package" || args[1] != "--dir" || args[3] != "--out") return 70;
        if (args[2] != Environment.GetEnvironmentVariable("KERMINAL_JOB_TEST_INPUT") || args[4] != Environment.GetEnvironmentVariable("KERMINAL_JOB_TEST_OUTPUT")) return 71;
        Console.WriteLine("fake cli stdout"); Console.Error.WriteLine("fake cli stderr");
        if (Environment.GetEnvironmentVariable("KERMINAL_JOB_TEST_FAIL") == "1") return 23;
        if (Environment.GetEnvironmentVariable("KERMINAL_JOB_TEST_ORPHAN") == "1") {
            Directory.CreateDirectory(args[4]);
            var info=Environment.GetEnvironmentVariable("KERMINAL_JOB_TEST_APPX") == "1"
                ? new System.Diagnostics.ProcessStartInfo(Environment.GetEnvironmentVariable("KERMINAL_JOB_TEST_PWSH"), "-NoLogo -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 25\"")
                : new System.Diagnostics.ProcessStartInfo(System.Reflection.Assembly.GetExecutingAssembly().Location, "--sleep-child");
            info.UseShellExecute=false; info.CreateNoWindow=true;
            var child=System.Diagnostics.Process.Start(info);
            File.WriteAllText(Path.Combine(args[4], "child.txt"),child.Id.ToString()+":"+child.StartTime.ToUniversalTime().Ticks.ToString()+":"+System.Diagnostics.Process.GetCurrentProcess().Id.ToString());
            return 0;
        }
        Directory.CreateDirectory(args[4]); File.WriteAllText(Path.Combine(args[4], "args.ok"), "ok"); return 0;
    }
}
'@
    [IO.File]::WriteAllText($sourcePath,$source)
    $start=[Diagnostics.ProcessStartInfo]::new()
    $start.FileName=$compiler;$start.UseShellExecute=$false;$start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true;$start.RedirectStandardError=$true
    foreach($argument in @('/nologo','/target:exe',('/out:'+$exe),$sourcePath)){$start.ArgumentList.Add($argument)}
    $compile=[Diagnostics.Process]::Start($start)
    $stdout=$compile.StandardOutput.ReadToEndAsync();$stderr=$compile.StandardError.ReadToEndAsync()
    try {
        if(-not $compile.WaitForExit(20000)){throw 'The local test compiler timed out.'}
        if($compile.ExitCode -ne 0){throw "The local test compiler failed: $($stdout.GetAwaiter().GetResult()) $($stderr.GetAwaiter().GetResult())"}
    } finally {if(-not $compile.HasExited){$compile.Kill();[void]$compile.WaitForExit(10000)};$compile.Dispose()}
    # 打包函数在收尾阶段调用共用清理函数；只抽取 Invoke-RestartPackage 会让测试缺少真实依赖。
    $source=Get-TestFunctionSource -Path (Join-Path $PSScriptRoot 'plugin-restart-native.ps1') -Name 'Close-OwnedRestartProcess'
    . ([scriptblock]::Create($source))
    $source=Get-TestFunctionSource -Path (Join-Path $PSScriptRoot 'plugin-restart-native.ps1') -Name 'Invoke-RestartPackage'
    . ([scriptblock]::Create($source))
    $inputPath=Join-Path $Directory 'fixture with space';[void][IO.Directory]::CreateDirectory($inputPath)
    $working=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
    $names=@('KERMINAL_JOB_TEST_INPUT','KERMINAL_JOB_TEST_OUTPUT','KERMINAL_JOB_TEST_FAIL','KERMINAL_JOB_TEST_ORPHAN','KERMINAL_JOB_TEST_APPX','KERMINAL_JOB_TEST_PWSH')
    $saved=@{};foreach($name in $names){$saved[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
    try {
        $env:KERMINAL_JOB_TEST_INPUT=$inputPath
        $env:KERMINAL_JOB_TEST_OUTPUT=Join-Path $Directory 'successful package'
        $env:KERMINAL_JOB_TEST_FAIL='0'
        $env:KERMINAL_JOB_TEST_ORPHAN='0';$env:KERMINAL_JOB_TEST_APPX='0'
        $env:KERMINAL_JOB_TEST_PWSH=Join-Path $PSHOME 'pwsh.exe'
        Invoke-RestartPackage -Cli $exe -WorkingDirectory $working -Base $Directory -Name 'successful package' -InputDirectory $inputPath -Timeout 10
        if(-not (Test-Path -LiteralPath (Join-Path $env:KERMINAL_JOB_TEST_OUTPUT 'args.ok'))){throw 'Fake CLI arguments were not accepted.'}
        if((Get-Content -Raw -LiteralPath (Join-Path $Directory 'successful package.log')) -notmatch 'fake cli stdout' -or (Get-Content -Raw -LiteralPath (Join-Path $Directory 'successful package.stderr.log')) -notmatch 'fake cli stderr'){throw 'Separate CLI logs are incomplete.'}
        $env:KERMINAL_JOB_TEST_OUTPUT=Join-Path $Directory 'failed package'
        $env:KERMINAL_JOB_TEST_FAIL='1'
        try {Invoke-RestartPackage -Cli $exe -WorkingDirectory $working -Base $Directory -Name 'failed package' -InputDirectory $inputPath -Timeout 10;throw 'A failed fake CLI was accepted.'}
        catch {if($_.Exception.Message -notmatch 'exitCode=23'){throw}}
        $env:KERMINAL_JOB_TEST_OUTPUT=Join-Path $Directory 'orphan package'
        $env:KERMINAL_JOB_TEST_FAIL='0';$env:KERMINAL_JOB_TEST_ORPHAN='1'
        $clock=[Diagnostics.Stopwatch]::StartNew()
        try {
            Invoke-RestartPackage -Cli $exe -WorkingDirectory $working -Base $Directory -Name 'orphan package' -InputDirectory $inputPath -Timeout 10
            $unexpected=([IO.File]::ReadAllText((Join-Path $env:KERMINAL_JOB_TEST_OUTPUT 'child.txt'))).Split(':')
            $details="elapsed=$($clock.Elapsed.TotalSeconds) child=$($unexpected[0])"
            try {
                $unexpectedProcess=[Diagnostics.Process]::GetProcessById([int]$unexpected[0])
                try {
                    if($unexpectedProcess.StartTime.ToUniversalTime().Ticks -eq [long]$unexpected[1]){
                        $details+=" live=$(-not $unexpectedProcess.HasExited) path=$($unexpectedProcess.MainModule.FileName)"
                        [void]$unexpectedProcess.WaitForExit(30000)
                    }
                } finally {$unexpectedProcess.Dispose()}
            } catch [ArgumentException] {$details+=' already exited'}
            throw "A live descendant was accepted as a normal exit: $details"
        } catch {if($_.Exception.Message -notmatch 'CLI packaging left live descendants'){throw}}
        $identity=([IO.File]::ReadAllText((Join-Path $env:KERMINAL_JOB_TEST_OUTPUT 'child.txt'))).Split(':')
        try {$survivor=[Diagnostics.Process]::GetProcessById([int]$identity[0])} catch [ArgumentException] {$survivor=$null}
        if($null -ne $survivor){
            try {if($survivor.StartTime.ToUniversalTime().Ticks -eq [long]$identity[1] -and -not $survivor.WaitForExit(0)){throw 'Packaging left its owned descendant running.'}} finally {$survivor.Dispose()}
        }
        # 系统托管应用的创建可能绕开 Job；负例只按精确句柄等待自然退出，不能据归零推断所有进程已回收。
        $env:KERMINAL_JOB_TEST_OUTPUT=Join-Path $Directory 'appx package';$env:KERMINAL_JOB_TEST_APPX='1'
        $desktop='KerminalRail-settings-test-'+[Guid]::NewGuid().ToString('N')
        $owner=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage($exe,$working,(Join-Path $Directory 'appx-cli'),(Join-Path $Directory 'appx.out'),(Join-Path $Directory 'appx.err'),$desktop,$inputPath,$env:KERMINAL_JOB_TEST_OUTPUT)
        try {
            if(-not $owner.WaitForExit(10000)){throw 'Diagnostic fake CLI did not exit.'}
            $info=([IO.File]::ReadAllText((Join-Path $env:KERMINAL_JOB_TEST_OUTPUT 'child.txt'))).Split(':')
            $child=[Diagnostics.Process]::GetProcessById([int]$info[0]);$null=$child.SafeHandle
            try {
                if($child.StartTime.ToUniversalTime().Ticks -ne [long]$info[1]){throw 'Diagnostic child identity changed.'}
                $job=$owner.GetJobSnapshot()
                try {$membership=[Kerminal.PluginRestartAcceptance.RestartHandle]::Open([int]$info[0],$owner);$member=$true;$membership.Dispose()}
                catch {
                    $cause=$_.Exception.GetBaseException()
                    if($cause -isnot [InvalidOperationException] -or -not $cause.Message.Contains('Successor escaped the owned Job')){throw}
                    $member=$false
                }
                $osParent=(Get-CimInstance Win32_Process -Filter "ProcessId = $($info[0])").ParentProcessId
                Write-Output "OBSERVED: installed pwsh child=$($info[0]) reportedCreator=$($info[2]) osParent=$osParent member=$member active=$($job.ActiveProcesses) total=$($job.TotalProcesses); Job counts cover members only"
                if($member){$owner.TerminateJob(1)}
                if(-not $child.WaitForExit(30000)){throw 'Diagnostic child did not exit in time.'}
            } finally {$child.Dispose()}
        } finally {
            try {if($owner.GetJobSnapshot().ActiveProcesses -gt 0){$owner.TerminateJob(1)};[void]$owner.WaitForExit(10000)} finally {$owner.Dispose()}
        }
    } finally {foreach($name in $names){[Environment]::SetEnvironmentVariable($name,$saved[$name],'Process')}}
    Write-Output 'PASS: real packaging entry point quotes arguments and reports failures'
}

function New-TestChildCommand {
    # 子进程只写入专属临时标识并短时休眠，不访问用户应用或输入桌面。
    # @author kongweiguang
    param([bool]$WithDescendant)
    $code=@'
$ErrorActionPreference='Stop'
$self=[Diagnostics.Process]::GetCurrentProcess()
$items=@(@{id=$self.Id;startTicks=$self.StartTime.ToUniversalTime().Ticks})
if($env:KERMINAL_ACCEPTANCE_JOB_TEST_DESCENDANT -eq '1'){
    $start=[Diagnostics.ProcessStartInfo]::new()
    $start.FileName=Join-Path $PSHOME 'pwsh.exe'
    $start.UseShellExecute=$false; $start.CreateNoWindow=$true
    foreach($argument in @('-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 25')){$start.ArgumentList.Add($argument)}
    $child=[Diagnostics.Process]::Start($start)
    $items+=@{id=$child.Id;startTicks=$child.StartTime.ToUniversalTime().Ticks}
}
$path=$env:KERMINAL_ACCEPTANCE_JOB_TEST_MARKER
[IO.File]::WriteAllText("$path.tmp",($items|ConvertTo-Json -Compress))
[IO.File]::Move("$path.tmp",$path)
Start-Sleep -Seconds 25
'@
    $env:KERMINAL_ACCEPTANCE_JOB_TEST_DESCENDANT=if($WithDescendant){'1'}else{'0'}
    $env:KERMINAL_ACCEPTANCE_JOB_TEST_COMMAND=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($code))
}

function Start-TestWorker {
    # 测试 worker 是本轮直接创建的唯一外层进程，超时只允许通过其保留句柄终止。
    # @author kongweiguang
    param([string]$WorkerCase,[string]$Directory)
    $start=[Diagnostics.ProcessStartInfo]::new()
    $start.FileName=Join-Path $PSHOME 'pwsh.exe'
    $start.UseShellExecute=$false; $start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true; $start.RedirectStandardError=$true
    foreach($argument in @('-NoLogo','-NoProfile','-NonInteractive','-File',$PSCommandPath,'-Case',$WorkerCase,'-Scratch',$Directory)){$start.ArgumentList.Add($argument)}
    $process=[Diagnostics.Process]::Start($start)
    return [pscustomobject]@{Process=$process;Output=$process.StandardOutput.ReadToEndAsync();Error=$process.StandardError.ReadToEndAsync()}
}

function Assert-TestWorkerExit {
    # 输出异步读取且等待有界，异常只回收本次 worker，不按名称或进程树猜测。
    # @author kongweiguang
    param($Worker,[int]$Milliseconds=30000)
    if(-not $Worker.Process.WaitForExit($Milliseconds)){throw 'The owned test worker timed out.'}
    if(-not $Worker.Output.Wait(10000) -or -not $Worker.Error.Wait(10000)){throw 'The test worker output did not close in time.'}
    $out=$Worker.Output.GetAwaiter().GetResult(); $err=$Worker.Error.GetAwaiter().GetResult()
    if($Worker.Process.ExitCode -ne 0){throw "Test worker failed ($($Worker.Process.ExitCode)): $out $err"}
    if($out){Write-Output $out.Trim()}
}

function Stop-TestWorker {
    # 仅终止自己启动的 worker；其 Job 的最后句柄关闭负责回收受试子代。
    # @author kongweiguang
    param($Worker)
    if($null -ne $Worker){
        try {if(-not $Worker.Process.HasExited){$Worker.Process.Kill();if(-not $Worker.Process.WaitForExit(10000)){throw 'The owned test worker did not exit.'}}}
        finally {$Worker.Process.Dispose()}
    }
}

function Open-TestIdentities {
    # 启动时间与 PID 同时匹配并保留原内核句柄，防止 PID 复用误判为受试进程。
    # @author kongweiguang
    param([string]$Path)
    $items=@(Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json)
    $result=[Collections.Generic.List[object]]::new()
    try {
        foreach($item in $items){
            $process=[Diagnostics.Process]::GetProcessById([int]$item.id)
            if($process.StartTime.ToUniversalTime().Ticks -ne [long]$item.startTicks){$process.Dispose();throw 'Test process identity changed.'}
            $null=$process.SafeHandle
            $result.Add($process)
        }
        return ,$result
    } catch {foreach($process in $result){$process.Dispose()};throw}
}

function Assert-StartupCleanupAggregate {
    # 主失败必须位于聚合首项，后续每个注入的失败都必须单独保留。
    # @author kongweiguang
    param($Record,[string]$Primary,[bool]$HasPrimary)
    $error=$Record.Exception
    while($null -ne $error -and $error -isnot [AggregateException]){$error=$error.InnerException}
    if($null -eq $error){throw 'Expected an aggregate containing startup and cleanup failures.'}
    $items=@($error.InnerExceptions)
    $expectedCount=if($HasPrimary){5}else{4}
    if($items.Count -ne $expectedCount){throw "Expected $expectedCount independent errors, received $($items.Count): $error"}
    if($HasPrimary -and -not $items[0].Message.Contains($Primary)){throw "Original startup error was not retained first: $($items[0])"}
    $offset=if($HasPrimary){1}else{0}
    $expected=@('关闭启动期间的标准输出句柄失败','终止启动失败的验收 Job 失败','关闭启动失败的验收 Job 失败','关闭启动失败的验收桌面失败')
    for($i=0;$i -lt $expected.Count;$i++){
        if(-not $items[$i+$offset].Message.Contains($expected[$i])){throw "Cleanup failure missing or out of order: $($expected[$i])"}
    }
}

function Assert-TestChildReaped {
    # 仅用测试创建的 PID 和启动时间核对，避免 PID 重用误判或触碰其他进程。
    # @author kongweiguang
    param([string]$Marker)
    if(-not (Test-Path -LiteralPath $Marker -PathType Leaf)){throw 'The test child identity was not captured.'}
    $identity=([IO.File]::ReadAllText($Marker)).Split(':')
    try {$process=[Diagnostics.Process]::GetProcessById([int]$identity[0])} catch [ArgumentException] {return}
    try {
        if($process.StartTime.ToUniversalTime().Ticks -eq [long]$identity[1] -and -not $process.WaitForExit(0)){throw 'The owned startup-failure child is still running.'}
    } finally {$process.Dispose()}
}

function Assert-InjectedStartFailure {
    # 意外成功时也只终止本轮返回的 owner，避免故障注入测试本身遗留进程。
    # @author kongweiguang
    param([string]$Cli,[string]$WorkingDirectory,[string]$Directory,[string]$Primary,[bool]$HasPrimary)
    $owner=$null
    try {
        $desktop='KerminalRail-settings-test-'+[Guid]::NewGuid().ToString('N')
        $owner=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage($Cli,$WorkingDirectory,(Join-Path $Directory 'fault'),(Join-Path $Directory 'fault.out'),(Join-Path $Directory 'fault.err'),$desktop,$Directory,$Directory)
        throw 'Injected cleanup failures were accepted as a successful launch.'
    } catch {Assert-StartupCleanupAggregate -Record $_ -Primary $Primary -HasPrimary $HasPrimary}
    finally {
        if($null -ne $owner){
            try {if($owner.GetJobSnapshot().ActiveProcesses -gt 0){$owner.TerminateJob(1)};if(-not $owner.WaitForExit(10000)){throw 'Unexpected test owner did not exit.'}}
            finally {$owner.Dispose()}
        }
    }
}

if($Case -ne 'all'){
    if(-not $Scratch){throw 'Worker scratch directory is required.'}
    Import-TestInitializer -File 'plugin-restart-native.ps1' -Name 'Initialize-RestartLauncher' -SyntheticCommand -PauseBeforeResume:($Case -ceq 'suspended-owner') -ForceJobSetupFailure:($Case -ceq 'setup-failure') -InjectCleanupFailures:($Case -ceq 'cleanup-failure')
    $cli=Join-Path $PSHOME 'pwsh.exe'
    $working=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
    $env:KERMINAL_ACCEPTANCE_JOB_TEST_MARKER=Join-Path $Scratch "$Case.json"
    New-TestChildCommand -WithDescendant ($Case -ceq 'owner')
    if($Case -ceq 'cleanup-failure'){
        # 各注入路径均先真实关闭测试资源，再返回模拟失败；主错误和全部后续错误都必须可见。
        $self=[Diagnostics.Process]::GetCurrentProcess()
        for($i=0;$i -le 5;$i++){
            # 先预热三条路径，再采样句柄，避免把首次加载 PowerShell/诊断程序集误报为泄漏。
            if($i -eq 1){$self.Refresh();$before=$self.HandleCount}
            $env:KERMINAL_ACCEPTANCE_JOB_TEST_AFTER_CREATE='0'
            Assert-InjectedStartFailure -Cli (Join-Path $Scratch 'missing.exe') -WorkingDirectory $working -Directory $Scratch -Primary 'CreateProcessW on the private desktop failed.' -HasPrimary $true
            $env:KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER=Join-Path $Scratch 'before-resume.txt'
            $env:KERMINAL_ACCEPTANCE_JOB_TEST_AFTER_CREATE='1'
            Assert-InjectedStartFailure -Cli $cli -WorkingDirectory $working -Directory $Scratch -Primary '测试：首线程恢复前启动失败' -HasPrimary $true
            Assert-TestChildReaped $env:KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER
            Remove-Item -LiteralPath $env:KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER -Force
            $env:KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER=Join-Path $Scratch 'cleanup-after-start.txt'
            $env:KERMINAL_ACCEPTANCE_JOB_TEST_AFTER_CREATE='0'
            Assert-InjectedStartFailure -Cli $cli -WorkingDirectory $working -Directory $Scratch -Primary '' -HasPrimary $false
            Assert-TestChildReaped $env:KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER
            Remove-Item -LiteralPath $env:KERMINAL_ACCEPTANCE_JOB_TEST_CREATED_MARKER -Force
        }
        $self.Refresh();$after=$self.HandleCount;$self.Dispose()
        if($after-$before -gt 3){throw "Injected cleanup failures leaked handles: before=$before after=$after"}
        Write-Output 'PASS: startup and all cleanup failures remain visible; owned resources are reaped'
        return
    }
    if($Case -ceq 'setup-failure'){
        $self=[Diagnostics.Process]::GetCurrentProcess()
        for($i=0;$i -le 20;$i++){
            if($i -eq 1){$self.Refresh();$before=$self.HandleCount}
            $owner=$null
            try {
                $desktop='KerminalRail-settings-test-'+[Guid]::NewGuid().ToString('N')
                $owner=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage($cli,$working,(Join-Path $Scratch 'bad-setup'),(Join-Path $Scratch 'bad-setup.out'),(Join-Path $Scratch 'bad-setup.err'),$desktop,$Scratch,$Scratch)
                throw 'A child started with an invalid Job configuration.'
            } catch {
                $cause=$_.Exception.GetBaseException()
                if($cause -isnot [ComponentModel.Win32Exception] -or -not $cause.Message.Contains('配置验收 Job 关闭即终止失败')){throw}
            } finally {
                if($null -ne $owner){try {if($owner.GetJobSnapshot().ActiveProcesses -gt 0){$owner.TerminateJob(1)};[void]$owner.WaitForExit(10000)} finally {$owner.Dispose()}}
            }
        }
        $self.Refresh();$after=$self.HandleCount;$self.Dispose()
        if($after-$before -gt 3 -or (Test-Path -LiteralPath $env:KERMINAL_ACCEPTANCE_JOB_TEST_MARKER)){throw 'Failed Job setup leaked handles or started a child.'}
        Write-Output 'PASS: failed Job setup prevents child creation and closes handles'
        return
    }
    if($Case -ceq 'startup-failure'){
        # 无效可执行文件使 CreateProcess 失败，反复检查专属 Job 与桌面句柄均被释放。
        $self=[Diagnostics.Process]::GetCurrentProcess()
        for($i=0;$i -le 20;$i++){
            if($i -eq 1){$self.Refresh();$before=$self.HandleCount}
            try {
                $desktop='KerminalRail-settings-test-'+[Guid]::NewGuid().ToString('N')
                $null=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage((Join-Path $Scratch 'missing.exe'),$working,(Join-Path $Scratch 'bad'),(Join-Path $Scratch 'bad.out'),(Join-Path $Scratch 'bad.err'),$desktop,$Scratch,$Scratch)
                throw 'A missing executable unexpectedly started.'
            } catch {if($_.Exception.GetBaseException() -isnot [ComponentModel.Win32Exception]){throw}}
        }
        $self.Refresh();$after=$self.HandleCount;$self.Dispose()
        if($after-$before -gt 3){throw "Failed startup leaked handles: before=$before after=$after"}
        Write-Output 'PASS: failed process startup closes owned handles'
        return
    }
    if($Case -ceq 'package-timeout'){
        # 同样先引入打包函数依赖的共用清理函数，避免合成超时用例因缺少定义而误报失败。
        $source=Get-TestFunctionSource -Path (Join-Path $PSScriptRoot 'plugin-restart-native.ps1') -Name 'Close-OwnedRestartProcess'
        . ([scriptblock]::Create($source))
        $source=Get-TestFunctionSource -Path (Join-Path $PSScriptRoot 'plugin-restart-native.ps1') -Name 'Invoke-RestartPackage'
        . ([scriptblock]::Create($source))
        $clock=[Diagnostics.Stopwatch]::StartNew()
        try {Invoke-RestartPackage -Cli $cli -WorkingDirectory $working -Base $Scratch -Name 'timeout' -InputDirectory $Scratch -Timeout 3;throw 'Synthetic packaging did not time out.'}
        catch {if($_.Exception.Message -notmatch 'CLI packaging timed out'){throw}}
        if($clock.Elapsed.TotalSeconds -gt 20){throw 'Synthetic packaging cleanup exceeded its bound.'}
        if(-not (Test-Path -LiteralPath $env:KERMINAL_ACCEPTANCE_JOB_TEST_MARKER)){throw 'Synthetic packaging child did not start.'}
        $identities=@(Get-Content -LiteralPath $env:KERMINAL_ACCEPTANCE_JOB_TEST_MARKER -Raw | ConvertFrom-Json)
        foreach($item in $identities){
            try {$survivor=[Diagnostics.Process]::GetProcessById([int]$item.id)} catch [ArgumentException] {continue}
            try {if($survivor.StartTime.ToUniversalTime().Ticks -eq [long]$item.startTicks -and -not $survivor.WaitForExit(0)){throw 'Timed-out CLI still runs.'}} finally {$survivor.Dispose()}
        }
        Write-Output 'PASS: packaging timeout terminates the exclusive Job'
        return
    }
    $desktop='KerminalRail-settings-test-'+[Guid]::NewGuid().ToString('N')
    if($Case -ceq 'suspended-owner'){
        $owner=$null
        try {
            $owner=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage($cli,$working,(Join-Path $Scratch 'suspended'),(Join-Path $Scratch 'suspended.out'),(Join-Path $Scratch 'suspended.err'),$desktop,$Scratch,$Scratch)
            throw 'The test runner was not killed while the child was suspended.'
        } finally {
            if($null -ne $owner){try {if($owner.GetJobSnapshot().ActiveProcesses -gt 0){$owner.TerminateJob(1)};[void]$owner.WaitForExit(10000)} finally {$owner.Dispose()}}
        }
    }
    $owner=[Kerminal.PluginRestartAcceptance.NativeDesktopLauncher]::StartPackage($cli,$working,(Join-Path $Scratch 'owner'),(Join-Path $Scratch 'owner.out'),(Join-Path $Scratch 'owner.err'),$desktop,$Scratch,$Scratch)
    try {
        $deadline=[Diagnostics.Stopwatch]::StartNew()
        while(-not (Test-Path -LiteralPath $env:KERMINAL_ACCEPTANCE_JOB_TEST_MARKER)){
            if($owner.WaitForExit(0)){throw 'Synthetic child exited before publishing identities.'}
            if($deadline.Elapsed.TotalSeconds -gt 10){throw 'Synthetic child startup timed out.'}
            Start-Sleep -Milliseconds 50
        }
        [IO.File]::WriteAllText((Join-Path $Scratch 'owner.ready'),'ready')
        Start-Sleep -Seconds 30
    } finally {
        try {if($owner.GetJobSnapshot().ActiveProcesses -gt 0){$owner.TerminateJob(1)};if(-not $owner.WaitForExit(10000)){throw 'Synthetic child did not exit.'}}
        finally {$owner.Dispose()}
    }
    return
}

$scratchRoot=Join-Path ([IO.Path]::GetTempPath()) ('kerminal-job-test-'+[Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($scratchRoot)
$worker=$null; $handles=$null
try {
    Import-TestWebViewLauncher
    foreach($item in @(
        @('plugin-webview-native.ps1','Initialize-PluginWebViewLauncher'),
        @('plugin-extensions-native.ps1','Initialize-ExtensionsLauncher'),
        @('plugin-list-performance.ps1','Initialize-PluginListLauncher'),
        @('plugin-performance.ps1','Initialize-PerformanceLauncher'),
        @('plugin-restart-native.ps1','Initialize-RestartLauncher')
    )){Import-TestInitializer -File $item[0] -Name $item[1]}
    Write-Output 'PASS: shared launcher and five AST consumers compile'
    Test-JobSetupFailure
    $packageTest=Join-Path $scratchRoot 'package-entry';[void][IO.Directory]::CreateDirectory($packageTest)
    Test-PackageEntrypoint -Directory $packageTest
    foreach($caseName in @('setup-failure','startup-failure','cleanup-failure','package-timeout')){
        $directory=Join-Path $scratchRoot $caseName;[void][IO.Directory]::CreateDirectory($directory)
        $worker=Start-TestWorker -WorkerCase $caseName -Directory $directory
        try {Assert-TestWorkerExit $worker} finally {Stop-TestWorker $worker;$worker=$null}
    }
    $directory=Join-Path $scratchRoot 'owner';[void][IO.Directory]::CreateDirectory($directory)
    $worker=Start-TestWorker -WorkerCase 'owner' -Directory $directory
    $clock=[Diagnostics.Stopwatch]::StartNew()
    while(-not (Test-Path -LiteralPath (Join-Path $directory 'owner.ready'))){
        if($worker.Process.HasExited){Assert-TestWorkerExit $worker;throw 'Owner worker exited before ready.'}
        if($clock.Elapsed.TotalSeconds -gt 20){throw 'Owner worker did not become ready.'}
        Start-Sleep -Milliseconds 50
    }
    $handles=Open-TestIdentities (Join-Path $directory 'owner.json')
    if($handles.Count -ne 2){throw 'Expected one child and one inherited descendant.'}
    foreach($process in $handles){if($process.WaitForExit(0)){throw 'Test child exited before owner termination.'}}
    $worker.Process.Kill()
    if(-not $worker.Process.WaitForExit(10000)){throw 'Owned runner did not stop.'}
    foreach($process in $handles){if(-not $process.WaitForExit(10000)){throw 'Runner termination left an owned descendant alive.'}}
    Write-Output 'PASS: forced runner termination reaps child and grandchild'
    foreach($process in $handles){$process.Dispose()};$handles=$null
    Stop-TestWorker $worker;$worker=$null

    $directory=Join-Path $scratchRoot 'suspended-owner';[void][IO.Directory]::CreateDirectory($directory)
    $worker=Start-TestWorker -WorkerCase 'suspended-owner' -Directory $directory
    $marker=Join-Path $directory 'suspended-owner.json'
    $clock=[Diagnostics.Stopwatch]::StartNew()
    while(-not (Test-Path -LiteralPath $marker)){
        if($worker.Process.HasExited){Assert-TestWorkerExit $worker;throw 'Suspended worker exited before publishing its child.'}
        if($clock.Elapsed.TotalSeconds -gt 20){throw 'Suspended worker did not publish its child.'}
        Start-Sleep -Milliseconds 50
    }
    $child=[Diagnostics.Process]::GetProcessById([int][IO.File]::ReadAllText($marker))
    $handles=[Collections.Generic.List[object]]::new();$handles.Add($child)
    $null=$child.SafeHandle
    if($child.StartTime.ToUniversalTime() -lt $worker.Process.StartTime.ToUniversalTime() -or $child.WaitForExit(0)){throw 'Suspended child identity or lifetime is invalid.'}
    $worker.Process.Kill()
    if(-not $worker.Process.WaitForExit(10000) -or -not $child.WaitForExit(10000)){throw 'Killing the runner left its suspended child alive.'}
    Write-Output 'PASS: forced runner termination reaps child before first resume'
} finally {
    if($null -ne $worker){Stop-TestWorker $worker}
    if($null -ne $handles){foreach($process in $handles){$process.Dispose()}}
    Remove-Item -LiteralPath $scratchRoot -Recurse -Force
}
