# @author kongweiguang
"""独立验证 staging inventory、双 updater 契约和签名失败边界。"""

from __future__ import annotations

import hashlib
import json
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_manifests


class ManifestBuilderTests(unittest.TestCase):
    """通过临时产物树验证 manifest 规则，不触碰真实用户安装目录。"""

    def setUp(self) -> None:
        """每个用例重新创建隔离 staging，避免相互依赖和残留产物。"""
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)
        self.inventory = self._create_inventory()

    def tearDown(self) -> None:
        """关闭临时目录句柄，确保失败测试也释放所有 fixture 文件。"""
        self.temporary_directory.cleanup()

    def test_packager_configs_include_license_resources_and_deep_links(self) -> None:
        """确保各平台许可证资源有效，Linux 随内嵌 CJK 字体另带其 OFL 正文。"""
        packaging_root = Path(__file__).resolve().parents[1]
        workspace_root = packaging_root.parent
        expected = {
            ("../LICENSE", "licenses/LICENSE"),
            ("../THIRD_PARTY_NOTICES.md", "licenses/THIRD_PARTY_NOTICES.md"),
            (
                "../licenses/THIRD_PARTY_DEPENDENCIES.html",
                "licenses/THIRD_PARTY_DEPENDENCIES.html",
            ),
        }

        for platform in ("windows", "macos", "linux"):
            with self.subTest(platform=platform):
                config_path = packaging_root / f"Packager.{platform}.toml"
                with config_path.open("rb") as config_file:
                    config = tomllib.load(config_file)
                self.assertEqual(config.get("out-dir"), "../target/package-output", config_path.name)
                self.assertEqual(
                    (config_path.parent / config["out-dir"]).resolve(),
                    (workspace_root / "target" / "package-output").resolve(),
                    config_path.name,
                )
                main_binaries = [binary for binary in config.get("binaries", []) if binary.get("main")]
                self.assertEqual(main_binaries, [{"path": "kerminal", "main": True}], config_path.name)
                self.assertEqual(
                    config.get("deep-link-protocols"),
                    [{"schemes": ["kerminal"], "role": "viewer"}],
                    config_path.name,
                )
                asset_paths = [config["license-file"], *config.get("icons", [])]
                asset_paths.extend(resource["src"] for resource in config.get("resources", []))
                for asset_path in asset_paths:
                    resolved_asset = (config_path.parent / asset_path).resolve()
                    self.assertTrue(resolved_asset.is_relative_to(workspace_root), (config_path.name, asset_path))
                resources = {
                    (resource["src"], resource["target"])
                    for resource in config.get("resources", [])
                }
                platform_expected = set(expected)
                if platform == "linux":
                    font_license = (
                        "../licenses/NOTO-CJK-SIL-OFL-1.1.txt",
                        "licenses/NOTO-CJK-SIL-OFL-1.1.txt",
                    )
                    platform_expected.add(font_license)
                    self.assertTrue(
                        (workspace_root / "licenses/NOTO-CJK-SIL-OFL-1.1.txt").is_file()
                    )
                self.assertTrue(platform_expected.issubset(resources), config_path.name)

    def _write_file(self, relative_path: str, content: bytes) -> None:
        """按 inventory 路径生成非空测试文件，保持路径处理与 Windows 一致。"""
        destination = self.root.joinpath(*Path(relative_path).parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(content)

    def _create_inventory(self) -> dict[str, object]:
        """构造所有受支持平台与架构的临时包，固定为测试专用版本和 SHA。"""
        specifications = [
            ("windows", "x86_64", "nsis", "Kerminal_0.4.0_x64-setup.exe", True, True),
            ("linux", "x86_64", "appimage", "Kerminal_0.4.0_amd64.AppImage", False, True),
            ("linux", "x86_64", "deb", "kerminal_0.4.0_amd64.deb", False, False),
            (
                "linux",
                "x86_64",
                "appimage-updater",
                "Kerminal_0.4.0_amd64.AppImage.tar.gz",
                True,
                False,
            ),
        ]
        for arch in ("x86_64", "aarch64"):
            specifications.extend(
                [
                    ("macos", arch, "app-bundle", f"Kerminal-{arch}.app", False, False),
                    ("macos", arch, "dmg", f"Kerminal-{arch}.dmg", False, False),
                    (
                        "macos",
                        arch,
                        "app-updater",
                        f"Kerminal-{arch}.app.tar.gz",
                        True,
                        True,
                    ),
                ]
            )

        artifacts: list[dict[str, object]] = []
        for index, (platform, arch, kind, relative_path, needs_new_signature, needs_legacy_signature) in enumerate(
            specifications
        ):
            if kind == "app-bundle":
                self._write_file(f"{relative_path}/Contents/Info.plist", f"{arch}:{index}".encode())
            else:
                self._write_file(relative_path, f"payload-{platform}-{arch}-{kind}".encode())
            signature_path = None
            legacy_signature_path = None
            if needs_new_signature:
                signature_path = f"signatures/new/{relative_path}.sig"
                self._write_file(signature_path, b"untrusted comment: new test key\nAQID\n")
            if needs_legacy_signature:
                legacy_signature_path = f"signatures/legacy/{relative_path}.sig"
                self._write_file(legacy_signature_path, b"untrusted comment: legacy test key\nBAUG\n")
            artifacts.append(
                {
                    "platform": platform,
                    "arch": arch,
                    "kind": kind,
                    "path": relative_path,
                    "signaturePath": signature_path,
                    "legacySignaturePath": legacy_signature_path,
                }
            )
        return {
            "schemaVersion": 1,
            "version": "0.4.0-dev.0",
            "sourceCommit": "0123456789abcdef0123456789abcdef01234567",
            "publishedAt": "2026-10-16T02:00:00Z",
            "notes": "fixture only",
            "artifacts": artifacts,
        }

    def test_generates_distinct_new_and_legacy_platform_keys(self) -> None:
        """新 updater 使用 macos 与 format；旧 Tauri feed 独立映射 darwin。"""
        cargo_metadata, legacy_metadata, report = build_manifests.validate_inventory(
            self.root,
            self.inventory,
            "https://updates.invalid/kerminal/0.4.0",
            "test",
            verify_crypto=False,
        )

        self.assertEqual(
            set(cargo_metadata["platforms"]),
            {"windows-x86_64", "linux-x86_64", "macos-x86_64", "macos-aarch64"},
        )
        self.assertEqual(
            set(legacy_metadata["platforms"]),
            {"windows-x86_64", "linux-x86_64", "darwin-x86_64", "darwin-aarch64"},
        )
        self.assertEqual(cargo_metadata["platforms"]["windows-x86_64"]["format"], "nsis")
        self.assertEqual(cargo_metadata["platforms"]["linux-x86_64"]["format"], "appimage")
        self.assertTrue(cargo_metadata["platforms"]["linux-x86_64"]["url"].endswith("AppImage.tar.gz"))
        self.assertTrue(legacy_metadata["platforms"]["linux-x86_64"]["url"].endswith("AppImage"))
        self.assertNotEqual(
            cargo_metadata["platforms"]["linux-x86_64"]["signature"],
            legacy_metadata["platforms"]["linux-x86_64"]["signature"],
        )
        self.assertEqual(cargo_metadata["platforms"]["macos-aarch64"]["format"], "app")
        self.assertNotIn("format", legacy_metadata["platforms"]["darwin-aarch64"])
        self.assertNotEqual(
            cargo_metadata["platforms"]["windows-x86_64"]["signature"],
            legacy_metadata["platforms"]["windows-x86_64"]["signature"],
        )
        self.assertEqual(report["artifactManifest"]["signingClass"], "test")

    def test_updater_fixture_is_generated_by_the_packaging_manifest_builder(self) -> None:
        """固定 Rust 解析回归输入必须来自真实打包清单生成函数。"""
        cargo_metadata, _, _ = build_manifests.validate_inventory(
            self.root,
            self.inventory,
            "https://updates.invalid/kerminal/0.4.0",
            "test",
            verify_crypto=False,
        )
        fixture_path = Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "updater" / "cargo-packager-updater-0.2.3.json"
        fixture = json.loads(fixture_path.read_text(encoding="utf-8"))

        self.assertEqual(fixture, cargo_metadata)

    def test_verification_requires_distinct_update_trust_roots(self) -> None:
        """新 updater 和旧 Tauri 的验签公钥不得因参数缺省而坍缩为同一信任根。"""
        with self.assertRaisesRegex(build_manifests.ManifestError, "不同的 minisign 公钥"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                public_key="same test public key",
                legacy_public_key="same test public key",
            )

    def test_records_verified_status_only_after_crypto_verifier(self) -> None:
        """不执行验签的测试文件明确标为未验证，不伪造密码学通过状态。"""
        _, _, report = build_manifests.validate_inventory(
            self.root,
            self.inventory,
            "https://updates.invalid/kerminal/0.4.0",
            "test",
            verify_crypto=False,
        )
        statuses = {
            item["signingStatus"]
            for item in report["artifactManifest"]["artifacts"]
            if item["kind"] in build_manifests.NEW_UPDATER_KINDS
        }
        self.assertEqual(statuses, {"unverified-test"})
        legacy_statuses = {
            item["legacySigningStatus"]
            for item in report["artifactManifest"]["artifacts"]
            if item["kind"] in build_manifests.LEGACY_UPDATER_KINDS
        }
        self.assertEqual(legacy_statuses, {"unverified-test"})

        with patch.object(build_manifests, "verify_signature") as verifier:
            _, _, verified_report = build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                public_key="test public key",
                legacy_public_key="legacy test public key",
                verify_crypto=True,
            )
        self.assertEqual(verifier.call_count, 8)
        verified_statuses = {
            item["signingStatus"]
            for item in verified_report["artifactManifest"]["artifacts"]
            if item["kind"] in build_manifests.NEW_UPDATER_KINDS
        }
        self.assertEqual(verified_statuses, {"verified"})
        verified_legacy_statuses = {
            item["legacySigningStatus"]
            for item in verified_report["artifactManifest"]["artifacts"]
            if item["kind"] in build_manifests.LEGACY_UPDATER_KINDS
        }
        self.assertEqual(verified_legacy_statuses, {"verified"})
        self.assertEqual(
            {call.args[3] for call in verifier.call_args_list},
            {"test public key", "legacy test public key"},
        )

    def test_rejects_incomplete_architecture_matrix(self) -> None:
        """缺少 Intel 或 Apple Silicon 的任何发行产物都不能生成 feeds。"""
        self.inventory["artifacts"].pop()
        with self.assertRaisesRegex(build_manifests.ManifestError, "矩阵不完整"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_rejects_duplicate_platform_artifact(self) -> None:
        """重复记录可能让发布过程选择不确定文件，因此按矩阵身份拒绝。"""
        self.inventory["artifacts"].append(dict(self.inventory["artifacts"][0]))
        with self.assertRaisesRegex(build_manifests.ManifestError, "重复"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_rejects_missing_new_updater_signature(self) -> None:
        """新 updater 产物缺签名时拒绝 Cargo feed，不拿旧 Tauri 签名兜底。"""
        windows = self.inventory["artifacts"][0]
        windows["signaturePath"] = None
        with self.assertRaisesRegex(build_manifests.ManifestError, "新 updater 产物"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_rejects_missing_legacy_updater_signature(self) -> None:
        """旧 Tauri Linux feed 必须签名原始 AppImage，不能借新 tar.gz 签名兼容。"""
        appimage = next(item for item in self.inventory["artifacts"] if item["kind"] == "appimage")
        appimage["legacySignaturePath"] = None
        with self.assertRaisesRegex(build_manifests.ManifestError, "旧 Tauri updater 产物"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_production_never_skips_signature_verification(self) -> None:
        """正式 latest.json 与测试签名必须隔离，不能绕过公钥校验发布。"""
        with self.assertRaisesRegex(build_manifests.ManifestError, "禁止跳过"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.example.com/kerminal/0.4.0",
                "production",
                verify_crypto=False,
            )

    def test_production_cli_requires_a_separate_legacy_public_key(self) -> None:
        """生产旧 Tauri feed 必须显式指定旧公钥，不能静默共用新信任根。"""
        with self.assertRaisesRegex(build_manifests.ManifestError, "旧 updater minisign 公钥"):
            build_manifests.main(
                [
                    "--artifact-root",
                    str(self.root),
                    "--inventory",
                    str(self.root / "inventory.json"),
                    "--base-url",
                    "https://updates.example.com/kerminal/0.4.0",
                    "--output-dir",
                    str(self.root / "metadata"),
                    "--signing-class",
                    "production",
                    "--public-key",
                    "new-production-public-key",
                ]
            )

    def test_rejects_path_traversal(self) -> None:
        """inventory 不得把 staging 外任意文件作为待发布安装包。"""
        self.inventory["artifacts"][0]["path"] = "../outside.exe"
        with self.assertRaisesRegex(build_manifests.ManifestError, "路径"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_rejects_test_metadata_with_public_domain(self) -> None:
        """临时密钥对应的测试文档只允许保留域名，避免误指向生产下载地址。"""
        with self.assertRaisesRegex(build_manifests.ManifestError, r"\.invalid"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://example.com/releases/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_rejects_unlisted_staging_files(self) -> None:
        """发布 staging 中出现 inventory 之外文件时必须先补齐来源清单。"""
        self._write_file("unknown.txt", b"unclassified")
        with self.assertRaisesRegex(build_manifests.ManifestError, "inventory 不一致"):
            build_manifests.validate_inventory(
                self.root,
                self.inventory,
                "https://updates.invalid/kerminal/0.4.0",
                "test",
                verify_crypto=False,
            )

    def test_checksums_use_payload_bytes(self) -> None:
        """SHA-256 由 staging 原始字节计算，并将 sidecar 独立纳入清单。"""
        _, _, report = build_manifests.validate_inventory(
            self.root,
            self.inventory,
            "https://updates.invalid/kerminal/0.4.0",
            "test",
            verify_crypto=False,
        )
        entries = {entry["fileName"]: entry["sha256"] for entry in report["checksums"]["artifacts"]}
        payload = self.root / "Kerminal_0.4.0_x64-setup.exe"
        self.assertEqual(entries[payload.name], hashlib.sha256(payload.read_bytes()).hexdigest())
        self.assertIn("signatures/new/Kerminal_0.4.0_x64-setup.exe.sig", entries)
        self.assertIn("signatures/legacy/Kerminal_0.4.0_x64-setup.exe.sig", entries)

    def test_test_output_names_cannot_shadow_production_feed(self) -> None:
        """测试签名永远输出专用文件名，不能生成可被误上传的 latest.json。"""
        names = build_manifests.build_output_names("test")
        self.assertEqual(names["cargo"], "kerminal-updater.test.json")
        self.assertEqual(names["legacy"], "latest.tauri-legacy.test.json")
        self.assertNotIn("latest.json", names.values())

    def test_metadata_write_failure_restores_the_previous_output_set(self) -> None:
        """多份 feed 提交中途失败时恢复已替换文件，避免留下新旧契约混合状态。"""
        output_directory = self.root / "metadata"
        output_directory.mkdir()
        targets = {
            output_directory / "cargo.json": b"old cargo feed",
            output_directory / "legacy.json": b"old legacy feed",
            output_directory / "manifest.json": b"old artifact manifest",
        }
        for target, contents in targets.items():
            target.write_bytes(contents)

        original_replace = build_manifests.os.replace
        failure_injected = False

        def fail_once(source: str | Path, destination: str | Path) -> None:
            nonlocal failure_injected
            if Path(destination).name == "manifest.json" and not failure_injected:
                failure_injected = True
                raise OSError("injected write failure")
            original_replace(source, destination)

        with patch.object(build_manifests.os, "replace", side_effect=fail_once):
            with self.assertRaisesRegex(build_manifests.ManifestError, "已恢复"):
                build_manifests.write_output_set(
                    output_directory,
                    {target.name: {"version": "0.4.0"} for target in targets},
                )

        self.assertTrue(failure_injected)
        for target, contents in targets.items():
            self.assertEqual(target.read_bytes(), contents)
        self.assertEqual({path.name for path in output_directory.iterdir()}, {path.name for path in targets})


if __name__ == "__main__":
    unittest.main()
