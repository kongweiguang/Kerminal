# @author kongweiguang
"""用合成 workspace 回归候选产物定位和路径保护。"""

from __future__ import annotations

import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import collect_candidates


class CandidateCollectorTests(unittest.TestCase):
    """只在临时目录验证打包路径，不运行 Cargo 或访问用户数据。"""

    def setUp(self) -> None:
        """每例创建同构的最小 workspace，确保测试与本地 checkout 隔离。"""
        self.temporary = tempfile.TemporaryDirectory()
        self.workspace = Path(self.temporary.name)
        self._write("Cargo.toml", '[workspace.package]\nversion = "1.2.3-rc.1"\n')
        self._write(
            "crates/kerminal-desktop/Cargo.toml",
            '[[bin]]\nname = "kerminal"\npath = "src/main.rs"\n',
        )
        config = (
            'product-name = "Kerminal"\n'
            'out-dir = "../target/package-output"\n'
            '\n[[binaries]]\npath = "kerminal"\nmain = true\n'
        )
        self._write("packaging/Packager.windows.toml", config)
        self._write("packaging/Packager.linux.toml", config)
        self._write("packaging/Packager.macos.toml", config)

    def tearDown(self) -> None:
        """释放每例独立的合成目录，避免在仓库 target 留测试产物。"""
        self.temporary.cleanup()

    def _write(self, relative: str, content: str | bytes) -> Path:
        """按 workspace 相对路径创建 fixture，保留真实 manifest 读取边界。"""
        destination = self.workspace / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, str):
            destination.write_text(content, encoding="utf-8")
        else:
            destination.write_bytes(content)
        return destination

    def test_packager_output_resolves_to_explicit_workspace_target_directory(self) -> None:
        """验证 cargo-packager 从显式配置目录解析 out-dir 到仓库 target。"""
        layout = collect_candidates._load_layout(self.workspace, "x86_64-pc-windows-msvc")
        self.assertEqual(layout.package_output, self.workspace / "target/package-output")
        self.assertTrue(layout.package_output.is_dir())

    def test_packager_output_rejects_configured_path_drift(self) -> None:
        """即使路径仍位于仓库内，out-dir 偏离固定候选目录也要拒绝收集。"""
        self._write(
            "packaging/Packager.windows.toml",
            'product-name = "Kerminal"\n'
            'out-dir = "../crates/kerminal-desktop"\n'
            '\n[[binaries]]\npath = "kerminal"\nmain = true\n',
        )
        with self.assertRaises(collect_candidates.PackagingError):
            collect_candidates._load_layout(self.workspace, "x86_64-pc-windows-msvc")

    def test_packager_output_rejects_absolute_path(self) -> None:
        """绝对 out-dir 绕过显式配置目录约束，必须拒绝。"""
        self._write(
            "packaging/Packager.windows.toml",
            'product-name = "Kerminal"\nout-dir = "C:/outside/package-output"\n',
        )
        with self.assertRaises(collect_candidates.PackagingError):
            collect_candidates._load_layout(self.workspace, "x86_64-pc-windows-msvc")

    def test_prepare_removes_only_exact_package_files_not_dependency_bundles(self) -> None:
        """验证 prepare 只移除当前目标一级产物，不触碰 target 中依赖缓存。"""
        expected = self._write(
            "target/package-output/kerminal_1.2.3-rc.1_x86_64-setup.exe", b"old installer"
        )
        unrelated = self._write("target/package-output/other-tool.exe", b"keep")
        dependency_bundle = self._write("target/dependency/bundle/cache.keep", b"keep")

        collect_candidates.prepare_candidates(self.workspace, "x86_64-pc-windows-msvc")

        self.assertFalse(expected.exists())
        self.assertTrue(unrelated.exists())
        self.assertTrue(dependency_bundle.exists())

    def test_stage_directory_refuses_nested_content_instead_of_recursive_delete(self) -> None:
        """候选目录只允许平铺文件，避免 cleanup 递归进入未知目录或 junction。"""
        nested = self._write("target/package-candidates/x86_64-pc-windows-msvc/nested/keep", b"keep")
        with self.assertRaises(collect_candidates.PackagingError):
            collect_candidates.prepare_candidates(self.workspace, "x86_64-pc-windows-msvc")
        self.assertTrue(nested.exists())

    def test_windows_collector_ignores_nested_dependency_packages(self) -> None:
        """只收集显式输出目录内唯一的 NSIS 文件，不遍历 target/bundle 缓存。"""
        package = self._write(
            "target/package-output/kerminal_1.2.3-rc.1_x86_64-setup.exe", b"installer"
        )
        self._write("target/some-dependency/bundle/other_1.exe", b"dependency")

        result = collect_candidates.collect_candidates(self.workspace, "x86_64-pc-windows-msvc")

        self.assertEqual([path.name for path in result.iterdir()], [package.name])
        self.assertEqual((result / package.name).read_bytes(), b"installer")

    def test_linux_collector_requires_each_expected_direct_package(self) -> None:
        """验证 AppImage、DEB 与新 updater 归档都来自当前 packager 输出。"""
        appimage = self._write(
            "target/package-output/kerminal_1.2.3-rc.1_x86_64.AppImage", b"appimage"
        )
        deb = self._write("target/package-output/kerminal_1.2.3-rc.1_amd64.deb", b"deb")
        self._write("target/dependency/bundle/fake.deb", b"dependency")

        result = collect_candidates.collect_candidates(self.workspace, "x86_64-unknown-linux-gnu")

        self.assertEqual(
            {item.name for item in result.iterdir()},
            {appimage.name, deb.name, f"{appimage.name}.tar.gz"},
        )
        with tarfile.open(result / f"{appimage.name}.tar.gz", mode="r:gz") as archive:
            self.assertEqual([item.name for item in archive.getmembers()], [appimage.name])

    def test_duplicate_direct_packages_fail_closed(self) -> None:
        """重复匹配时拒绝生成候选包，避免任意选取旧版或错误架构文件。"""
        self._write("target/package-output/kerminal_1.2.3-rc.1_x86_64.AppImage", b"one")
        self._write("target/package-output/kerminal_1.2.3-rc.1_aarch64.AppImage", b"two")
        self._write("target/package-output/kerminal_1.2.3-rc.1_amd64.deb", b"deb")

        with self.assertRaises(collect_candidates.PackagingError):
            collect_candidates.collect_candidates(self.workspace, "x86_64-unknown-linux-gnu")

    def test_macos_collector_archives_only_the_named_top_level_app(self) -> None:
        """嵌套 helper app 不会被计为产品，app bundle 和 updater tar 均保留。"""
        app_bundle = self.workspace / "target/package-output/Kerminal.app"
        (app_bundle / "Contents/Frameworks/Helper.app/Contents").mkdir(parents=True)
        (app_bundle / "Contents/Info.plist").write_text("plist", encoding="utf-8")
        self._write("target/package-output/Kerminal_1.2.3-rc.1_x64.dmg", b"dmg")

        result = collect_candidates.collect_candidates(self.workspace, "x86_64-apple-darwin")

        self.assertEqual(
            {item.name for item in result.iterdir()},
            {"Kerminal_1.2.3-rc.1_x64.dmg", "Kerminal.app.tar.gz", "Kerminal.app"},
        )
        with tarfile.open(result / "Kerminal.app.tar.gz", mode="r:gz") as archive:
            members = {item.name.rstrip("/") for item in archive.getmembers()}
        self.assertIn("Kerminal.app/Contents/Info.plist", members)
        self.assertIn("Kerminal.app/Contents/Frameworks/Helper.app/Contents", members)

    def test_target_symlink_escape_is_rejected_before_cleanup(self) -> None:
        """测试 target 根逃逸时 fail closed；Windows 无链接权限则在创建处跳过。"""
        outside = Path(self.temporary.name).parent / f"{Path(self.temporary.name).name}-outside"
        outside.mkdir()
        sentinel = outside / "keep"
        sentinel.write_bytes(b"keep")
        target = self.workspace / "target"
        try:
            target.symlink_to(outside, target_is_directory=True)
        except OSError as error:
            self.skipTest(f"directory symlinks are unavailable: {error}")
        try:
            with self.assertRaises(collect_candidates.PackagingError):
                collect_candidates.prepare_candidates(self.workspace, "x86_64-pc-windows-msvc")
            self.assertTrue(sentinel.exists())
        finally:
            target.unlink(missing_ok=True)
            sentinel.unlink(missing_ok=True)
            outside.rmdir()


if __name__ == "__main__":
    unittest.main()
