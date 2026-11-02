# @author kongweiguang
"""验证候选签名工具的目标选择与新旧私钥进程隔离。"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import sign_candidates


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


if __name__ == "__main__":
    unittest.main()
