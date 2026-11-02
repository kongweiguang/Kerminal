# @author kongweiguang
# 保留用户 profile 与 prompt；仅在默认 Enter handler 可安全包装时公布 shell integration。

if ($global:__kerminal_shell_integration_active) {
    return
}

# PSReadLine 2.0.0 没有查询用的 Chord 参数；按真实能力选择查询，不隐藏参数绑定错误。
$getKeyHandler = Get-Command Get-PSReadLineKeyHandler -ErrorAction SilentlyContinue
if (-not $getKeyHandler) {
    return
}
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

# .NET Framework 仍保留 RFC2396 的五个标点；统一编码为协议接受的 RFC3986 子集。
# @author kongweiguang
function global:ConvertTo-KerminalEditingField([string]$Value) {
    return [Uri]::EscapeDataString($Value).Replace('!', '%21').Replace("'", '%27').Replace('(', '%28').Replace(')', '%29').Replace('*', '%2A')
}

# 专用组合键只在未绑定时安装；真实行由 PSReadLine 读取，不修改光标、正文或预测设置。
# @author kongweiguang
$global:__kerminal_edit_nonce = '__KERMINAL_EDIT_NONCE__'
$global:__kerminal_edit_pid = $PID
$editingChord = 'Ctrl+Alt+Shift+F12'
$editingHandler = @(if ($getKeyHandler.Parameters.ContainsKey('Chord')) {
    & $getKeyHandler -Chord $editingChord
} else {
    & $getKeyHandler | Where-Object { $_.Key -match 'Ctrl' -and $_.Key -match 'Alt' -and $_.Key -match 'Shift' -and $_.Key -match 'F12$' }
})
$editingInstalled = 0
if ($editingHandler.Count -eq 0) {
    Set-PSReadLineKeyHandler -Chord $editingChord -ScriptBlock {
        if ($PID -ne $global:__kerminal_edit_pid -or -not $global:__kerminal_prompt_ready) { return }
        $line = ''
        $cursor = 0
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        if ($line.Length -gt 4000) {
            $tooLong = $line.Length -gt 8000
            if (-not $tooLong) {
                # PSReadLine 的 Length 是 UTF-16，配对代理项各自只占一个 Unicode 标量。
                $pairs = [regex]::Matches($line, '[\uD800-\uDBFF][\uDC00-\uDFFF]').Count
                $tooLong = ($line.Length - $pairs) -gt 4000
            }
            if ($tooLong) {
                [Console]::Write("$([char]27)]6977;Kerminal;1;R;$global:__kerminal_edit_nonce;powershell;input_too_long;$(ConvertTo-KerminalEditingField $PSVersionTable.PSVersion.ToString())`a")
                return
            }
        }
        $directory = '-'
        if ($PWD.Provider.Name -eq 'FileSystem') { $directory = ConvertTo-KerminalEditingField $PWD.ProviderPath }
        $prediction = '?'
        $readLineOptions = Get-PSReadLineOption
        if (-not $readLineOptions.PSObject.Properties['PredictionSource'] -or [string]$readLineOptions.PredictionSource -eq 'None') { $prediction = '0' }
        [Console]::Write("$([char]27)]6977;Kerminal;1;S;$global:__kerminal_edit_nonce;powershell;utf16;$cursor;$(ConvertTo-KerminalEditingField $PSVersionTable.PSVersion.ToString());$directory;$(ConvertTo-KerminalEditingField $line);$prediction`a")
    } -BriefDescription 'KerminalReadEditingSnapshot' -LongDescription 'Read the current line without editing or executing it'
    $editingInstalled = 1
}
[Console]::Write("$([char]27)]6977;Kerminal;1;H;$global:__kerminal_edit_nonce;powershell;$editingInstalled;$(ConvertTo-KerminalEditingField $PSVersionTable.PSVersion.ToString())`a")

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
