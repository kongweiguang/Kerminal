# @author kongweiguang
"""验证发布说明生成器的分节归类、去重与空分节省略。"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release_notes


class ReleaseNotesTests(unittest.TestCase):
    """用真实提交标题样式验证归类边界，不依赖具体版本号或仓库状态。"""

    def test_feat_and_fix_are_placed_in_their_sections(self) -> None:
        """功能与修复提交必须落到对应分节，并去掉类型与 scope 前缀。"""
        sections = release_notes.collect_sections(
            [
                "feat(gpui): Agent 终端跟随各停靠宿主",
                "fix(ci): 补齐 macOS/Linux lint 与 fish 探测",
            ]
        )

        self.assertEqual(sections["新功能"], ["Agent 终端跟随各停靠宿主"])
        self.assertEqual(sections["修复"], ["补齐 macOS/Linux lint 与 fish 探测"])
        self.assertEqual(sections["改进"], [])

    def test_release_housekeeping_is_skipped(self) -> None:
        """chore 提交只是版本号提升等仓库杂务，不得出现在对外发布说明里。"""
        self.assertIsNone(release_notes.classify("chore(release): 发布 v0.4.0-beta.4"))

    def test_breaking_marker_and_missing_scope_are_handled(self) -> None:
        """无 scope 与破坏性标记的标题仍要正确归类并保留摘要。"""
        self.assertEqual(
            release_notes.classify("feat!: 重建原生工作区"), ("新功能", "重建原生工作区")
        )

    def test_unconventional_subject_falls_back_to_improvements(self) -> None:
        """不符合约定式提交的标题按改进收录，避免真实变更被静默丢弃。"""
        self.assertEqual(
            release_notes.classify("更新 README 与界面截图"),
            ("改进", "更新 README 与界面截图"),
        )

    def test_duplicate_summaries_appear_once_per_section(self) -> None:
        """同一摘要重复出现时只保留一次，避免重建发布页时条目翻倍。"""
        sections = release_notes.collect_sections(
            ["fix: 广播取消后停止写入后续目标", "fix(core): 广播取消后停止写入后续目标"]
        )

        self.assertEqual(sections["修复"], ["广播取消后停止写入后续目标"])

    def test_render_keeps_section_order_and_omits_empty_sections(self) -> None:
        """分节顺序固定为新功能、改进、修复，没有条目的分节不输出空标题。"""
        rendered = release_notes.render(
            "0.4.0-beta.4",
            {"新功能": ["重建原生工作区"], "改进": [], "修复": ["修复门禁检出差异"]},
            "abc123",
        )

        self.assertLess(rendered.index("### 新功能"), rendered.index("### 修复"))
        self.assertNotIn("### 改进", rendered)
        self.assertLess(rendered.index("### 修复"), rendered.index("### 下载"))
        self.assertIn("提交：`abc123`", rendered)


if __name__ == "__main__":
    unittest.main()