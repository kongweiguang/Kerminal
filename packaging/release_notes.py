#!/usr/bin/env python3
# @author kongweiguang
"""按约定式提交生成 GPUI beta 发布说明的新功能、改进、修复分节。

发布说明正文由代码生成而不是在 workflow 里手写固定文本，原因是每个 beta 的实际变更
来自上一个发布 tag 到当前 tag 之间的提交，手写正文容易和真实产出脱节。分类只依赖提交
标题，避免额外维护一份与 git 历史重复的变更清单文件。
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# 分节顺序同时决定发布说明的展示顺序，保持“新功能、改进、修复”的阅读顺序。
SECTION_ORDER = ("新功能", "改进", "修复")

# 约定式提交类型到分节的映射。chore 只记录版本号提升和仓库杂务，读者关心的变更都在
# 其他类型里；其余非 fix 类型代表行为、质量或工程改善，统一归入“改进”。
CATEGORY_BY_TYPE = {
    "feat": "新功能",
    "fix": "修复",
    "perf": "改进",
    "refactor": "改进",
    "docs": "改进",
    "build": "改进",
    "test": "改进",
    "ci": "改进",
    "style": "改进",
    "revert": "改进",
}

# 匹配 "type(scope)!: summary"。scope 和破坏性标记对发布说明读者没有价值，只保留摘要。
SUBJECT_PATTERN = re.compile(r"^(?P<type>[a-z]+)(?:\([^)]*\))?!?:\s*(?P<summary>\S.*)$")

# 未按约定式提交书写的标题无法判断类型，按“改进”收录，避免真实变更被静默丢弃。
FALLBACK_SECTION = "改进"

DOWNLOAD_SECTION = (
    "### 下载",
    "",
    "- Windows x64：NSIS 安装包（当前用户安装）",
    "- Linux x64：AppImage / DEB",
    "- macOS x64 / arm64：DMG",
)

NOTICE_SECTION = (
    "### 注意",
    "",
    "- 所有包均为未签名构建。macOS 可能需要在 Finder 中右键“打开”，Windows 可能触发 SmartScreen 提示。",
    "- 本 beta 未配置更新器签名和更新 feed，不会自动更新。",
    "- GPUI 版数据根为 `~/.kerminal-gpui`，与旧版 `~/.kerminal` 隔离；旧配置迁移见仓库 README。",
    "- 下载后可用 `SHA256SUMS.txt` 校验安装包。",
)


def classify(subject: str) -> tuple[str, str] | None:
    """把单条提交标题映射为（分节，说明）；返回 None 表示该提交不进入发布说明。

    只读取标题而不读取正文，因为提交正文里常带内部排查细节，不适合直接公开；
    返回说明时去掉类型与 scope 前缀，让分节里的条目读起来是一句完整的中文说明。
    """
    subject = subject.strip()
    if not subject:
        return None
    match = SUBJECT_PATTERN.match(subject)
    if match is None:
        return (FALLBACK_SECTION, subject)
    section = CATEGORY_BY_TYPE.get(match.group("type"))
    if section is None:
        return None
    return (section, match.group("summary").strip())


def collect_sections(subjects: list[str]) -> dict[str, list[str]]:
    """按分节聚合提交摘要，保持提交顺序并去掉重复条目。

    发布 tag 的重建可能与首次发布取到同一批提交，重复条目会让说明看起来像重复发布，
    因此按分节内首次出现的顺序去重，不排序，便于读者按时间顺序理解变更。
    """
    sections: dict[str, list[str]] = {section: [] for section in SECTION_ORDER}
    for subject in subjects:
        classified = classify(subject)
        if classified is None:
            continue
        section, summary = classified
        entries = sections.setdefault(section, [])
        if summary not in entries:
            entries.append(summary)
    return sections


def render(version: str, sections: dict[str, list[str]], source_commit: str) -> str:
    """渲染完整的发布说明正文；没有条目的分节直接省略，避免出现空标题。

    正文保持固定顺序：变更分节在前，下载与注意事项在后，即读者先看“这版改了什么”，
    再看“怎么拿到它、有什么限制”。
    """
    lines = [
        f"**Kerminal GPUI {version} beta**",
        "",
        "这是 Kerminal 原生 GPUI 重写分支的 beta 测试包。功能迁移和原生验收仍在进行中，请勿用于生产环境。",
    ]

    populated = [section for section in SECTION_ORDER if sections.get(section)]
    if populated:
        for section in populated:
            lines.extend(["", f"### {section}", ""])
            lines.extend(f"- {entry}" for entry in sections[section])
    else:
        lines.extend(["", "本次发布没有可归类的变更提交。"])

    lines.extend(["", *DOWNLOAD_SECTION, "", *NOTICE_SECTION])
    lines.extend(["", f"提交：`{source_commit}`"])
    return "\n".join(lines) + "\n"


def _git(arguments: list[str], repository: Path) -> subprocess.CompletedProcess[str]:
    """在指定仓库执行 git；调用方决定是否把返回码视为失败。

    git 输出用于生成公开发布说明，因此固定 UTF-8 解码，避免 Windows 默认代码页把
    中文提交标题解成乱码。
    """
    return subprocess.run(
        ["git", *arguments],
        cwd=repository,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def resolve_previous_tag(tag: str, repository: Path) -> str | None:
    """取当前 tag 之前最近的发布 tag；首个版本或浅克隆取不到时返回 None。

    `git describe` 按图距离取最近的 tag，因此 beta.N 会命中 beta.N-1，
    首个 beta 会命中上一个正式版，无需在 workflow 里传递上一次发布的 tag。
    """
    result = _git(
        ["describe", "--tags", "--abbrev=0", "--match", "v*", f"{tag}^"],
        repository,
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def collect_subjects(tag: str, previous_tag: str | None, repository: Path) -> list[str]:
    """读取发布区间内非合并提交的标题；没有上一个 tag 时取全部历史。"""
    revision = f"{previous_tag}..{tag}" if previous_tag else tag
    result = _git(
        ["log", "--no-merges", "--pretty=format:%s", revision],
        repository,
    )
    if result.returncode != 0:
        raise SystemExit(
            f"git log 失败：{revision}\n{result.stderr.strip()}"
        )
    return [line for line in result.stdout.splitlines() if line.strip()]


def resolve_commit(tag: str, repository: Path) -> str:
    """取 tag 指向的完整提交号，写进发布说明方便对照源码快照。"""
    result = _git(["rev-parse", f"{tag}^{{commit}}"], repository)
    if result.returncode != 0:
        raise SystemExit(f"git rev-parse 失败：{tag}\n{result.stderr.strip()}")
    return result.stdout.strip()


def build_notes(tag: str, version: str, repository: Path) -> str:
    """根据真实提交历史生成发布说明正文，供 workflow 与手工重建共用。"""
    previous_tag = resolve_previous_tag(tag, repository)
    subjects = collect_subjects(tag, previous_tag, repository)
    sections = collect_sections(subjects)
    return render(version, sections, resolve_commit(tag, repository))


def main(argv: list[str] | None = None) -> int:
    """命令行入口：写文件或输出到标准输出，便于 CI 与本地预览。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True, help="发布 tag，例如 v0.4.0-beta.4")
    parser.add_argument("--version", help="发布版本号，缺省去掉 tag 的 v 前缀")
    parser.add_argument("--repository", default=".", help="仓库根目录，缺省当前目录")
    parser.add_argument("--out", help="输出文件路径，缺省写到标准输出")
    arguments = parser.parse_args(argv)

    repository = Path(arguments.repository).resolve()
    version = arguments.version or arguments.tag.lstrip("v")
    notes = build_notes(arguments.tag, version, repository)
    if arguments.out:
        output = Path(arguments.out)
        output.write_text(notes, encoding="utf-8", newline="\n")
    else:
        sys.stdout.write(notes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())