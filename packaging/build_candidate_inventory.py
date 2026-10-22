#!/usr/bin/env python3
# @author kongweiguang
"""从真实候选包目录构建并验证新旧 updater 的双信任 inventory。"""

from __future__ import annotations

import argparse
import posixpath
import re
import sys
import tarfile
from pathlib import Path, PurePosixPath
from typing import Any

import build_manifests
import collect_candidates


TARGETS = {
    "x86_64-pc-windows-msvc": ("windows", "x86_64", {"nsis"}),
    "x86_64-unknown-linux-gnu": (
        "linux",
        "x86_64",
        {"appimage", "deb", "appimage-updater"},
    ),
    "x86_64-apple-darwin": ("macos", "x86_64", {"app-bundle", "dmg", "app-updater"}),
    "aarch64-apple-darwin": ("macos", "aarch64", {"app-bundle", "dmg", "app-updater"}),
}


class CandidateInventoryError(ValueError):
    """描述候选包目录、签名目录或 metadata 输出不符合约定。"""


def _relative_directory(root: Path, value: str, label: str) -> tuple[str, Path]:
    """只接受候选根内的相对目录，避免签名输入越过候选包边界。"""
    if not isinstance(value, str) or not value or "\\" in value:
        raise CandidateInventoryError(f"{label} 必须是候选根内的 POSIX 相对目录")
    relative = PurePosixPath(value)
    if relative.is_absolute() or any(part in ("", ".", "..") for part in relative.parts):
        raise CandidateInventoryError(f"{label} 不得为绝对路径或包含路径回退")
    candidate = root.joinpath(*relative.parts)
    current = root
    for component in relative.parts:
        current = current / component
        if current.is_symlink():
            raise CandidateInventoryError(f"{label} 路径不得穿过符号链接")
    if not candidate.is_dir():
        raise CandidateInventoryError(f"{label} 必须是候选根内的真实目录")
    resolved_root = root.resolve(strict=True)
    try:
        candidate.resolve(strict=True).relative_to(resolved_root)
    except (OSError, ValueError) as error:
        raise CandidateInventoryError(f"{label} 不能逃逸候选包目录") from error
    return relative.as_posix(), candidate


def _classify_package(path: Path) -> str | None:
    """按真实产物的格式后缀分类，不依赖产品名、版本号或架构文件名。"""
    name = path.name.lower()
    if path.is_dir() and name.endswith(".app"):
        return "app-bundle"
    if not path.is_file():
        return None
    if name.endswith(".appimage.tar.gz"):
        return "appimage-updater"
    if name.endswith(".app.tar.gz"):
        return "app-updater"
    if name.endswith(".appimage"):
        return "appimage"
    if name.endswith(".deb"):
        return "deb"
    if name.endswith(".dmg"):
        return "dmg"
    if name.endswith("-setup.exe"):
        return "nsis"
    return None


def _validate_updater_archive(archive: Path, expected_root: str, *, directory_root: bool) -> None:
    """校验无签名候选归档的 updater 目录布局，不将结构检查当作验签。"""
    if archive.is_symlink() or not archive.is_file() or archive.stat().st_size == 0:
        raise CandidateInventoryError(f"updater archive must be a non-empty regular file: {archive}")
    try:
        with tarfile.open(archive, mode="r:gz") as package:
            members = package.getmembers()
    except (OSError, tarfile.TarError) as error:
        raise CandidateInventoryError(f"updater archive is not a readable gzip tar: {archive}") from error
    if not members:
        raise CandidateInventoryError(f"updater archive is empty: {archive}")

    top_level: set[str] = set()
    names: set[str] = set()
    has_expected_root = False
    has_app_info = False
    for member in members:
        raw_name = member.name
        normalized_input = raw_name[:-1] if member.isdir() and raw_name.endswith("/") else raw_name
        name = PurePosixPath(normalized_input)
        if (
            not raw_name
            or "\\" in raw_name
            or name.is_absolute()
            or not name.parts
            or posixpath.normpath(normalized_input) != normalized_input
            or any(part in ("", ".", "..") for part in name.parts)
        ):
            raise CandidateInventoryError(f"updater archive contains an unsafe path: {raw_name!r}")
        normalized_name = name.as_posix()
        if normalized_name in names:
            raise CandidateInventoryError(f"updater archive contains a duplicate path: {normalized_name}")
        names.add(normalized_name)
        top_level.add(name.parts[0])
        if normalized_name == expected_root:
            has_expected_root = member.isdir() if directory_root else member.isfile()
        if normalized_name == f"{expected_root}/Contents/Info.plist" and member.isfile():
            has_app_info = True

        if member.issym() or member.islnk():
            link_name = member.linkname
            if not link_name or "\\" in link_name or PurePosixPath(link_name).is_absolute():
                raise CandidateInventoryError(f"updater archive contains an unsafe link: {normalized_name}")
            if member.issym():
                link_base = PurePosixPath(normalized_name).parent.as_posix()
                resolved_link = posixpath.normpath(posixpath.join(link_base, link_name))
            else:
                resolved_link = posixpath.normpath(link_name)
            if resolved_link != expected_root and not resolved_link.startswith(f"{expected_root}/"):
                raise CandidateInventoryError(f"updater archive link escapes its product root: {normalized_name}")
        elif not (member.isfile() or member.isdir()):
            raise CandidateInventoryError(f"updater archive contains an unsupported entry: {raw_name}")

    if top_level != {expected_root} or not has_expected_root:
        raise CandidateInventoryError(
            f"updater archive must contain only its expected top-level product: {expected_root}"
        )
    if directory_root and not has_app_info:
        raise CandidateInventoryError(f"macOS updater archive is missing {expected_root}/Contents/Info.plist")


def validate_unsigned_candidate_set(candidate_directory: Path) -> list[dict[str, str]]:
    """验证四目标候选目录的结构和实际文件集，不读取签名或生成更新 feed。"""
    try:
        root = candidate_directory.resolve(strict=True)
    except OSError as error:
        raise CandidateInventoryError("候选包目录不存在") from error
    if candidate_directory.is_symlink() or not root.is_dir():
        raise CandidateInventoryError("候选包目录必须是普通目录")

    actual_targets = {child.name for child in root.iterdir()}
    required_targets = set(TARGETS)
    if actual_targets != required_targets:
        missing = sorted(required_targets - actual_targets)
        unexpected = sorted(actual_targets - required_targets)
        details = []
        if missing:
            details.append(f"缺少 target：{', '.join(missing)}")
        if unexpected:
            details.append(f"包含未知顶层路径：{', '.join(unexpected)}")
        raise CandidateInventoryError("unsigned candidate set 不完整：" + "; ".join(details))

    summary: list[dict[str, str]] = []
    for target, (_platform, _arch, expected_kinds) in TARGETS.items():
        target_directory = root / target
        if target_directory.is_symlink() or not target_directory.is_dir():
            raise CandidateInventoryError(f"候选包缺少真实 target 目录：{target}")
        discovered: dict[str, Path] = {}
        for artifact in sorted(target_directory.iterdir(), key=lambda item: item.name):
            if artifact.is_symlink():
                raise CandidateInventoryError(f"候选产物不能是符号链接：{artifact.relative_to(root)}")
            kind = _classify_package(artifact)
            if kind is None:
                raise CandidateInventoryError(
                    f"候选目录包含未识别的直接产物：{artifact.relative_to(root)}"
                )
            if kind not in expected_kinds:
                raise CandidateInventoryError(
                    f"{target} 中的产物类型 {kind} 与 target 不匹配：{artifact.name}"
                )
            if kind in discovered:
                raise CandidateInventoryError(f"{target} 存在重复的 {kind} 产物")
            if artifact.is_file() and artifact.stat().st_size == 0:
                raise CandidateInventoryError(f"候选产物不能为空：{artifact.relative_to(root)}")
            if kind == "app-bundle":
                try:
                    collect_candidates._validate_app_bundle(artifact, target_directory)
                except collect_candidates.PackagingError as error:
                    raise CandidateInventoryError(f"macOS app bundle 无效：{artifact.name}") from error
            discovered[kind] = artifact

        missing_kinds = expected_kinds - discovered.keys()
        if missing_kinds:
            raise CandidateInventoryError(
                f"{target} 缺少产物类型：{', '.join(sorted(missing_kinds))}"
            )

        if target == "x86_64-unknown-linux-gnu":
            archive = discovered["appimage-updater"]
            appimage = discovered["appimage"]
            expected_root = archive.name.removesuffix(".tar.gz")
            if expected_root != appimage.name:
                raise CandidateInventoryError("Linux updater archive 必须对应同一候选集中的 raw AppImage")
            _validate_updater_archive(archive, expected_root, directory_root=False)
        elif target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
            archive = discovered["app-updater"]
            app_bundle = discovered["app-bundle"]
            expected_root = archive.name.removesuffix(".tar.gz")
            if expected_root != app_bundle.name:
                raise CandidateInventoryError("macOS updater archive 必须对应同一候选集中的 .app bundle")
            _validate_updater_archive(archive, expected_root, directory_root=True)

        summary.extend(
            {"target": target, "kind": kind, "path": f"{target}/{artifact.name}"}
            for kind, artifact in sorted(discovered.items())
        )
    return summary


def _signature_path(
    root: Path,
    signature_directory: str,
    target: str,
    artifact_name: str,
    label: str,
) -> str:
    """按 actual artifact 名称配对对应信任根的官方 minisign sidecar。"""
    relative = PurePosixPath(signature_directory, target, f"{artifact_name}.sig")
    current = root
    for component in relative.parts:
        current = current / component
        if current.is_symlink():
            raise CandidateInventoryError("签名 sidecar 路径不得穿过符号链接")
    try:
        normalized, sidecar = build_manifests.safe_path(root, relative.as_posix())
    except build_manifests.ManifestError as error:
        raise CandidateInventoryError(
            f"缺少 {label} sidecar：{relative.as_posix()}（签名须由对应官方工具生成后单独放入）"
        ) from error
    if sidecar.is_symlink() or not sidecar.is_file():
        raise CandidateInventoryError(
            f"缺少 {label} sidecar：{normalized}（签名须由对应官方工具生成后单独放入）"
        )
    return normalized


def build_inventory(
    candidate_directory: Path,
    *,
    version: str,
    source_commit: str,
    new_signatures_directory: str,
    legacy_signatures_directory: str,
    published_at: str | None = None,
    notes: str = "",
) -> dict[str, Any]:
    """从每个 target 的直接输出读取实际包名，严格构造现有 builder 接受的 inventory。"""
    try:
        root = candidate_directory.resolve(strict=True)
    except OSError as error:
        raise CandidateInventoryError("候选包目录不存在") from error
    if not root.is_dir() or candidate_directory.is_symlink():
        raise CandidateInventoryError("候选包目录必须是普通目录")

    new_directory, _ = _relative_directory(root, new_signatures_directory, "新 updater sidecar 目录")
    legacy_directory, _ = _relative_directory(root, legacy_signatures_directory, "旧 Tauri sidecar 目录")
    directories_overlap = (
        new_directory == legacy_directory
        or new_directory.startswith(legacy_directory + "/")
        or legacy_directory.startswith(new_directory + "/")
    )
    if directories_overlap:
        raise CandidateInventoryError("新旧 updater sidecar 必须位于互不重叠的目录")
    signature_roots = {new_directory.split("/")[0], legacy_directory.split("/")[0]}
    if signature_roots & TARGETS.keys():
        raise CandidateInventoryError("sidecar 目录不得与 Cargo target 产物目录重叠")

    artifacts: list[dict[str, str | None]] = []
    for target, (platform, arch, expected_kinds) in TARGETS.items():
        target_directory = root / target
        if target_directory.is_symlink() or not target_directory.is_dir():
            raise CandidateInventoryError(f"候选包缺少真实 target 目录：{target}")
        discovered: dict[str, Path] = {}
        for package in sorted(target_directory.iterdir(), key=lambda item: item.name):
            if package.is_symlink():
                raise CandidateInventoryError(f"候选产物不能是符号链接：{package.relative_to(root)}")
            kind = _classify_package(package)
            if kind is None:
                raise CandidateInventoryError(
                    f"候选目录包含未识别的直接产物：{package.relative_to(root)}"
                )
            if kind not in expected_kinds:
                raise CandidateInventoryError(
                    f"{target} 中的产物类型 {kind} 与 target 不匹配：{package.name}"
                )
            if kind in discovered:
                raise CandidateInventoryError(f"{target} 存在重复的 {kind} 产物")
            if package.is_file() and package.stat().st_size == 0:
                raise CandidateInventoryError(f"候选产物不能为空：{package.relative_to(root)}")
            discovered[kind] = package

        missing = expected_kinds - discovered.keys()
        if missing:
            raise CandidateInventoryError(f"{target} 缺少产物类型：{', '.join(sorted(missing))}")

        for kind in sorted(discovered):
            package = discovered[kind]
            relative_path = package.relative_to(root).as_posix()
            needs_new = kind in build_manifests.NEW_UPDATER_KINDS
            needs_legacy = kind in build_manifests.LEGACY_UPDATER_KINDS
            artifacts.append(
                {
                    "platform": platform,
                    "arch": arch,
                    "kind": kind,
                    "path": relative_path,
                    "signaturePath": (
                        _signature_path(root, new_directory, target, package.name, "新 updater")
                        if needs_new
                        else None
                    ),
                    "legacySignaturePath": (
                        _signature_path(root, legacy_directory, target, package.name, "旧 Tauri updater")
                        if needs_legacy
                        else None
                    ),
                }
            )

    allowed_top_level_directories = TARGETS.keys() | signature_roots
    unexpected = {
        child.name
        for child in root.iterdir()
        if child.is_dir() and not child.is_symlink() and child.name not in allowed_top_level_directories
    }
    if unexpected:
        raise CandidateInventoryError(f"候选目录含未登记子目录：{', '.join(sorted(unexpected))}")

    return {
        "schemaVersion": 1,
        "version": version,
        "sourceCommit": source_commit,
        "publishedAt": published_at,
        "notes": notes,
        "artifacts": artifacts,
    }


def _read_public_key(path: Path, label: str) -> str:
    """只解析 minisign 公钥文件的公钥行，拒绝访问 secret-key 文件正文。"""
    if path.is_symlink() or not path.is_file():
        raise CandidateInventoryError(f"{label} 必须是普通公钥文件")
    try:
        with path.open("r", encoding="utf-8") as stream:
            header = stream.readline().strip()
            normalized_header = header.lower()
            public_key_prefix = "untrusted comment: minisign public key"
            if not normalized_header.startswith(public_key_prefix) or (
                normalized_header != public_key_prefix
                and normalized_header[len(public_key_prefix)] not in (":", " ")
            ):
                raise CandidateInventoryError(f"{label} 必须是 minisign .pub 公钥文件")
            value = stream.readline().strip()
    except (OSError, UnicodeError) as error:
        raise CandidateInventoryError(f"无法读取 {label}") from error
    if not re.fullmatch(r"RW[A-Za-z0-9+/]+={0,2}", value):
        raise CandidateInventoryError(f"{label} 内容为空或格式错误")
    return value


def build_and_validate(arguments: argparse.Namespace) -> None:
    """使用现有 builder 的双公钥验签与 metadata 映射，禁止跳过候选签名校验。"""
    if arguments.candidate_dir.is_symlink():
        raise CandidateInventoryError("候选包目录不能是符号链接")
    if arguments.inventory.is_symlink() or arguments.metadata_output.is_symlink():
        raise CandidateInventoryError("inventory 与 metadata 输出不能是符号链接")
    root = arguments.candidate_dir.resolve(strict=True)
    inventory_path = arguments.inventory.resolve(strict=False)
    metadata_directory = arguments.metadata_output.resolve(strict=False)
    for label, output_path in (("inventory", inventory_path), ("metadata", metadata_directory)):
        try:
            output_path.relative_to(root)
        except ValueError:
            continue
        raise CandidateInventoryError(f"{label} 输出必须放在候选目录之外")
    if inventory_path.parent != metadata_directory:
        raise CandidateInventoryError("inventory 必须与 updater metadata 写入同一目录以便整体回滚")

    new_key = _read_public_key(arguments.new_public_key, "新 updater minisign 公钥")
    legacy_key = _read_public_key(arguments.legacy_public_key, "旧 Tauri updater minisign 公钥")
    inventory = build_inventory(
        arguments.candidate_dir,
        version=arguments.version,
        source_commit=arguments.source_commit,
        published_at=arguments.published_at,
        notes=arguments.notes,
        new_signatures_directory=arguments.new_signatures_directory,
        legacy_signatures_directory=arguments.legacy_signatures_directory,
    )
    cargo_metadata, legacy_metadata, report = build_manifests.validate_inventory(
        root,
        inventory,
        arguments.base_url,
        arguments.signing_class,
        public_key=new_key,
        legacy_public_key=legacy_key,
        verifier=arguments.minisign,
        verify_crypto=True,
    )

    names = build_manifests.build_output_names(arguments.signing_class)
    if inventory_path.name in names.values():
        raise CandidateInventoryError("inventory 文件名不能覆盖 updater metadata")
    build_manifests.write_output_set(
        metadata_directory,
        {
            inventory_path.name: inventory,
            names["cargo"]: cargo_metadata,
            names["legacy"]: legacy_metadata,
            names["manifest"]: report["artifactManifest"],
            names["checksums"]: report["checksums"],
        },
    )


def parse_arguments(argv: list[str] | None = None) -> argparse.Namespace:
    """候选结构检查与双信任签验分成显式模式，避免 unsigned 冒充 feed 验证。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate-dir", type=Path, required=True)
    parser.add_argument("--validate-unsigned", action="store_true")
    parser.add_argument("--new-signatures-dir", dest="new_signatures_directory")
    parser.add_argument("--legacy-signatures-dir", dest="legacy_signatures_directory")
    parser.add_argument("--new-public-key", type=Path)
    parser.add_argument("--legacy-public-key", type=Path)
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--metadata-output", type=Path)
    parser.add_argument("--version")
    parser.add_argument("--source-commit")
    parser.add_argument("--published-at")
    parser.add_argument("--notes")
    parser.add_argument("--base-url")
    parser.add_argument("--signing-class", choices=("test", "production"))
    parser.add_argument("--minisign")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    """分别执行 unsigned 结构校验或严格双信任根验签，拒绝隐式降级。"""
    try:
        arguments = parse_arguments(argv)
        if arguments.validate_unsigned:
            signing_only = (
                "new_signatures_directory",
                "legacy_signatures_directory",
                "new_public_key",
                "legacy_public_key",
                "inventory",
                "metadata_output",
                "version",
                "source_commit",
                "published_at",
                "notes",
                "base_url",
                "signing_class",
                "minisign",
            )
            if any(getattr(arguments, name) is not None for name in signing_only):
                raise CandidateInventoryError(
                    "unsigned 结构检查不能接收签名或 feed 参数；请使用双信任签验模式"
                )
            summary = validate_unsigned_candidate_set(arguments.candidate_dir)
            print(
                "Unsigned candidate matrix is structurally complete; "
                "no signatures or updater metadata were verified or generated."
            )
            for artifact in summary:
                print(f"{artifact['target']} {artifact['kind']}: {artifact['path']}")
            return 0
        required = (
            "new_signatures_directory",
            "legacy_signatures_directory",
            "new_public_key",
            "legacy_public_key",
            "inventory",
            "metadata_output",
            "version",
            "source_commit",
            "base_url",
            "signing_class",
        )
        missing = [name for name in required if getattr(arguments, name) is None]
        if missing:
            raise CandidateInventoryError(
                "双信任签验模式缺少显式参数：" + ", ".join(missing)
            )
        if arguments.notes is None:
            arguments.notes = ""
        if arguments.minisign is None:
            arguments.minisign = "minisign"
        build_and_validate(arguments)
    except (
        CandidateInventoryError,
        build_manifests.ManifestError,
        collect_candidates.PackagingError,
        OSError,
        tarfile.TarError,
    ) as error:
        print(f"candidate inventory failed: {error}", file=sys.stderr)
        return 2
    print("已验证候选包双信任签名并生成 inventory 与独立 updater metadata。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
