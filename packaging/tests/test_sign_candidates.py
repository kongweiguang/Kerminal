# @author kongweiguang
"""验证候选签名工具的目标选择、新旧私钥进程隔离与探针分类。"""

from __future__ import annotations

import base64
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_manifests
import sign_candidates


def _minisign_public_text(seed: int) -> str:
    """构造结构合法的测试公钥文本；真实验签在测试中替换，避免依赖本机 Minisign。"""
    raw = b"Ed" + bytes([seed]) * 40
    return (
        f"untrusted comment: minisign public key: TEST{seed}\n"
        f"{base64.b64encode(raw).decode('ascii')}"
    )


def _encoded_public_key(text: str) -> str:
    """按仓库 updater 契约把两行公钥包装成 Base64，供环境变量读取路径复用。"""
    return base64.b64encode(text.encode("ascii")).decode("ascii")


class CandidateSigningTests(unittest.TestCase):
    """不使用实际签名私钥，验证唯一输出路径和敏感环境传递。"""

    def test_separates_trust_roots_and_moves_signatures_without_overwrite(self) -> None:
        """Windows 包需分别签名，两个子进程不能拿到对方的凭据。"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "x86_64-pc-windows-msvc"
            target.mkdir()
            artifact = target / "fixture-setup.exe"
            artifact.write_bytes(b"unsigned test payload")
            summary = [{"target": target.name, "kind": "nsis", "path": f"{target.name}/{artifact.name}"}]
            environments = []

            def simulate_signer(command: list[str], **kwargs: object) -> SimpleNamespace:
                """模拟官方工具输出，检查实际传入的参数和隔离环境。"""
                environments.append(kwargs["env"])
                signed_file = Path(command[-1])
                signed_file.with_name(signed_file.name + ".sig").write_bytes(b"test sidecar")
                return SimpleNamespace(returncode=0)

            test_environment = {
                "KERMINAL_GPUI_SIGNING_PRIVATE_KEY": "isolated-new-key",
                "TAURI_SIGNING_PRIVATE_KEY": "isolated-legacy-key",
            }
            with (
                patch.dict(os.environ, test_environment, clear=True),
                patch.object(sign_candidates.build_candidate_inventory, "validate_unsigned_candidate_set", return_value=summary),
                patch.object(sign_candidates.shutil, "which", return_value="cargo-packager"),
                patch.object(sign_candidates.subprocess, "run", side_effect=simulate_signer),
            ):
                sign_candidates.sign_candidates(root, "cargo-packager")
            self.assertEqual(len(environments), 2)
            self.assertEqual(environments[0]["CARGO_PACKAGER_SIGN_PRIVATE_KEY"], "isolated-new-key")
            self.assertEqual(environments[1]["CARGO_PACKAGER_SIGN_PRIVATE_KEY"], "isolated-legacy-key")
            for environment in environments:
                self.assertNotIn("KERMINAL_GPUI_SIGNING_PRIVATE_KEY", environment)
                self.assertNotIn("TAURI_SIGNING_PRIVATE_KEY", environment)
            self.assertTrue((root / "signatures/new" / target.name / (artifact.name + ".sig")).is_file())
            self.assertTrue((root / "signatures/legacy" / target.name / (artifact.name + ".sig")).is_file())
            self.assertFalse(artifact.with_name(artifact.name + ".sig").exists())

    def test_probe_signs_and_verifies_each_trust_root_with_fixed_fixture(self) -> None:
        """探针必须用固定小文件分别签名，并以各自公钥独立验签，不接触真实产物。"""
        gpui_text = _minisign_public_text(1)
        legacy_text = _minisign_public_text(2)
        signer_calls = []
        verify_calls = []
        environments = []
        run_options = []

        def simulate_signer(command: list[str], **kwargs: object) -> SimpleNamespace:
            """记录签名命令并把固定探针文件标记为已签名。"""
            artifact = Path(command[-1])
            signer_calls.append((artifact.name, artifact.read_bytes()))
            environments.append(kwargs["env"])
            run_options.append(kwargs)
            artifact.with_name(artifact.name + ".sig").write_bytes(b"probe sidecar")
            return SimpleNamespace(returncode=0)

        def record_verify(root: Path, artifact: Path, sidecar: Path, public_key: str, verifier: str, *, environment: dict[str, str]) -> None:
            """验证公钥检查没有继承任一签名私钥，不调用宿主 Minisign。"""
            self.assertFalse(any("SIGNING_PRIVATE_KEY" in key or "SIGN_PRIVATE_KEY" in key for key in environment))
            verify_calls.append((artifact.read_bytes(), public_key, verifier))

        with tempfile.TemporaryDirectory() as directory:
            legacy_key_path = Path(directory) / "legacy.pub"
            legacy_key_path.write_text(legacy_text, encoding="ascii")
            test_environment = {
                "KERMINAL_GPUI_SIGNING_PRIVATE_KEY": "probe-new-key",
                "TAURI_SIGNING_PRIVATE_KEY": "probe-legacy-key",
                "KERMINAL_GPUI_UPDATER_PUBLIC_KEY": _encoded_public_key(gpui_text),
            }
            with (
                patch.dict(os.environ, test_environment, clear=True),
                patch.object(sign_candidates.shutil, "which", return_value="cargo-packager"),
                patch.object(sign_candidates.subprocess, "run", side_effect=simulate_signer),
                patch.object(sign_candidates.build_manifests, "verify_signature", side_effect=record_verify),
            ):
                sign_candidates.probe_signing("cargo-packager", legacy_key_path)

        self.assertEqual(
            [name for name, _ in signer_calls],
            ["gpui-signing-probe.bin", "legacy-signing-probe.bin"],
        )
        for _, payload in signer_calls:
            self.assertEqual(payload, b"kerminal signing probe\n")
        self.assertEqual(
            [key for _, key, _ in verify_calls],
            [gpui_text.splitlines()[1], legacy_text.splitlines()[1]],
        )
        self.assertEqual([verifier for _, _, verifier in verify_calls], ["minisign", "minisign"])
        self.assertEqual(environments[0]["CARGO_PACKAGER_SIGN_PRIVATE_KEY"], "probe-new-key")
        self.assertEqual(environments[1]["CARGO_PACKAGER_SIGN_PRIVATE_KEY"], "probe-legacy-key")
        for environment in environments:
            self.assertNotIn("KERMINAL_GPUI_SIGNING_PRIVATE_KEY", environment)
            self.assertNotIn("TAURI_SIGNING_PRIVATE_KEY", environment)
        for options in run_options:
            self.assertIs(options["stdout"], subprocess.DEVNULL)
            self.assertIs(options["stderr"], subprocess.DEVNULL)

    def test_probe_classifies_signing_failure_by_purpose(self) -> None:
        """签名阶段失败要能区分旧版凭据，避免再次落到笼统的候选签名失败。"""
        legacy_text = _minisign_public_text(2)

        def fail_legacy_signing(command: list[str], **kwargs: object) -> SimpleNamespace:
            """让 GPUI 成功、旧版失败，验证失败标签跟随具体用途。"""
            artifact = Path(command[-1])
            if artifact.name.startswith("legacy"):
                return SimpleNamespace(returncode=1)
            artifact.with_name(artifact.name + ".sig").write_bytes(b"probe sidecar")
            return SimpleNamespace(returncode=0)

        with tempfile.TemporaryDirectory() as directory:
            legacy_key_path = Path(directory) / "legacy.pub"
            legacy_key_path.write_text(legacy_text, encoding="ascii")
            test_environment = {
                "KERMINAL_GPUI_SIGNING_PRIVATE_KEY": "probe-new-key",
                "TAURI_SIGNING_PRIVATE_KEY": "probe-legacy-key",
                "KERMINAL_GPUI_UPDATER_PUBLIC_KEY": _encoded_public_key(_minisign_public_text(1)),
            }
            with (
                patch.dict(os.environ, test_environment, clear=True),
                patch.object(sign_candidates.shutil, "which", return_value="cargo-packager"),
                patch.object(sign_candidates.subprocess, "run", side_effect=fail_legacy_signing),
                patch.object(sign_candidates.build_manifests, "verify_signature"),
            ):
                with self.assertRaises(sign_candidates.CandidateSigningError) as raised:
                    sign_candidates.probe_signing("cargo-packager", legacy_key_path)
        self.assertEqual(raised.exception.code, "legacy-signing-failed")

    def test_probe_classifies_public_key_verification_failure_by_purpose(self) -> None:
        """签名成功但验签失败必须归入公钥阶段，便于区分密钥配对问题与签名工具问题。"""
        legacy_text = _minisign_public_text(2)

        def simulate_signer(command: list[str], **kwargs: object) -> SimpleNamespace:
            """为两个用途生成 sidecar，让流程进入公钥验证阶段。"""
            artifact = Path(command[-1])
            artifact.with_name(artifact.name + ".sig").write_bytes(b"probe sidecar")
            return SimpleNamespace(returncode=0)

        def reject_gpui_verify(root: Path, artifact: Path, sidecar: Path, public_key: str, verifier: str, *, environment: dict[str, str]) -> None:
            """只让 GPUI 验签失败，确认失败标签携带用途与阶段。"""
            if artifact.name.startswith("gpui"):
                raise build_manifests.ManifestError("minisign 验签失败")

        with tempfile.TemporaryDirectory() as directory:
            legacy_key_path = Path(directory) / "legacy.pub"
            legacy_key_path.write_text(legacy_text, encoding="ascii")
            test_environment = {
                "KERMINAL_GPUI_SIGNING_PRIVATE_KEY": "probe-new-key",
                "TAURI_SIGNING_PRIVATE_KEY": "probe-legacy-key",
                "KERMINAL_GPUI_UPDATER_PUBLIC_KEY": _encoded_public_key(_minisign_public_text(1)),
            }
            with (
                patch.dict(os.environ, test_environment, clear=True),
                patch.object(sign_candidates.shutil, "which", return_value="cargo-packager"),
                patch.object(sign_candidates.subprocess, "run", side_effect=simulate_signer),
                patch.object(sign_candidates.build_manifests, "verify_signature", side_effect=reject_gpui_verify),
            ):
                with self.assertRaises(sign_candidates.CandidateSigningError) as raised:
                    sign_candidates.probe_signing("cargo-packager", legacy_key_path)
        self.assertEqual(raised.exception.code, "gpui-public-key-verification-failed")

    def test_missing_legacy_credentials_is_classified(self) -> None:
        """旧版凭据缺失要在启动签名工具前给出独立分类，便于定位 secret 配置。"""
        with patch.dict(
            os.environ,
            {"KERMINAL_GPUI_SIGNING_PRIVATE_KEY": "probe-new-key"},
            clear=True,
        ), patch.object(sign_candidates.shutil, "which", return_value="cargo-packager"):
            with self.assertRaises(sign_candidates.CandidateSigningError) as raised:
                sign_candidates.probe_signing("cargo-packager", Path("unused.pub"))
        self.assertEqual(raised.exception.code, "legacy-signing-credentials-missing")


if __name__ == "__main__":
    unittest.main()
