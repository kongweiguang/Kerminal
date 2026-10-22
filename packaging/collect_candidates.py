# @author kongweiguang
"""Safely stage the exact unsigned files emitted by cargo-packager."""

from __future__ import annotations

import argparse
import fnmatch
import os
import shutil
import sys
import tarfile
import tomllib
from dataclasses import dataclass
from pathlib import Path


SUPPORTED_TARGETS = {
    "x86_64-pc-windows-msvc": "windows",
    "x86_64-unknown-linux-gnu": "linux",
    "x86_64-apple-darwin": "macos",
    "aarch64-apple-darwin": "macos",
}


class PackagingError(RuntimeError):
    """表示候选包的路径或产物集合不满足封闭契约。"""


@dataclass(frozen=True)
class PackageLayout:
    """描述单次目标打包所允许读取和清理的精确路径。"""

    workspace: Path
    target: str
    target_root: Path
    package_crate: Path
    package_output: Path
    stage_output: Path
    version: str
    binary_name: str
    product_name: str


def _resolve_child(base: Path, candidate: Path, label: str) -> Path:
    """将删除/收集路径限制在已解析的父目录内，拒绝符号链接逃逸。"""
    resolved_base = base.resolve(strict=True)
    resolved_candidate = candidate.resolve(strict=False)
    if resolved_candidate == resolved_base or not resolved_candidate.is_relative_to(resolved_base):
        raise PackagingError(f"{label} must stay below {resolved_base}: {resolved_candidate}")
    return resolved_candidate


def _load_layout(workspace: Path, target: str) -> PackageLayout:
    """按 cargo-packager 0.11.8 的配置目录解析 out-dir，并锁定仓库 target 边界。"""
    if target not in SUPPORTED_TARGETS:
        raise PackagingError(f"unsupported packaging target: {target}")

    workspace = workspace.resolve(strict=True)
    package_crate = _resolve_child(
        workspace,
        workspace / "crates" / "kerminal-desktop",
        "desktop package crate",
    )
    if package_crate != workspace / "crates" / "kerminal-desktop":
        raise PackagingError(f"desktop package crate must not redirect through a link: {package_crate}")

    packaging_directory = workspace / "packaging"
    resolved_packaging_directory = _resolve_child(
        workspace,
        packaging_directory,
        "packager config directory",
    )
    if resolved_packaging_directory != packaging_directory:
        raise PackagingError(
            f"packager config directory must not redirect through a link: {packaging_directory}"
        )

    try:
        with (workspace / "Cargo.toml").open("rb") as cargo_file:
            workspace_manifest = tomllib.load(cargo_file)
        with (package_crate / "Cargo.toml").open("rb") as cargo_file:
            desktop_manifest = tomllib.load(cargo_file)
        platform = SUPPORTED_TARGETS[target]
        config_path = packaging_directory / f"Packager.{platform}.toml"
        if config_path.is_symlink() or config_path.resolve(strict=True) != config_path:
            raise PackagingError(f"packager config must be a regular workspace file: {config_path}")
        with config_path.open("rb") as config_file:
            packager_config = tomllib.load(config_file)
        version = workspace_manifest["workspace"]["package"]["version"]
        desktop_binary_names = {
            binary.get("name") for binary in desktop_manifest.get("bin", [])
        }
        main_binaries = [
            binary
            for binary in packager_config.get("binaries", [])
            if binary.get("main") is True
        ]
        if len(main_binaries) != 1:
            raise PackagingError("each packager config must declare exactly one main binary")
        binary_name = Path(main_binaries[0]["path"]).name
        if binary_name != "kerminal" or binary_name not in desktop_binary_names:
            raise PackagingError("packager main binary must match the desktop kerminal target")
        product_name = packager_config["product-name"]
        configured_out_dir = Path(packager_config["out-dir"])
    except (KeyError, OSError, StopIteration, tomllib.TOMLDecodeError) as error:
        raise PackagingError(f"could not read package identity: {error}") from error

    target_root = workspace / "target"
    proposed_target_root = _resolve_child(workspace, target_root, "Cargo target directory")
    if proposed_target_root != target_root:
        raise PackagingError(f"Cargo target directory must not redirect through a link: {target_root}")
    target_root.mkdir(parents=True, exist_ok=True)
    resolved_target_root = _resolve_child(workspace, target_root, "Cargo target directory")
    if resolved_target_root != target_root:
        raise PackagingError(f"Cargo target directory must not redirect through a link: {target_root}")

    if configured_out_dir.is_absolute():
        raise PackagingError("cargo-packager out-dir must be relative to the explicit config directory")
    package_output = (packaging_directory / configured_out_dir).resolve(strict=False)
    expected_package_output = (resolved_target_root / "package-output").resolve(strict=False)
    if package_output != expected_package_output:
        raise PackagingError(
            "cargo-packager out-dir must resolve to the workspace target/package-output directory"
        )
    _resolve_child(resolved_target_root, package_output, "cargo-packager output directory")
    if package_output.is_symlink():
        raise PackagingError(f"cargo-packager output directory must be a real target child: {package_output}")
    package_output.mkdir(parents=True, exist_ok=True)
    if package_output.is_symlink() or package_output.resolve(strict=True) != expected_package_output:
        raise PackagingError(f"cargo-packager output directory must be a real target child: {package_output}")

    stage_output = _resolve_child(
        resolved_target_root,
        resolved_target_root / "package-candidates" / target,
        "candidate staging directory",
    )
    if stage_output != resolved_target_root / "package-candidates" / target:
        raise PackagingError(f"candidate staging directory must be a direct target child: {stage_output}")
    return PackageLayout(
        workspace=workspace,
        target=target,
        target_root=resolved_target_root,
        package_crate=package_crate,
        package_output=package_output,
        stage_output=stage_output,
        version=version,
        binary_name=binary_name,
        product_name=product_name,
    )


def _artifact_patterns(layout: PackageLayout) -> dict[str, str]:
    """只匹配 cargo-packager 直接写出的安装包，不搜索依赖目录。"""
    prefix = f"{layout.binary_name}_{layout.version}_"
    if layout.target == "x86_64-pc-windows-msvc":
        return {"NSIS installer": f"{prefix}*-setup.exe"}
    if layout.target == "x86_64-unknown-linux-gnu":
        return {
            "AppImage": f"{prefix}*.AppImage",
            "DEB": f"{prefix}*.deb",
        }
    return {"DMG": f"{layout.product_name}_{layout.version}_*.dmg"}


def _stale_archive_patterns(layout: PackageLayout) -> dict[str, str]:
    """清理由旧打包流程遗留的 updater 归档，避免误将缓存当作本轮输入。"""
    prefix = f"{layout.binary_name}_{layout.version}_"
    if layout.target == "x86_64-unknown-linux-gnu":
        return {"AppImage updater archive": f"{prefix}*.AppImage.tar.gz"}
    if layout.target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
        return {"APP updater archive": f"{layout.product_name}.app.tar.gz"}
    return {}


def _clear_flat_files(directory: Path, allowed_root: Path, label: str) -> None:
    """只移除本轮平铺输出文件；目录和链接一律拒绝，避免递归误删。"""
    if not directory.exists():
        return
    resolved_directory = _resolve_child(allowed_root, directory, label)
    if directory.is_symlink() or not resolved_directory.is_dir():
        raise PackagingError(f"{label} must be a real directory: {directory}")
    for child in directory.iterdir():
        if child.is_symlink() or not child.is_file():
            raise PackagingError(f"refusing to remove non-file candidate: {child}")
        _resolve_child(resolved_directory, child, f"{label} file")
        child.unlink()


def _remove_exact_bundle(directory: Path, allowed_root: Path, bundle_name: str, label: str) -> None:
    """只递归清理已解析在指定 target 输出根内的唯一产品 bundle。"""
    bundle = directory / bundle_name
    if not bundle.exists() and not bundle.is_symlink():
        return
    resolved_bundle = _resolve_child(directory, bundle, label)
    if bundle.is_symlink() or not bundle.is_dir():
        raise PackagingError(f"refusing to remove a non-directory {label}: {bundle}")
    expected_bundle = directory.resolve(strict=True) / bundle_name
    if resolved_bundle != expected_bundle or not resolved_bundle.is_relative_to(allowed_root.resolve(strict=True)):
        raise PackagingError(f"{label} must resolve directly below its approved output directory: {bundle}")
    shutil.rmtree(resolved_bundle)


def _clear_candidate_stage(layout: PackageLayout) -> None:
    """删除本目标候选目录中的平铺文件和已知 app bundle，拒绝未知子目录。"""
    if not layout.stage_output.exists() and not layout.stage_output.is_symlink():
        return
    resolved_stage = _resolve_child(layout.target_root, layout.stage_output, "candidate staging directory")
    if layout.stage_output.is_symlink() or resolved_stage != layout.stage_output or not resolved_stage.is_dir():
        raise PackagingError(f"candidate staging directory must be a real target child: {layout.stage_output}")

    macos_target = layout.target in ("x86_64-apple-darwin", "aarch64-apple-darwin")
    bundle_name = f"{layout.product_name}.app"
    for child in resolved_stage.iterdir():
        if child.is_symlink():
            raise PackagingError(f"refusing to remove symlink candidate: {child}")
        if child.is_file():
            _resolve_child(resolved_stage, child, "candidate staging file")
            child.unlink()
        elif macos_target and child.is_dir() and child.name == bundle_name:
            _remove_exact_bundle(resolved_stage, resolved_stage, bundle_name, "staged macOS app bundle")
        else:
            raise PackagingError(f"refusing to remove unknown candidate directory: {child}")


def _matches(directory: Path, pattern: str, label: str) -> Path:
    """要求在 cargo-packager 的直接输出目录内唯一命中，排除嵌套框架和旧缓存。"""
    results = [
        child
        for child in directory.iterdir()
        if fnmatch.fnmatchcase(child.name, pattern)
    ]
    if len(results) != 1:
        raise PackagingError(f"expected one {label}, found {len(results)} in {directory}")
    result = results[0]
    if result.is_symlink() or not result.is_file():
        raise PackagingError(f"{label} is not a regular package file: {result}")
    _resolve_child(directory, result, label)
    if result.stat().st_size == 0:
        raise PackagingError(f"{label} is empty: {result}")
    return result


def prepare_candidates(workspace: Path, target: str) -> Path:
    """清除当前目标的直接旧产物，保证包命令失败时不会上传历史文件。"""
    layout = _load_layout(workspace, target)
    stale_patterns = _artifact_patterns(layout) | _stale_archive_patterns(layout)
    for label, pattern in stale_patterns.items():
        for child in layout.package_output.iterdir():
            if fnmatch.fnmatchcase(child.name, pattern):
                if child.is_symlink() or not child.is_file():
                    raise PackagingError(f"refusing to remove non-file package output: {child}")
                _resolve_child(layout.package_output, child, label)
                child.unlink()

    if layout.target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
        _remove_exact_bundle(
            layout.package_output,
            layout.target_root,
            f"{layout.product_name}.app",
            "macOS app bundle",
        )
    _clear_candidate_stage(layout)
    layout.stage_output.mkdir(parents=True, exist_ok=True)
    return layout.stage_output


def _validate_app_bundle(bundle: Path, allowed_root: Path) -> Path:
    """确认 macOS bundle 完整且其相对链接不逃逸，再允许复制和 tar 打包。"""
    resolved_bundle = _resolve_child(allowed_root, bundle, "macOS app bundle")
    if bundle.is_symlink() or not bundle.is_dir():
        raise PackagingError(f"expected top-level macOS app bundle: {bundle}")
    if resolved_bundle != bundle or not (resolved_bundle / "Contents" / "Info.plist").is_file():
        raise PackagingError(f"macOS app bundle is missing Contents/Info.plist: {bundle}")
    for current, directories, files in os.walk(resolved_bundle, followlinks=False):
        for name in [*directories, *files]:
            entry = Path(current) / name
            if not entry.is_symlink():
                continue
            try:
                entry.resolve(strict=True).relative_to(resolved_bundle)
            except (OSError, ValueError) as error:
                raise PackagingError(f"macOS app bundle contains an invalid or escaping symlink: {entry}") from error
            if entry.is_dir():
                directories.remove(name)
    return resolved_bundle


def _create_updater_archive(source: Path, archive: Path, allowed_root: Path) -> Path:
    """按 cargo-packager updater 的单顶层 tar 布局生成无签名候选归档。"""
    resolved_source = _resolve_child(allowed_root, source, "updater archive source")
    resolved_archive = _resolve_child(allowed_root, archive, "updater archive output")
    if resolved_source != source or resolved_archive != archive or archive.exists() or archive.is_symlink():
        raise PackagingError(f"updater archive input or output is not a fresh direct candidate: {archive}")
    if not source.is_file() and not source.is_dir():
        raise PackagingError(f"updater archive source is not a file or directory: {source}")
    try:
        # cargo-packager 0.11.8 util::create_tar_from_dir uses the source basename as the archive root.
        with tarfile.open(archive, mode="w:gz", format=tarfile.PAX_FORMAT) as output:
            output.add(source, arcname=source.name, recursive=True)
    except (OSError, tarfile.TarError) as error:
        archive.unlink(missing_ok=True)
        raise PackagingError(f"could not create updater-compatible candidate archive: {archive}") from error
    if not archive.is_file() or archive.stat().st_size == 0:
        raise PackagingError(f"updater archive is empty: {archive}")
    return archive


def collect_candidates(workspace: Path, target: str) -> Path:
    """收集真实安装包并生成 cargo-packager-updater 接受的未签名 tar 载荷。"""
    layout = _load_layout(workspace, target)
    _clear_candidate_stage(layout)
    layout.stage_output.mkdir(parents=True, exist_ok=True)

    for label, pattern in _artifact_patterns(layout).items():
        source = _matches(layout.package_output, pattern, label)
        shutil.copy2(source, layout.stage_output / source.name)

    macos_target = layout.target in ("x86_64-apple-darwin", "aarch64-apple-darwin")
    if macos_target:
        app_bundle = layout.package_output / f"{layout.product_name}.app"
        resolved_app = _validate_app_bundle(app_bundle, layout.package_output)
        staged_app = layout.stage_output / app_bundle.name
        shutil.copytree(resolved_app, staged_app, symlinks=True)
        _validate_app_bundle(staged_app, layout.stage_output)
        _create_updater_archive(
            staged_app,
            layout.stage_output / f"{staged_app.name}.tar.gz",
            layout.stage_output,
        )
    elif layout.target == "x86_64-unknown-linux-gnu":
        appimage = next(
            child
            for child in layout.stage_output.iterdir()
            if child.is_file() and child.name.endswith(".AppImage")
        )
        _create_updater_archive(appimage, layout.stage_output / f"{appimage.name}.tar.gz", layout.stage_output)

    if layout.target == "x86_64-pc-windows-msvc":
        expected_files = 1
    elif layout.target == "x86_64-unknown-linux-gnu":
        expected_files = 3
    else:
        expected_files = 2
    expected_directories = 1 if macos_target else 0
    staged_files = [child for child in layout.stage_output.iterdir() if child.is_file() and not child.is_symlink()]
    staged_directories = [child for child in layout.stage_output.iterdir() if child.is_dir() and not child.is_symlink()]
    if len(staged_files) != expected_files or len(staged_directories) != expected_directories:
        raise PackagingError(
            "candidate file set is incomplete: "
            f"expected {expected_files} files and {expected_directories} directories, "
            f"found {len(staged_files)} files and {len(staged_directories)} directories"
        )
    return layout.stage_output


def main(argv: list[str] | None = None) -> int:
    """提供 workflow 可重复调用的 prepare/collect 子命令，并将失败显式返回。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "collect"))
    parser.add_argument("--target", required=True, choices=tuple(SUPPORTED_TARGETS))
    parser.add_argument("--workspace", type=Path)
    arguments = parser.parse_args(argv)

    workspace = arguments.workspace or Path(os.environ.get("GITHUB_WORKSPACE", Path(__file__).resolve().parents[1]))
    try:
        output = (
            prepare_candidates(workspace, arguments.target)
            if arguments.action == "prepare"
            else collect_candidates(workspace, arguments.target)
        )
    except (OSError, PackagingError, tarfile.TarError) as error:
        print(f"package candidate {arguments.action} failed: {error}", file=sys.stderr)
        return 1
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
