#!/usr/bin/env python3
# @author kongweiguang
"""用现有签名工具分别生成 GPUI 与旧版更新器的候选 sidecar，并可提前探测密钥配对。"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import build_candidate_inventory
import build_manifests
import updater_key


PURPOSE_GPUI = "gpui"
PURPOSE_LEGACY = "legacy"

_STAGE_SIGNING = "signing"
_STAGE_PUBLIC_KEY = "public-key-verification"
_STAGE_CREDENTIALS = "credentials"

# 失败分类全部是固定代码，避免把路径、命令输出、密码或私钥片段带进 CI 日志。
_FAILURE_CODES = {
    (PURPOSE_GPUI, _STAGE_SIGNING): "gpui-signing-failed",
    (PURPOSE_LEGACY, _STAGE_SIGNING): "legacy-signing-failed",
    (PURPOSE_GPUI, _STAGE_PUBLIC_KEY): "gpui-public-key-verification-failed",
    (PURPOSE_LEGACY, _STAGE_PUBLIC_KEY): "legacy-public-key-verification-failed",
    (PURPOSE_GPUI, _STAGE_CREDENTIALS): "gpui-signing-credentials-missing",
    (PURPOSE_LEGACY, _STAGE_CREDENTIALS): "legacy-signing-credentials-missing",
}

# 候选集路径沿用既有发布契约 signatures/new 与 signatures/legacy，分类标签只用于日志。
_PURPOSE_DIRECTORIES = {PURPOSE_GPUI: "new", PURPOSE_LEGACY: "legacy"}

# 探针只使用固定、无敏感内容的小文件，确保验证的是密钥配对而不是真实产物。
_PROBE_PAYLOAD = b"kerminal signing probe\n"
_PROBE_NAMES = {
    PURPOSE_GPUI: "gpui-signing-probe.bin",
    PURPOSE_LEGACY: "legacy-signing-probe.bin",
}

_FIXED_CODES = frozenset(_FAILURE_CODES.values()) | {
    "candidate-signatures-directory-present",
    "signature-sidecar-present",
    "signer-unavailable",
    "signing-credentials-not-independent",
    "signing-public-keys-not-independent",
}


class CandidateSigningError(ValueError):
    """候选集或签名环境错误；code 只允许固定分类，确保 CI 日志不泄露秘密。"""

    def __init__(self, code: str) -> None:
        # 限制错误文本来源，防止调用方把路径或工具输出拼进会进入 CI 日志的消息。
        if code not in _FIXED_CODES:
            raise ValueError("unregistered candidate signing error code")
        super().__init__(code)
        self.code = code


def _resolve_signer(signer: str) -> str:
    """提前解析绝对路径，避免子进程因工作目录或 PATH 变化调用到其他程序。"""
    signer_path = shutil.which(signer)
    if signer_path is None:
        raise CandidateSigningError("signer-unavailable")
    return os.path.abspath(signer_path)


def _load_credentials() -> dict[str, tuple[str, str, set[str]]]:
    """按用途读取凭据；缺失或复用同一私钥时给出可安全区分的固定分类。"""
    credentials = {
        PURPOSE_GPUI: (
            os.environ.get("KERMINAL_GPUI_SIGNING_PRIVATE_KEY", ""),
            os.environ.get("KERMINAL_GPUI_SIGNING_PRIVATE_KEY_PASSWORD", ""),
            build_manifests.NEW_UPDATER_KINDS,
        ),
        PURPOSE_LEGACY: (
            os.environ.get("TAURI_SIGNING_PRIVATE_KEY", ""),
            os.environ.get("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", ""),
            build_manifests.LEGACY_UPDATER_KINDS,
        ),
    }
    for purpose, (private_key, _, _) in credentials.items():
        if not private_key:
            raise CandidateSigningError(_FAILURE_CODES[(purpose, _STAGE_CREDENTIALS)])
    if credentials[PURPOSE_GPUI][0] == credentials[PURPOSE_LEGACY][0]:
        # 两套信任根必须独立；共用私钥会让下一次轮换误伤另一条更新链。
        raise CandidateSigningError("signing-credentials-not-independent")
    return credentials


def _signing_environment() -> dict[str, str]:
    """清除所有签名相关变量，防止子进程读到其他用途凭据或产生冗余日志。"""
    secret_names = {
        "KERMINAL_GPUI_SIGNING_PRIVATE_KEY",
        "KERMINAL_GPUI_SIGNING_PRIVATE_KEY_PASSWORD",
        "TAURI_SIGNING_PRIVATE_KEY",
        "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
        "CARGO_PACKAGER_SIGN_PRIVATE_KEY",
        "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD",
    }
    environment = {key: value for key, value in os.environ.items() if key not in secret_names}
    environment["RUST_LOG"] = "warn"
    return environment


def _sign_artifact(
    signer_path: str,
    artifact: Path,
    private_key: str,
    password: str,
    base_environment: dict[str, str],
    purpose: str,
    working_directory: Path,
) -> Path:
    """在只含本用途私钥的子进程里签名；抑制原始输出并把失败收敛为固定分类。"""
    child_environment = dict(base_environment)
    child_environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY"] = private_key
    child_environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD"] = password
    generated = artifact.with_name(artifact.name + ".sig")
    if generated.exists() or generated.is_symlink():
        raise CandidateSigningError("signature-sidecar-present")
    try:
        result = subprocess.run(
            [signer_path, "signer", "-q", "sign", str(artifact)],
            check=False,
            env=child_environment,
            cwd=working_directory,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError as error:
        raise CandidateSigningError(_FAILURE_CODES[(purpose, _STAGE_SIGNING)]) from error
    if result.returncode != 0 or generated.is_symlink() or not generated.is_file():
        raise CandidateSigningError(_FAILURE_CODES[(purpose, _STAGE_SIGNING)])
    return generated


def _verify_public_key(
    artifact: Path, sidecar: Path, public_key: str, purpose: str,
    base_environment: dict[str, str],
) -> None:
    """复用发布阶段的 Minisign 调用，避免只需公钥的验签进程继承私钥。"""
    try:
        build_manifests.verify_signature(
            artifact.parent, artifact, sidecar, public_key, "minisign",
            environment=base_environment,
        )
    except build_manifests.ManifestError as error:
        raise CandidateSigningError(_FAILURE_CODES[(purpose, _STAGE_PUBLIC_KEY)]) from error


def _gpui_public_key() -> str:
    """复用 updater 公钥解析并只取 Minisign -P 需要的公钥行，不回显变量内容。"""
    encoded = os.environ.get("KERMINAL_GPUI_UPDATER_PUBLIC_KEY", "")
    try:
        return updater_key.decode_public_key(encoded).splitlines()[1]
    except (updater_key.UpdateKeyError, IndexError) as error:
        raise CandidateSigningError(_FAILURE_CODES[(PURPOSE_GPUI, _STAGE_PUBLIC_KEY)]) from error


def _legacy_public_key(public_key_path: Path) -> str:
    """旧版公钥来自仓库内 pinned 文件，读取失败同样归入旧版公钥验证分类。"""
    try:
        return public_key_path.read_text(encoding="ascii").splitlines()[1]
    except (OSError, UnicodeError, IndexError) as error:
        raise CandidateSigningError(_FAILURE_CODES[(PURPOSE_LEGACY, _STAGE_PUBLIC_KEY)]) from error


def probe_signing(signer: str, legacy_public_key_path: Path) -> None:
    """用固定临时小文件实际签名并独立验签，在完整矩阵前暴露凭据或公钥错配。"""
    signer_path = _resolve_signer(signer)
    credentials = _load_credentials()
    base_environment = _signing_environment()
    public_keys = {
        PURPOSE_GPUI: _gpui_public_key(),
        PURPOSE_LEGACY: _legacy_public_key(legacy_public_key_path),
    }
    if public_keys[PURPOSE_GPUI] == public_keys[PURPOSE_LEGACY]:
        # 公钥相同意味着签名检查无法区分两条更新链，必须在构建前拒绝。
        raise CandidateSigningError("signing-public-keys-not-independent")
    with tempfile.TemporaryDirectory(prefix="kerminal-signing-probe-") as directory:
        root = Path(directory).resolve()
        for purpose, (private_key, password, _) in credentials.items():
            artifact = root / _PROBE_NAMES[purpose]
            artifact.write_bytes(_PROBE_PAYLOAD)
            generated = _sign_artifact(
                signer_path, artifact, private_key, password, base_environment, purpose, root
            )
            _verify_public_key(artifact, generated, public_keys[purpose], purpose, base_environment)


def sign_candidates(candidate_directory: Path, signer: str) -> None:
    """只签发当前候选矩阵的必需对象；新旧私钥仅注入各自的子进程环境。"""
    summary = build_candidate_inventory.validate_unsigned_candidate_set(candidate_directory)
    root = candidate_directory.resolve(strict=True)
    signatures = root / "signatures"
    if signatures.exists() or signatures.is_symlink():
        raise CandidateSigningError("candidate-signatures-directory-present")
    signer_path = _resolve_signer(signer)
    credentials = _load_credentials()
    base_environment = _signing_environment()
    for purpose, (private_key, password, kinds) in credentials.items():
        for record in summary:
            if record["kind"] not in kinds:
                continue
            _, artifact = build_manifests.safe_path(root, record["path"])
            destination = (
                signatures
                / _PURPOSE_DIRECTORIES[purpose]
                / record["target"]
                / (artifact.name + ".sig")
            )
            if destination.exists() or destination.is_symlink():
                raise CandidateSigningError("signature-sidecar-present")
            destination.parent.mkdir(parents=True, exist_ok=True)
            generated = _sign_artifact(
                signer_path, artifact, private_key, password, base_environment, purpose, root
            )
            os.rename(generated, destination)


def main() -> int:
    """默认签发候选；探针模式让 CI 在完整矩阵前单独验证凭据与公钥配对。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate-dir", type=Path)
    parser.add_argument("--signer", default="cargo-packager")
    parser.add_argument(
        "--probe",
        action="store_true",
        help="sign and verify a fixed temporary fixture instead of a candidate directory",
    )
    parser.add_argument(
        "--legacy-public-key",
        type=Path,
        default=Path("packaging/tauri-legacy-updater.pub"),
    )
    arguments = parser.parse_args()
    if not arguments.probe and arguments.candidate_dir is None:
        parser.error("--candidate-dir is required unless --probe is used")
    try:
        if arguments.probe:
            probe_signing(arguments.signer, arguments.legacy_public_key)
        else:
            sign_candidates(arguments.candidate_dir, arguments.signer)
    except (
        CandidateSigningError,
        build_candidate_inventory.CandidateInventoryError,
        build_manifests.ManifestError,
        OSError,
    ) as error:
        print(f"candidate signing failed: {error}", file=sys.stderr)
        return 2
    if arguments.probe:
        print("Signing probe passed: both credentials signed and both public keys verified the fixture.")
    else:
        print("Candidate sidecars generated locally; signatures still require independent verification.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
