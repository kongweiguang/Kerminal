# @author kongweiguang
# 保留用户 profile 与 prompt；仅在默认 Enter handler 可安全包装时公布 shell integration。

if ($global:__kerminal_shell_integration_active) {
    return
}

# PSReadLine 2.0.0 没有查询用的 Chord 参数；按真实能力选择查询，不隐藏参数绑定错误。
$getKeyHandler = Get-Command Get-PSReadLineKeyHandler
$handler = @(if ($getKeyHandler.Parameters.ContainsKey('Chord')) {
    & $getKeyHandler -Chord Enter
} else {
    & $getKeyHandler | Where-Object { $_.Key -eq 'Enter' }
})
if ($handler.Count -ne 1 -or $handler[0].Function -ne 'AcceptLine') {
    return
}

$global:__kerminal_shell_integration_active = $true
$global:__kerminal_prompt_ready = $false
$global:__kerminal_command_active = $false
$global:__kerminal_original_prompt = ${function:prompt}

# Enter 只包装已确认的默认绑定；显式 ESC 字符兼容不支持 `e 的 Windows PowerShell 5.1。
# @author kongweiguang
Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
    if ($global:__kerminal_prompt_ready -and -not $global:__kerminal_command_active) {
        [Console]::Write("$([char]27)]133;C`a")
        $global:__kerminal_prompt_ready = $false
        $global:__kerminal_command_active = $true
    }
    [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
} -BriefDescription 'KerminalAcceptLine' -LongDescription 'Emit OSC 133 before accepting an interactive command'

# 先冻结上一命令状态；目录使用 .NET URI 编码且只接受 FileSystem provider，不执行额外命令。
# 失败时 LASTEXITCODE 可能属于更早的 native 命令；无法归属的退出码保持 Unknown，不改写用户全局状态。
# @author kongweiguang
function global:prompt {
    $commandSucceeded = $?
    $promptText = [string](& $global:__kerminal_original_prompt)
    $prefix = ''

    if ($global:__kerminal_command_active) {
        if ($commandSucceeded) {
            $prefix = "$([char]27)]133;D;0`a"
        } else {
            $prefix = "$([char]27)]133;D`a"
        }
        $global:__kerminal_command_active = $false
    }

    if ($PWD.Provider.Name -eq 'FileSystem') {
        $directory = $PWD.ProviderPath.Replace('\', '/')
        if ($directory -match '^[A-Za-z]:/') {
            $directory = '/' + $directory
        }
        $encodedDirectory = [Uri]::EscapeDataString($directory).Replace('%2F', '/')
        $prefix += "$([char]27)]7;file://$encodedDirectory`a"
    }

    $global:__kerminal_prompt_ready = $true
    return $prefix + "$([char]27)]133;A`a" + $promptText + "$([char]27)]133;B`a"
}
