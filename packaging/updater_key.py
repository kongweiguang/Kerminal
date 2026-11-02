#!/usr/bin/env python3
# @author kongweiguang
"""检查编译期 updater 公钥并为真实构建生成无秘密的信任根收据。"""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import sys
from pathlib import Path

import build_candidate_inventory


class UpdateKeyError(ValueError):
    """描述发布公钥或构建收据无效，不回显环境变量内容。"""


def decode_public_key(encoded: str) -> str:
    """与 Rust updater 一致地解析 Base64 包装的 Minisign 公钥文本。"""
    if not encoded or len(encoded) > 8192:
        raise UpdateKeyError("更新公钥缺失或过长")
    try:
        raw = base64.b64decode(encoded, validate=True)
        text = raw.decode("ascii")
    except (binascii.Error, UnicodeError, ValueError) as error:
        raise UpdateKeyError("更新公钥不是有效的 Base64 Minisign 文本") from error
    if base64.b64encode(raw).decode("ascii") != encoded:
        raise UpdateKeyError("更新公钥 Base64 编码不是规范形式")
    lines = text.splitlines()
    if len(lines) != 2 or not lines[0].lower().startswith("untrusted comment: minisign public key"):
        raise UpdateKeyError("更新公钥不是 Minisign 公钥文件")
    try:
        public_bytes = base64.b64decode(lines[1], validate=True)
    except (binascii.Error, ValueError) as error:
        raise UpdateKeyError("更新公钥内容无效") from error
    if len(public_bytes) != 42 or public_bytes[:2] not in (b"Ed", b"ED"):
        raise UpdateKeyError("更新公钥算法或长度无效")
    return text


def build_attestation(binary: Path, encoded: str, target: str, source_commit: str, version: str) -> dict:
    """逐块检查二进制包含预期公钥并记录摘要，不保存密钥或用户环境。"""
    decode_public_key(encoded)
    if target not in build_candidate_inventory.TARGETS:
        raise UpdateKeyError("构建 target 不受支持")
    if not re.fullmatch(r"[a-fA-F0-9]{40}", source_commit):
        raise UpdateKeyError("构建提交必须是完整 Git SHA")
    version = build_candidate_inventory.build_manifests.validate_version(version)
    if binary.is_symlink() or not binary.is_file():
        raise UpdateKeyError("待验证的应用二进制不存在或不是普通文件")
    key_bytes = encoded.encode("ascii")
    digest = hashlib.sha256()
    tail = b""
    found = False
    with binary.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
            window = tail + chunk
            found |= key_bytes in window
            tail = window[-(len(key_bytes) - 1):]
    if not found:
        raise UpdateKeyError("应用二进制没有嵌入预期的更新公钥")
    return {
        "schemaVersion": 1,
        "target": target,
        "sourceCommit": source_commit.lower(),
        "version": version,
        "publicKeySha256": hashlib.sha256(key_bytes).hexdigest(),
        "binarySha256": digest.hexdigest(),
    }


def verify_attestations(directory: Path, encoded: str, source_commit: str, version: str) -> None:
    """要求四个平台都使用相同公钥、源码和版本，防止签名作业混入其他构建。"""
    decode_public_key(encoded)
    if directory.is_symlink() or not directory.is_dir():
        raise UpdateKeyError("构建收据目录无效")
    expected_names = {f"{target}.json" for target in build_candidate_inventory.TARGETS}
    if {path.name for path in directory.iterdir()} != expected_names:
        raise UpdateKeyError("构建收据目标集合不完整")
    expected_key = hashlib.sha256(encoded.encode("ascii")).hexdigest()
    for target in build_candidate_inventory.TARGETS:
        path = directory / f"{target}.json"
        if path.is_symlink() or not path.is_file():
            raise UpdateKeyError("构建收据必须是普通文件")
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise UpdateKeyError("构建收据无法读取") from error
        if (
            not isinstance(record, dict)
            or record.get("schemaVersion") != 1
            or record.get("target") != target
            or record.get("sourceCommit") != source_commit.lower()
            or record.get("version") != version
            or record.get("publicKeySha256") != expected_key
            or not isinstance(record.get("binarySha256"), str)
            or not re.fullmatch(r"[0-9a-f]{64}", record["binarySha256"])
        ):
            raise UpdateKeyError("构建收据与发布公钥、源码或版本不一致")


def main() -> int:
    """只从指定环境变量取公钥，输出普通 .pub 或不含公钥正文的构建收据。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env", default="KERMINAL_GPUI_UPDATER_PUBLIC_KEY")
    parser.add_argument("--public-key-file", type=Path)
    parser.add_argument("--attestation", type=Path)
    parser.add_argument("--verify-attestations", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--source-commit")
    parser.add_argument("--version")
    arguments = parser.parse_args()
    try:
        encoded = os.environ.get(arguments.env, "")
        decoded = decode_public_key(encoded)
        if arguments.public_key_file:
            arguments.public_key_file.parent.mkdir(parents=True, exist_ok=True)
            arguments.public_key_file.write_text(decoded, encoding="ascii")
        if arguments.attestation:
            if not all((arguments.binary, arguments.target, arguments.source_commit, arguments.version)):
                raise UpdateKeyError("构建收据缺少二进制、target、提交或版本")
            record = build_attestation(
                arguments.binary, encoded, arguments.target, arguments.source_commit, arguments.version
            )
            arguments.attestation.parent.mkdir(parents=True, exist_ok=True)
            arguments.attestation.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
        if arguments.verify_attestations:
            if not arguments.source_commit or not arguments.version:
                raise UpdateKeyError("构建收据校验缺少提交或版本")
            verify_attestations(arguments.verify_attestations, encoded, arguments.source_commit, arguments.version)
    except (UpdateKeyError, OSError, build_candidate_inventory.build_manifests.ManifestError) as error:
        print(f"updater public key validation failed: {error}", file=sys.stderr)
        return 2
    print("Updater public key and requested build attestations are valid.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
