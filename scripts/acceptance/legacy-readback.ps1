# @author kongweiguang
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Prepare', 'Execute')]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [string]$RunId,

    [string]$LegacyRepository,

    [string]$PrebuiltReaderExe,

    [string]$ExpectedReaderSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:BaselineCommit = '17c5d4be46172ca508f92546bfae5fe5ef477f11'
$script:RepositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
if ([string]::IsNullOrWhiteSpace($LegacyRepository)) {
    $LegacyRepository = Join-Path (Split-Path -Parent $script:RepositoryRoot) 'kerminal'
}
$script:LegacyRepository = [IO.Path]::GetFullPath($LegacyRepository)
$script:TargetRoot = [IO.Path]::GetFullPath((Join-Path $script:RepositoryRoot 'target'))
$script:AcceptanceRoot = [IO.Path]::GetFullPath((Join-Path $script:RepositoryRoot 'target\acceptance\legacy-readback'))
$script:ArchiveRoot = [IO.Path]::GetFullPath((Join-Path $script:RepositoryRoot "target\acceptance\legacy-reference\$($script:BaselineCommit)"))
$script:PrebuiltReaderBuild = [ordered]@{
    runId = 'hcore-20261017-legacy-r1'
    preparedJsonSha256 = 'edcf5626332a15af83a5e3709e98e410d607adc411f1ffe74e32f68bb2063523'
    executableRelativePath = 'hcore-20261017-legacy-r1\cargo-target\debug\deps\legacy_readback-39ccd5d38456a139.exe'
    executableSha256 = 'a498f4fb8d3e95bbf5e7984cdcf292f135337bae465fb0e2386b465e98881ca9'
    baselineCommit = '17c5d4be46172ca508f92546bfae5fe5ef477f11'
    archiveTarSha256 = 'b480da531aaf4e23df52965f22b56e454ae5826e78099bb233b692c72f113d30'
    archiveSourceManifestSha256 = '7341cc253361540bb6b13447cd1d211da894fd9251fb7a2b1939ba167a6e61e0'
    injectedManifestSha256 = 'f0aec14e014efa308598ab2a8949bac40c82cb3d6ce1cf2608aa59bf976e7ca6'
    oldCargoLockSha256 = '079543cc153e9be7188a0bea8d0ade544a39df7af01e56c6f66c2b8afbb6b49a'
    injectedTestSourceSha256 = '92c02699827c70c64e2764622cf4c57980d52c933a21dd7f482c0ca468a6fdb7'
}

$hasPrebuiltReader = -not [string]::IsNullOrWhiteSpace($PrebuiltReaderExe)
$hasExpectedReaderHash = -not [string]::IsNullOrWhiteSpace($ExpectedReaderSha256)
if ($hasPrebuiltReader -ne $hasExpectedReaderHash) {
    throw 'PrebuiltReaderExe 与 ExpectedReaderSha256 必须同时提供，或同时省略。'
}
if ($Mode -eq 'Prepare' -and $hasPrebuiltReader) {
    throw '预编译 reader 仅可用于 Execute 模式。'
}
if ($hasExpectedReaderHash -and $ExpectedReaderSha256 -notmatch '^[0-9a-fA-F]{64}$') {
    throw 'ExpectedReaderSha256 必须是 64 位十六进制 SHA-256。'
}
$script:UsePrebuiltReader = $hasPrebuiltReader

function Assert-ContainedPath {
    # 路径边界按规范化绝对路径检查，避免 run-id 或外部参数把旧版写入导向工作区外。
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
        throw "$Label 路径必须位于指定验收根目录内。"
    }
}

function Assert-NoReparsePoint {
    # 拒绝符号链接和目录联接，确保解包、复制和旧服务写入都留在验收副本。
    param([Parameter(Mandatory = $true)][string]$Path)

    $absolutePath = [IO.Path]::GetFullPath($Path)
    $current = [IO.Path]::GetPathRoot($absolutePath)
    $relative = $absolutePath.Substring($current.Length)
    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    foreach ($part in $relative.Split($separators, [StringSplitOptions]::RemoveEmptyEntries)) {
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "拒绝经过重解析点访问验收路径：$current"
            }
        }
    }
}

function Assert-NotUserHomePath {
    # 旧版初始化有 home 默认路径，因此整个验收输出都必须远离用户目录。
    param([Parameter(Mandatory = $true)][string]$Path)

    $separators = [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $candidate = [IO.Path]::GetFullPath($Path).TrimEnd($separators)
    $userHome = [IO.Path]::GetFullPath($script:UserHome).TrimEnd($separators)
    $prefix = $userHome + [IO.Path]::DirectorySeparatorChar
    if ($candidate.Equals($userHome, [StringComparison]::OrdinalIgnoreCase) -or $candidate.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw '拒绝将旧版归档、fixture 或回读副本写入用户 home。'
    }
}

function Get-TreeManifest {
    # 对数据树记录相对路径、长度和 SHA-256；旧版源码清单可排除 Tauri 明确标记的四个构建生成 schema，配置快照始终完整。
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [switch]$ExcludeGeneratedLegacySchemas
    )

    $generatedLegacySchemas = @(
        'src-tauri/gen/schemas/acl-manifests.json',
        'src-tauri/gen/schemas/capabilities.json',
        'src-tauri/gen/schemas/desktop-schema.json',
        'src-tauri/gen/schemas/windows-schema.json'
    )

    $rootPath = (Resolve-Path -LiteralPath $Root).Path.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    Assert-NoReparsePoint -Path $rootPath
    $entries = @()
    foreach ($item in Get-ChildItem -LiteralPath $rootPath -Recurse -Force | Sort-Object FullName) {
        Assert-NoReparsePoint -Path $item.FullName
        if ($item.PSIsContainer) { continue }
        $relative = [IO.Path]::GetRelativePath($rootPath, $item.FullName).Replace('\', '/')
        if ($ExcludeGeneratedLegacySchemas -and $generatedLegacySchemas -ccontains $relative) { continue }
        $entries += [ordered]@{
            path = $relative
            length = [long]$item.Length
            sha256 = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    return ,$entries
}

function Get-ManifestDiff {
    # 以路径为键比较前后清单，报告新增、删除和内容变化但不输出任何文件正文。
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Before,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$After
    )

    $beforeByPath = @{}
    foreach ($entry in $Before) { $beforeByPath[[string]$entry.path] = $entry }
    $afterByPath = @{}
    foreach ($entry in $After) { $afterByPath[[string]$entry.path] = $entry }
    $changes = @()
    foreach ($path in ($beforeByPath.Keys + $afterByPath.Keys | Sort-Object -Unique)) {
        if (-not $beforeByPath.ContainsKey($path)) {
            $changes += [ordered]@{ kind = 'added'; path = $path }
        } elseif (-not $afterByPath.ContainsKey($path)) {
            $changes += [ordered]@{ kind = 'removed'; path = $path }
        } elseif ($beforeByPath[$path].sha256 -ne $afterByPath[$path].sha256 -or $beforeByPath[$path].length -ne $afterByPath[$path].length) {
            $changes += [ordered]@{ kind = 'changed'; path = $path }
        }
    }
    return ,$changes
}

function Get-JsonSha256 {
    # 使用与 prepared 清单相同的紧凑 JSON 序列化计算摘要，以固定 reader 对应的完整归档清单。
    param([Parameter(Mandatory = $true)]$Value)

    $json = ConvertTo-Json -InputObject $Value -Depth 8 -Compress
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($json)
        return ([BitConverter]::ToString($sha256.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha256.Dispose()
    }
}

function Resolve-ContainedFile {
    # 先拒绝路径穿越与重解析点，再解析实际文件位置，防止预编译 reader 借链接逃离验收目录。
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Label
    )

    Assert-ContainedPath -Path $Path -Root $Root -Label $Label
    Assert-NoReparsePoint -Path $Path
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label 必须是已存在的普通文件。" }
    $resolvedPath = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path).ProviderPath)
    Assert-ContainedPath -Path $resolvedPath -Root $Root -Label "$Label 实际目标"
    Assert-NoReparsePoint -Path $resolvedPath
    return $resolvedPath
}

function Assert-PrebuiltReaderProvenance {
    # 固定旧 reader 只能读取与已登记构建逐文件相同的 17c 归档、lock、注入 manifest 和测试源。
    param(
        [Parameter(Mandatory = $true)]$Prepared,
        [Parameter(Mandatory = $true)][string]$ArchiveSource,
        [Parameter(Mandatory = $true)][string]$ArchiveTar
    )

    $record = $script:PrebuiltReaderBuild
    if ($record.baselineCommit -ne $script:BaselineCommit) { throw '预编译 reader 登记基线与脚本固定基线不一致。' }
    $registeredRunRoot = Join-Path $script:AcceptanceRoot $record.runId
    $registeredPreparedPath = Join-Path $registeredRunRoot 'prepared.json'
    $registeredSourceRoot = Join-Path $registeredRunRoot 'legacy'
    Assert-ContainedPath -Path $registeredRunRoot -Root $script:AcceptanceRoot -Label 'reader 构建记录 run'
    Assert-NoReparsePoint -Path $registeredRunRoot
    Assert-NoReparsePoint -Path $registeredPreparedPath
    if (-not (Test-Path -LiteralPath $registeredPreparedPath -PathType Leaf)) { throw 'reader 登记的 prepared.json 不存在。' }
    if ((Get-FileHash -LiteralPath $registeredPreparedPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $record.preparedJsonSha256) {
        throw 'reader 登记的 prepared.json 已变化，不能证明其构建来源。'
    }

    $registeredPrepared = Get-Content -LiteralPath $registeredPreparedPath -Raw | ConvertFrom-Json -AsHashtable
    if ($registeredPrepared.runId -ne $record.runId -or $registeredPrepared.baselineCommit -ne $record.baselineCommit) {
        throw 'reader 登记的 prepared.json run-id 或冻结基线不匹配。'
    }
    if ($registeredPrepared.outputs.archiveTarSha256 -ne $record.archiveTarSha256) {
        throw 'reader 登记的冻结归档摘要与构建记录不匹配。'
    }
    $expectedRegisteredTar = Join-Path $script:ArchiveRoot 'source.tar'
    Assert-NoReparsePoint -Path $expectedRegisteredTar
    if (-not [IO.Path]::GetFullPath([string]$registeredPrepared.outputs.archiveTar).Equals([IO.Path]::GetFullPath($expectedRegisteredTar), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'reader 登记的旧版 tar 路径不在固定归档位置。'
    }
    if ((Get-FileHash -LiteralPath $expectedRegisteredTar -Algorithm SHA256).Hash.ToLowerInvariant() -ne $record.archiveTarSha256) {
        throw 'reader 登记的冻结旧版 tar 已变化。'
    }
    if ((Get-JsonSha256 -Value $registeredPrepared.outputs.archiveSourceManifest) -ne $record.archiveSourceManifestSha256) {
        throw 'reader 登记的归档源码清单与构建记录不匹配。'
    }
    if ($registeredPrepared.outputs.archiveManifestIsolation.sha256After -ne $record.injectedManifestSha256) {
        throw 'reader 登记的私有 workspace manifest 摘要与构建记录不匹配。'
    }
    if ($Prepared.outputs.archiveTarSha256 -ne $record.archiveTarSha256) {
        throw '本次 run 的冻结归档摘要与 reader 构建记录不匹配。'
    }
    if (-not [IO.Path]::GetFullPath([string]$Prepared.outputs.archiveSource).Equals([IO.Path]::GetFullPath($ArchiveSource), [StringComparison]::OrdinalIgnoreCase)) {
        throw '本次 run 的源码目录与 prepared.json 不匹配。'
    }
    if ((Get-JsonSha256 -Value $Prepared.outputs.archiveSourceManifest) -ne $record.archiveSourceManifestSha256) {
        throw '本次 run 的归档源码清单与 reader 构建记录不匹配。'
    }
    if ($Prepared.outputs.archiveManifestIsolation.sha256After -ne $record.injectedManifestSha256) {
        throw '本次 run 的私有 workspace manifest 与 reader 构建记录不匹配。'
    }

    $expectedRegisteredSource = Join-Path $registeredRunRoot 'legacy'
    if (-not [IO.Path]::GetFullPath([string]$registeredPrepared.outputs.archiveSource).Equals([IO.Path]::GetFullPath($expectedRegisteredSource), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'reader 登记的源码目录不在固定构建 run 的预期位置。'
    }
    Assert-ContainedPath -Path $registeredSourceRoot -Root $script:AcceptanceRoot -Label 'reader 登记的归档源码'
    Assert-NoReparsePoint -Path $registeredSourceRoot
    $registeredManifest = Get-TreeManifest -Root $registeredSourceRoot -ExcludeGeneratedLegacySchemas
    if ((Get-JsonSha256 -Value $registeredManifest) -ne $record.archiveSourceManifestSha256) {
        throw 'reader 构建所用的归档源码树已变化。'
    }

    # Invoke-PreparedRun 已将当前源码树与本次 prepared 清单逐项比较；这里只验证其固定摘要，避免重复遍历整个归档。
    Assert-NoReparsePoint -Path $ArchiveSource
    if ((Get-FileHash -LiteralPath $ArchiveTar -Algorithm SHA256).Hash.ToLowerInvariant() -ne $record.archiveTarSha256) {
        throw '本次 run 使用的冻结旧版 tar 与 reader 构建来源不一致。'
    }
    $expectedCurrentSource = Join-Path (Split-Path -Parent $ArchiveSource) 'legacy'
    if (-not [IO.Path]::GetFullPath($ArchiveSource).Equals([IO.Path]::GetFullPath($expectedCurrentSource), [StringComparison]::OrdinalIgnoreCase)) {
        throw '本次 run 的源码目录与 prepared.json 中的固定目录布局不匹配。'
    }

    $registeredFiles = @{
        'src-tauri/Cargo.toml' = $record.injectedManifestSha256
        'src-tauri/Cargo.lock' = $record.oldCargoLockSha256
        'src-tauri/tests/legacy_readback.rs' = $record.injectedTestSourceSha256
    }
    foreach ($relativePath in $registeredFiles.Keys) {
        $relativeNativePath = $relativePath.Replace([char]'/', [IO.Path]::DirectorySeparatorChar)
        foreach ($sourceRoot in @($registeredSourceRoot, $ArchiveSource)) {
            $sourcePath = Join-Path $sourceRoot $relativeNativePath
            Assert-NoReparsePoint -Path $sourcePath
            if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) { throw "reader 来源缺少已登记文件：$relativePath" }
            $actualHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actualHash -ne $registeredFiles[$relativePath]) { throw "reader 来源文件摘要与登记值不匹配：$relativePath" }
        }
    }

    $expectedRegisteredTar = Join-Path $script:ArchiveRoot 'source.tar'
    Assert-NoReparsePoint -Path $ArchiveTar
    if (-not [IO.Path]::GetFullPath($ArchiveTar).Equals([IO.Path]::GetFullPath($expectedRegisteredTar), [StringComparison]::OrdinalIgnoreCase)) {
        throw '本次 run 的旧版 tar 路径与固定归档位置不匹配。'
    }
}

function Assert-SafeIdentifier {
    # TOML 文件名由 fixture ID 派生，因此只允许稳定的文件名字符。
    param([Parameter(Mandatory = $true)][string]$Value, [Parameter(Mandatory = $true)][string]$Name)
    if ($Value -notmatch '^[A-Za-z0-9_-]{1,128}$') {
        throw "expected.json 中的 $Name 不是安全的合成标识符。"
    }
}

function Read-AndValidateFixture {
    # 在触碰旧版解析器前验证完整输入契约及主机信任材料关联，避免模糊的旧错误掩盖 fixture 缺项。
    param([Parameter(Mandatory = $true)][string]$GeneratedRoot)

    $expectedPath = Join-Path $GeneratedRoot 'expected.json'
    $configPath = Join-Path $GeneratedRoot 'config'
    if (-not (Test-Path -LiteralPath $expectedPath -PathType Leaf) -or -not (Test-Path -LiteralPath $configPath -PathType Container)) {
        $id = Split-Path -Leaf (Split-Path -Parent $GeneratedRoot)
        throw "fixture 尚未生成。请先由 H 执行：`$env:KERMINAL_LEGACY_READBACK_RUN_ID='$id'; cargo test --locked -p kerminal-infrastructure --test legacy_readback_fixture -- --ignored --exact generate_real_legacy_readback_fixture"
    }
    Assert-NoReparsePoint -Path $expectedPath
    Assert-NoReparsePoint -Path $configPath
    $expected = Get-Content -LiteralPath $expectedPath -Raw | ConvertFrom-Json -AsHashtable
    if ($expected.contractVersion -ne 1) { throw '仅支持 contractVersion=1 的旧版回读 fixture。' }
    foreach ($name in @('profileId', 'hostId', 'groupId', 'snippetId', 'workflowId')) {
        Assert-SafeIdentifier -Value ([string]$expected[$name]) -Name $name
    }
    Assert-SafeIdentifier -Value ([string]$expected.vault.keyId) -Name 'vault.keyId'
    $secretReference = [string]$expected.host.secretRef
    $vaultEntryId = [string]$expected.vault.entryId
    $secretReferenceMatch = [regex]::Match(
        $secretReference,
        '^credential:kerminal:ssh-host:(?<hostId>[A-Za-z0-9_-]{1,128}):target:password:v1$',
        [System.Text.RegularExpressions.RegexOptions]::CultureInvariant
    )
    if (
        -not $secretReferenceMatch.Success -or
        $secretReferenceMatch.Groups['hostId'].Value -ne [string]$expected.hostId -or
        $vaultEntryId -cne $secretReference
    ) {
        throw 'fixture secret_ref 必须是关联到同一 SSH host 的合成 vault 引用，且与 vault entry ID 一致。'
    }
    foreach ($name in @('entryId')) {
        Assert-SafeIdentifier -Value ([string]$expected.history[$name]) -Name "history.$name"
    }
    foreach ($name in @('activeTabId', 'focusedPaneId')) {
        Assert-SafeIdentifier -Value ([string]$expected.workspace[$name]) -Name "workspace.$name"
    }
    if ($expected.settings.themeMode -notin @('dark', 'light', 'system')) {
        throw 'settings.themeMode 必须是旧版可识别的 dark、light 或 system。'
    }
    if ([int]$expected.hostSchemaVersion -ne 2 -or [int]$expected.workspace.version -ne 3) {
        throw 'fixture 必须覆盖 host schema v2 与 workspace session v3。'
    }
    if ([string]$expected.vault.plaintextSha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'vault.plaintextSha256 必须是 64 位 SHA-256，不得包含明文 secret。'
    }
    if ([string]::IsNullOrWhiteSpace([string]$expected.host.secretRef)) {
        throw 'fixture host.secretRef 必须引用非空的合成 vault 凭据。'
    }
    $knownHost = [string]$expected.knownHosts.host
    $knownPort = 0
    $validKnownPort = [int]::TryParse(
        [string]$expected.knownHosts.port,
        [Globalization.NumberStyles]::None,
        [Globalization.CultureInfo]::InvariantCulture,
        [ref]$knownPort
    )
    if ([string]::IsNullOrWhiteSpace($knownHost) -or $knownHost -match '[\r\n\0]') {
        throw 'fixture knownHosts.host 必须是单行非空主机名。'
    }
    if (-not $validKnownPort -or $knownPort -lt 1 -or $knownPort -gt 65535) {
        throw 'fixture knownHosts.port 必须是 1–65535 范围内的端口。'
    }
    $hostPort = 0
    $validHostPort = [int]::TryParse(
        [string]$expected.host.port,
        [Globalization.NumberStyles]::None,
        [Globalization.CultureInfo]::InvariantCulture,
        [ref]$hostPort
    )
    if (-not $validHostPort -or $knownHost -ne [string]$expected.host.host -or $knownPort -ne $hostPort) {
        throw 'fixture knownHosts.host/port 必须与 host endpoint 一致。'
    }
    $algorithm = [string]$expected.knownHosts.algorithm
    if ([string]::IsNullOrWhiteSpace($algorithm) -or $algorithm -match '[\r\n\0]') {
        throw 'fixture knownHosts.algorithm 必须是单行非空值。'
    }
    $fingerprint = [string]$expected.knownHosts.fingerprintSha256
    if ([string]::IsNullOrWhiteSpace($fingerprint) -or $fingerprint -match '[\r\n\0]') {
        throw 'fixture knownHosts.fingerprintSha256 必须是单行非空 SHA-256 指纹。'
    }
    $serialized = $expected | ConvertTo-Json -Depth 32 -Compress
    if ($serialized -match '"(?:password|plaintext|privateKey|credentialSecret)"\s*:') {
        throw 'expected.json 禁止包含凭据明文或私钥字段。'
    }

    $requiredFiles = @(
        'settings.toml',
        "profiles\$($expected.profileId).toml",
        'hosts\groups.toml',
        "hosts\$($expected.hostId).toml",
        'known_hosts',
        "snippets\$($expected.snippetId).toml",
        "workflows\$($expected.workflowId).toml",
        'secrets\vault-key.toml',
        'secrets\vault.toml',
        'data\command.sqlite',
        'workspace\session.json'
    )
    foreach ($relative in $requiredFiles) {
        $filePath = Join-Path $configPath $relative
        if (-not (Test-Path -LiteralPath $filePath -PathType Leaf)) { throw "fixture 缺少必需文件：$relative" }
        Assert-NoReparsePoint -Path $filePath
    }
    return $expected
}

function Initialize-LegacyArchive {
    # 固定 SHA 的 tar 只归档一次；每个 run 再独立展开，避免注入测试污染共享 oracle。
    param()

    if (-not (Test-Path -LiteralPath $script:LegacyRepository -PathType Container)) {
        throw "找不到旧版只读 Git 仓库：$script:LegacyRepository"
    }
    $resolvedCommit = (& git -C $script:LegacyRepository rev-parse $script:BaselineCommit).Trim()
    if ($LASTEXITCODE -ne 0 -or $resolvedCommit -ne $script:BaselineCommit) {
        throw "旧版仓库未能解析冻结基线 $($script:BaselineCommit)。"
    }
    $objectType = (& git -C $script:LegacyRepository cat-file -t $script:BaselineCommit).Trim()
    if ($LASTEXITCODE -ne 0 -or $objectType -ne 'commit') { throw '冻结旧版基线不是可用 Git commit。' }

    Assert-ContainedPath -Path $script:ArchiveRoot -Root (Join-Path $script:RepositoryRoot 'target\acceptance\legacy-reference') -Label '旧版归档'
    Assert-NoReparsePoint -Path (Join-Path $script:RepositoryRoot 'target')
    $markerPath = Join-Path $script:ArchiveRoot 'archive.json'
    $tarPath = Join-Path $script:ArchiveRoot 'source.tar'
    $pristineRoot = Join-Path $script:ArchiveRoot 'pristine'
    Assert-NoReparsePoint -Path $script:TargetRoot
    Assert-NotUserHomePath -Path $script:ArchiveRoot
    Assert-NoReparsePoint -Path (Split-Path -Parent $script:ArchiveRoot)
    if (Test-Path -LiteralPath $script:ArchiveRoot) {
        Assert-NoReparsePoint -Path $script:ArchiveRoot
        if (-not (Test-Path -LiteralPath $markerPath -PathType Leaf) -or -not (Test-Path -LiteralPath $tarPath -PathType Leaf) -or -not (Test-Path -LiteralPath $pristineRoot -PathType Container)) {
            throw "冻结归档目标已存在但没有完整性标记，拒绝覆盖：$script:ArchiveRoot"
        }
        $archiveMarker = Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json
        if ($archiveMarker.commit -ne $script:BaselineCommit) { throw '已有旧版归档标记与指定 SHA 不匹配。' }
        $actualHash = (Get-FileHash -LiteralPath $tarPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $archiveMarker.sha256) { throw '已有冻结旧版归档 tar 的 SHA-256 不匹配。' }
        return $tarPath
    }

    $archiveParent = Split-Path -Parent $script:ArchiveRoot
    New-Item -ItemType Directory -Path $archiveParent -Force | Out-Null
    New-Item -ItemType Directory -Path $script:ArchiveRoot | Out-Null
    & git -C $script:LegacyRepository archive --format=tar --output=$tarPath $script:BaselineCommit
    if ($LASTEXITCODE -ne 0) { throw 'git archive 未能生成冻结旧版源码包。' }
    New-Item -ItemType Directory -Path $pristineRoot | Out-Null
    & tar -xf $tarPath -C $pristineRoot
    if ($LASTEXITCODE -ne 0) { throw 'tar 未能检查冻结旧版源码包。' }
    if (-not (Test-Path -LiteralPath (Join-Path $pristineRoot 'src-tauri\Cargo.lock') -PathType Leaf)) {
        throw '冻结旧版归档缺少 Cargo.lock，无法执行 --locked 回读。'
    }
    Assert-NoReparsePoint -Path $script:ArchiveRoot
    $archiveHash = (Get-FileHash -LiteralPath $tarPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [ordered]@{ commit = $script:BaselineCommit; sha256 = $archiveHash } |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $markerPath -Encoding utf8NoBOM
    return $tarPath
}

function Add-ArchiveWorkspaceBoundary {
    # Cargo 会沿 target 目录向上发现新仓库 workspace；只在固定旧版的私有归档副本中加边界，保持旧 Git 对象和 lock 原样。
    param([Parameter(Mandatory = $true)][string]$ArchiveSource)

    $manifestPath = Join-Path $ArchiveSource 'src-tauri\Cargo.toml'
    Assert-ContainedPath -Path $manifestPath -Root $ArchiveSource -Label '旧版 Cargo manifest'
    Assert-NoReparsePoint -Path $manifestPath
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw '固定旧版归档缺少 src-tauri/Cargo.toml。'
    }

    $manifest = [IO.File]::ReadAllText($manifestPath)
    if ($manifest -match '(?m)^\s*\[workspace(?:\.[^\]]+)?\]\s*$') {
        throw '固定旧版 manifest 已声明 workspace，拒绝重复注入隔离边界。'
    }
    if ($manifest -match '(?m)^\s*(?:version|edition|authors|license|rust-version)\s*=\s*[^\r\n]*\.workspace\s*(?:#.*)?$') {
        throw '固定旧版 package 继承 workspace 字段，不能安全追加空 workspace 边界。'
    }

    $beforeHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::AppendAllText(
        $manifestPath,
        "`r`n[workspace]`r`n",
        [Text.UTF8Encoding]::new($false)
    )
    $afterHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    return [ordered]@{
        path = 'src-tauri/Cargo.toml'
        change = 'empty workspace boundary appended to this private run copy only'
        sha256Before = $beforeHash
        sha256After = $afterHash
    }
}

function Prepare-Run {
    # 每次验收都使用全新 run-id 和私有数据副本，并隔离旧 Cargo workspace 发现路径，不碰候选源或冻结 Git 对象。
    param([Parameter(Mandatory = $true)][string]$Id)

    if ($Id -notmatch '^[A-Za-z0-9_-]{1,80}$') { throw 'RunId 仅允许 1–80 位 ASCII 字母、数字、下划线和连字符。' }
    $runRoot = Join-Path $script:AcceptanceRoot $Id
    Assert-ContainedPath -Path $runRoot -Root $script:AcceptanceRoot -Label 'run'
    Assert-NotUserHomePath -Path $runRoot
    Assert-NotUserHomePath -Path (Join-Path $runRoot 'generated\config')
    Assert-NoReparsePoint -Path $script:RepositoryRoot
    if (Test-Path -LiteralPath $runRoot) {
        Assert-NoReparsePoint -Path $runRoot
        $unexpectedChildren = @(Get-ChildItem -LiteralPath $runRoot -Force | Where-Object { $_.Name -ne 'generated' })
        if ($unexpectedChildren.Count -gt 0) { throw "run-id 已初始化或有非 fixture 内容；为保留证据拒绝覆盖：$Id" }
    }

    $generatedRoot = Join-Path $runRoot 'generated'
    $expected = Read-AndValidateFixture -GeneratedRoot $generatedRoot
    $archiveTar = Initialize-LegacyArchive
    if (-not (Test-Path -LiteralPath $runRoot)) { New-Item -ItemType Directory -Path $runRoot | Out-Null }
    $archiveSource = Join-Path $runRoot 'legacy'
    New-Item -ItemType Directory -Path $archiveSource | Out-Null
    & tar -xf $archiveTar -C $archiveSource
    if ($LASTEXITCODE -ne 0) { throw '无法将冻结旧版源码展开到本次私有 run。' }
    $testDirectory = Join-Path $archiveSource 'src-tauri\tests'
    $rustTarget = Join-Path $testDirectory 'legacy_readback.rs'
    if (Test-Path -LiteralPath $rustTarget) { throw '归档中已存在同名测试，拒绝覆盖非本 harness 文件。' }

    New-Item -ItemType Directory -Path $testDirectory -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'legacy-readback\legacy_readback.rs') -Destination $rustTarget
    $archiveManifestIsolation = Add-ArchiveWorkspaceBoundary -ArchiveSource $archiveSource
    $readbackRoot = Join-Path $runRoot 'readback'
    $configCopy = Join-Path $readbackRoot 'config'
    New-Item -ItemType Directory -Path $readbackRoot | Out-Null
    Copy-Item -LiteralPath (Join-Path $generatedRoot 'config') -Destination $configCopy -Recurse
    Copy-Item -LiteralPath (Join-Path $generatedRoot 'expected.json') -Destination (Join-Path $readbackRoot 'expected.json')

    $before = Get-TreeManifest -Root $configCopy
    $prepared = [ordered]@{
        contractVersion = 1
        baselineCommit = $script:BaselineCommit
        preparedAtUtc = [DateTime]::UtcNow.ToString('o')
        runId = $Id
        generatedExpectedSha256 = (Get-FileHash -LiteralPath (Join-Path $generatedRoot 'expected.json') -Algorithm SHA256).Hash.ToLowerInvariant()
        generatedConfig = Get-TreeManifest -Root (Join-Path $generatedRoot 'config')
        readbackConfig = $before
        outputs = [ordered]@{
            archiveSource = $archiveSource
            archiveTar = $archiveTar
            archiveTarSha256 = (Get-FileHash -LiteralPath $archiveTar -Algorithm SHA256).Hash.ToLowerInvariant()
            archiveManifestIsolation = $archiveManifestIsolation
            archiveSourceManifest = Get-TreeManifest -Root $archiveSource -ExcludeGeneratedLegacySchemas
            configRoot = $configCopy
            expectedPath = Join-Path $readbackRoot 'expected.json'
        }
    }
    $preparePath = Join-Path $runRoot 'prepared.json'
    $prepared | ConvertTo-Json -Depth 24 | Set-Content -LiteralPath $preparePath -Encoding utf8NoBOM
    Write-Output "prepared: $preparePath"
    Write-Output "legacy archive: $archiveSource"
    Write-Output "legacy manifest workspace boundary: $($archiveManifestIsolation.sha256Before) -> $($archiveManifestIsolation.sha256After)"
    Write-Output "readback copy: $configCopy"
    Write-Output "fixture IDs: profile=$($expected.profileId); host=$($expected.hostId); group=$($expected.groupId); snippet=$($expected.snippetId); workflow=$($expected.workflowId)"
}

function Invoke-PreparedRun {
    # 先核对冻结输入，再将旧服务、SQLite、vault 和日志输出限定到本次 target/run 副本。
    param(
        [Parameter(Mandatory = $true)][string]$Id,
        [string]$ReaderExecutablePath,
        [string]$ReaderHash
    )

    if ($Id -notmatch '^[A-Za-z0-9_-]{1,80}$') { throw 'RunId 仅允许 1–80 位 ASCII 字母、数字、下划线和连字符。' }
    $runRoot = Join-Path $script:AcceptanceRoot $Id
    Assert-ContainedPath -Path $runRoot -Root $script:AcceptanceRoot -Label 'run'
    $preparedPath = Join-Path $runRoot 'prepared.json'
    $startedPath = Join-Path $runRoot 'execution.started'
    if (-not (Test-Path -LiteralPath $preparedPath -PathType Leaf)) { throw "run 尚未 prepare：$Id" }
    if (Test-Path -LiteralPath $startedPath) { throw '该 run 已开始执行；为避免不确定结果被重复运行，不自动重试。' }
    Assert-NoReparsePoint -Path $runRoot
    $prepared = Get-Content -LiteralPath $preparedPath -Raw | ConvertFrom-Json -AsHashtable
    if ($prepared.baselineCommit -ne $script:BaselineCommit -or $prepared.runId -ne $Id) { throw 'prepared.json 基线或 run-id 不匹配。' }

    $archiveSource = [string]$prepared.outputs.archiveSource
    $archiveTar = [string]$prepared.outputs.archiveTar
    $readbackRoot = [string]$prepared.outputs.configRoot
    $expectedPath = [string]$prepared.outputs.expectedPath
    $expectedArchiveSource = Join-Path $runRoot 'legacy'
    $expectedArchiveTar = Join-Path $script:ArchiveRoot 'source.tar'
    $expectedReadbackRoot = Join-Path $runRoot 'readback\config'
    $expectedManifestPath = Join-Path $runRoot 'readback\expected.json'
    foreach ($pair in @(
        @($archiveSource, $expectedArchiveSource),
        @($archiveTar, $expectedArchiveTar),
        @($readbackRoot, $expectedReadbackRoot),
        @($expectedPath, $expectedManifestPath)
    )) {
        if (-not [IO.Path]::GetFullPath($pair[0]).Equals([IO.Path]::GetFullPath($pair[1]), [StringComparison]::OrdinalIgnoreCase)) {
            throw 'prepared.json 输出路径与固定 run 目录布局不匹配。'
        }
    }
    Assert-ContainedPath -Path $archiveSource -Root $runRoot -Label '旧版归档源码'
    Assert-ContainedPath -Path $archiveTar -Root $script:ArchiveRoot -Label '旧版归档 tar'
    Assert-ContainedPath -Path $readbackRoot -Root (Join-Path $runRoot 'readback') -Label '回读数据副本'
    Assert-ContainedPath -Path $expectedPath -Root (Join-Path $runRoot 'readback') -Label '回读 manifest'
    Assert-NoReparsePoint -Path $archiveSource
    Assert-NoReparsePoint -Path $readbackRoot
    if ((Get-FileHash -LiteralPath $archiveTar -Algorithm SHA256).Hash.ToLowerInvariant() -ne $prepared.outputs.archiveTarSha256) {
        throw '冻结旧版 tar 在 prepare 后变化，拒绝执行。'
    }
    $currentArchiveManifest = Get-TreeManifest -Root $archiveSource -ExcludeGeneratedLegacySchemas
    if ((ConvertTo-Json -InputObject $currentArchiveManifest -Depth 8 -Compress) -ne (ConvertTo-Json -InputObject $prepared.outputs.archiveSourceManifest -Depth 8 -Compress)) {
        throw 'run 内旧版源码副本在 prepare 后变化，拒绝执行。'
    }

    $actualExpectedHash = (Get-FileHash -LiteralPath $expectedPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualExpectedHash -ne $prepared.generatedExpectedSha256) { throw '回读 expected.json 与 prepare 时的 fixture 不同。' }
    $readbackExpectedShaBefore = $actualExpectedHash
    $generatedRoot = Join-Path $runRoot 'generated'
    $generatedExpectedPath = Join-Path $generatedRoot 'expected.json'
    if ((Get-FileHash -LiteralPath $generatedExpectedPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $prepared.generatedExpectedSha256) {
        throw '生成器源 expected.json 在 prepare 后变化，拒绝执行。'
    }
    $generatedBefore = Get-TreeManifest -Root (Join-Path $generatedRoot 'config')
    if ((ConvertTo-Json -InputObject $generatedBefore -Depth 8 -Compress) -ne (ConvertTo-Json -InputObject $prepared.generatedConfig -Depth 8 -Compress)) {
        throw '生成器源配置在 prepare 后变化，拒绝执行。'
    }
    $before = Get-TreeManifest -Root $readbackRoot
    if ((ConvertTo-Json -InputObject $before -Depth 8 -Compress) -ne (ConvertTo-Json -InputObject $prepared.readbackConfig -Depth 8 -Compress)) {
        throw '回读数据副本在 prepare 后发生变化，拒绝执行旧版写操作。'
    }

    $readerExecutable = $null
    $readerExecutableSha256 = $null
    if ($script:UsePrebuiltReader) {
        $readerExecutable = Resolve-ContainedFile -Path $ReaderExecutablePath -Root $script:AcceptanceRoot -Label '预编译 reader'
        $registeredReaderPath = Resolve-ContainedFile -Path (Join-Path $script:AcceptanceRoot $script:PrebuiltReaderBuild.executableRelativePath) -Root $script:AcceptanceRoot -Label '登记的预编译 reader'
        if (-not $readerExecutable.Equals($registeredReaderPath, [StringComparison]::OrdinalIgnoreCase)) {
            throw '预编译 reader 路径与固定构建记录不匹配。'
        }
        $readerExecutableSha256 = (Get-FileHash -LiteralPath $readerExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($readerExecutableSha256 -ne $script:PrebuiltReaderBuild.executableSha256 -or $ReaderHash.ToLowerInvariant() -ne $script:PrebuiltReaderBuild.executableSha256) {
            throw '预编译 reader 的实际摘要、调用方摘要与固定构建记录必须完全一致。'
        }
        Assert-PrebuiltReaderProvenance -Prepared $prepared -ArchiveSource $archiveSource -ArchiveTar $archiveTar
    }

    $started = "started $([DateTime]::UtcNow.ToString('o'))`n"
    Set-Content -LiteralPath $startedPath -Value $started -Encoding utf8NoBOM
    $legacyConfigRoot = Join-Path $archiveSource 'src-tauri'
    $cargoManifest = Join-Path $legacyConfigRoot 'Cargo.toml'
    $targetDirectory = Join-Path $runRoot 'cargo-target'
    $rustLog = Join-Path $runRoot 'legacy-rust-test.log'
    $workspaceLog = Join-Path $runRoot 'legacy-workspace-test.log'
    $workspaceSource = Join-Path $archiveSource 'src\features\workspace\workspaceSession.ts'
    $bundlePath = Join-Path $runRoot 'workspaceSession.bundle.mjs'
    $nodeTest = Join-Path $PSScriptRoot 'legacy-readback\legacy_workspace_readback.mjs'
    $nodeCommand = Get-Command node -ErrorAction SilentlyContinue
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    $nodeExe = if ($nodeCommand) { $nodeCommand.Source } else { $null }
    $cargoExe = if ($cargoCommand) { $cargoCommand.Source } else { $null }
    $esbuildExe = Join-Path $script:LegacyRepository 'node_modules\.bin\esbuild.cmd'
    $rustExit = $null
    $cargoExit = $null
    $esbuildExit = $null
    $workspaceExit = $null
    $unavailable = @()
    $readerExecutionMode = if ($script:UsePrebuiltReader) { 'registered_prebuilt' } else { 'cargo' }

    $savedConfigEnv = $env:KERMINAL_CONFIG_ROOT
    $savedRootEnv = $env:KERMINAL_LEGACY_READBACK_ROOT
    $savedExpectedEnv = $env:KERMINAL_LEGACY_READBACK_EXPECTED
    $savedWorkspaceEnv = $env:KERMINAL_LEGACY_WORKSPACE_BUNDLE
    $executionErrors = @()
    try {
        $env:KERMINAL_CONFIG_ROOT = $readbackRoot
        $env:KERMINAL_LEGACY_READBACK_ROOT = $readbackRoot
        $env:KERMINAL_LEGACY_READBACK_EXPECTED = $expectedPath

        if ($script:UsePrebuiltReader) {
            try {
                Push-Location -LiteralPath $legacyConfigRoot
                try {
                    & $readerExecutable --exact frozen_legacy_services_read_generated_configuration_and_data --nocapture *> $rustLog
                    $rustExit = $LASTEXITCODE
                } finally { Pop-Location }
            } catch {
                $executionErrors += '无法启动或完成登记的预编译旧版 Rust reader。'
            }
        } elseif (-not $cargoExe) {
            $unavailable += 'cargo 不可用，Rust 旧版服务未执行。'
        } else {
            try {
                Push-Location -LiteralPath $legacyConfigRoot
                try {
                    & $cargoExe test --locked --manifest-path $cargoManifest --target-dir $targetDirectory --test legacy_readback -- --nocapture *> $rustLog
                    $cargoExit = $LASTEXITCODE
                    $rustExit = $cargoExit
                } finally { Pop-Location }
            } catch {
                $executionErrors += '无法启动或完成旧版 Rust 回读进程。'
            }
        }

        if (-not $nodeExe -or -not (Test-Path -LiteralPath $esbuildExe -PathType Leaf)) {
            $unavailable += 'Node.js 或旧工作区 node_modules 中的 esbuild 不可用，workspace v3 解码未执行；未安装或修改依赖。'
        } else {
            try {
                & $esbuildExe --bundle --platform=node --format=esm --target=es2022 --outfile=$bundlePath $workspaceSource *> (Join-Path $runRoot 'workspace-bundle.log')
                $esbuildExit = $LASTEXITCODE
                if ($esbuildExit -eq 0) {
                    $env:KERMINAL_LEGACY_WORKSPACE_BUNDLE = $bundlePath
                    & $nodeExe $nodeTest *> $workspaceLog
                    $workspaceExit = $LASTEXITCODE
                }
            } catch {
                $executionErrors += '无法构建或运行旧版 workspace 解码验证。'
            }
        }
    } catch {
        $executionErrors += '验收执行遇到未处理的本地进程或环境错误。'
    } finally {
        $env:KERMINAL_CONFIG_ROOT = $savedConfigEnv
        $env:KERMINAL_LEGACY_READBACK_ROOT = $savedRootEnv
        $env:KERMINAL_LEGACY_READBACK_EXPECTED = $savedExpectedEnv
        $env:KERMINAL_LEGACY_WORKSPACE_BUNDLE = $savedWorkspaceEnv
    }

    $after = Get-TreeManifest -Root $readbackRoot
    $readbackChanges = @()
    foreach ($change in (Get-ManifestDiff -Before $before -After $after)) {
        if ($null -ne $change) {
            $readbackChanges += [ordered]@{ scope = 'readback'; kind = $change.kind; path = $change.path }
        }
    }
    $generatedAfter = Get-TreeManifest -Root (Join-Path $generatedRoot 'config')
    $generatedChanges = @()
    foreach ($change in (Get-ManifestDiff -Before $generatedBefore -After $generatedAfter)) {
        if ($null -ne $change) {
            $generatedChanges += [ordered]@{ scope = 'generated'; kind = $change.kind; path = $change.path }
        }
    }
    if ((Get-FileHash -LiteralPath $generatedExpectedPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $prepared.generatedExpectedSha256) {
        $generatedChanges += [ordered]@{ scope = 'generated'; kind = 'changed'; path = 'expected.json' }
    }
    $readbackExpectedShaAfter = (Get-FileHash -LiteralPath $expectedPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $readbackManifestChanges = @()
    if ($readbackExpectedShaAfter -ne $readbackExpectedShaBefore) {
        $readbackManifestChanges += [ordered]@{ scope = 'readback_manifest'; kind = 'changed'; path = 'expected.json' }
    }
    $changes = @($readbackChanges + $generatedChanges + $readbackManifestChanges)
    $allExecuted = ($null -ne $rustExit) -and ($null -ne $workspaceExit) -and ($null -ne $esbuildExit)
    $failed = ($executionErrors.Count -gt 0) -or (($null -ne $rustExit) -and ($rustExit -ne 0)) -or (($null -ne $esbuildExit) -and ($esbuildExit -ne 0)) -or (($null -ne $workspaceExit) -and ($workspaceExit -ne 0))
    $passed = ($rustExit -eq 0) -and ($workspaceExit -eq 0) -and ($esbuildExit -eq 0)
    $outcome = if ($failed) { 'failed' } elseif (-not $allExecuted) { 'incomplete' } elseif (-not $passed) { 'failed' } elseif ($changes.Count -gt 0) { 'passed_with_data_mutations' } else { 'passed' }
    $report = [ordered]@{
        contractVersion = 1
        baselineCommit = $script:BaselineCommit
        runId = $Id
        finishedAtUtc = [DateTime]::UtcNow.ToString('o')
        outcome = $outcome
        readerExecutionMode = $readerExecutionMode
        readerExitCode = $rustExit
        readerExecutableSha256 = $readerExecutableSha256
        readerBuild = if ($script:UsePrebuiltReader) { $script:PrebuiltReaderBuild } else { $null }
        cargoExitCode = $cargoExit
        esbuildExitCode = $esbuildExit
        workspaceNodeExitCode = $workspaceExit
        unavailable = $unavailable
        executionErrors = $executionErrors
        generatedExpectedSha256Before = $prepared.generatedExpectedSha256
        generatedExpectedSha256After = (Get-FileHash -LiteralPath $generatedExpectedPath -Algorithm SHA256).Hash.ToLowerInvariant()
        readbackExpectedSha256Before = $readbackExpectedShaBefore
        readbackExpectedSha256After = $readbackExpectedShaAfter
        generatedConfigBefore = $generatedBefore
        generatedConfigAfter = $generatedAfter
        readbackConfigBefore = $before
        readbackConfigAfter = $after
        dataChanges = $changes
        logs = @('legacy-rust-test.log', 'workspace-bundle.log', 'legacy-workspace-test.log')
    }
    $reportPath = Join-Path $runRoot 'result.json'
    $report | ConvertTo-Json -Depth 16 | Set-Content -LiteralPath $reportPath -Encoding utf8NoBOM
    Write-Output "result: $outcome"
    Write-Output "report: $reportPath"
    Write-Output "data changes: $($changes.Count)"
    Write-Output "reader mode: $readerExecutionMode"
    if ($null -ne $readerExecutableSha256) { Write-Output "reader sha256: $readerExecutableSha256" }
    foreach ($reason in $unavailable) { Write-Output "not run: $reason" }
    foreach ($reason in $executionErrors) { Write-Output "error: $reason" }
    if ($outcome -in @('failed', 'incomplete')) { exit 1 }
}

$script:UserHome = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
Assert-NoReparsePoint -Path $script:RepositoryRoot
Assert-NoReparsePoint -Path $script:TargetRoot
Assert-NotUserHomePath -Path $script:AcceptanceRoot
Assert-NoReparsePoint -Path $script:AcceptanceRoot
if (-not (Test-Path -LiteralPath $script:AcceptanceRoot)) {
    New-Item -ItemType Directory -Path $script:AcceptanceRoot -Force | Out-Null
}
Assert-NoReparsePoint -Path $script:AcceptanceRoot
if ($Mode -eq 'Prepare') {
    Prepare-Run -Id $RunId
} else {
    Invoke-PreparedRun -Id $RunId -ReaderExecutablePath $PrebuiltReaderExe -ReaderHash $ExpectedReaderSha256
}

