[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$RunDirectory,

    [string]$ReferenceDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# @author kongweiguang
$script:PageDefinitions = @(
    [pscustomobject]@{ Id = 'appearance'; Label = '界面外观' },
    [pscustomobject]@{ Id = 'terminal'; Label = '终端' },
    [pscustomobject]@{ Id = 'keywords'; Label = '关键词高亮' },
    [pscustomobject]@{ Id = 'suggestions'; Label = '命令提示' },
    [pscustomobject]@{ Id = 'sftp'; Label = 'SFTP' },
    [pscustomobject]@{ Id = 'external-launch'; Label = '外部启动' },
    [pscustomobject]@{ Id = 'mcp'; Label = 'MCP' },
    [pscustomobject]@{ Id = 'sync'; Label = '同步' },
    [pscustomobject]@{ Id = 'desktop'; Label = '桌面' },
    [pscustomobject]@{ Id = 'shortcuts'; Label = '快捷键列表' },
    [pscustomobject]@{ Id = 'about'; Label = '关于' }
)
$script:ThemeDefinitions = @(
    [pscustomobject]@{ Id = 'light'; Label = '浅色' },
    [pscustomobject]@{ Id = 'dark'; Label = '深色' },
    [pscustomobject]@{ Id = 'system'; Label = '跟随系统' }
)
$script:DisclosureDefinitions = @(
    [pscustomobject]@{ Id = 'terminal-renderer'; Label = '终端渲染' },
    [pscustomobject]@{ Id = 'sftp-advanced'; Label = 'SFTP 高级选项' },
    [pscustomobject]@{ Id = 'external-launch-compatibility'; Label = '外部启动兼容选项' },
    [pscustomobject]@{ Id = 'mcp-connection-info'; Label = 'MCP 连接信息' },
    [pscustomobject]@{ Id = 'desktop-notifications-advanced'; Label = '桌面通知高级选项' }
)

function ConvertTo-HtmlText {
    # 所有来自 JSON、参数或文件名的可见文本统一编码，保留原始值用于检查但不允许注入标记。
    # @author kongweiguang
    param([AllowNull()][object]$Value)

    if ($null -eq $Value) {
        return ''
    }
    return [Net.WebUtility]::HtmlEncode([string]$Value)
}

function Get-JsonProperty {
    # 以属性枚举读取 JSON 字段，缺字段时返回 null，避免旧 schema 或失败报告触发严格模式异常。
    # @author kongweiguang
    param(
        [AllowNull()][object]$InputObject,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($null -eq $InputObject) {
        return $null
    }
    $property = $InputObject.PSObject.Properties | Where-Object { $_.Name -ceq $Name } | Select-Object -First 1
    if ($null -eq $property) {
        return $null
    }
    return ,$property.Value
}

function Test-PositiveJsonNumber {
    # 以 invariant culture 验证 JSON 数值字段，异常类型或溢出值只会判失败，不中断索引生成。
    # @author kongweiguang
    param([AllowNull()][object]$Value)

    if ($null -eq $Value) {
        return $false
    }
    $number = 0.0
    $parsed = [double]::TryParse(
        [string]$Value,
        [Globalization.NumberStyles]::Float,
        [Globalization.CultureInfo]::InvariantCulture,
        [ref]$number
    )
    return $parsed -and $number -gt 0 -and -not [double]::IsInfinity($number) -and -not [double]::IsNaN($number)
}

function Add-CheckRow {
    # 收集脚本能够从结构化证据直接确认的状态，不将截图存在等同于功能验收通过。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][ValidateSet('pass', 'fail', 'unknown')][string]$State,
        [Parameter(Mandatory = $true)][string]$Detail
    )

    $Rows.Add([pscustomobject]@{
        Label = $Label
        State = $State
        Detail = $Detail
    })
}

function Get-StatusLabel {
    # 状态文案由受控枚举映射，避免从报告中读取任意 CSS class 或 UI 标签。
    # @author kongweiguang
    param([Parameter(Mandatory = $true)][ValidateSet('pass', 'fail', 'unknown')][string]$State)

    switch ($State) {
        'pass' { return '通过' }
        'fail' { return '失败' }
        default { return '未验证' }
    }
}

function Get-SafePngInfo {
    # 仅接受指定目录下的普通 PNG 文件并核对 PNG 签名；拒绝路径穿越、重解析文件和伪装扩展名。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$FileName
    )

    $invalid = [pscustomobject]@{ IsValid = $false; Href = ''; Width = 0; Height = 0 }
    if ([string]::IsNullOrWhiteSpace($FileName) -or
        [IO.Path]::GetFileName($FileName) -cne $FileName -or
        [IO.Path]::GetExtension($FileName) -cne '.png') {
        return $invalid
    }

    $filePath = Join-Path $Directory $FileName
    if (-not (Test-Path -LiteralPath $filePath -PathType Leaf)) {
        return $invalid
    }

    $file = Get-Item -LiteralPath $filePath -Force
    if (($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $file.Length -lt 24) {
        return $invalid
    }

    $stream = $null
    try {
        $stream = [IO.File]::OpenRead($file.FullName)
        $header = New-Object byte[] 24
        if ($stream.Read($header, 0, $header.Length) -ne $header.Length) {
            return $invalid
        }
    } catch {
        return $invalid
    } finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
    }

    $signature = [byte[]]@(137, 80, 78, 71, 13, 10, 26, 10)
    for ($index = 0; $index -lt $signature.Length; $index++) {
        if ($header[$index] -ne $signature[$index]) {
            return $invalid
        }
    }
    if ([Text.Encoding]::ASCII.GetString($header, 12, 4) -cne 'IHDR') {
        return $invalid
    }

    $width = [uint64]$header[16] * 16777216 + [uint64]$header[17] * 65536 + [uint64]$header[18] * 256 + [uint64]$header[19]
    $height = [uint64]$header[20] * 16777216 + [uint64]$header[21] * 65536 + [uint64]$header[22] * 256 + [uint64]$header[23]
    $indexUri = [Uri]::new((Join-Path $script:RunRoot 'index.html'))
    $imageUri = [Uri]::new($file.FullName)
    return [pscustomobject]@{
        IsValid = $true
        Href = $indexUri.MakeRelativeUri($imageUri).ToString()
        Width = $width
        Height = $height
    }
}

function Find-ReportRecord {
    # 只按已知标识匹配对象数组，缺失项由调用方显示为失败或未验证。
    # @author kongweiguang
    param(
        [AllowNull()][object]$Records,
        [Parameter(Mandatory = $true)][string]$PropertyName,
        [Parameter(Mandatory = $true)][string]$ExpectedValue
    )

    foreach ($record in @($Records)) {
        if ($null -ne $record -and [string](Get-JsonProperty -InputObject $record -Name $PropertyName) -ceq $ExpectedValue) {
            return $record
        }
    }
    return $null
}

function Add-CaptureCard {
    # 按预期文件名展示截图；没有结构化记录时可作为原始证据浏览，但明确不标成通过。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string]$Label,
        [AllowNull()][object]$CaptureRecord,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded
    )

    $recordFile = [string](Get-JsonProperty -InputObject $CaptureRecord -Name 'file')
    $recordMatches = $null -ne $CaptureRecord -and $recordFile -ceq $FileName
    $png = Get-SafePngInfo -Directory $Directory -FileName $FileName
    if (-not $ReportLoaded) {
        $state = 'unknown'
        $detail = if ($png.IsValid) { '截图文件存在；验收报告缺失，未验证。' } else { '验收报告缺失，且未找到有效 PNG。' }
    } elseif (-not $recordMatches) {
        $state = 'fail'
        $detail = '报告未将预期截图与此检查关联。'
    } elseif (-not $png.IsValid) {
        $state = 'fail'
        $detail = '报告登记的 PNG 缺失或文件签名无效。'
    } else {
        $state = 'pass'
        $detail = '报告登记与目录内 PNG 相符。'
    }
    Add-CheckRow -Rows $Rows -Label $Label -State $state -Detail $detail

    $safeLabel = ConvertTo-HtmlText $Label
    $safeState = ConvertTo-HtmlText (Get-StatusLabel $state)
    $stateClass = ConvertTo-HtmlText $state
    [void]$Builder.AppendLine('<figure class="capture-card">')
    [void]$Builder.AppendLine("<figcaption><span>$safeLabel</span><span class=`"badge $stateClass`">$safeState</span></figcaption>")
    if ($png.IsValid) {
        $href = ConvertTo-HtmlText $png.Href
        $alt = ConvertTo-HtmlText "$Label screenshot"
        [void]$Builder.AppendLine("<a class=`"image-link`" href=`"$href`" target=`"_blank`" rel=`"noopener`"><img loading=`"lazy`" src=`"$href`" alt=`"$alt`"></a>")
        [void]$Builder.AppendLine("<div class=`"image-meta`">$($png.Width) × $($png.Height) px · $(ConvertTo-HtmlText $FileName)</div>")
    } else {
        [void]$Builder.AppendLine('<div class="capture-missing">没有有效的目录内 PNG</div>')
    }
    [void]$Builder.AppendLine("<div class=`"capture-note`">$(ConvertTo-HtmlText $detail)</div>")
    [void]$Builder.AppendLine('</figure>')
}

function Add-ReferenceCard {
    # 参考图只能从用户明确指定的目录加载；并排展示不代表已证明 DPI、字体或像素环境一致。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $png = $null
    if (-not [string]::IsNullOrWhiteSpace($script:ReferenceRoot)) {
        $png = Get-SafePngInfo -Directory $script:ReferenceRoot -FileName $FileName
    }

    [void]$Builder.AppendLine('<figure class="capture-card reference-card">')
    [void]$Builder.AppendLine("<figcaption>$(ConvertTo-HtmlText $Label)</figcaption>")
    if ($null -ne $png -and $png.IsValid) {
        $href = ConvertTo-HtmlText $png.Href
        $alt = ConvertTo-HtmlText "$Label screenshot"
        [void]$Builder.AppendLine("<a class=`"image-link`" href=`"$href`" target=`"_blank`" rel=`"noopener`"><img loading=`"lazy`" src=`"$href`" alt=`"$alt`"></a>")
        [void]$Builder.AppendLine("<div class=`"image-meta`">$($png.Width) × $($png.Height) px · $(ConvertTo-HtmlText $FileName)</div>")
        [void]$Builder.AppendLine('<div class="capture-note">参考图仅供并排查看；同环境未验证，未执行 pixel diff。</div>')
    } else {
        [void]$Builder.AppendLine('<div class="capture-missing">无同环境参考图；未执行 pixel diff。</div>')
    }
    [void]$Builder.AppendLine('</figure>')
}

function Add-ReportCheckTable {
    # 状态汇总只输出白名单检查字段，不序列化或复制完整 report/config 内容。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows
    )

    [void]$Builder.AppendLine('<section class="report-section" id="checks"><h2>真实检查结果</h2>')
    [void]$Builder.AppendLine('<div class="table-scroll"><table><thead><tr><th>检查项</th><th>状态</th><th>观测结果</th></tr></thead><tbody>')
    foreach ($row in $Rows) {
        $label = ConvertTo-HtmlText $row.Label
        $detail = ConvertTo-HtmlText $row.Detail
        $status = ConvertTo-HtmlText (Get-StatusLabel $row.State)
        $stateClass = ConvertTo-HtmlText $row.State
        [void]$Builder.AppendLine("<tr><th>$label</th><td><span class=`"badge $stateClass`">$status</span></td><td>$detail</td></tr>")
    }
    if ($Rows.Count -eq 0) {
        [void]$Builder.AppendLine('<tr><td colspan="3">无可读取的检查结果。</td></tr>')
    }
    [void]$Builder.AppendLine('</tbody></table></div></section>')
}

function Add-PageGallery {
    # 每页固定展示三个主题的首屏和底部，以稳定文件命名组织而不从报告自定义路径生成链接。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [AllowNull()][object]$Report,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded,
        [Parameter(Mandatory = $true)][string]$RunRoot
    )

    [void]$Builder.AppendLine('<section class="report-section" id="pages"><h2>设置页面截图 · 11 页 × 3 主题</h2>')
    [void]$Builder.AppendLine('<p class="section-note">每页分别展示首屏与底部视口。截图存在只证明文件可查看，不单独代表交互或视觉验收通过。</p>')
    foreach ($pageDefinition in $script:PageDefinitions) {
        $pageId = [string]$pageDefinition.Id
        [void]$Builder.AppendLine("<article class=`"page-section`" id=`"page-$(ConvertTo-HtmlText $pageId)`"><h3>$(ConvertTo-HtmlText $pageDefinition.Label)</h3><div class=`"theme-grid`">")
        foreach ($themeDefinition in $script:ThemeDefinitions) {
            $themeId = [string]$themeDefinition.Id
            $themeRecord = Find-ReportRecord -Records (Get-JsonProperty $Report 'themes') -PropertyName 'requestedMode' -ExpectedValue $themeId
            $pageRecord = Find-ReportRecord -Records (Get-JsonProperty $themeRecord 'pages') -PropertyName 'pageId' -ExpectedValue $pageId
            $observedPage = [string](Get-JsonProperty $pageRecord 'observedPageId')
            $ready = Get-JsonProperty $pageRecord 'settingsReady'
            $navigation = [string](Get-JsonProperty $pageRecord 'navigationSelector')
            $pageState = if (-not $ReportLoaded) {
                'unknown'
            } elseif ($null -ne $pageRecord -and $ready -is [bool] -and $ready -and $observedPage -ceq $pageId -and $navigation -ceq "preferences-nav-$pageId") {
                'pass'
            } else {
                'fail'
            }
            $pageDetail = if ($null -eq $pageRecord) {
                '报告未提供此主题/页面记录。'
            } else {
                "expected page=$pageId; observed page=$observedPage; settingsReady=$ready; navigation=$navigation"
            }
            Add-CheckRow -Rows $Rows -Label "$($themeDefinition.Label) / $($pageDefinition.Label) 页面就绪" -State $pageState -Detail $pageDetail

            [void]$Builder.AppendLine("<section class=`"theme-column`"><h4>$(ConvertTo-HtmlText $themeDefinition.Label)</h4><div class=`"capture-pair`">")
            $firstFile = "settings-$themeId-$pageId.png"
            $bottomFile = "settings-$themeId-$pageId-bottom.png"
            Add-CaptureCard -Builder $Builder -Rows $Rows -Directory $RunRoot -FileName $firstFile -Label '首屏' -CaptureRecord (Get-JsonProperty $pageRecord 'capture') -ReportLoaded $ReportLoaded
            if ($themeId -eq 'dark') {
                Add-ReferenceCard -Builder $Builder -FileName "settings-dark-$pageId.png" -Label '深色参考图'
            }
            [void]$Builder.AppendLine('</div><div class="capture-pair">')
            Add-CaptureCard -Builder $Builder -Rows $Rows -Directory $RunRoot -FileName $bottomFile -Label '底部' -CaptureRecord (Get-JsonProperty $pageRecord 'bottomCapture') -ReportLoaded $ReportLoaded
            [void]$Builder.AppendLine('</div></section>')
        }
        [void]$Builder.AppendLine('</div></article>')
    }
    [void]$Builder.AppendLine('</section>')
}

function Add-ExpandedGallery {
    # 展开态由固定 disclosure 清单驱动，缺报告记录的截图即使存在也只作为未验证证据。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [AllowNull()][object]$Report,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded,
        [Parameter(Mandatory = $true)][string]$RunRoot
    )

    [void]$Builder.AppendLine('<section class="report-section" id="expanded"><h2>展开状态</h2><div class="evidence-grid">')
    $checks = Get-JsonProperty $Report 'disclosureChecks'
    foreach ($definition in $script:DisclosureDefinitions) {
        $check = Find-ReportRecord -Records $checks -PropertyName 'expected' -ExpectedValue "$($definition.Id):expanded"
        $passed = Get-JsonProperty $check 'passed'
        $observed = [string](Get-JsonProperty $check 'observed')
        $state = if (-not $ReportLoaded) { 'unknown' } elseif ($passed -is [bool] -and $passed -and $observed -ceq "$($definition.Id):expanded") { 'pass' } else { 'fail' }
        Add-CheckRow -Rows $Rows -Label "$($definition.Label) 展开检查" -State $state -Detail "expected=$($definition.Id):expanded; observed=$observed"
        $capture = Get-JsonProperty $check 'capture'
        $fileName = "settings-disclosure-system-$($definition.Id).png"
        Add-CaptureCard -Builder $Builder -Rows $Rows -Directory $RunRoot -FileName $fileName -Label $definition.Label -CaptureRecord $capture -ReportLoaded $ReportLoaded
    }
    [void]$Builder.AppendLine('</div></section>')
}

function Add-ColorGallery {
    # 颜色编辑和拾色器截图按专用文件名成组展示，报告清单之外的同名文件不能冒充已检查截图。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [AllowNull()][object]$Report,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded,
        [Parameter(Mandatory = $true)][string]$RunRoot
    )

    $colorCaptures = @(
        [pscustomobject]@{ File = 'settings-functional-keyword-colors.png'; Label = '自定义颜色编辑' },
        [pscustomobject]@{ File = 'settings-functional-keyword-color-picker.png'; Label = '颜色弹层打开' },
        [pscustomobject]@{ File = 'settings-functional-keyword-saved.png'; Label = '保存后的关键词规则' }
    )
    $reportCaptures = Get-JsonProperty $Report 'screenshots'
    [void]$Builder.AppendLine('<section class="report-section" id="colors"><h2>关键词颜色与弹层</h2><div class="evidence-grid">')
    foreach ($capture in $colorCaptures) {
        $record = Find-ReportRecord -Records $reportCaptures -PropertyName 'file' -ExpectedValue $capture.File
        Add-CaptureCard -Builder $Builder -Rows $Rows -Directory $RunRoot -FileName $capture.File -Label $capture.Label -CaptureRecord $record -ReportLoaded $ReportLoaded
    }
    [void]$Builder.AppendLine('</div></section>')
}

function Add-InteractionGallery {
    # schema 4 的同步截图保留成功、失败及恢复，旧 schema 不伪造缺失证据。
    # 补充展示语言下拉、弹框开关和密度检查截图，完整保留报告中与实际交互有关的额外画面。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [AllowNull()][object]$Report,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded,
        [Parameter(Mandatory = $true)][string]$RunRoot
    )

    $knownCaptures = @(
        [pscustomobject]@{ File = 'settings-opened.png'; Label = '设置弹框打开' },
        [pscustomobject]@{ File = 'settings-functional-language-select-open.png'; Label = '语言下拉打开' },
        [pscustomobject]@{ File = 'settings-functional-language-select-dismissed.png'; Label = 'Escape 关闭语言下拉' },
        [pscustomobject]@{ File = 'settings-density-spacious.png'; Label = '宽松密度回读' },
        [pscustomobject]@{ File = 'settings-density-compact.png'; Label = '紧凑密度回读' }
    )
    $reportCaptures = Get-JsonProperty $Report 'screenshots'
    if ([int](Get-JsonProperty $Report 'schemaVersion') -ge 4) {
        $knownCaptures += @(
            [pscustomobject]@{ File = 'settings-functional-sync-local-remote-roundtrip.png'; Label = '本地仓库同步成功' },
            [pscustomobject]@{ File = 'settings-functional-sync-failure-preserves-local.png'; Label = '远端不可用保留本地提交' },
            [pscustomobject]@{ File = 'settings-functional-sync-retry-confirms-remote.png'; Label = '恢复后同步回读' }
        )
    }
    [void]$Builder.AppendLine('<section class="report-section" id="interactions"><h2>其他交互截图</h2><div class="evidence-grid">')
    foreach ($capture in $knownCaptures) {
        $record = Find-ReportRecord -Records $reportCaptures -PropertyName 'file' -ExpectedValue $capture.File
        Add-CaptureCard -Builder $Builder -Rows $Rows -Directory $RunRoot -FileName $capture.File -Label $capture.Label -CaptureRecord $record -ReportLoaded $ReportLoaded
    }
    [void]$Builder.AppendLine('</div></section>')
}

function Add-ReportChecks {
    # Git 断言从原生回执读取，不能仅按同步按钮或页面存在判为闭环。
    # 保留 JSON 数组的原始形状供逐条核对；结果只取结构化验收证据，不读取 stdout、stderr 或配置文件。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Rows,
        [AllowNull()][object]$Report,
        [AllowNull()][object]$ProcessResult,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded,
        [Parameter(Mandatory = $true)][bool]$ProcessLoaded
    )

    if (-not $ProcessLoaded) {
        Add-CheckRow $Rows 'process-result.json' 'fail' '文件缺失或 JSON 无法读取。'
    } else {
        foreach ($item in @(
            [pscustomobject]@{ Property = 'processExited'; Label = '验收进程已退出'; Expected = $true },
            [pscustomobject]@{ Property = 'timedOut'; Label = '验收进程未超时'; Expected = $false },
            [pscustomobject]@{ Property = 'privateDesktopClosed'; Label = '隔离桌面已关闭'; Expected = $true }
        )) {
            $actual = Get-JsonProperty $ProcessResult $item.Property
            $state = if ($actual -isnot [bool]) { 'fail' } elseif ($actual -eq $item.Expected) { 'pass' } else { 'fail' }
            Add-CheckRow $Rows $item.Label $state "expected=$($item.Expected); observed=$actual"
        }
        $exitCode = Get-JsonProperty $ProcessResult 'exitCode'
        $parsedExitCode = 0
        $validExitCode = [int]::TryParse([string]$exitCode, [ref]$parsedExitCode)
        $exitState = if ($validExitCode -and $parsedExitCode -eq 0) { 'pass' } else { 'fail' }
        Add-CheckRow $Rows '验收进程退出码' $exitState "observed=$exitCode; exitCodeHex=$(Get-JsonProperty $ProcessResult 'exitCodeHex')"
        $desktopName = [string](Get-JsonProperty $ProcessResult 'desktopName')
        $desktopState = if ([string]::IsNullOrWhiteSpace($desktopName)) { 'fail' } else { 'pass' }
        Add-CheckRow $Rows '隔离桌面标识' $desktopState "desktopName=$desktopName"
    }

    if (-not $ReportLoaded) {
        Add-CheckRow $Rows 'report.json' 'fail' '报告缺失或 JSON 无法读取；本次设置检查结果未获验收，不得视为通过。'
        return
    }

    $schemaVersion = Get-JsonProperty $Report 'schemaVersion'
    $captureMode = [string](Get-JsonProperty $Report 'captureMode')
    $captureBackend = [string](Get-JsonProperty $Report 'captureBackend')
    $parsedSchemaVersion = 0
    $validSchemaVersion = [int]::TryParse([string]$schemaVersion, [ref]$parsedSchemaVersion)
    $reportState = if ($validSchemaVersion -and $parsedSchemaVersion -in @(3, 4) -and $captureMode -ceq 'hidden_native_window' -and $captureBackend -ceq 'gpui_window_render_to_image') { 'pass' } else { 'fail' }
    Add-CheckRow $Rows '报告 schema 与渲染方式' $reportState "schemaVersion=$schemaVersion; captureMode=$captureMode; captureBackend=$captureBackend"

    $windowVisible = Get-JsonProperty $Report 'windowVisible'
    $windowActive = Get-JsonProperty $Report 'windowActive'
    $windowState = if ($windowVisible -is [bool] -and -not $windowVisible) { 'pass' } else { 'fail' }
    Add-CheckRow $Rows '窗口保持隐藏' $windowState "windowVisible=$windowVisible; windowActive=$windowActive"
    $width = Get-JsonProperty $Report 'windowWidth'
    $height = Get-JsonProperty $Report 'windowHeight'
    $modalGeneration = Get-JsonProperty $Report 'settingsModalGeneration'
    $dimensionsState = if ((Test-PositiveJsonNumber $width) -and (Test-PositiveJsonNumber $height) -and (Test-PositiveJsonNumber $modalGeneration)) { 'pass' } else { 'fail' }
    Add-CheckRow $Rows '设置弹框生命周期与窗口尺寸' $dimensionsState "modalGeneration=$modalGeneration; viewport=${width}×${height}"

    $isolation = Get-JsonProperty $Report 'nativeIsolation'
    $platformSupported = Get-JsonProperty $isolation 'platformSupported'
    $isolationProven = Get-JsonProperty $isolation 'isolationProven'
    $threadDesktop = [string](Get-JsonProperty $isolation 'uiThreadDesktopName')
    $inputDesktop = [string](Get-JsonProperty $isolation 'inputDesktopName')
    $reportProcessId = Get-JsonProperty $isolation 'processId'
    $processDesktop = [string](Get-JsonProperty $ProcessResult 'desktopName')
    $processId = Get-JsonProperty $ProcessResult 'processId'
    $isolatedState = if ($platformSupported -is [bool] -and $platformSupported -and
        $isolationProven -is [bool] -and $isolationProven -and
        -not [string]::IsNullOrWhiteSpace($threadDesktop) -and
        $threadDesktop -ceq $processDesktop -and
        -not [string]::IsNullOrWhiteSpace($inputDesktop) -and
        $threadDesktop -cne $inputDesktop -and
        $null -ne $reportProcessId -and $null -ne $processId -and [string]$reportProcessId -ceq [string]$processId) { 'pass' } else { 'fail' }
    Add-CheckRow $Rows '原生隔离证明' $isolatedState "platformSupported=$platformSupported; isolationProven=$isolationProven; UI desktop=$threadDesktop; input desktop=$inputDesktop; report PID=$reportProcessId; process PID=$processId"
    $configDirectoryRemoved = Get-JsonProperty $ProcessResult 'configDirectoryRemoved'
    $cleanupState = if ($configDirectoryRemoved -isnot [bool]) {
        'unknown'
    } elseif ($configDirectoryRemoved) {
        'pass'
    } else {
        'fail'
    }
    $cleanupDetail = if ($configDirectoryRemoved -is [bool]) {
        "expected=True; observed=$configDirectoryRemoved"
    } else {
        'process-result.json 未记录 bool configDirectoryRemoved；未验证清理结果。'
    }
    Add-CheckRow $Rows '配置目录清理回执' $cleanupState $cleanupDetail

    $language = Get-JsonProperty $Report 'languageEscapeCheck'
    $languagePassed = Get-JsonProperty $language 'passed'
    $languageExpected = [string](Get-JsonProperty $language 'expectedLanguage')
    $languageObserved = [string](Get-JsonProperty $language 'observedLanguage')
    $languageFlags = @('firstEscapeClosedSelect', 'firstEscapePreservedModal', 'firstEscapePreservedLanguage', 'secondEscapeClosedModal', 'focusReturnedToTrigger', 'modalReopened')
    $languageState = if ($languagePassed -is [bool] -and $languagePassed -and $languageExpected -ceq $languageObserved) { 'pass' } else { 'fail' }
    foreach ($flagName in $languageFlags) {
        if ((Get-JsonProperty $language $flagName) -isnot [bool] -or -not (Get-JsonProperty $language $flagName)) {
            $languageState = 'fail'
        }
    }
    Add-CheckRow $Rows '语言下拉 Escape 与焦点返回' $languageState "expected language=$languageExpected; observed language=$languageObserved; checks=$($languageFlags -join ', ')"

    $themes = Get-JsonProperty $Report 'themes'
    foreach ($themeDefinition in $script:ThemeDefinitions) {
        $theme = Find-ReportRecord -Records $themes -PropertyName 'requestedMode' -ExpectedValue $themeDefinition.Id
        $persisted = [string](Get-JsonProperty $theme 'persistedMode')
        $themeState = if ($null -ne $theme -and $persisted -ceq $themeDefinition.Id) { 'pass' } else { 'fail' }
        $appearance = [string](Get-JsonProperty $theme 'windowAppearance')
        $effective = [string](Get-JsonProperty $theme 'effectiveTheme')
        Add-CheckRow $Rows "$($themeDefinition.Label) 主题持久化回读" $themeState "requested=$($themeDefinition.Id); persisted=$persisted; windowAppearance=$appearance; effectiveTheme=$effective"
        $pages = Get-JsonProperty $theme 'pages'
        if (@($pages).Count -ne $script:PageDefinitions.Count) {
            Add-CheckRow $Rows "$($themeDefinition.Label) 页面覆盖数量" 'fail' "expected=$($script:PageDefinitions.Count); observed=$(@($pages).Count)"
        }
    }

    if ($parsedSchemaVersion -ge 4) {
        $syncChecks = Get-JsonProperty $Report 'syncChecks'
        $syncActions = @('sync-local-remote-roundtrip', 'sync-failure-preserves-local', 'sync-retry-confirms-remote')
        foreach ($action in $syncActions) {
            $check = Find-ReportRecord -Records $syncChecks -PropertyName 'action' -ExpectedValue $action
            $expected = [string](Get-JsonProperty $check 'expected')
            $observed = [string](Get-JsonProperty $check 'observed')
            $passed = Get-JsonProperty $check 'passed'
            $state = if ($passed -is [bool] -and $passed -and $expected.Length -gt 0 -and $expected -ceq $observed) { 'pass' } else { 'fail' }
            Add-CheckRow $Rows "Git $action" $state "expected=$expected; observed=$observed"
        }
        if ($syncChecks.Count -ne 3) { Add-CheckRow $Rows 'Git 同步检查数量' 'fail' "expected=3; observed=$($syncChecks.Count)" }
    }

    $densityChecks = Get-JsonProperty $Report 'densityChecks'
    if ($null -eq $densityChecks) {
        $densityChecks = @()
    }
    if ($densityChecks.Count -ne 2) {
        Add-CheckRow $Rows '密度设置检查数量' 'fail' "expected=2; observed=$($densityChecks.Count)"
    }
    foreach ($check in $densityChecks) {
        $passed = Get-JsonProperty $check 'passed'
        $expected = [string](Get-JsonProperty $check 'expected')
        $observed = [string](Get-JsonProperty $check 'observed')
        $state = if ($passed -is [bool] -and $passed -and $expected -ceq $observed) { 'pass' } else { 'fail' }
        Add-CheckRow $Rows "密度设置 $expected" $state "expected=$expected; observed=$observed"
    }

    $disclosureChecks = Get-JsonProperty $Report 'disclosureChecks'
    if ($null -eq $disclosureChecks) {
        $disclosureChecks = @()
    }
    foreach ($definition in $script:DisclosureDefinitions) {
        $check = Find-ReportRecord -Records $disclosureChecks -PropertyName 'expected' -ExpectedValue "$($definition.Id):expanded"
        $passed = Get-JsonProperty $check 'passed'
        $observed = [string](Get-JsonProperty $check 'observed')
        $state = if ($passed -is [bool] -and $passed -and $observed -ceq "$($definition.Id):expanded") { 'pass' } else { 'fail' }
        Add-CheckRow $Rows "$($definition.Label) 展开检查" $state "expected=$($definition.Id):expanded; observed=$observed"
    }
    if ($disclosureChecks.Count -ne $script:DisclosureDefinitions.Count) {
        Add-CheckRow $Rows '展开检查数量' 'fail' "expected=$($script:DisclosureDefinitions.Count); observed=$($disclosureChecks.Count)"
    }

    $keywordCheck = Get-JsonProperty $Report 'keywordRuleCheck'
    $keywordPassed = Get-JsonProperty $keywordCheck 'passed'
    $keywordExpected = [string](Get-JsonProperty $keywordCheck 'expected')
    $keywordObserved = [string](Get-JsonProperty $keywordCheck 'observed')
    $keywordState = if ($keywordPassed -is [bool] -and $keywordPassed -and $keywordExpected -ceq $keywordObserved) { 'pass' } else { 'fail' }
    Add-CheckRow $Rows '关键词规则保存与取消回读' $keywordState "expected=$keywordExpected; observed=$keywordObserved"
}

function Write-SettingsReport {
    # 允许失败 run 生成可审阅索引；index.html 只覆盖自身，不改动原始 JSON、截图或配置目录。
    # @author kongweiguang
    param(
        [Parameter(Mandatory = $true)][string]$RunRoot,
        [AllowNull()][object]$Report,
        [AllowNull()][object]$ProcessResult,
        [Parameter(Mandatory = $true)][bool]$ReportLoaded,
        [Parameter(Mandatory = $true)][bool]$ProcessLoaded,
        [Parameter(Mandatory = $true)][string]$ReportReadStatus,
        [Parameter(Mandatory = $true)][string]$ProcessReadStatus
    )

    $rows = [Collections.Generic.List[object]]::new()
    Add-CheckRow $rows 'report.json 读取状态' $(if ($ReportLoaded) { 'pass' } else { 'fail' }) $ReportReadStatus
    Add-CheckRow $rows 'process-result.json 读取状态' $(if ($ProcessLoaded) { 'pass' } else { 'fail' }) $ProcessReadStatus
    Add-ReportChecks -Rows $rows -Report $Report -ProcessResult $ProcessResult -ReportLoaded $ReportLoaded -ProcessLoaded $ProcessLoaded

    $body = [Text.StringBuilder]::new()
    Add-PageGallery -Builder $body -Rows $rows -Report $Report -ReportLoaded $ReportLoaded -RunRoot $RunRoot
    Add-ExpandedGallery -Builder $body -Rows $rows -Report $Report -ReportLoaded $ReportLoaded -RunRoot $RunRoot
    Add-ColorGallery -Builder $body -Rows $rows -Report $Report -ReportLoaded $ReportLoaded -RunRoot $RunRoot
    Add-InteractionGallery -Builder $body -Rows $rows -Report $Report -ReportLoaded $ReportLoaded -RunRoot $RunRoot

    $failCount = @($rows | Where-Object { $_.State -eq 'fail' }).Count
    $unknownCount = @($rows | Where-Object { $_.State -eq 'unknown' }).Count
    $passCount = @($rows | Where-Object { $_.State -eq 'pass' }).Count
    $runStatus = if ($ReportLoaded -and $ProcessLoaded -and $failCount -eq 0 -and $unknownCount -eq 0) { 'PASS' } else { 'FAIL' }
    $statusClass = if ($runStatus -eq 'PASS') { 'pass' } else { 'fail' }
    $runName = ConvertTo-HtmlText (Split-Path -Leaf $RunRoot)
    $headline = if ($ReportLoaded) {
        '状态根据 report.json 的检查字段和 process-result.json 回执计算。截图文件存在本身不会变成检查通过。'
    } else {
        'report.json 缺失或无法读取。本次验收明确失败；可浏览的 PNG 只作为原始截图，不代表检查通过。'
    }

    $html = [Text.StringBuilder]::new()
    [void]$html.AppendLine('<!doctype html>')
    [void]$html.AppendLine('<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">')
    [void]$html.AppendLine('<title>Kerminal Settings Acceptance</title>')
    [void]$html.AppendLine(@'
<style>
:root{color-scheme:light dark;font-family:Segoe UI,system-ui,sans-serif;background:#f4f6f8;color:#18212b}*{box-sizing:border-box}body{margin:0}main{max-width:1600px;margin:0 auto;padding:28px}.top{position:sticky;top:0;z-index:5;background:rgba(244,246,248,.96);backdrop-filter:blur(12px);border-bottom:1px solid #c9d2dc;padding:18px 28px;margin:-28px -28px 24px}.top h1{font-size:24px;margin:0 0 8px}.top p{margin:6px 0;color:#536170}.badge{display:inline-block;border-radius:4px;padding:3px 8px;font-size:12px;font-weight:700}.badge.pass{background:#d8f2e7;color:#14583d}.badge.fail{background:#ffe0dd;color:#8a201a}.badge.unknown{background:#e6e9ed;color:#46515e}.summary{display:flex;gap:16px;align-items:center;flex-wrap:wrap}.summary strong{font-size:18px}.report-section{margin:28px 0}.report-section h2{font-size:20px;margin:0 0 10px}.section-note,.capture-note{color:#536170;font-size:13px}.page-section{margin:24px 0;border-top:1px solid #ccd4dd;padding-top:16px}.page-section h3{font-size:17px;margin:0 0 12px}.theme-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px}.theme-column{min-width:0;background:#e9edf1;border:1px solid #cbd3dc;border-radius:6px;padding:12px}.theme-column h4{margin:0 0 10px;font-size:15px}.capture-pair{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:8px;margin-bottom:8px}.evidence-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:12px}.capture-card{min-width:0;margin:0;background:#fff;border:1px solid #cbd3dc;border-radius:5px;padding:8px}.capture-card figcaption{display:flex;justify-content:space-between;align-items:center;gap:6px;font-size:13px;font-weight:650;margin-bottom:7px}.image-link{display:block;background:#111;line-height:0}.capture-card img{display:block;width:100%;height:auto;max-height:480px;object-fit:contain}.image-meta{font-size:11px;color:#536170;margin-top:5px;overflow-wrap:anywhere}.capture-note{margin-top:5px}.capture-missing{min-height:100px;display:grid;place-items:center;background:#f0f2f4;color:#6a4550;text-align:center;padding:12px;font-size:13px}.reference-card{border-style:dashed}.table-scroll{overflow:auto;border:1px solid #cbd3dc;border-radius:5px;background:#fff}table{width:100%;border-collapse:collapse;font-size:13px}th,td{text-align:left;padding:8px 10px;border-bottom:1px solid #e1e5e9;vertical-align:top}thead{background:#e9edf1}tbody tr:last-child th,tbody tr:last-child td{border-bottom:0}td{overflow-wrap:anywhere}footer{border-top:1px solid #cbd3dc;padding:16px 0;color:#536170;font-size:12px}@media(prefers-color-scheme:dark){:root{background:#171c22;color:#e6edf3}.top{background:rgba(23,28,34,.96);border-color:#39434e}.top p,.section-note,.capture-note,.image-meta,footer{color:#a9b5c2}.page-section{border-color:#39434e}.theme-column{background:#202831;border-color:#39434e}.capture-card,.table-scroll{background:#202831;border-color:#39434e}thead{background:#29333e}th,td{border-color:#39434e}.capture-missing{background:#252e38;color:#ffc7c2}.badge.pass{background:#173f31;color:#a5e8c4}.badge.fail{background:#512b2c;color:#ffb7b1}.badge.unknown{background:#303942;color:#c5ced7}}@media(max-width:960px){main{padding:16px}.top{margin:-16px -16px 18px;padding:14px 16px}.theme-grid{grid-template-columns:1fr}.evidence-grid{grid-template-columns:repeat(2,minmax(0,1fr))}.capture-pair{grid-template-columns:repeat(2,minmax(0,1fr))}}@media(max-width:560px){.evidence-grid,.capture-pair{grid-template-columns:1fr}.capture-card img{max-height:360px}}
</style></head><body><main>
'@)
    [void]$html.AppendLine('<header class="top">')
    [void]$html.AppendLine("<h1>Kerminal GPUI 设置验收 · $runName</h1>")
    [void]$html.AppendLine("<div class=`"summary`"><strong class=`"badge $statusClass`">Run $runStatus</strong><span>通过 $passCount</span><span>失败 $failCount</span><span>未验证 $unknownCount</span></div>")
    [void]$html.AppendLine("<p>$(ConvertTo-HtmlText $headline)</p>")
    [void]$html.AppendLine("<p>检查来源：report.json = $(ConvertTo-HtmlText $ReportReadStatus)；process-result.json = $(ConvertTo-HtmlText $ProcessReadStatus)。配置文件与 stdout/stderr 内容不会嵌入此索引。</p>")
    [void]$html.AppendLine('</header>')
    Add-ReportCheckTable -Builder $html -Rows $rows
    [void]$html.Append($body.ToString())
    [void]$html.AppendLine('<footer>参考图片仅按文件名并列；未确认相同窗口尺寸、DPI、字体和主题运行环境时，不计算或宣称 pixel diff。</footer>')
    [void]$html.AppendLine('</main></body></html>')

    $indexPath = Join-Path $RunRoot 'index.html'
    [IO.File]::WriteAllText($indexPath, $html.ToString(), [Text.UTF8Encoding]::new($false))
    return [pscustomobject]@{ Path = $indexPath; RunStatus = $runStatus; PassCount = $passCount; FailCount = $failCount; UnknownCount = $unknownCount }
}

if (-not (Test-Path -LiteralPath $RunDirectory -PathType Container)) {
    throw "RunDirectory must name an existing settings-parity run directory: $RunDirectory"
}
$script:RunRoot = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $RunDirectory).Path)
$script:ReferenceRoot = ''
if (-not [string]::IsNullOrWhiteSpace($ReferenceDirectory)) {
    if (-not (Test-Path -LiteralPath $ReferenceDirectory -PathType Container)) {
        throw "ReferenceDirectory must name an existing directory: $ReferenceDirectory"
    }
    $script:ReferenceRoot = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $ReferenceDirectory).Path)
}

$reportPath = Join-Path $script:RunRoot 'report.json'
$processPath = Join-Path $script:RunRoot 'process-result.json'
$report = $null
$processResult = $null
$reportLoaded = $false
$processLoaded = $false
$reportReadStatus = '文件缺失。'
$processReadStatus = '文件缺失。'
if (Test-Path -LiteralPath $reportPath -PathType Leaf) {
    try {
        $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json -ErrorAction Stop
        $reportLoaded = $null -ne $report
        $reportReadStatus = if ($reportLoaded) { 'JSON 已读取。' } else { 'JSON 内容为空或为 null。' }
    } catch {
        $reportReadStatus = '文件存在但无法解析为 JSON。'
    }
}
if (Test-Path -LiteralPath $processPath -PathType Leaf) {
    try {
        $processResult = Get-Content -LiteralPath $processPath -Raw | ConvertFrom-Json -ErrorAction Stop
        $processLoaded = $null -ne $processResult
        $processReadStatus = if ($processLoaded) { 'JSON 已读取。' } else { 'JSON 内容为空或为 null。' }
    } catch {
        $processReadStatus = '文件存在但无法解析为 JSON。'
    }
}

$result = Write-SettingsReport `
    -RunRoot $script:RunRoot `
    -Report $report `
    -ProcessResult $processResult `
    -ReportLoaded $reportLoaded `
    -ProcessLoaded $processLoaded `
    -ReportReadStatus $reportReadStatus `
    -ProcessReadStatus $processReadStatus
Write-Output "Run status: $($result.RunStatus)"
Write-Output "HTML index: $($result.Path)"
Write-Output "Checks: passed=$($result.PassCount), failed=$($result.FailCount), unverified=$($result.UnknownCount)"
