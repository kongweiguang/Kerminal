#!/usr/bin/env python3
"""Validate staged release artifacts and build distinct updater contracts.

@author kongweiguang
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path, PurePosixPath
from typing import Any, Callable
from urllib.parse import quote, urlsplit


class ManifestError(ValueError):
    """描述可修复的打包清单错误，不输出签名或密钥内容。"""


@dataclass(frozen=True)
class Artifact:
    """规范化后的单个实际产物及其经验证的本地来源。"""

    platform: str
    arch: str
    kind: str
    path: str
    signature_path: str | None
    legacy_signature_path: str | None
    size_bytes: int
    sha256: str
    signature: str | None
    signature_sha256: str | None
    legacy_signature: str | None
    legacy_signature_sha256: str | None
    signing_status: str
    legacy_signing_status: str


REQUIRED_ARTIFACTS = {
    ("windows", "x86_64", "nsis"),
    ("linux", "x86_64", "appimage"),
    ("linux", "x86_64", "deb"),
    ("linux", "x86_64", "appimage-updater"),
    ("macos", "x86_64", "app-bundle"),
    ("macos", "x86_64", "dmg"),
    ("macos", "x86_64", "app-updater"),
    ("macos", "aarch64", "app-bundle"),
    ("macos", "aarch64", "dmg"),
    ("macos", "aarch64", "app-updater"),
}

NEW_UPDATER_KINDS = {"nsis", "appimage-updater", "app-updater"}
LEGACY_UPDATER_KINDS = {"nsis", "appimage", "app-updater"}
KIND_BUNDLE = {
    "nsis": "nsis",
    "appimage": "appimage",
    "deb": "deb",
    "app-bundle": "app",
    "dmg": "dmg",
    "appimage-updater": "appimage",
    "app-updater": "app",
}
KIND_EXTENSIONS = {
    "nsis": ".exe",
    "appimage": ".appimage",
    "deb": ".deb",
    "app-bundle": ".app",
    "dmg": ".dmg",
    "appimage-updater": ".appimage.tar.gz",
    "app-updater": ".app.tar.gz",
}


def validate_version(value: Any) -> str:
    """限制版本为可被 Rust semver 与两个更新器共同解析的完整版本。"""
    if not isinstance(value, str):
        raise ManifestError("清单缺少字符串 version")
    pattern = r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$"
    if not re.fullmatch(pattern, value):
        raise ManifestError("version 不是有效的完整 SemVer")
    return value


def validate_release_date(value: Any) -> str | None:
    """只接受带时区的 RFC 3339 发布时间，避免本地时区产生歧义。"""
    if value is None:
        return None
    if not isinstance(value, str):
        raise ManifestError("publishedAt 必须是 RFC 3339 字符串或 null")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise ManifestError("publishedAt 不是有效的 RFC 3339 日期") from error
    if parsed.tzinfo is None:
        raise ManifestError("publishedAt 必须包含时区")
    return value


def validate_base_url(value: str, signing_class: str) -> str:
    """拒绝不安全或可能误发的 URL，测试签名只能指向保留的 .invalid 域名。"""
    parsed = urlsplit(value)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
        raise ManifestError("base URL 必须是无凭据的 HTTPS URL")
    if parsed.query or parsed.fragment:
        raise ManifestError("base URL 不允许 query 或 fragment")
    if signing_class == "test" and not parsed.hostname.lower().endswith(".invalid"):
        raise ManifestError("测试签名的 base URL 必须使用 .invalid 域名")
    return value.rstrip("/")


def safe_path(root: Path, value: Any) -> tuple[str, Path]:
    """把清单相对路径约束在产物 staging 根目录内，拒绝跨盘与目录穿越。"""
    if not isinstance(value, str) or not value or "\\" in value:
        raise ManifestError("产物路径必须是非空 POSIX 相对路径")
    relative = PurePosixPath(value)
    if relative.is_absolute() or any(part in ("", ".", "..") for part in relative.parts):
        raise ManifestError("产物路径不得为绝对路径或包含路径回退")
    resolved_root = root.resolve(strict=True)
    candidate = root.joinpath(*relative.parts)
    try:
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(resolved_root)
    except (OSError, ValueError) as error:
        raise ManifestError("产物路径不存在或超出 staging 根目录") from error
    return relative.as_posix(), candidate


def paths_overlap(first: str, second: str) -> bool:
    """拒绝把 bundle 内文件、bundle 目录或签名 sidecar 交叉登记成两个产物。"""
    return (
        first == second
        or first.startswith(second.rstrip("/") + "/")
        or second.startswith(first.rstrip("/") + "/")
    )


def fingerprint(root: Path, relative: str, candidate: Path) -> tuple[int, str, set[str]]:
    """对普通文件或 .app 目录生成稳定哈希，同时枚举用于拒绝未登记文件。"""
    if candidate.is_symlink():
        raise ManifestError("产物入口不得是符号链接")
    if candidate.is_file():
        digest = hashlib.sha256()
        size_bytes = 0
        with candidate.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                size_bytes += len(chunk)
                digest.update(chunk)
        return size_bytes, digest.hexdigest(), {relative}
    root_resolved = root.resolve(strict=True)
    digest = hashlib.sha256()
    size_bytes = 0
    included_paths: set[str] = set()

    def add_file(path: Path, relative_path: str) -> None:
        nonlocal size_bytes
        stat = path.lstat()
        normalized = PurePosixPath(relative_path).as_posix()
        included_paths.add(normalized)
        digest.update(b"F\0" + normalized.encode("utf-8") + b"\0")
        if path.is_symlink():
            try:
                target = path.resolve(strict=True)
                target.relative_to(root_resolved)
            except (OSError, ValueError) as error:
                raise ManifestError(".app 内符号链接必须指向 staging 根目录内") from error
            link_text = os.readlink(path).replace("\\", "/").encode("utf-8")
            digest.update(b"L" + len(link_text).to_bytes(8, "big") + link_text)
            return
        if not path.is_file():
            raise ManifestError("产物仅允许普通文件、目录和内部符号链接")
        size_bytes += stat.st_size
        digest.update(b"B" + stat.st_size.to_bytes(8, "big"))
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)

    if candidate.is_dir():
        if not relative.lower().endswith(".app"):
            raise ManifestError("只有 .app bundle 可以作为目录型产物")
        included_paths.add(relative)
        digest.update(b"D\0" + relative.encode("utf-8") + b"\0")
        for current, directories, files in os.walk(candidate, followlinks=False):
            current_path = Path(current)
            directories.sort()
            files.sort()
            for name in list(directories):
                path = current_path / name
                nested = path.relative_to(root).as_posix()
                if path.is_symlink():
                    add_file(path, nested)
                    directories.remove(name)
                else:
                    included_paths.add(nested)
                    digest.update(b"D\0" + nested.encode("utf-8") + b"\0")
            for name in files:
                add_file(current_path / name, (current_path / name).relative_to(root).as_posix())
    else:
        add_file(candidate, relative)
    return size_bytes, digest.hexdigest(), included_paths


def read_signature(root: Path, relative: str | None) -> tuple[str | None, str | None, set[str]]:
    """读取非空 ASCII minisign sidecar；密钥验证由单独的 verifier 完成。"""
    if relative is None:
        return None, None, set()
    normalized, path = safe_path(root, relative)
    if path.is_dir() or path.is_symlink():
        raise ManifestError("签名 sidecar 必须是普通文件")
    try:
        signature_bytes = path.read_bytes()
        content = signature_bytes.decode("ascii").strip()
    except (OSError, UnicodeError) as error:
        raise ManifestError("签名 sidecar 不存在或不是 ASCII 文本") from error
    if not content or "\x00" in content:
        raise ManifestError("签名 sidecar 为空或格式损坏")
    return content, hashlib.sha256(signature_bytes).hexdigest(), {normalized}


def verify_signature(
    root: Path, artifact_path: Path, signature_path: Path, public_key: str, verifier: str
) -> None:
    """使用 minisign 对 staging 中的原始产物做验签，失败时不泄漏签名正文。"""
    command = [verifier, "-Vm", str(artifact_path), "-P", public_key, "-x", str(signature_path)]
    try:
        result = subprocess.run(command, check=False, capture_output=True, text=True)
    except OSError as error:
        raise ManifestError("找不到 minisign 验签程序") from error
    if result.returncode != 0:
        raise ManifestError("minisign 验签失败；已拒绝生成可安装更新元数据")


def _check_artifact_kind(kind: Any, platform: Any, arch: Any, path: str, candidate: Path) -> tuple[str, str, str]:
    """按平台协议校验每个产物的实际扩展名和目录形态。"""
    if not all(isinstance(item, str) for item in (kind, platform, arch)):
        raise ManifestError("产物 platform、arch、kind 必须是字符串")
    key = (platform, arch, kind)
    if key not in REQUIRED_ARTIFACTS:
        raise ManifestError("清单包含不支持的平台、架构或产物类型")
    expected = KIND_EXTENSIONS[kind]
    if not path.lower().endswith(expected):
        raise ManifestError(f"{kind} 产物扩展名不匹配")
    if (kind == "app-bundle") != candidate.is_dir():
        raise ManifestError("只有 macOS app-bundle 产物应为目录")
    return platform, arch, kind


def validate_inventory(
    root: Path,
    inventory: Any,
    base_url: str,
    signing_class: str,
    public_key: str | None = None,
    legacy_public_key: str | None = None,
    verifier: str = "minisign",
    verify_crypto: bool = True,
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    """分别验证新旧 updater 信任根，再构造两个互不混用的 JSON 契约。"""
    if not isinstance(inventory, dict) or inventory.get("schemaVersion") != 1:
        raise ManifestError("artifact inventory schemaVersion 必须为 1")
    version = validate_version(inventory.get("version"))
    source_commit = inventory.get("sourceCommit")
    if not isinstance(source_commit, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", source_commit):
        raise ManifestError("sourceCommit 必须是 40 位 Git commit SHA")
    published_at = validate_release_date(inventory.get("publishedAt"))
    notes = inventory.get("notes", "")
    if not isinstance(notes, str):
        raise ManifestError("notes 必须是字符串")
    base_url = validate_base_url(base_url, signing_class)
    if verify_crypto and not public_key:
        raise ManifestError("新 updater 验签要求 public key 内容")
    if verify_crypto and not legacy_public_key:
        raise ManifestError("旧 Tauri updater 验签要求单独提供 public key 内容")
    if verify_crypto and public_key == legacy_public_key:
        raise ManifestError("新旧 updater 必须使用不同的 minisign 公钥")
    raw_artifacts = inventory.get("artifacts")
    if not isinstance(raw_artifacts, list):
        raise ManifestError("artifacts 必须是数组")

    seen: set[tuple[str, str, str]] = set()
    payload_paths: set[str] = set()
    covered_paths: set[str] = set()
    signature_paths: dict[str, tuple[str, str, str]] = {}
    app_bundle_paths: set[str] = set()
    artifacts: list[Artifact] = []
    new_updates: dict[str, Artifact] = {}
    legacy_updates: dict[str, Artifact] = {}
    for raw in raw_artifacts:
        if not isinstance(raw, dict):
            raise ManifestError("每个 artifact 必须是对象")
        if set(raw) != {
            "platform",
            "arch",
            "kind",
            "path",
            "signaturePath",
            "legacySignaturePath",
        }:
            raise ManifestError(
                "artifact 字段必须是 platform、arch、kind、path、signaturePath、legacySignaturePath"
            )
        relative, path = safe_path(root, raw["path"])
        platform, arch, kind = _check_artifact_kind(raw["kind"], raw["platform"], raw["arch"], relative, path)
        identity = (platform, arch, kind)
        if identity in seen:
            raise ManifestError("产物矩阵存在重复平台、架构和类型")
        seen.add(identity)
        if relative in payload_paths or any(
            paths_overlap(relative, covered_path) for covered_path in covered_paths
        ) or any(paths_overlap(relative, signature_path) for signature_path in signature_paths):
            raise ManifestError("多个产物不能共享同一路径")
        payload_paths.add(relative)
        size_bytes, payload_hash, payload_paths_for_inventory = fingerprint(root, relative, path)
        if size_bytes == 0:
            raise ManifestError("零字节产物不能进入发布清单")
        covered_paths.update(payload_paths_for_inventory)
        if kind == "app-bundle":
            app_bundle_paths.add(relative)

        signature_path_value = raw["signaturePath"]
        legacy_signature_path_value = raw["legacySignaturePath"]
        needs_new_signature = kind in NEW_UPDATER_KINDS
        needs_legacy_signature = kind in LEGACY_UPDATER_KINDS
        if (signature_path_value is not None) != needs_new_signature:
            raise ManifestError("新 updater 产物必须且仅能使用新签名 sidecar")
        if (legacy_signature_path_value is not None) != needs_legacy_signature:
            raise ManifestError("旧 Tauri updater 产物必须且仅能使用旧签名 sidecar")
        signature_path = None
        legacy_signature_path = None
        signature_text = None
        signature_hash = None
        legacy_signature_text = None
        legacy_signature_hash = None
        status = "not-applicable"
        legacy_status = "not-applicable"
        if needs_new_signature:
            signature_path, sidecar_path = safe_path(root, signature_path_value)
            signature_text, signature_hash, signature_files = read_signature(root, signature_path)
            covered_paths.update(signature_files)
            if paths_overlap(signature_path, relative) or any(
                paths_overlap(signature_path, payload) for payload in payload_paths
            ):
                raise ManifestError("新签名 sidecar 路径与产物路径冲突")
            if any(paths_overlap(signature_path, covered) for covered in covered_paths - signature_files):
                raise ManifestError("新签名 sidecar 路径与产物内容冲突")
            owner = signature_paths.get(signature_path)
            if owner is not None and owner != identity:
                raise ManifestError("不同 updater 产物不能共享签名 sidecar")
            signature_paths[signature_path] = identity
            if verify_crypto:
                if not public_key:
                    raise ManifestError("新 updater 验签要求 public key 内容")
                verify_signature(root, path, sidecar_path, public_key, verifier)
                status = "verified"
            elif signing_class == "test":
                status = "unverified-test"
            else:
                raise ManifestError("生产 metadata 禁止跳过 minisign 验签")

        if needs_legacy_signature:
            legacy_signature_path, legacy_sidecar_path = safe_path(root, legacy_signature_path_value)
            legacy_signature_text, legacy_signature_hash, legacy_signature_files = read_signature(
                root, legacy_signature_path
            )
            covered_paths.update(legacy_signature_files)
            if legacy_signature_path == signature_path:
                raise ManifestError("新旧 updater 必须使用独立签名 sidecar")
            if paths_overlap(legacy_signature_path, relative) or any(
                paths_overlap(legacy_signature_path, payload) for payload in payload_paths
            ):
                raise ManifestError("旧签名 sidecar 路径与产物路径冲突")
            if any(
                paths_overlap(legacy_signature_path, covered)
                for covered in covered_paths - legacy_signature_files
            ):
                raise ManifestError("旧签名 sidecar 路径与产物内容冲突")
            owner = signature_paths.get(legacy_signature_path)
            if owner is not None and owner != identity:
                raise ManifestError("不同 updater 产物不能共享签名 sidecar")
            signature_paths[legacy_signature_path] = identity
            if verify_crypto:
                if not legacy_public_key:
                    raise ManifestError("旧 Tauri updater 验签要求 public key 内容")
                verify_signature(root, path, legacy_sidecar_path, legacy_public_key, verifier)
                legacy_status = "verified"
            elif signing_class == "test":
                status = "unverified-test"
                legacy_status = "unverified-test"
            else:
                raise ManifestError("生产 metadata 禁止跳过 minisign 验签")
        else:
            covered_paths.update(payload_paths_for_inventory)
        artifact = Artifact(
            platform=platform,
            arch=arch,
            kind=kind,
            path=relative,
            signature_path=signature_path,
            legacy_signature_path=legacy_signature_path,
            size_bytes=size_bytes,
            sha256=payload_hash,
            signature=signature_text,
            signature_sha256=signature_hash,
            legacy_signature=legacy_signature_text,
            legacy_signature_sha256=legacy_signature_hash,
            signing_status=status,
            legacy_signing_status=legacy_status,
        )
        artifacts.append(artifact)
        if needs_new_signature:
            new_updates[identity[0] + "-" + identity[1]] = artifact
        if needs_legacy_signature:
            legacy_updates[identity[0] + "-" + identity[1]] = artifact

    if seen != REQUIRED_ARTIFACTS:
        missing = sorted(REQUIRED_ARTIFACTS - seen)
        extra = sorted(seen - REQUIRED_ARTIFACTS)
        raise ManifestError(f"平台产物矩阵不完整；missing={missing}, extra={extra}")

    actual_paths: set[str] = set()
    for current, directories, files in os.walk(root, followlinks=False):
        current_path = Path(current)
        for directory in list(directories):
            path = current_path / directory
            relative_path = path.relative_to(root).as_posix()
            if path.is_symlink():
                actual_paths.add(relative_path)
                directories.remove(directory)
            elif any(
                relative_path == app_path or relative_path.startswith(app_path + "/")
                for app_path in app_bundle_paths
            ):
                actual_paths.add(relative_path)
        actual_paths.update((current_path / filename).relative_to(root).as_posix() for filename in files)
    if actual_paths != covered_paths:
        unlisted = sorted(actual_paths - covered_paths)
        missing = sorted(covered_paths - actual_paths)
        raise ManifestError(f"staging 文件与 inventory 不一致；unlisted={unlisted}, missing={missing}")

    cargo_platforms: dict[str, Any] = {}
    tauri_platforms: dict[str, Any] = {}
    for key, artifact in sorted(new_updates.items()):
        platform_key = key
        url = f"{base_url}/{quote(artifact.path, safe='/-._~')}"
        cargo_platforms[platform_key] = {
            "url": url,
            "signature": artifact.signature,
            "format": KIND_BUNDLE[artifact.kind],
        }

    for key, artifact in sorted(legacy_updates.items()):
        old_platform = "darwin" if artifact.platform == "macos" else artifact.platform
        old_key = f"{old_platform}-{artifact.arch}"
        url = f"{base_url}/{quote(artifact.path, safe='/-._~')}"
        tauri_platforms[old_key] = {"url": url, "signature": artifact.legacy_signature}

    expected_platforms = {
        "windows-x86_64",
        "linux-x86_64",
        "macos-x86_64",
        "macos-aarch64",
    }
    if set(cargo_platforms) != expected_platforms:
        raise ManifestError("updater metadata 平台 key 与 runtime 架构不一致")
    if set(tauri_platforms) != {
        "windows-x86_64",
        "linux-x86_64",
        "darwin-x86_64",
        "darwin-aarch64",
    }:
        raise ManifestError("legacy Tauri metadata 平台 key 与 v0.3.45 不一致")

    header: dict[str, Any] = {"version": version, "notes": notes}
    if published_at is not None:
        header["pub_date"] = published_at
    cargo_metadata = {**header, "platforms": cargo_platforms}
    legacy_metadata = {**header, "platforms": tauri_platforms}

    artifact_rows: list[dict[str, Any]] = []
    checksum_rows: list[dict[str, Any]] = []
    for artifact in sorted(artifacts, key=lambda item: (item.platform, item.arch, item.kind)):
        artifact_rows.append(
            {
                "fileName": artifact.path,
                "platform": artifact.platform,
                "arch": artifact.arch,
                "bundle": KIND_BUNDLE[artifact.kind],
                "kind": artifact.kind,
                "sizeBytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "signatureFile": artifact.signature_path,
                "signatureSha256": artifact.signature_sha256,
                "legacySignatureFile": artifact.legacy_signature_path,
                "legacySignatureSha256": artifact.legacy_signature_sha256,
                "signingStatus": artifact.signing_status,
                "legacySigningStatus": artifact.legacy_signing_status,
            }
        )
        checksum_rows.append({"fileName": artifact.path, "sha256": artifact.sha256})
        for purpose, sidecar_path, sidecar_hash, sidecar_status in (
            (
                "new-updater",
                artifact.signature_path,
                artifact.signature_sha256,
                artifact.signing_status,
            ),
            (
                "legacy-tauri",
                artifact.legacy_signature_path,
                artifact.legacy_signature_sha256,
                artifact.legacy_signing_status,
            ),
        ):
            if sidecar_path is None or sidecar_hash is None:
                continue
            existing_signature = next(
                (
                    row
                    for row in artifact_rows
                    if row["kind"] == "signature" and row["fileName"] == sidecar_path
                ),
                None,
            )
            if existing_signature is None:
                artifact_rows.append(
                    {
                        "fileName": sidecar_path,
                        "platform": artifact.platform,
                        "arch": artifact.arch,
                        "bundle": "minisign-signature",
                        "kind": "signature",
                        "signingPurpose": [purpose],
                        "sizeBytes": Path(root, sidecar_path).stat().st_size,
                        "sha256": sidecar_hash,
                        "signatureFile": None,
                        "signatureSha256": None,
                        "signingStatus": sidecar_status,
                    }
                )
                checksum_rows.append({"fileName": sidecar_path, "sha256": sidecar_hash})
            else:
                existing_signature["signingPurpose"].append(purpose)

    artifact_manifest = {
        "schemaVersion": 1,
        "version": version,
        "sourceCommit": source_commit,
        "signingClass": signing_class,
        "artifacts": artifact_rows,
    }
    checksums = {"schemaVersion": 1, "version": version, "artifacts": checksum_rows}
    return cargo_metadata, legacy_metadata, {"artifactManifest": artifact_manifest, "checksums": checksums}


def _write_temporary(path: Path, payload: str | bytes) -> Path:
    """在目标同一目录完整落盘临时内容，确保后续替换留在同一文件系统。"""
    mode = "wb" if isinstance(payload, bytes) else "w"
    options: dict[str, Any] = {"dir": path.parent, "prefix": f".{path.name}.", "suffix": ".tmp", "delete": False}
    if mode == "w":
        options.update(encoding="utf-8", newline="\n")
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(mode=mode, **options) as stream:
            temporary_path = Path(stream.name)
            stream.write(payload)
        return temporary_path
    except BaseException:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
        raise


def write_output_set(output_dir: Path, output_values: dict[str, dict[str, Any]]) -> None:
    """先完整生成全套 JSON，再提交替换并在单文件失败时回滚已替换的旧 feed。"""
    output_dir.mkdir(parents=True, exist_ok=True)
    targets = {output_dir / name: json.dumps(value, ensure_ascii=False, indent=2) + "\n" for name, value in output_values.items()}
    staged: dict[Path, Path] = {}
    originals: dict[Path, bytes | None] = {}
    committed: list[Path] = []
    try:
        for target, serialized in targets.items():
            if target.is_symlink():
                raise ManifestError("更新 metadata 目标不能是符号链接")
            originals[target] = target.read_bytes() if target.exists() else None
            staged[target] = _write_temporary(target, serialized)

        for target, temporary in list(staged.items()):
            os.replace(temporary, target)
            committed.append(target)
            staged.pop(target)
    except OSError as error:
        rollback_failed = False
        for target in reversed(committed):
            try:
                original = originals[target]
                if original is None:
                    target.unlink(missing_ok=True)
                else:
                    backup = _write_temporary(target, original)
                    try:
                        os.replace(backup, target)
                    finally:
                        backup.unlink(missing_ok=True)
            except OSError:
                rollback_failed = True
        detail = "且回滚未完整完成" if rollback_failed else "并已恢复此前 metadata"
        raise ManifestError(f"写入更新 metadata 失败，{detail}") from error
    finally:
        for temporary in staged.values():
            temporary.unlink(missing_ok=True)


def build_output_names(signing_class: str) -> dict[str, str]:
    """为测试与正式签名产物使用不同文件名，防止测试 JSON 冒充 production feed。"""
    suffix = ".test" if signing_class == "test" else ""
    return {
        "cargo": f"kerminal-updater{suffix}.json",
        "legacy": "latest.tauri-legacy.test.json" if signing_class == "test" else "latest.json",
        "manifest": f"artifact-manifest{suffix}.json",
        "checksums": f"SHA256SUMS{suffix}.json",
    }


def parse_arguments(arguments: list[str] | None = None) -> argparse.Namespace:
    """要求调用者显式传入 staging、URL 和签名类别，杜绝默认生产端点。"""
    parser = argparse.ArgumentParser(description="校验 Kerminal 产物并生成独立 updater JSON")
    parser.add_argument("--artifact-root", type=Path, required=True)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--signing-class", choices=("test", "production"), required=True)
    parser.add_argument("--public-key", help="新 updater 的 minisign 公钥正文；不接受私钥")
    parser.add_argument("--legacy-public-key", help="旧 Tauri updater 的 minisign 公钥正文；不接受私钥")
    parser.add_argument("--minisign", default="minisign", help="minisign 可执行文件路径")
    parser.add_argument(
        "--skip-signature-verification",
        action="store_true",
        help="仅用于产生带 .test 后缀的结构测试文件，不验证密码学签名",
    )
    return parser.parse_args(arguments)


def main(arguments: list[str] | None = None) -> int:
    """校验真实 staging 产物后输出新旧协议文件；错误时不写入任何 metadata。"""
    options = parse_arguments(arguments)
    if options.signing_class == "production" and options.skip_signature_verification:
        raise ManifestError("production metadata 不允许跳过签名验证")
    if not options.skip_signature_verification and not options.public_key:
        raise ManifestError("新 updater 密码学验签必须提供 minisign 公钥正文")
    if options.signing_class == "production" and not options.legacy_public_key:
        raise ManifestError("production legacy Tauri metadata 必须提供旧 updater minisign 公钥正文")
    if not options.skip_signature_verification and not options.legacy_public_key:
        raise ManifestError("旧 Tauri updater minisign 公钥必须显式指定")
    if options.signing_class == "test" and options.skip_signature_verification:
        if not urlsplit(options.base_url).hostname or not urlsplit(options.base_url).hostname.endswith(".invalid"):
            raise ManifestError("未验签的测试 metadata 只能使用 .invalid 域名")

    try:
        inventory = json.loads(options.inventory.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ManifestError("无法读取有效的 artifact inventory JSON") from error
    cargo_metadata, legacy_metadata, report = validate_inventory(
        options.artifact_root,
        inventory,
        options.base_url,
        options.signing_class,
        public_key=options.public_key,
        legacy_public_key=options.legacy_public_key,
        verifier=options.minisign,
        verify_crypto=not options.skip_signature_verification,
    )
    names = build_output_names(options.signing_class)
    output_values = {
        names["cargo"]: cargo_metadata,
        names["legacy"]: legacy_metadata,
        names["manifest"]: report["artifactManifest"],
        names["checksums"]: report["checksums"],
    }
    write_output_set(options.output_dir, output_values)
    print(f"已生成 {len(output_values)} 份本地 metadata（signingClass={options.signing_class}）。")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ManifestError as error:
        print(f"打包清单错误：{error}", file=sys.stderr)
        raise SystemExit(2)
