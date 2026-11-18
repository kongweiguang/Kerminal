#requires -Version 7.0
# @author kongweiguang
<#
.SYNOPSIS
验证 Kerminal Windows 安装验收所需的正式资产并按固定摘要暂存。

.DESCRIPTION
只读取 GitHub Actions 已签名候选 artifact 或既有 Release 资产，核对仓库、tag 提交、
候选运行结论、候选 head_sha、SHA256SUMS 和安装器摘要后，把两个安装器复制到独立暂存目录。
脚本不创建、修改、发布 Release，不读取或输出任何令牌和签名私钥。

.PARAMETER AssetSource
candidate-artifact 使用 actions:read 下载签名候选；draft-release 读取既有 Release 资产。

.PARAMETER DownloadRoot
每次运行新建的空下载目录，只存放本次网络下载内容。

.PARAMETER StagingRoot
与下载目录分离的空暂存目录；安装器验证后复制到这里，后续 harness 只从这里运行安装器。

.PARAMETER EvidenceRoot
证据目录；写入 provenance.json、摘要和两份小型哈希清单。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('candidate-artifact', 'draft-release')]
    [string]$AssetSource,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedRepository,

    [Parameter(Mandatory = $true)]
    [string]$ReleaseTag,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string]$ExpectedInstallerSha256,

    [Parameter(Mandatory = $true)]
    [string]$OldReleaseTag,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string]$OldInstallerSha256,

    [Parameter(Mandatory = $true)]
    [string]$DownloadRoot,

    [Parameter(Mandatory = $true)]
    [string]$StagingRoot,

    [Parameter(Mandatory = $true)]
    [string]$EvidenceRoot,

    [string]$CandidateRunId = '44341234343'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# gh 以 UTF-8 写 stdout；Windows 控制台默认 OEM 代码页会把含非 ASCII 的 JSON 解码坏。
# 在调用任何 gh 前统一输出编码，保证 API 与 Release 描述里的中文不会破坏 JSON 解析。
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Text.UTF8Encoding]::new($false)

$script:ExpectedInstallerHash = $ExpectedInstallerSha256.ToLowerInvariant()
$script:OldInstallerHash = $OldInstallerSha256.ToLowerInvariant()

function Write-JsonFile {
    <#
    统一以无 BOM UTF-8 写 JSON，避免 Windows 默认编码让下游解析把 BOM 当正文。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][object]$Value
    )

    # InputObject 保证空数组也写成 []，而不会被管道展开成空文件。
    $json = ConvertTo-Json -InputObject $Value -Depth 20
    [IO.File]::WriteAllText($Path, $json + "`n", [Text.UTF8Encoding]::new($false))
}

function Assert-True {
    <#
    用类型化失败替代静默继续；安装验收的任何来源不一致都必须阻断后续安装。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Invoke-GhText {
    <#
    只通过 gh 的 argv 传参，不拼接 shell 字符串；失败时只回显参数类型，避免把令牌带进日志。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $output = & gh @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "gh 调用失败（退出码 $LASTEXITCODE）：gh $($Arguments -join ' ')"
    }
    return (($output | Out-String).Trim())
}

function Invoke-GhJson {
    <#
    gh JSON 输出统一在这里解析；空白输出按失败处理，避免把空对象当成有效来源。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $text = Invoke-GhText -Arguments $Arguments
    Assert-True (-not [string]::IsNullOrWhiteSpace($text)) "gh 返回空 JSON：gh $($Arguments -join ' ')"
    return ($text | ConvertFrom-Json -Depth 100)
}

function New-EmptyDirectory {
    <#
    下载、暂存和证据必须来自本次运行；发现非空目录就拒绝复用，避免旧文件混入验收。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    $full = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $full) {
        $items = @(Get-ChildItem -LiteralPath $full -Force)
        Assert-True ($items.Count -eq 0) "目录必须为空，拒绝复用：$full"
    } else {
        [void](New-Item -ItemType Directory -Path $full -Force)
    }
    return $full
}

function New-EvidenceDirectory {
    <#
    证据目录允许工作流预先存在，但脚本自己的文件仍写入独立的证据根，不覆盖他人记录。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    $full = [IO.Path]::GetFullPath($Path)
    if (-not (Test-Path -LiteralPath $full)) {
        [void](New-Item -ItemType Directory -Path $full -Force)
    }
    return $full
}

function Test-PathUnder {
    <#
    用规范化绝对路径判断嵌套关系，防止把安装器或证据写进下载目录而混淆来源。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root
    )

    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $candidate = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
    $container = [IO.Path]::GetFullPath($Root).TrimEnd($separators)
    return $candidate.StartsWith(
        $container + [IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase
    )
}

function Assert-SeparateDirectories {
    <#
    下载、暂存和证据不能互相嵌套；可信 harness 与下载内容分离是本次验收的硬边界。

    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Download,
        [Parameter(Mandatory = $true)][string]$Staging,
        [Parameter(Mandatory = $true)][string]$Evidence
    )

    foreach ($pair in @(
            @($Download, $Staging, '下载目录与暂存目录'),
            @($Download, $Evidence, '下载目录与证据目录'),
            @($Staging, $Evidence, '暂存目录与证据目录')
        )) {
        Assert-True (-not (Test-PathUnder -Path $pair[0] -Root $pair[1])) "$($pair[2])不能嵌套：$($pair[0]) 位于 $($pair[1]) 内"
        Assert-True (-not (Test-PathUnder -Path $pair[1] -Root $pair[0])) "$($pair[2])不能嵌套：$($pair[1]) 位于 $($pair[0]) 内"
    }
}

function Get-Sha256Hex {
    <#
    只使用文件字节摘要判断资产身份，不信任文件名、大小或 Release 页面的文字说明。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-FileHash {
    <#
    下载完成后再次按本地字节验摘要，覆盖传输截断或缓存替换并为后续安装提供原件证明。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$Label
    )

    Assert-True (Test-Path -LiteralPath $Path -PathType Leaf) "$Label 不存在：$Path"
    $actual = Get-Sha256Hex -Path $Path
    Assert-True ($actual -eq $ExpectedSha256.ToLowerInvariant()) "$Label SHA-256 不匹配：期望 $ExpectedSha256，实际 $actual"
    return $actual
}

function Assert-ReleaseAssetDigest {
    <#
    Release 资产的 digest 由 GitHub 记录；先核对平台记录再下载，可发现资产被替换或指向了错误文件。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][object]$Asset,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$Label
    )

    Assert-True ([string]$Asset.state -eq 'uploaded') "$Label 资产状态不是 uploaded"
    $digest = [string]$Asset.digest
    Assert-True (-not [string]::IsNullOrWhiteSpace($digest)) "$Label 缺少 GitHub 资产 digest"
    Assert-True ($digest -ieq "sha256:$ExpectedSha256") "$Label GitHub digest 不匹配：$digest"
}

function Get-PropertyValue {
    <#
    StrictMode 下访问不存在的属性会抛错；产物清单的 app-bundle 行没有 githubAssetName，
    统一返回 null 才能按名称筛选而不是中断验证。
    @author kongweiguang
    #>
    param($Object, [string]$Name)

    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Read-ReleaseChecksums {
    <#
    SHA256SUMS.json 是发布链内部的固定摘要清单；只接受其中与安装器同名的唯一行。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$AssetName,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256
    )

    Assert-True (Test-Path -LiteralPath $Path -PathType Leaf) "缺少 SHA256SUMS.json：$Path"
    $sums = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json -Depth 20
    Assert-True ([string]$sums.version -eq $Version) "SHA256SUMS.json 版本不是 $Version"
    $rows = @($sums.artifacts | Where-Object {
            (Get-PropertyValue -Object $_ -Name 'githubAssetName') -eq $AssetName -or
            (Get-PropertyValue -Object $_ -Name 'fileName') -eq $AssetName
        })
    Assert-True ($rows.Count -eq 1) "SHA256SUMS.json 中 $AssetName 的行数不是 1"
    Assert-True ([string]$rows[0].sha256 -ieq $ExpectedSha256) "SHA256SUMS.json 中 $AssetName 的摘要不匹配"
}

function Read-SignedManifest {
    <#
    签名清单绑定版本和源提交；只接受生产签名和 GitHub 平铺布局，避免拿测试候选冒充发布资产。
    @author kongweiguang
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Commit,
        [Parameter(Mandatory = $true)][string]$AssetName,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256
    )

    Assert-True (Test-Path -LiteralPath $Path -PathType Leaf) "缺少 artifact-manifest.json：$Path"
    $manifest = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json -Depth 20
    Assert-True ([string]$manifest.version -eq $Version) "签名清单版本不是 $Version"
    Assert-True ([string]$manifest.sourceCommit -eq $Commit) "签名清单源提交不是 $Commit"
    Assert-True ([string]$manifest.signingClass -eq 'production') '签名清单不是 production'
    Assert-True ([string]$manifest.distributionLayout -eq 'github-release') '签名清单不是 github-release 布局'
    $rows = @($manifest.artifacts | Where-Object { (Get-PropertyValue -Object $_ -Name 'githubAssetName') -eq $AssetName })
    Assert-True ($rows.Count -eq 1) "签名清单中 $AssetName 的行数不是 1"
    Assert-True ([string]$rows[0].sha256 -ieq $ExpectedSha256) "签名清单中 $AssetName 的摘要不匹配"
    # Windows NSIS 同时承担新更新器和旧 Tauri 契约，两个签名都必须是 verified 才说明该资产确实经过双签名校验。
    Assert-True ([string]$rows[0].signingStatus -eq 'verified') "签名清单中 $AssetName 的新更新器签名未验证"
    Assert-True ([string]$rows[0].legacySigningStatus -eq 'verified') "签名清单中 $AssetName 的旧 Tauri 签名未验证"
}

if ($ReleaseTag -notmatch '^v(?<version>[0-9]+\.[0-9]+\.[0-9]+)$') {
    throw "ReleaseTag 必须是 vX.Y.Z：$ReleaseTag"
}
$version = $Matches['version']
if ($OldReleaseTag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "OldReleaseTag 必须是 vX.Y.Z：$OldReleaseTag"
}
$oldVersion = $OldReleaseTag.Substring(1)
if ($CandidateRunId -notmatch '^[0-9]+$') {
    throw "CandidateRunId 必须是数字：$CandidateRunId"
}

$newAssetName = "windows-x86_64-kerminal_${version}_x64-setup.exe"
$oldAssetName = "Kerminal_${oldVersion}_x64-setup.exe"

if ($env:GITHUB_REPOSITORY -and $env:GITHUB_REPOSITORY -ne $ExpectedRepository) {
    throw "工作流仓库 $env:GITHUB_REPOSITORY 与固定仓库 $ExpectedRepository 不一致"
}

$download = New-EmptyDirectory -Path $DownloadRoot
$staging = New-EmptyDirectory -Path $StagingRoot
$evidence = New-EvidenceDirectory -Path $EvidenceRoot
Assert-SeparateDirectories -Download $download -Staging $staging -Evidence $evidence

$tagCommit = Invoke-GhText -Arguments @('api', "repos/$ExpectedRepository/commits/$ReleaseTag", '--jq', '.sha')
Assert-True ($tagCommit -match '^[0-9a-f]{40}$') "无法解析 $ReleaseTag 的提交：$tagCommit"

$provenance = [ordered]@{
    schema           = 1
    recordedAt       = (Get-Date).ToUniversalTime().ToString('o')
    repository       = $ExpectedRepository
    assetSource      = $AssetSource
    releaseTag       = $ReleaseTag
    releaseVersion   = $version
    tagCommit        = $tagCommit
    newInstaller     = $null
    oldInstaller     = $null
    candidateRun     = $null
    release          = $null
    staged           = $null
}

if ($AssetSource -eq 'candidate-artifact') {
    $run = Invoke-GhJson -Arguments @('api', "repos/$ExpectedRepository/actions/runs/$CandidateRunId")
    Assert-True ([string]$run.repository.full_name -eq $ExpectedRepository) '候选运行仓库不匹配'
    Assert-True ([string]$run.path -eq '.github/workflows/gpui-package.yml') '候选运行不是 gpui-package.yml'
    Assert-True ([string]$run.event -eq 'workflow_dispatch') '候选运行不是手动触发'
    Assert-True ([string]$run.status -eq 'completed' -and [string]$run.conclusion -eq 'success') '候选运行不是成功完成状态'
    Assert-True ([string]$run.head_sha -eq $tagCommit) "候选 head_sha $($run.head_sha) 与 tag 提交 $tagCommit 不一致"

    $attempt = [int]$run.run_attempt
    Assert-True ($attempt -ge 1) "候选运行 attempt 无效：$attempt"
    $artifactName = "gpui-signed-candidate-$CandidateRunId-$attempt"
    $artifactList = Invoke-GhJson -Arguments @('api', "repos/$ExpectedRepository/actions/runs/$CandidateRunId/artifacts?per_page=100")
    $artifact = @($artifactList.artifacts | Where-Object { [string]$_.name -eq $artifactName })
    Assert-True ($artifact.Count -eq 1) "候选 artifact $artifactName 不存在或不唯一"
    Assert-True ($artifact[0].expired -eq $false) "候选 artifact $artifactName 已过期"
    Assert-True ([int64]$artifact[0].size_in_bytes -gt 0) "候选 artifact $artifactName 为空"

    $candidateDownload = New-EmptyDirectory -Path (Join-Path $download 'candidate')
    [void](Invoke-GhText -Arguments @('run', 'download', $CandidateRunId, '--name', $artifactName, '--dir', $candidateDownload))

    $newInstaller = Join-Path $candidateDownload $newAssetName
    $newHash = Assert-FileHash -Path $newInstaller -ExpectedSha256 $script:ExpectedInstallerHash -Label '候选安装器'
    Read-ReleaseChecksums -Path (Join-Path $candidateDownload 'SHA256SUMS.json') -Version $version -AssetName $newAssetName -ExpectedSha256 $newHash
    Read-SignedManifest -Path (Join-Path $candidateDownload 'artifact-manifest.json') -Version $version -Commit $tagCommit -AssetName $newAssetName -ExpectedSha256 $newHash

    Copy-Item -LiteralPath (Join-Path $candidateDownload 'SHA256SUMS.json') -Destination (Join-Path $evidence 'SHA256SUMS.json') -Force
    Copy-Item -LiteralPath (Join-Path $candidateDownload 'artifact-manifest.json') -Destination (Join-Path $evidence 'artifact-manifest.json') -Force

    $provenance.candidateRun = [ordered]@{
        runId      = [int64]$CandidateRunId
        attempt    = $attempt
        artifact   = $artifactName
        headSha    = [string]$run.head_sha
        conclusion = [string]$run.conclusion
    }
    $provenance.newInstaller = [ordered]@{
        name   = $newAssetName
        sha256 = $newHash
        size   = (Get-Item -LiteralPath $newInstaller).Length
        source = $artifactName
    }
} else {
    try {
        $release = Invoke-GhJson -Arguments @('release', 'view', $ReleaseTag, '--json', 'tagName,isDraft,isPrerelease,assets')
    } catch {
        throw "无法读取 Release $ReleaseTag。草稿资产需要能看到草稿的令牌；默认只读 GITHUB_TOKEN 请使用 candidate-artifact。$($_.Exception.Message)"
    }
    Assert-True ([string]$release.tagName -eq $ReleaseTag) "Release tag 不匹配：$($release.tagName)"
    $asset = @($release.assets | Where-Object { [string]$_.name -eq $newAssetName })
    Assert-True ($asset.Count -eq 1) "Release 中 $newAssetName 不存在或不唯一"
    Assert-ReleaseAssetDigest -Asset $asset[0] -ExpectedSha256 $script:ExpectedInstallerHash -Label 'Release 安装器'

    $releaseDownload = New-EmptyDirectory -Path (Join-Path $download 'release')
    [void](Invoke-GhText -Arguments @('release', 'download', $ReleaseTag, '--dir', $releaseDownload, '--pattern', $newAssetName))
    [void](Invoke-GhText -Arguments @('release', 'download', $ReleaseTag, '--dir', $releaseDownload, '--pattern', 'SHA256SUMS.json'))
    [void](Invoke-GhText -Arguments @('release', 'download', $ReleaseTag, '--dir', $releaseDownload, '--pattern', 'artifact-manifest.json'))

    $newInstaller = Join-Path $releaseDownload $newAssetName
    $newHash = Assert-FileHash -Path $newInstaller -ExpectedSha256 $script:ExpectedInstallerHash -Label 'Release 安装器'
    Read-ReleaseChecksums -Path (Join-Path $releaseDownload 'SHA256SUMS.json') -Version $version -AssetName $newAssetName -ExpectedSha256 $newHash
    Read-SignedManifest -Path (Join-Path $releaseDownload 'artifact-manifest.json') -Version $version -Commit $tagCommit -AssetName $newAssetName -ExpectedSha256 $newHash

    Copy-Item -LiteralPath (Join-Path $releaseDownload 'SHA256SUMS.json') -Destination (Join-Path $evidence 'SHA256SUMS.json') -Force
    Copy-Item -LiteralPath (Join-Path $releaseDownload 'artifact-manifest.json') -Destination (Join-Path $evidence 'artifact-manifest.json') -Force

    $provenance.release = [ordered]@{
        tag         = [string]$release.tagName
        draft       = [bool]$release.isDraft
        prerelease  = [bool]$release.isPrerelease
        assetDigest = [string]$asset[0].digest
    }
    $provenance.newInstaller = [ordered]@{
        name   = $newAssetName
        sha256 = $newHash
        size   = (Get-Item -LiteralPath $newInstaller).Length
        source = "release:$ReleaseTag"
    }
}

$oldRelease = Invoke-GhJson -Arguments @('release', 'view', $OldReleaseTag, '--json', 'tagName,isDraft,isPrerelease,assets')
Assert-True ([string]$oldRelease.tagName -eq $OldReleaseTag) "旧 Release tag 不匹配：$($oldRelease.tagName)"
Assert-True ($oldRelease.isDraft -eq $false) "旧发布 $OldReleaseTag 不应是草稿"
$oldAsset = @($oldRelease.assets | Where-Object { [string]$_.name -eq $oldAssetName })
Assert-True ($oldAsset.Count -eq 1) "旧发布中 $oldAssetName 不存在或不唯一"
Assert-ReleaseAssetDigest -Asset $oldAsset[0] -ExpectedSha256 $script:OldInstallerHash -Label '旧发布安装器'

$oldDownload = New-EmptyDirectory -Path (Join-Path $download 'old')
[void](Invoke-GhText -Arguments @('release', 'download', $OldReleaseTag, '--dir', $oldDownload, '--pattern', $oldAssetName))
$oldInstaller = Join-Path $oldDownload $oldAssetName
$oldHash = Assert-FileHash -Path $oldInstaller -ExpectedSha256 $script:OldInstallerHash -Label '旧发布安装器'
$oldTagCommit = Invoke-GhText -Arguments @('api', "repos/$ExpectedRepository/commits/$OldReleaseTag", '--jq', '.sha')
Assert-True ($oldTagCommit -match '^[0-9a-f]{40}$') "无法解析旧发布提交：$oldTagCommit"

$stagedNew = Join-Path $staging 'new-setup.exe'
$stagedOld = Join-Path $staging 'old-setup.exe'
Copy-Item -LiteralPath $newInstaller -Destination $stagedNew -Force
Copy-Item -LiteralPath $oldInstaller -Destination $stagedOld -Force
$stagedNewHash = Assert-FileHash -Path $stagedNew -ExpectedSha256 $newHash -Label '暂存新安装器'
$stagedOldHash = Assert-FileHash -Path $stagedOld -ExpectedSha256 $oldHash -Label '暂存旧安装器'

$provenance.oldInstaller = [ordered]@{
    name       = $oldAssetName
    sha256     = $oldHash
    size       = (Get-Item -LiteralPath $oldInstaller).Length
    releaseTag = $OldReleaseTag
    tagCommit  = $oldTagCommit
    digest     = [string]$oldAsset[0].digest
}
$provenance.staged = [ordered]@{
    newInstaller = $stagedNew
    newSha256    = $stagedNewHash
    oldInstaller = $stagedOld
    oldSha256    = $stagedOldHash
}

Write-JsonFile -Path (Join-Path $evidence 'provenance.json') -Value $provenance

$sourceLine = if ($AssetSource -eq 'candidate-artifact') {
    "- 来源：成功签名候选运行 ``$CandidateRunId``（attempt $($provenance.candidateRun.attempt)，artifact ``$($provenance.candidateRun.artifact)``）"
} else {
    "- 来源：Release ``$ReleaseTag``（draft=$($provenance.release.draft)）"
}
$markdown = @"
## Windows installer provenance

$sourceLine
- 仓库：``$ExpectedRepository``
- Release：``$ReleaseTag``，提交 ``$tagCommit``
- 新安装器：``$newAssetName`` SHA-256 ``$stagedNewHash``
- 旧安装器：``$oldAssetName``（``$OldReleaseTag``）SHA-256 ``$stagedOldHash``
- 下载、暂存和证据目录彼此独立；安装器只从暂存目录进入验收 harness。
"@
[IO.File]::WriteAllText((Join-Path $evidence 'provenance.md'), $markdown + "`n", [Text.UTF8Encoding]::new($false))
if ($env:GITHUB_STEP_SUMMARY) {
    Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value ($markdown + "`n")
}

Write-Output "Provenance verified: $newAssetName ($stagedNewHash) from $AssetSource; old $oldAssetName ($stagedOldHash)."
