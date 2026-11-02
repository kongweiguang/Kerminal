#!/usr/bin/env python3
# @author kongweiguang
"""将已验签的候选产物整理成与 GitHub Release feed 一致的平铺文件集。"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

import build_manifests


def _copy_verified(source: Path, destination: Path, expected_hash: str) -> None:
    """复制前后均校验原始文件摘要，避免输入变化使 feed 指向未经验证的字节。"""
    digest = hashlib.sha256()
    with source.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected_hash:
        raise build_manifests.ManifestError("待发布文件摘要与已验证清单不一致")
    shutil.copyfile(source, destination)
    copied = hashlib.sha256()
    with destination.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            copied.update(chunk)
    if copied.hexdigest() != expected_hash:
        raise build_manifests.ManifestError("发布暂存文件复制后摘要不一致")


def stage_release_assets(candidate: Path, metadata: Path, destination: Path, base_url: str) -> None:
    """只暂存完整的 production 签验结果；失败时不产生可误上传的目标目录。"""
    base_url = build_manifests.validate_base_url(base_url, "production")
    if candidate.is_symlink() or metadata.is_symlink() or destination.is_symlink():
        raise build_manifests.ManifestError("发布输入与输出不得是符号链接")
    root = candidate.resolve(strict=True)
    metadata_root = metadata.resolve(strict=True)
    if not root.is_dir() or not metadata_root.is_dir() or destination.exists():
        raise build_manifests.ManifestError("候选、metadata 或输出目录状态无效")
    names = build_manifests.build_output_names("production")
    documents = {}
    document_bytes = {}
    for name in names.values():
        path = metadata_root / name
        if path.is_symlink() or not path.is_file():
            raise build_manifests.ManifestError("缺少完整的生产更新 metadata")
        try:
            document_bytes[name] = path.read_bytes()
            documents[name] = json.loads(document_bytes[name])
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise build_manifests.ManifestError("生产更新 metadata 无法读取") from error
    manifest = documents[names["manifest"]]
    if (
        not isinstance(manifest, dict)
        or manifest.get("schemaVersion") != 1
        or manifest.get("signingClass") != "production"
        or manifest.get("distributionLayout") != "github-release"
        or not isinstance(manifest.get("artifacts"), list)
    ):
        raise build_manifests.ManifestError("生产签名清单或 GitHub 资源布局无效")
    version = build_manifests.validate_version(manifest.get("version"))
    rows = manifest["artifacts"]
    by_name: dict[str, dict] = {}
    for row in rows:
        if not isinstance(row, dict):
            raise build_manifests.ManifestError("产物清单包含无效项")
        kind = row.get("kind")
        if kind == "app-bundle":
            continue
        name = row.get("githubAssetName")
        if not isinstance(name, str) or name.casefold() in by_name:
            raise build_manifests.ManifestError("GitHub 资源名缺失或冲突")
        by_name[name.casefold()] = row
        platform, arch, path = row.get("platform"), row.get("arch"), row.get("fileName")
        if not all(isinstance(value, str) for value in (platform, arch, path)):
            raise build_manifests.ManifestError("产物身份缺失")
        if kind == "signature":
            purposes = row.get("signingPurpose")
            if not isinstance(purposes, list) or len(purposes) != 1 or not path.endswith(".sig"):
                raise build_manifests.ManifestError("签名资源用途无效")
            expected_name = build_manifests.github_asset_name(platform, arch, path[:-4], purposes[0])
            if row.get("signingStatus") != "verified":
                raise build_manifests.ManifestError("签名资源未完成验签")
        else:
            expected_name = build_manifests.github_asset_name(platform, arch, path)
            if kind in build_manifests.NEW_UPDATER_KINDS and row.get("signingStatus") != "verified":
                raise build_manifests.ManifestError("新 updater 产物未完成验签")
            if kind in build_manifests.LEGACY_UPDATER_KINDS and row.get("legacySigningStatus") != "verified":
                raise build_manifests.ManifestError("旧 updater 产物未完成验签")
        if name != expected_name:
            raise build_manifests.ManifestError("GitHub 资源名与实际产物不匹配")

    for feed_name, purpose, expected_platforms in (
        (names["cargo"], "new-updater", {"windows-x86_64", "linux-x86_64", "macos-x86_64", "macos-aarch64"}),
        (names["legacy"], "legacy-tauri", {"windows-x86_64", "linux-x86_64", "darwin-x86_64", "darwin-aarch64"}),
    ):
        feed = documents[feed_name]
        if not isinstance(feed, dict) or feed.get("version") != version:
            raise build_manifests.ManifestError("更新清单版本与产物清单不一致")
        platforms = feed.get("platforms")
        if not isinstance(platforms, dict) or set(platforms) != expected_platforms:
            raise build_manifests.ManifestError("更新清单平台矩阵不完整")
        for platform_key, entry in platforms.items():
            if not isinstance(entry, dict) or not isinstance(entry.get("url"), str):
                raise build_manifests.ManifestError("更新清单缺少资源地址")
            url = entry["url"]
            asset_name = urlsplit(url).path.rsplit("/", 1)[-1]
            if url != f"{base_url}/{asset_name}" or asset_name.casefold() not in by_name:
                raise build_manifests.ManifestError("更新清单指向未暂存的资源")
            platform, arch = platform_key.split("-", 1)
            platform = "macos" if platform == "darwin" else platform
            payload = by_name[asset_name.casefold()]
            expected_kind = {
                "windows": "nsis",
                "linux": "appimage-updater" if purpose == "new-updater" else "appimage",
                "macos": "app-updater",
            }.get(platform)
            if (
                expected_kind is None
                or payload.get("platform") != platform
                or payload.get("arch") != arch
                or payload.get("kind") != expected_kind
                or (purpose == "new-updater" and entry.get("format") != build_manifests.KIND_BUNDLE[expected_kind])
            ):
                raise build_manifests.ManifestError("更新清单的平台、架构或产物格式不匹配")
            signature_name = f"{asset_name}.{purpose}.sig"
            signature_row = by_name.get(signature_name.casefold())
            if signature_row is None:
                raise build_manifests.ManifestError("更新清单缺少对应的已验签 sidecar")
            encoded_signature, _, _ = build_manifests.read_signature(root, signature_row["fileName"])
            if entry.get("signature") != encoded_signature:
                raise build_manifests.ManifestError("更新清单签名与已验证 sidecar 不一致")

    destination_parent = destination.parent.resolve(strict=True)
    if not destination_parent.is_dir() or destination.parent.is_symlink():
        raise build_manifests.ManifestError("发布暂存父目录无效")
    resolved_destination = destination_parent / destination.name
    if resolved_destination.is_relative_to(root) or resolved_destination.is_relative_to(metadata_root):
        raise build_manifests.ManifestError("发布暂存目录不能位于候选或 metadata 内")
    with tempfile.TemporaryDirectory(prefix="kerminal-release-", dir=destination_parent) as staging:
        staging_path = Path(staging) / "assets"
        staging_path.mkdir()
        for row in by_name.values():
            _, source = build_manifests.safe_path(root, row["fileName"])
            expected_hash = row.get("sha256")
            if not isinstance(expected_hash, str):
                raise build_manifests.ManifestError("发布资源缺少摘要")
            _copy_verified(source, staging_path / row["githubAssetName"], expected_hash)
        for name, contents in document_bytes.items():
            (staging_path / name).write_bytes(contents)
        os.rename(staging_path, destination)


def main() -> int:
    """执行纯本地暂存，不创建 Release、不上传资产。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate-dir", type=Path, required=True)
    parser.add_argument("--metadata-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    arguments = parser.parse_args()
    try:
        stage_release_assets(
            arguments.candidate_dir, arguments.metadata_dir, arguments.output_dir, arguments.base_url
        )
    except (build_manifests.ManifestError, OSError) as error:
        print(f"release asset staging failed: {error}", file=sys.stderr)
        return 2
    print("Verified GitHub release assets staged locally; nothing was published.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
