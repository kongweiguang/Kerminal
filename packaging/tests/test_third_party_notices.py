# @author kongweiguang
"""核对许可证导航、cargo-about 目标与报告模板保持同一交付契约。"""

from pathlib import Path
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ThirdPartyNoticeTests(unittest.TestCase):
    """把第三方来源声明与自动报告入口固定为可重复验证的打包契约。"""

    def test_notice_separates_reference_from_copied_source(self) -> None:
        """确保仅作参考的实现与明确登记的 vendored 源码保留不同来源记录。"""
        notice = (ROOT / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")

        self.assertIn("80f1bb45fa5b8f278a98a6e38a016c2f89c7eb12", notice)
        self.assertIn("没有移植 Zed 的类型、函数或代码片段", notice)
        self.assertIn("17c5d4be46172ca508f92546bfae5fe5ef477f11", notice)
        self.assertIn("不是随包带入的上游命令手册或源码", notice)
        self.assertIn("不是第三方静态资源", notice)
        self.assertIn("11 个 GPUI 终端项目", notice)
        self.assertIn("licenses/THIRD_PARTY_DEPENDENCIES.html", notice)
        self.assertIn("ksni 0.3.6", notice)
        self.assertIn("GNU 项目将其公有领域条款及附加宽松许可说明为与 GNU GPL 兼容", notice)
        self.assertIn("cargo-packager-updater 0.2.3", notice)
        self.assertIn("除下表明确登记的 vendored 源码外", notice)
        self.assertIn("本项目独立实现，仅参考", notice)
        self.assertIn("vendor/cargo-packager-updater/UPSTREAM_PATCHES.md", notice)

        vendor_source = (
            ROOT / "vendor" / "cargo-packager-updater" / "UPSTREAM_PATCHES.md"
        ).read_text(encoding="utf-8")
        self.assertIn("copied from the exact 0.2.3 Cargo registry package", vendor_source)
        self.assertIn("is narrowly patched", vendor_source)

    def test_cargo_about_checks_product_targets_and_transitive_dependencies(self) -> None:
        """让每个发布平台与传递依赖进入报告，并让未审许可证保持阻断。"""
        configuration = tomllib.loads((ROOT / "about.toml").read_text(encoding="utf-8"))

        self.assertEqual(
            set(configuration["targets"]),
            {
                "x86_64-pc-windows-msvc",
                "x86_64-unknown-linux-gnu",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
            },
        )
        self.assertFalse(configuration["ignore-transitive-dependencies"])
        self.assertFalse(configuration["ignore-build-dependencies"])
        self.assertTrue(configuration["ignore-dev-dependencies"])
        self.assertNotIn("GPL-3.0-only", configuration["accepted"])
        self.assertNotIn("NCSA", configuration["accepted"])
        self.assertNotIn("GPL-3.0-or-later", configuration["accepted"])
        self.assertIn("Unlicense", configuration["accepted"])
        for package in (
            "kerminal-domain",
            "kerminal-application",
            "kerminal-terminal",
            "kerminal-infrastructure",
            "kerminal-ui",
            "kerminal-mcp",
            "kerminal-desktop",
            "xtask",
        ):
            self.assertIn("GPL-3.0-or-later", configuration[package]["accepted"])

    def test_template_includes_each_package_and_original_license_text(self) -> None:
        """避免模板退化成只列 SPDX 名称而遗漏包版本或许可证正文。"""
        template = (ROOT / "licenses" / "third-party-dependencies.hbs").read_text(
            encoding="utf-8"
        )

        self.assertIn("{{#each licenses}}", template)
        self.assertIn("{{#each used_by}}", template)
        self.assertIn("{{crate.name}} {{crate.version}}", template)
        self.assertIn("<pre>{{text}}</pre>", template)

    def test_project_license_remains_separate(self) -> None:
        """确认项目 GPL 正文仍使用根 LICENSE，而不冒充第三方依赖许可证。"""
        project_license = (ROOT / "LICENSE").read_text(encoding="utf-8")
        notice = (ROOT / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")

        self.assertIn("GNU GENERAL PUBLIC LICENSE", project_license)
        self.assertIn("GPL-3.0-or-later", notice)
