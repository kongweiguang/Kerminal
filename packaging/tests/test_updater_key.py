# @author kongweiguang
"""验证发布构建的公钥和四平台信任根收据。"""

from __future__ import annotations

import base64
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_candidate_inventory
import updater_key

PUBLIC_KEY = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"
ENCODED_KEY = base64.b64encode(
    f"untrusted comment: minisign public key test fixture\n{PUBLIC_KEY}\n".encode("ascii")
).decode("ascii")
COMMIT = "0123456789abcdef0123456789abcdef01234567"
VERSION = "0.4.0-dev.0"


class UpdateKeyTests(unittest.TestCase):
    """校验构建真实嵌入的信任根，防止签发与应用所信任的密钥不一致。"""

    def test_rejects_placeholder_and_private_key_header(self) -> None:
        """空值、伪造 Base64 和秘密密钥文件都不能进入公开构建配置。"""
        for value in ("", "placeholder-key", base64.b64encode(b"untrusted comment: secret key\nabc\n").decode()):
            with self.subTest(value=value), self.assertRaises(updater_key.UpdateKeyError):
                updater_key.decode_public_key(value)

    def test_attestation_checks_binary_contains_full_key_across_chunks(self) -> None:
        """公钥跨越读取块边界时仍可命中，缺失时拒绝生成收据。"""
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "kerminal.exe"
            binary.write_bytes(b"A" * (1024 * 1024 - 10) + ENCODED_KEY.encode() + b"end")
            record = updater_key.build_attestation(
                binary, ENCODED_KEY, "x86_64-pc-windows-msvc", COMMIT, VERSION
            )
            self.assertEqual(record["sourceCommit"], COMMIT)
            self.assertNotIn(ENCODED_KEY, json.dumps(record))
            binary.write_bytes(b"no updater public key")
            with self.assertRaises(updater_key.UpdateKeyError):
                updater_key.build_attestation(
                    binary, ENCODED_KEY, "x86_64-pc-windows-msvc", COMMIT, VERSION
                )

    def test_all_target_attestations_must_match_same_key_and_source(self) -> None:
        """签名阶段不能混入不同提交、公钥或缺失架构的构建产物。"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "binary"
            binary.write_bytes(ENCODED_KEY.encode())
            receipts = root / "receipts"
            receipts.mkdir()
            for target in build_candidate_inventory.TARGETS:
                record = updater_key.build_attestation(binary, ENCODED_KEY, target, COMMIT, VERSION)
                (receipts / f"{target}.json").write_text(json.dumps(record), encoding="utf-8")
            updater_key.verify_attestations(receipts, ENCODED_KEY, COMMIT, VERSION)
            receipt = receipts / "x86_64-pc-windows-msvc.json"
            changed = json.loads(receipt.read_text(encoding="utf-8"))
            changed["sourceCommit"] = "0" * 40
            receipt.write_text(json.dumps(changed), encoding="utf-8")
            with self.assertRaises(updater_key.UpdateKeyError):
                updater_key.verify_attestations(receipts, ENCODED_KEY, COMMIT, VERSION)


if __name__ == "__main__":
    unittest.main()
