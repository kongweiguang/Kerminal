# @author kongweiguang
"""验证官方 minisign sidecar 与 updater 传输编码的互通。"""

from __future__ import annotations

import base64
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_manifests

PUBLIC_KEY = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"
SIGNATURE = (
    b"untrusted comment: signature from minisign secret key\n"
    b"RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/"
    b"z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n"
    b"trusted comment: timestamp:1556193335\tfile:test\n"
    b"y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg=="
)


class UpdaterSignatureTests(unittest.TestCase):
    """检查两个发布工具的 sidecar 可生成同一 SDK 契约并经过实际验签。"""

    def test_raw_and_wrapped_sidecars_produce_sdk_base64(self) -> None:
        """两个签名输入格式都必须还原为完整 Minisign 文本，不对原文件摘要做归一化。"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, data in (("raw.sig", SIGNATURE), ("wrapped.sig", base64.b64encode(SIGNATURE))):
                (root / name).write_bytes(data)
                encoded, _, _ = build_manifests.read_signature(root, name)
                self.assertEqual(base64.b64decode(encoded, validate=True), SIGNATURE)

    def test_invalid_sidecar_is_rejected(self) -> None:
        """损坏 sidecar 不能作为可安装 metadata 的签名。"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "bad.sig").write_bytes(b"not-a-minisign-signature")
            with self.assertRaises(build_manifests.ManifestError):
                build_manifests.read_signature(root, "bad.sig")

    @unittest.skipUnless(shutil.which("minisign"), "requires the official minisign executable")
    def test_official_minisign_verifies_raw_and_wrapped_sidecars(self) -> None:
        """用官方工具验证真实公开测试向量，证明包装转换不绕过密码学检查。"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "test"
            artifact.write_bytes(b"test")
            for name, data in (("raw.sig", SIGNATURE), ("wrapped.sig", base64.b64encode(SIGNATURE))):
                sidecar = root / name
                sidecar.write_bytes(data)
                build_manifests.verify_signature(root, artifact, sidecar, PUBLIC_KEY, "minisign")
            artifact.write_bytes(b"changed")
            with self.assertRaises(build_manifests.ManifestError):
                build_manifests.verify_signature(root, artifact, sidecar, PUBLIC_KEY, "minisign")

    @unittest.skipUnless(
        shutil.which("minisign") and shutil.which("cargo-packager"),
        "requires the official signer and minisign executables",
    )
    def test_packager_signer_output_is_accepted_by_minisign_and_metadata(self) -> None:
        """复用上游公开测试密钥签署临时文件，核对工具实际输出与验签适配。"""
        registry = Path.home() / ".cargo" / "registry" / "src"
        fixtures = [
            root / "cargo-packager-updater-0.2.3" / "tests"
            for root in registry.iterdir()
            if root.is_dir()
        ] if registry.is_dir() else []
        fixture = next(
            (root for root in fixtures if (root / "dummy.key").is_file() and (root / "dummy.pub.key").is_file()),
            None,
        )
        if fixture is None:
            self.skipTest("upstream public test-key fixture is unavailable")
        public_file = base64.b64decode((fixture / "dummy.pub.key").read_text(encoding="ascii").strip())
        public_key = public_file.decode("ascii").splitlines()[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "test-package.exe"
            artifact.write_bytes(b"isolated package payload")
            environment = dict(os.environ)
            environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY"] = str(fixture / "dummy.key")
            environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD"] = ""
            environment["RUST_LOG"] = "warn"
            result = subprocess.run(
                ["cargo-packager", "signer", "-q", "sign", str(artifact)],
                check=False,
                env=environment,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self.assertEqual(result.returncode, 0)
            sidecar = artifact.with_name(artifact.name + ".sig")
            build_manifests.verify_signature(root, artifact, sidecar, public_key, "minisign")
            encoded, _, _ = build_manifests.read_signature(root, sidecar.name)
            self.assertEqual(base64.b64decode(encoded), base64.b64decode(sidecar.read_bytes()))
            artifact.write_bytes(b"changed")
            with self.assertRaises(build_manifests.ManifestError):
                build_manifests.verify_signature(root, artifact, sidecar, public_key, "minisign")


if __name__ == "__main__":
    unittest.main()
