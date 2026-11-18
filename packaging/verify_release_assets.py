#!/usr/bin/env python3
# @author kongweiguang
"""复核已签名候选，确保正式草稿只包含同一提交的完整更新资产。"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from pathlib import Path

import build_manifests
import updater_key


def verify_assets(root: Path, version: str, commit: str, public_key: str, legacy_key: str) -> None:
    """复核文件、双 feed 和签名，防止跨运行或文件篡改绕过候选流水线。"""
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("无效的发布提交")
    base_url = f"https://github.com/kongweiguang/Kerminal/releases/download/v{version}"
    manifest = json.loads((root / "artifact-manifest.json").read_text(encoding="utf-8"))
    if (manifest.get("version"), manifest.get("sourceCommit"), manifest.get("signingClass"),
            manifest.get("distributionLayout")) != (version, commit, "production", "github-release"):
        raise ValueError("签名清单与发布身份不匹配")
    rows = manifest.get("artifacts")
    if not isinstance(rows, list):
        raise ValueError("签名清单缺少产物")
    by_name = {}
    identities = set()
    for row in rows:
        kind = row.get("kind")
        if kind == "app-bundle":
            identities.add((row.get("platform"), row.get("arch"), kind))
            continue
        name = row.get("githubAssetName")
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9._-]+", name) or name.casefold() in by_name:
            raise ValueError("资产名称无效或重复")
        asset = root / name
        if asset.is_symlink() or not asset.is_file():
            raise ValueError("资产缺失或不是普通文件")
        with asset.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if digest != row.get("sha256"):
            raise ValueError("资产缺失或摘要不匹配")
        by_name[name.casefold()] = row
        if kind != "signature":
            identity = (row.get("platform"), row.get("arch"), kind)
            if identity in identities:
                raise ValueError("产物身份重复")
            identities.add(identity)
    if identities != build_manifests.REQUIRED_ARTIFACTS:
        raise ValueError("平台产物矩阵不完整")
    checksum = json.loads((root / "SHA256SUMS.json").read_text(encoding="utf-8"))
    checksum_rows = checksum.get("artifacts")
    if checksum.get("version") != version or not isinstance(checksum_rows, list):
        raise ValueError("摘要清单版本或内容无效")
    checks = {row.get("githubAssetName", row.get("fileName")): row.get("sha256") for row in checksum_rows}
    for row in rows:
        if checks.get(row.get("githubAssetName", row.get("fileName"))) != row.get("sha256"):
            raise ValueError("摘要清单与产物清单不一致")
    expected_files = set(by_name) | {name.casefold() for name in build_manifests.build_output_names("production").values()}
    if {path.name.casefold() for path in root.iterdir()} != expected_files:
        raise ValueError("正式资产包含遗漏或额外文件")
    for feed_name, purpose, key, platform_names in (
        ("kerminal-updater.json", "new-updater", public_key, {"windows-x86_64", "linux-x86_64", "macos-x86_64", "macos-aarch64"}),
        ("latest.json", "legacy-tauri", legacy_key, {"windows-x86_64", "linux-x86_64", "darwin-x86_64", "darwin-aarch64"}),
    ):
        feed = json.loads((root / feed_name).read_text(encoding="utf-8"))
        platforms = feed.get("platforms")
        if feed.get("version") != version or not isinstance(platforms, dict) or set(platforms) != platform_names:
            raise ValueError("更新清单版本或平台不完整")
        for platform_key, entry in platforms.items():
            platform, arch = platform_key.split("-", 1)
            platform = "macos" if platform == "darwin" else platform
            kind = {"windows": "nsis", "macos": "app-updater", "linux": "appimage-updater" if purpose == "new-updater" else "appimage"}[platform]
            matches = [row for row in by_name.values() if (row.get("platform"), row.get("arch"), row.get("kind")) == (platform, arch, kind)]
            if len(matches) != 1:
                raise ValueError("更新目标没有唯一产物")
            name = matches[0]["githubAssetName"]
            signature_name = f"{name}.{purpose}.sig"
            signature_row = by_name.get(signature_name.casefold())
            if not signature_row or signature_row.get("signingPurpose") != [purpose] or signature_row.get("signingStatus") != "verified":
                raise ValueError("更新目标缺少对应签名")
            encoded, _, _ = build_manifests.read_signature(root, signature_name)
            if entry.get("url") != f"{base_url}/{name}" or entry.get("signature") != encoded:
                raise ValueError("更新地址或签名与资产不一致")
            if purpose == "new-updater" and entry.get("format") != build_manifests.KIND_BUNDLE[kind]:
                raise ValueError("更新格式与目标不匹配")
            build_manifests.verify_signature(root, root / name, root / signature_name, key, "minisign")


def main() -> int:
    """以环境变量读取公钥，使用现有验签工具避免公开私钥或跳过校验。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    arguments = parser.parse_args()
    try:
        key = updater_key.decode_public_key(os.environ.get("KERMINAL_GPUI_UPDATER_PUBLIC_KEY", "")).splitlines()[1]
        legacy = Path("packaging/tauri-legacy-updater.pub").read_text(encoding="ascii").splitlines()[1]
        if key == legacy:
            raise ValueError("新旧更新器必须使用独立公钥")
        verify_assets(arguments.assets, arguments.version, arguments.commit, key, legacy)
    except (ValueError, OSError, KeyError, IndexError, TypeError, build_manifests.ManifestError, updater_key.UpdateKeyError) as error:
        print(f"发布资产校验失败：{error}", file=sys.stderr)
        return 2
    print("正式资产的版本、摘要和两套签名已验证。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
