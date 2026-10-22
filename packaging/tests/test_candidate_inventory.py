# @author kongweiguang
"""验证真实候选目录发现、双 sidecar 信任根和 updater metadata 接线。"""

from __future__ import annotations

import json
import io
import shutil
import sys
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_candidate_inventory
import build_manifests
import collect_candidates


class CandidateInventoryTests(unittest.TestCase):
    """以任意真实格式文件名验证 inventory，不依赖产品名或版本名模板。"""

    def setUp(self) -> None:
        """每项测试使用隔离目录，避免碰触工作区候选包和用户密钥。"""
        self.temporary = tempfile.TemporaryDirectory()
        self.base = Path(self.temporary.name)
        self.candidate = self.base / "candidate"
        self.candidate.mkdir()
        self.new_signatures = "signatures/new"
        self.legacy_signatures = "signatures/legacy"
        self._create_candidate_tree()
        self.new_public_key = self.base / "new.pub"
        self.legacy_public_key = self.base / "legacy.pub"
        self.new_public_key.write_text(
            "untrusted comment: minisign public key 9537F851E32E34A0\nRWTnewdevfixture=\n",
            encoding="utf-8",
        )
        self.legacy_public_key.write_text(
            "untrusted comment: minisign public key 0E8E89FB8BF4DF57\nRWTlegacydevfixture=\n",
            encoding="utf-8",
        )
        self.metadata_output = self.base / "metadata"
        self.inventory_path = self.metadata_output / "candidate-inventory.json"

    def tearDown(self) -> None:
        """释放临时产物和测试公钥，确保回归不遗留文件。"""
        self.temporary.cleanup()

    def _write_package(self, target: str, name: str, contents: bytes = b"candidate payload") -> Path:
        """按 target 内实际文件名写入候选包，不合成固定产品名。"""
        path = self.candidate / target / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)
        return path

    def _write_bundle(self, target: str, name: str) -> Path:
        """写入 macOS 顶层 app bundle，并加入嵌套 helper app 验证不误分类。"""
        bundle = self.candidate / target / name
        (bundle / "Contents" / "Frameworks" / "Helper.app" / "Contents").mkdir(parents=True)
        (bundle / "Contents" / "Info.plist").write_text("plist fixture", encoding="utf-8")
        return bundle

    def _write_signature(self, directory: str, target: str, artifact_name: str) -> Path:
        """按签名 trust root 分离 sidecar，并保持它与实际候选名一一对应。"""
        path = self.candidate / directory / target / f"{artifact_name}.sig"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("untrusted comment: local dev fixture\nAQID\n", encoding="ascii")
        return path

    def _collect_unsigned_candidate_set(self) -> Path:
        """用真实 collector 串接四目标包文件与 inventory 输入目录结构。"""
        workspace = self.base / "collector-workspace"
        (workspace / "crates/kerminal-desktop").mkdir(parents=True)
        (workspace / "packaging").mkdir()
        (workspace / "Cargo.toml").write_text(
            '[workspace.package]\nversion = "1.2.3-rc.1"\n', encoding="utf-8"
        )
        (workspace / "crates/kerminal-desktop/Cargo.toml").write_text(
            '[[bin]]\nname = "kerminal"\npath = "src/main.rs"\n', encoding="utf-8"
        )
        config = (
            'product-name = "Kerminal"\n'
            'out-dir = "../target/package-output"\n'
            '\n[[binaries]]\npath = "kerminal"\nmain = true\n'
        )
        for platform in ("windows", "linux", "macos"):
            (workspace / f"packaging/Packager.{platform}.toml").write_text(
                config, encoding="utf-8"
            )

        package_output = workspace / "target/package-output"
        candidate_set = self.base / "collected-candidate-set"
        candidate_set.mkdir()
        target_packages = {
            "x86_64-pc-windows-msvc": [
                ("kerminal_1.2.3-rc.1_x64-setup.exe", b"NSIS candidate")
            ],
            "x86_64-unknown-linux-gnu": [
                ("kerminal_1.2.3-rc.1_x86_64.AppImage", b"AppImage candidate"),
                ("kerminal_1.2.3-rc.1_amd64.deb", b"DEB candidate"),
            ],
            "x86_64-apple-darwin": [
                ("Kerminal_1.2.3-rc.1_x64.dmg", b"x64 DMG candidate")
            ],
            "aarch64-apple-darwin": [
                ("Kerminal_1.2.3-rc.1_aarch64.dmg", b"arm64 DMG candidate")
            ],
        }

        for target, files in target_packages.items():
            collect_candidates.prepare_candidates(workspace, target)
            for name, contents in files:
                destination = package_output / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(contents)
            if target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
                bundle = package_output / "Kerminal.app/Contents"
                bundle.mkdir(parents=True)
                (bundle / "Info.plist").write_text("app metadata", encoding="utf-8")

            staged = collect_candidates.collect_candidates(workspace, target)
            destination = candidate_set / target
            destination.mkdir()
            for artifact in staged.iterdir():
                staged_copy = destination / artifact.name
                if artifact.is_dir():
                    shutil.copytree(artifact, staged_copy, symlinks=True)
                else:
                    shutil.copy2(artifact, staged_copy)
        return candidate_set

    def _create_candidate_tree(self) -> None:
        """构造完整平台矩阵，包名不携带脚本可推测的 Kerminal/version 前缀。"""
        windows_target = "x86_64-pc-windows-msvc"
        windows = self._write_package(windows_target, "installer-from-run-42-setup.exe")
        self._write_signature(self.new_signatures, windows_target, windows.name)
        self._write_signature(self.legacy_signatures, windows_target, windows.name)

        linux_target = "x86_64-unknown-linux-gnu"
        raw_appimage = self._write_package(linux_target, "portable-binary.AppImage")
        self._write_package(linux_target, "package-from-job.deb")
        appimage_archive = self._write_package(linux_target, "signed-input.AppImage.tar.gz")
        self._write_signature(self.legacy_signatures, linux_target, raw_appimage.name)
        self._write_signature(self.new_signatures, linux_target, appimage_archive.name)

        for target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
            self._write_bundle(target, "display-name.app")
            self._write_package(target, "disk-image-from-packager.dmg")
            updater_archive = self._write_package(target, "update-bundle.app.tar.gz")
            self._write_signature(self.new_signatures, target, updater_archive.name)
            self._write_signature(self.legacy_signatures, target, updater_archive.name)

    def _arguments(self, signing_class: str = "test") -> SimpleNamespace:
        """构造显式 CLI 输入，避免测试依赖生产 feed、endpoint 或密钥。"""
        return SimpleNamespace(
            candidate_dir=self.candidate,
            new_signatures_directory=self.new_signatures,
            legacy_signatures_directory=self.legacy_signatures,
            new_public_key=self.new_public_key,
            legacy_public_key=self.legacy_public_key,
            inventory=self.inventory_path,
            metadata_output=self.metadata_output,
            version="0.4.0-dev.2",
            source_commit="0123456789abcdef0123456789abcdef01234567",
            published_at="2026-10-17T02:00:00Z",
            notes="isolated local candidate",
            base_url="https://updates.invalid/kerminal/0.4.0-dev.2",
            signing_class=signing_class,
            minisign="minisign",
        )

    def test_discovers_actual_paths_and_builds_two_verified_contracts(self) -> None:
        """真实格式扫描保留任意包名，并将新旧签名交给不同公钥验签。"""
        verifier_calls: list[tuple[Path, Path, str]] = []

        def capture_verification(
            _root: Path, artifact: Path, sidecar: Path, public_key: str, _verifier: str
        ) -> None:
            verifier_calls.append((artifact, sidecar, public_key))

        arguments = self._arguments()
        with patch.object(build_manifests, "verify_signature", side_effect=capture_verification):
            build_candidate_inventory.build_and_validate(arguments)

        inventory = json.loads(self.inventory_path.read_text(encoding="utf-8"))
        records = {item["path"]: item for item in inventory["artifacts"]}
        self.assertEqual(len(records), 10)
        self.assertIn("x86_64-pc-windows-msvc/installer-from-run-42-setup.exe", records)
        self.assertIn(
            "x86_64-unknown-linux-gnu/signed-input.AppImage.tar.gz",
            records,
        )
        self.assertEqual(len(verifier_calls), 8)
        self.assertEqual(
            {public_key for _, _, public_key in verifier_calls},
            {"RWTnewdevfixture=", "RWTlegacydevfixture="},
        )
        self.assertEqual(
            records["x86_64-pc-windows-msvc/installer-from-run-42-setup.exe"]["signaturePath"],
            "signatures/new/x86_64-pc-windows-msvc/installer-from-run-42-setup.exe.sig",
        )
        self.assertEqual(
            records["x86_64-pc-windows-msvc/installer-from-run-42-setup.exe"]["legacySignaturePath"],
            "signatures/legacy/x86_64-pc-windows-msvc/installer-from-run-42-setup.exe.sig",
        )

        cargo = json.loads((self.metadata_output / "kerminal-updater.test.json").read_text(encoding="utf-8"))
        legacy = json.loads(
            (self.metadata_output / "latest.tauri-legacy.test.json").read_text(encoding="utf-8")
        )
        self.assertTrue(
            cargo["platforms"]["linux-x86_64"]["url"].endswith("signed-input.AppImage.tar.gz")
        )
        self.assertTrue(
            legacy["platforms"]["linux-x86_64"]["url"].endswith("portable-binary.AppImage")
        )
        expected_legacy_signature = (
            self.candidate
            / self.legacy_signatures
            / "aarch64-apple-darwin"
            / "update-bundle.app.tar.gz.sig"
        ).read_bytes().decode("ascii").strip()
        self.assertEqual(
            legacy["platforms"]["darwin-aarch64"]["signature"],
            expected_legacy_signature,
        )

    def test_missing_required_signature_fails_before_writing_outputs(self) -> None:
        """不能用新旧信任根互相兜底；缺任一所需 sidecar 时不写清单。"""
        missing = (
            self.candidate
            / self.legacy_signatures
            / "x86_64-pc-windows-msvc"
            / "installer-from-run-42-setup.exe.sig"
        )
        missing.unlink()
        with self.assertRaisesRegex(build_candidate_inventory.CandidateInventoryError, "旧 Tauri updater"):
            build_candidate_inventory.build_and_validate(self._arguments())
        self.assertFalse(self.inventory_path.exists())
        self.assertFalse(self.metadata_output.exists())

    def test_signature_verification_failure_writes_no_inventory_or_feed(self) -> None:
        """官方 verifier 拒绝任一信任根时，所有输出均保持未生成。"""
        arguments = self._arguments()
        with patch.object(
            build_manifests,
            "verify_signature",
            side_effect=build_manifests.ManifestError("minisign 验签失败"),
        ):
            with self.assertRaisesRegex(build_manifests.ManifestError, "验签失败"):
                build_candidate_inventory.build_and_validate(arguments)
        self.assertFalse(self.inventory_path.exists())
        self.assertFalse(self.metadata_output.exists())

    def test_unknown_direct_candidate_file_is_rejected(self) -> None:
        """未被 updater 契约识别的文件不能静默漏出 checksums 和候选审计。"""
        self._write_package("x86_64-pc-windows-msvc", "unclassified-output.txt")
        with self.assertRaisesRegex(build_candidate_inventory.CandidateInventoryError, "未识别"):
            build_candidate_inventory.build_inventory(
                self.candidate,
                version="0.4.0",
                source_commit="0123456789abcdef0123456789abcdef01234567",
                new_signatures_directory=self.new_signatures,
                legacy_signatures_directory=self.legacy_signatures,
            )

    def test_duplicate_format_in_target_is_rejected(self) -> None:
        """一个 target 发现多个同类型包时拒绝任意选择，避免混入旧文件。"""
        self._write_package("x86_64-unknown-linux-gnu", "second-package.deb")
        with self.assertRaisesRegex(build_candidate_inventory.CandidateInventoryError, "重复的 deb"):
            build_candidate_inventory.build_inventory(
                self.candidate,
                version="0.4.0",
                source_commit="0123456789abcdef0123456789abcdef01234567",
                new_signatures_directory=self.new_signatures,
                legacy_signatures_directory=self.legacy_signatures,
            )

    def test_new_and_legacy_signature_directories_must_be_independent(self) -> None:
        """同目录或父子目录会模糊 trust-root ownership，必须 fail closed。"""
        with self.assertRaisesRegex(build_candidate_inventory.CandidateInventoryError, "互不重叠"):
            build_candidate_inventory.build_inventory(
                self.candidate,
                version="0.4.0",
                source_commit="0123456789abcdef0123456789abcdef01234567",
                new_signatures_directory="signatures",
                legacy_signatures_directory="signatures/legacy",
            )

    def test_private_key_header_is_rejected_before_reading_its_body(self) -> None:
        """遇到 secret-key 文件时只读头部并立即拒绝，不触碰私钥正文。"""
        class HeaderOnlyReader:
            """记录调用方是否尝试继续读取私钥内容。"""

            def __init__(self) -> None:
                self.reads = 0

            def __enter__(self) -> HeaderOnlyReader:
                """作为最小文件读取替身供密钥边界测试使用。"""
                return self

            def __exit__(self, *_: object) -> None:
                """满足 with 生命周期，不持有真实文件描述符。"""
                return None

            def readline(self) -> str:
                """只允许读取 secret-key 头；若读取正文则测试立即失败。"""
                self.reads += 1
                if self.reads > 1:
                    raise AssertionError("private key body must not be read")
                return "untrusted comment: minisign encrypted secret key\n"

        reader = HeaderOnlyReader()
        secret_key_path = self.base / "production.key"
        secret_key_path.write_text("fixture body is not read", encoding="utf-8")
        with patch.object(Path, "open", return_value=reader):
            with self.assertRaisesRegex(
                build_candidate_inventory.CandidateInventoryError,
                "必须是 minisign .pub",
            ):
                build_candidate_inventory._read_public_key(secret_key_path, "test key")
        self.assertEqual(reader.reads, 1)

    def test_app_bundle_scan_ignores_nested_helper_bundle_as_product(self) -> None:
        """只有目标目录的顶层 .app 是分发包，Frameworks 内 helper 不参与矩阵。"""
        inventory = build_candidate_inventory.build_inventory(
            self.candidate,
            version="0.4.0",
            source_commit="0123456789abcdef0123456789abcdef01234567",
            new_signatures_directory=self.new_signatures,
            legacy_signatures_directory=self.legacy_signatures,
        )
        bundles = [item for item in inventory["artifacts"] if item["kind"] == "app-bundle"]
        self.assertEqual(
            {item["path"] for item in bundles},
            {
                "x86_64-apple-darwin/display-name.app",
                "aarch64-apple-darwin/display-name.app",
            },
        )

    def test_unsigned_validator_consumes_the_real_collector_output_contract(self) -> None:
        """真实 collector 生成的四目标输出可直接进入无签名结构验证。"""
        candidate_set = self._collect_unsigned_candidate_set()

        artifacts = build_candidate_inventory.validate_unsigned_candidate_set(candidate_set)

        self.assertEqual(len(artifacts), 10)
        kinds_by_target = {
            target: {item["kind"] for item in artifacts if item["target"] == target}
            for target in build_candidate_inventory.TARGETS
        }
        self.assertEqual(kinds_by_target["x86_64-pc-windows-msvc"], {"nsis"})
        self.assertEqual(
            kinds_by_target["x86_64-unknown-linux-gnu"],
            {"appimage", "deb", "appimage-updater"},
        )
        self.assertEqual(
            kinds_by_target["x86_64-apple-darwin"],
            {"app-bundle", "dmg", "app-updater"},
        )
        self.assertEqual(
            kinds_by_target["aarch64-apple-darwin"],
            {"app-bundle", "dmg", "app-updater"},
        )

    def test_unsigned_cli_reports_structure_without_generating_feed(self) -> None:
        """unsigned 模式只校验实际文件树，不生成或伪称 updater metadata。"""
        candidate_set = self._collect_unsigned_candidate_set()
        output = io.StringIO()

        with redirect_stdout(output):
            result = build_candidate_inventory.main(
                ["--candidate-dir", str(candidate_set), "--validate-unsigned"]
            )

        self.assertEqual(result, 0)
        self.assertIn("no signatures or updater metadata were verified or generated", output.getvalue())
        self.assertFalse((candidate_set / "candidate-inventory.json").exists())
        self.assertFalse((candidate_set / "metadata").exists())

    def test_unsigned_cli_rejects_feed_and_signature_arguments(self) -> None:
        """结构检查与双信任签验参数互斥，禁止借 unsigned 模式跳过验签。"""
        error_output = io.StringIO()
        with redirect_stderr(error_output):
            result = build_candidate_inventory.main(
                [
                    "--candidate-dir",
                    str(self.candidate),
                    "--validate-unsigned",
                    "--new-public-key",
                    str(self.new_public_key),
                ]
            )
        self.assertEqual(result, 2)
        self.assertIn("不能接收签名或 feed 参数", error_output.getvalue())

    def test_unsigned_validator_rejects_archive_path_escape(self) -> None:
        """updater tar 成员路径逃逸时拒绝候选集，不解包或写入目标目录。"""
        candidate_set = self._collect_unsigned_candidate_set()
        linux_archive = next((candidate_set / "x86_64-unknown-linux-gnu").glob("*.AppImage.tar.gz"))
        with tarfile.open(linux_archive, mode="w:gz") as archive:
            member = tarfile.TarInfo("../outside.AppImage")
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))

        with self.assertRaisesRegex(
            build_candidate_inventory.CandidateInventoryError, "unsafe path"
        ):
            build_candidate_inventory.validate_unsigned_candidate_set(candidate_set)

    def test_unsigned_validator_requires_exact_target_matrix(self) -> None:
        """缺少架构或夹带未知 target 时拒绝生成可交给签名阶段的集合。"""
        candidate_set = self._collect_unsigned_candidate_set()
        (candidate_set / "aarch64-apple-darwin").rename(candidate_set / "unexpected-target")

        with self.assertRaisesRegex(
            build_candidate_inventory.CandidateInventoryError, "unsigned candidate set 不完整"
        ):
            build_candidate_inventory.validate_unsigned_candidate_set(candidate_set)

    def test_signed_cli_keeps_sidecar_and_key_arguments_mandatory(self) -> None:
        """新增 unsigned 检查入口不能让正式双信任 metadata 省略公钥或 sidecar。"""
        error_output = io.StringIO()
        with redirect_stderr(error_output):
            result = build_candidate_inventory.main(["--candidate-dir", str(self.candidate)])

        self.assertEqual(result, 2)
        self.assertIn("双信任签验模式缺少显式参数", error_output.getvalue())
        self.assertFalse(self.inventory_path.exists())
        self.assertFalse(self.metadata_output.exists())


if __name__ == "__main__":
    unittest.main()
