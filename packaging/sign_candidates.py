#!/usr/bin/env python3
# @author kongweiguang
"""用现有签名工具分别生成 GPUI 与旧版更新器的候选 sidecar。"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

import build_candidate_inventory
import build_manifests


class CandidateSigningError(ValueError):
    """表示候选集或签名环境不完整，错误不包含私钥与工具输出。"""


def sign_candidates(candidate_directory: Path, signer: str) -> None:
    """只签发当前候选矩阵的必需对象；新旧私钥仅注入各自的子进程环境。"""
    summary = build_candidate_inventory.validate_unsigned_candidate_set(candidate_directory)
    root = candidate_directory.resolve(strict=True)
    signatures = root / "signatures"
    if signatures.exists() or signatures.is_symlink():
        raise CandidateSigningError("候选集已存在签名目录，拒绝覆盖或复用")
    signer_path = shutil.which(signer)
    if signer_path is None:
        raise CandidateSigningError("找不到 cargo-packager 签名工具")
    signer_path = os.path.abspath(signer_path)
    keys = {
        "new": (
            os.environ.get("KERMINAL_GPUI_SIGNING_PRIVATE_KEY", ""),
            os.environ.get("KERMINAL_GPUI_SIGNING_PRIVATE_KEY_PASSWORD", ""),
            build_manifests.NEW_UPDATER_KINDS,
        ),
        "legacy": (
            os.environ.get("TAURI_SIGNING_PRIVATE_KEY", ""),
            os.environ.get("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", ""),
            build_manifests.LEGACY_UPDATER_KINDS,
        ),
    }
    if not keys["new"][0] or not keys["legacy"][0]:
        raise CandidateSigningError("新旧 updater 的签名凭据均须显式提供")
    if keys["new"][0] == keys["legacy"][0]:
        raise CandidateSigningError("新旧 updater 不能使用同一签名私钥")
    secret_names = {
        "KERMINAL_GPUI_SIGNING_PRIVATE_KEY",
        "KERMINAL_GPUI_SIGNING_PRIVATE_KEY_PASSWORD",
        "TAURI_SIGNING_PRIVATE_KEY",
        "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
        "CARGO_PACKAGER_SIGN_PRIVATE_KEY",
        "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD",
    }
    base_environment = {key: value for key, value in os.environ.items() if key not in secret_names}
    base_environment["RUST_LOG"] = "warn"
    for purpose, (private_key, password, kinds) in keys.items():
        child_environment = dict(base_environment)
        child_environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY"] = private_key
        child_environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD"] = password
        for record in summary:
            if record["kind"] not in kinds:
                continue
            _, artifact = build_manifests.safe_path(root, record["path"])
            generated = artifact.with_name(artifact.name + ".sig")
            destination = signatures / purpose / record["target"] / generated.name
            if generated.exists() or generated.is_symlink() or destination.exists() or destination.is_symlink():
                raise CandidateSigningError("候选签名 sidecar 已存在，拒绝覆盖")
            destination.parent.mkdir(parents=True, exist_ok=True)
            try:
                result = subprocess.run(
                    [signer_path, "signer", "-q", "sign", str(artifact)],
                    check=False,
                    env=child_environment,
                    cwd=root,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            except OSError as error:
                raise CandidateSigningError("无法启动签名工具") from error
            if result.returncode != 0 or generated.is_symlink() or not generated.is_file():
                raise CandidateSigningError("候选签名失败，禁止生成或发布 metadata")
            os.rename(generated, destination)


def main() -> int:
    """读取 CI 注入的两个现有签名密钥，只生成本地 sidecar。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate-dir", type=Path, required=True)
    parser.add_argument("--signer", default="cargo-packager")
    arguments = parser.parse_args()
    try:
        sign_candidates(arguments.candidate_dir, arguments.signer)
    except (
        CandidateSigningError,
        build_candidate_inventory.CandidateInventoryError,
        build_manifests.ManifestError,
        OSError,
    ) as error:
        print(f"candidate signing failed: {error}", file=sys.stderr)
        return 2
    print("Candidate sidecars generated locally; signatures still require independent verification.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
