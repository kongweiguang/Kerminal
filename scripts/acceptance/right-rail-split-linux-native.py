#!/usr/bin/env python3
"""Kerminal 右栏同 Tab 分屏切换的 Linux 原生外部验收（最小可行版）。

整个控制器、Xvfb 和被测应用运行在同一个 bubblewrap --unshare-all 命名空间内：
/tmp 是私有 tmpfs，X socket 不落到宿主文件系统；私有 PID 命名空间随控制器退出
回收全部后代。控制器用外部 XTest 发送真实鼠标/键盘事件，在两个 pane 键入无敏感
marker，再用只读 command.sqlite 证明同一 Tab 的不同 pane/session。截图在签名与
尺寸之外还做逐像素内容门禁（拒绝纯黑/纯色/均匀/透明帧）；右栏内容本身不由
整屏哈希或区域裁剪证明，只作为人工判断材料，不把 sqlite、进程内事件或截图
哈希任一单项说成视觉通过。

模式与副作用（如实口径）：
    --plan  纯 JSON 计划打印，无副作用。
    --check 静态依赖检查，并真实执行一次 bubblewrap --unshare-all 命名空间探针
            （内部运行 /usr/bin/python3 -I）；不启动 GUI、不启动应用、不建 run root。
            探针实测：pid-1 是本脚本启动的 bwrap、能力已清零、/home 与 /mnt 不可见、
            /tmp 为私有挂载、用户命名空间已建立，并真实执行一次 127.0.0.1 回环
            TCP 自连（bind/listen/connect/accept/echo，逐级限时、逐项关闭）。
            bind 成功不等于回环可用——新网络命名空间里 lo 默认 DOWN，必须完整
            自连成功；任一项未被证明即 NO-GO，不弱化网络隔离、不假装已验证。
    --run   完整实现，但被同一套前置门禁约束；任何门禁不可证明即失败关闭。
    --inner 仅供宿主启动器调用；控制器先用 /proc 证据证明自己运行在本脚本
            启动的 bwrap 命名空间里（pid-1 即 bwrap 且带 --unshare-all、cmdline
            携带本轮唯一 run id、CapEff 为零、/tmp 私有、/home 与 /mnt 不可见），
            证明不了就拒绝启动 Xvfb 或应用（在宿主直接执行 --inner 必然失败关闭）。
            注意 procfs 由最内层 PID namespace 挂载时 NSpid 只显示单层，这属于
            正常形态（已在 Linux 5.15 + bwrap 0.6.1 实测），不作为门禁依据。

run root（含全部截图证据）始终保留供人工复核；不提供自动删除。

用法：
    python3 right-rail-split-linux-native.py --plan
    python3 right-rail-split-linux-native.py --check [--app /abs/path/kerminal-desktop]
    python3 right-rail-split-linux-native.py --run --app /abs/path/kerminal-desktop

@author kongweiguang
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.util
import hashlib
import inspect
import json
import os
import pathlib
import random
import secrets
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from typing import Iterable, Optional
from urllib.parse import quote

# pwd 只在 Linux 存在；--plan 与纯单测必须能在任意平台运行，因此延迟导入。
try:
    import pwd as pwd_module
except ImportError:  # pragma: no cover - Windows 无 pwd，属预期
    pwd_module = None

SCHEMA_VERSION = 1
RUN_ID_ENV = "KERMINAL_LINUX_ACCEPTANCE_RUN_ID"
WINDOW_WIDTH = 1600
WINDOW_HEIGHT = 1000
SCREEN_DEPTH = 24
SCREEN_DPI = 96
SHELL_SETTLE_SECONDS = 10.0
SPLIT_SETTLE_SECONDS = 8.0
MARKER_TIMEOUT_SECONDS = 30.0
PANEL_SETTLE_SECONDS = 3.0
QUIT_TIMEOUT_SECONDS = 15.0
SANDBOX_TIMEOUT_SECONDS = 300.0
PROCESS_TERM_TIMEOUT_SECONDS = 5.0
PROCESS_KILL_TIMEOUT_SECONDS = 2.0
# 像素内容门禁阈值与 crates/kerminal-desktop/src/settings_acceptance/
# workbench_right_rail_split_switch.rs 的 PixelContentEvidence 同源：
# 全黑帧、纯色帧、均匀渐变帧的 distinct RGB 或亮度跨度都低于阈值，据此拒绝。
# 阈值是防伪门禁而不是"看到目标"的证据；右栏内容仍需人工复核截图。
PIXEL_MIN_DISTINCT_RGB = 8
PIXEL_MIN_LUMA_RANGE = 8
CURRENT_TIME = 0
IS_VIEWABLE = 2
REVERT_TO_PARENT = 2

KEY_CTRL = 0xFFE3
KEY_ALT = 0xFFE9
KEY_SHIFT = 0xFFE1

# 只键入验收 marker 需要的字符；无法用当前 XKB 两级映射表示的字符直接失败关闭。
CHAR_KEYSYMS = {
    " ": "space",
    "_": "underscore",
    "-": "minus",
    ".": "period",
    "/": "slash",
    ":": "colon",
}

# 同时兼容左右和上下分屏：A 偏左上、B 偏右下，两种方向都会落在不同 pane。
PANE_A_POINT = (0.30, 0.35)
PANE_B_POINT = (0.62, 0.65)

SANDBOX_ROOT = "/kerminal-run"
SANDBOX_CONTROLLER = "/kerminal-run/controller.py"
BASE_ENVIRONMENT = {
    "PATH": "/usr/local/bin:/usr/bin:/bin",
    "LANG": "C.UTF-8",
    "LC_ALL": "C.UTF-8",
}


class AcceptanceError(RuntimeError):
    """只携带固定阶段信息，不把路径、命令正文或环境值拼进外部错误。"""


class PreflightError(AcceptanceError):
    """静态前置条件不满足；此时不创建 display、不启动应用、不读取用户数据。"""


class CleanupError(AcceptanceError):
    """清理没有完全证明结束；保留原始错误并报告残余对象。"""


@dataclass
class PixelStats:
    """PNG 像素统计结果：不透明样本、不同 RGB 数与亮度跨度，用于防伪门禁。"""

    opaque_samples: int
    total_samples: int
    distinct_rgb: int
    luma_range: int


@dataclass
class ScreenshotRecord:
    """截图证据记录；regionStats 携带右栏假定区域的门禁统计。"""

    file_host: str
    sha256: str
    width: int
    height: int
    frame_stats: PixelStats
    region_stats: PixelStats


@dataclass
class DependencyReport:
    """只读依赖探测结果，供静态检查和最终报告复用。"""

    ready: bool = False
    blockers: list[str] = field(default_factory=list)
    tools: dict[str, str] = field(default_factory=dict)
    libraries: dict[str, str] = field(default_factory=dict)
    versions: dict[str, str] = field(default_factory=dict)
    sandbox: dict[str, object] = field(default_factory=dict)
    vulkan_icd: str = ""
    parent_display_present: bool = False
    parent_wayland_present: bool = False
    parent_dbus_present: bool = False
    sqlite_version: str = ""

    def blocker(self, message: str) -> None:
        """阻塞项去重，避免同一缺失依赖重复出现。"""
        if message not in self.blockers:
            self.blockers.append(message)


@dataclass
class RunPaths:
    """所有可写路径都锚定在本次独占 run root 内，便于清理时逐项核验。"""

    root: pathlib.Path
    home: pathlib.Path
    config: pathlib.Path
    evidence: pathlib.Path
    app: pathlib.Path
    shell: pathlib.Path
    xauthority: pathlib.Path
    controller: pathlib.Path
    xvfb_log: pathlib.Path
    app_log: pathlib.Path
    sandbox_log: pathlib.Path
    inner_report_file: pathlib.Path
    report_file: pathlib.Path


@dataclass
class WindowInfo:
    """真实 X11 顶层窗口身份和尺寸；后续点击都以该几何为基准。"""

    window_id: int
    title: str
    width: int
    height: int
    x: int
    y: int


class XWindowAttributes(ctypes.Structure):
    """Xlib 的 XWindowAttributes 布局；只需要 map_state 与客户端尺寸。"""

    _fields_ = [
        ("x", ctypes.c_int),
        ("y", ctypes.c_int),
        ("width", ctypes.c_int),
        ("height", ctypes.c_int),
        ("border_width", ctypes.c_int),
        ("depth", ctypes.c_int),
        ("visual", ctypes.c_void_p),
        ("root", ctypes.c_ulong),
        ("class_", ctypes.c_int),
        ("bit_gravity", ctypes.c_int),
        ("win_gravity", ctypes.c_int),
        ("backing_store", ctypes.c_int),
        ("backing_planes", ctypes.c_ulong),
        ("backing_pixel", ctypes.c_ulong),
        ("save_under", ctypes.c_int),
        ("colormap", ctypes.c_ulong),
        ("map_installed", ctypes.c_int),
        ("map_state", ctypes.c_int),
        ("all_event_masks", ctypes.c_long),
        ("your_event_mask", ctypes.c_long),
        ("do_not_propagate_mask", ctypes.c_long),
        ("override_redirect", ctypes.c_int),
        ("screen", ctypes.c_void_p),
    ]


def _run(
    argv: list[str],
    *,
    env: Optional[dict[str, str]] = None,
    timeout: float = 15.0,
    cwd: Optional[pathlib.Path] = None,
) -> subprocess.CompletedProcess[str]:
    """只执行有界、无 shell 的 argv；超时由调用方转换成固定错误。"""
    return subprocess.run(
        argv,
        env=env,
        cwd=str(cwd) if cwd else None,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=timeout,
        check=False,
    )


def _which(tool: str) -> str:
    """返回工具绝对路径；PATH 只用于静态定位，运行沙箱会重建环境。"""
    found = shutil.which(tool)
    return os.path.abspath(found) if found else ""


# PNG 门禁的输入上限：scrot 只产出 1600x1000 的截图，留出余量后拒绝畸形大图，
# 防止伪造 PNG 声称的尺寸或巨量 IDAT 在解码阶段消耗无界内存。
PNG_MAX_INPUT_BYTES = 16 * 1024 * 1024
PNG_MAX_DIMENSION = 4096
# 解压输出预算 = 完整原始扫描线数据（每行 1 字节 filter），任何超出都是炸弹。
PNG_MAX_RAW_BYTES = PNG_MAX_DIMENSION * (PNG_MAX_DIMENSION * 4 + 1)


def _decode_png_pixels(data: bytes) -> tuple[int, int, list[list[tuple[int, ...]]]]:
    """用纯标准库解码 8-bit 非交错 RGB(A)/调色板 PNG，返回尺寸与逐像素 RGBA。

    只接受 scrot 实际产出的格式组合（color type 2/3/6、bit depth 8、无隔行）；
    其他组合直接失败关闭，绝不把解码不了的截图当作已验证证据。透明度必须被
    正确解析而不是当成不透明：调色板读取 tRNS 逐项 alpha（缺失按 255），真彩
    tRNS 与 16-bit 只拒绝该帧，不猜测近似值。半透明像素保留真实 alpha，由
    _pixel_stats 只把 alpha==255 计入不透明样本。输入与解压输出都有硬上限，
    畸形 PNG 在解码阶段就被拒绝，不会消耗无界内存。
    """
    if len(data) > PNG_MAX_INPUT_BYTES:
        raise AcceptanceError(f"PNG input too large: {len(data)} bytes")
    if len(data) < 57 or data[:8] != b"\x89PNG\r\n\x1a\n":
        raise AcceptanceError("not a PNG signature")
    position = 8
    width = height = 0
    bit_depth = color_type = interlace = -1
    palette: list[tuple[int, int, int, int]] = []
    trns_seen = False
    idat = bytearray()
    while position + 8 <= len(data):
        length = int.from_bytes(data[position : position + 4], "big")
        chunk_type = data[position + 4 : position + 8]
        # 长度本身就在输入里，先拒绝超界长度再切片，避免按伪造长度撑大内存。
        if length > len(data) - position - 8:
            raise AcceptanceError("PNG chunk length exceeds input")
        chunk = data[position + 8 : position + 8 + length]
        if chunk_type == b"IHDR":
            # IHDR 固定 13 字节；长度不足说明是伪造块，直接索引会抛裸 IndexError
            # 逃出类型化错误边界，所以先校验长度再取字段。
            if length < 13:
                raise AcceptanceError("PNG IHDR chunk is shorter than 13 bytes")
            width = int.from_bytes(chunk[0:4], "big")
            height = int.from_bytes(chunk[4:8], "big")
            bit_depth = chunk[8]
            color_type = chunk[9]
            interlace = chunk[12]
        elif chunk_type == b"PLTE":
            if len(chunk) % 3 != 0:
                raise AcceptanceError("PNG palette length is not a multiple of 3")
            palette = [
                (chunk[i], chunk[i + 1], chunk[i + 2], 255)
                for i in range(0, len(chunk), 3)
            ]
        elif chunk_type == b"tRNS":
            trns_seen = True
            if color_type == 3:
                # 调色板 tRNS：逐项 alpha，短于调色板的尾部条目保持不透明。
                if len(chunk) > len(palette):
                    raise AcceptanceError("PNG tRNS longer than palette")
                for index, alpha in enumerate(chunk):
                    entry = palette[index]
                    palette[index] = (entry[0], entry[1], entry[2], alpha)
            elif color_type in (2, 6):
                # 真彩 tRNS 只定义"颜色键"透明，不是逐像素 alpha；门禁不能把它
                # 当成逐像素透明度，也无法安全近似，遇到就明确拒绝该帧。
                raise AcceptanceError(
                    "truecolor tRNS chroma-key transparency is not supported"
                )
        elif chunk_type == b"IDAT":
            idat.extend(chunk)
        elif chunk_type == b"IEND":
            break
        position += 12 + length
    if width <= 0 or height <= 0:
        raise AcceptanceError("PNG has no usable IHDR")
    if width > PNG_MAX_DIMENSION or height > PNG_MAX_DIMENSION:
        raise AcceptanceError(
            f"PNG dimensions {width}x{height} exceed the acceptance limit"
        )
    if bit_depth != 8 or interlace != 0:
        raise AcceptanceError(f"unsupported PNG depth/interlace: {bit_depth}/{interlace}")
    if color_type not in (2, 3, 6):
        raise AcceptanceError(f"unsupported PNG color type: {color_type}")
    if color_type == 3 and not palette:
        raise AcceptanceError("palette PNG has no PLTE chunk")
    try:
        import zlib as zlib_module

        # decompressobj + 显式输出上限：畸形超长数据在预算处失败，而不是把
        # 解压结果全部驻留内存后再判断。
        decompressor = zlib_module.decompressobj()
        raw = decompressor.decompress(bytes(idat), PNG_MAX_RAW_BYTES + 1)
        if decompressor.unconsumed_tail or len(raw) > PNG_MAX_RAW_BYTES:
            raise AcceptanceError("PNG decompressed data exceeds the acceptance limit")
        raw += decompressor.flush()
        if len(raw) > PNG_MAX_RAW_BYTES:
            raise AcceptanceError("PNG decompressed data exceeds the acceptance limit")
    except AcceptanceError:
        raise
    except Exception as error:
        raise AcceptanceError(f"PNG idat decompression failed: {error}") from error
    channels = {2: 3, 3: 1, 6: 4}[color_type]
    stride = width * channels
    expected_raw = height * (stride + 1)
    if len(raw) < expected_raw:
        raise AcceptanceError("PNG scanline data is truncated")
    if color_type == 3 and trns_seen is False:
        pass  # 无 tRNS 的调色板整体不透明，palette 已经按 255 初始化。
    pixels: list[list[tuple[int, ...]]] = []
    previous = [0] * stride
    offset = 0
    # 每行前置 1 字节 filter；PNG filter 是可逆的，这里逐行还原再取原始通道值。
    for _ in range(height):
        filter_type = raw[offset]
        line = list(raw[offset + 1 : offset + 1 + stride])
        offset += 1 + stride
        if filter_type == 0:
            pass
        elif filter_type == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif filter_type == 2:
            for i in range(stride):
                line[i] = (line[i] + previous[i]) & 0xFF
        elif filter_type == 3:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif filter_type == 4:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                b = previous[i]
                c = previous[i - channels] if i >= channels else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                predictor = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + predictor) & 0xFF
        else:
            raise AcceptanceError(f"unsupported PNG filter type: {filter_type}")
        previous = line
        row: list[tuple[int, ...]] = []
        for x in range(width):
            if color_type == 3:
                index = line[x]
                if index >= len(palette):
                    raise AcceptanceError("PNG palette index out of range")
                row.append(palette[index])
            else:
                base = x * channels
                if channels == 3:
                    row.append((line[base], line[base + 1], line[base + 2], 255))
                else:
                    row.append(
                        (
                            line[base],
                            line[base + 1],
                            line[base + 2],
                            line[base + 3],
                        )
                    )
        pixels.append(row)
    return width, height, pixels


def _pixel_stats(
    pixels: list[list[tuple[int, ...]]],
    x0: int,
    y0: int,
    x1: int,
    y1: int,
) -> PixelStats:
    """统计区域内完全不透明样本数、不同 RGB 数与亮度跨度。

    只有 alpha==255 才算不透明：半透明像素的 RGB 是与背景混合前的残留值，
    按"完全不透明"计入会伪造颜色与亮度跨度（透明门禁的绕过路径）。与
    Windows 侧 PixelContentEvidence 的 alpha==0 跳过口径对齐并更严格。
    """
    colors: set[tuple[int, ...]] = set()
    opaque = 0
    total = 0
    luma_min = 255
    luma_max = 0
    for y in range(max(0, y0), min(len(pixels), y1)):
        row = pixels[y]
        for x in range(max(0, x0), min(len(row), x1)):
            sample = row[x]
            total += 1
            # 像素统一为 RGBA 四元组；alpha 不足 255 一律不算不透明样本。
            if len(sample) < 4 or sample[3] != 255:
                continue
            opaque += 1
            colors.add(sample[:3])
            r, g, b = sample[0], sample[1], sample[2]
            # 整数亮度近似，避免引入依赖；只用于均匀性判断，不做色彩学声明。
            luma = (r * 299 + g * 587 + b * 114) // 1000
            luma_min = min(luma_min, luma)
            luma_max = max(luma_max, luma)
    return PixelStats(
        opaque_samples=opaque,
        total_samples=total,
        distinct_rgb=len(colors),
        luma_range=max(0, luma_max - luma_min) if opaque else 0,
    )


def _gate_pixel_stats(stats: PixelStats, label: str) -> None:
    """拒绝透明、半透明为主、纯黑、纯色与均匀帧；这是防伪门禁，不证明看到了右栏目标。"""
    if stats.total_samples <= 0:
        raise AcceptanceError(f"screenshot {label} has no pixels in the gated region")
    # 半透明为主（不透明样本不足一半）直接拒绝：alpha 1..254 的像素不参与
    # 颜色/亮度统计，避免"几乎不可见的一层内容"被当成真实画面。
    if stats.opaque_samples * 2 < stats.total_samples:
        raise AcceptanceError(
            f"screenshot {label} is mostly transparent "
            f"({stats.opaque_samples}/{stats.total_samples} fully opaque)"
        )
    if stats.distinct_rgb < PIXEL_MIN_DISTINCT_RGB:
        raise AcceptanceError(
            f"screenshot {label} looks blank or uniform: only "
            f"{stats.distinct_rgb} distinct RGB values (minimum {PIXEL_MIN_DISTINCT_RGB})"
        )
    if stats.luma_range < PIXEL_MIN_LUMA_RANGE:
        raise AcceptanceError(
            f"screenshot {label} looks flat: luma range {stats.luma_range} "
            f"(minimum {PIXEL_MIN_LUMA_RANGE})"
        )


def _real_home() -> pathlib.Path:
    """从系统账户读取真实 HOME，仅用于拒绝把 run root 放进去，不读取内容。"""
    if pwd_module is not None:
        try:
            return pathlib.Path(pwd_module.getpwuid(os.getuid()).pw_dir).resolve()
        except (KeyError, OSError):
            pass
    return pathlib.Path.home().resolve()


def _is_within(child: pathlib.Path, parent: pathlib.Path) -> bool:
    """路径包含判断先 resolve，避免符号链接或 `..` 把写入带回用户目录。"""
    try:
        child.resolve().relative_to(parent.resolve())
        return True
    except (OSError, ValueError):
        return False


def _repo_root() -> Optional[pathlib.Path]:
    """脚本被复制到仓库外时返回 None，不把父目录索引失败当成验收失败。"""
    try:
        return pathlib.Path(__file__).resolve().parents[2]
    except IndexError:
        return None


def _sha256_file(path: pathlib.Path) -> str:
    """只读取本次候选二进制并计算指纹，不使用缓存或猜测构建身份。"""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _load_xlibs(
    report: DependencyReport,
) -> tuple[Optional[ctypes.CDLL], Optional[ctypes.CDLL]]:
    """加载系统 X11/XTest 并显式声明函数签名，避免 ctypes 指针截断。"""
    x11_name = ctypes.util.find_library("X11")
    xtst_name = ctypes.util.find_library("Xtst")
    if not x11_name:
        report.blocker("libX11 is not discoverable")
    if not xtst_name:
        report.blocker("libXtst is not discoverable")
    if not x11_name or not xtst_name:
        return None, None
    try:
        x11 = ctypes.CDLL(x11_name)
        xtst = ctypes.CDLL(xtst_name)
    except OSError as error:
        report.blocker(f"cannot load X11/XTest libraries: {error}")
        return None, None

    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.restype = ctypes.c_int
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XDefaultRootWindow.restype = ctypes.c_ulong
    x11.XDefaultScreen.argtypes = [ctypes.c_void_p]
    x11.XDefaultScreen.restype = ctypes.c_int
    x11.XQueryTree.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_ulong),
        ctypes.POINTER(ctypes.c_ulong),
        ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)),
        ctypes.POINTER(ctypes.c_uint),
    ]
    x11.XQueryTree.restype = ctypes.c_int
    x11.XFree.argtypes = [ctypes.c_void_p]
    x11.XFree.restype = ctypes.c_int
    x11.XFetchName.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_char_p),
    ]
    x11.XFetchName.restype = ctypes.c_int
    x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    x11.XInternAtom.restype = ctypes.c_ulong
    x11.XGetWindowProperty.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_long,
        ctypes.c_long,
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_ulong),
        ctypes.POINTER(ctypes.c_int),
        ctypes.POINTER(ctypes.c_ulong),
        ctypes.POINTER(ctypes.c_ulong),
        ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte)),
    ]
    x11.XGetWindowProperty.restype = ctypes.c_int
    x11.XGetWindowAttributes.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.POINTER(XWindowAttributes),
    ]
    x11.XGetWindowAttributes.restype = ctypes.c_int
    x11.XMoveResizeWindow.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_uint,
        ctypes.c_uint,
    ]
    x11.XMoveResizeWindow.restype = ctypes.c_int
    x11.XMapRaised.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x11.XMapRaised.restype = ctypes.c_int
    x11.XSetInputFocus.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.c_int,
        ctypes.c_ulong,
    ]
    x11.XSetInputFocus.restype = ctypes.c_int
    x11.XGetInputFocus.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_ulong),
        ctypes.POINTER(ctypes.c_int),
    ]
    x11.XGetInputFocus.restype = ctypes.c_int
    x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XSync.restype = ctypes.c_int
    x11.XStringToKeysym.argtypes = [ctypes.c_char_p]
    x11.XStringToKeysym.restype = ctypes.c_ulong
    x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x11.XKeysymToKeycode.restype = ctypes.c_uint
    x11.XkbKeycodeToKeysym.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint,
        ctypes.c_int,
        ctypes.c_int,
    ]
    x11.XkbKeycodeToKeysym.restype = ctypes.c_ulong

    xtst.XTestFakeMotionEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_ulong,
    ]
    xtst.XTestFakeMotionEvent.restype = ctypes.c_int
    xtst.XTestFakeButtonEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint,
        ctypes.c_int,
        ctypes.c_ulong,
    ]
    xtst.XTestFakeButtonEvent.restype = ctypes.c_int
    xtst.XTestFakeKeyEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint,
        ctypes.c_int,
        ctypes.c_ulong,
    ]
    xtst.XTestFakeKeyEvent.restype = ctypes.c_int
    report.libraries["libX11"] = x11_name
    report.libraries["libXtst"] = xtst_name
    return x11, xtst


def _sandbox_prefix(bwrap: str, run_root: Optional[pathlib.Path]) -> list[str]:
    """构造唯一的沙箱挂载形状，让检查和真实运行使用同一套隔离参数。

    /tmp 是私有 tmpfs，X socket 不会落到宿主；--unshare-all 的 PID 命名空间
    在控制器退出时由内核回收全部后代。run_root 为 None 时只做只读探针。
    """
    argv = [
        bwrap,
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--clearenv",
        "--tmpfs",
        "/",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/sbin",
        "/sbin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--ro-bind",
        "/etc",
        "/etc",
        "--ro-bind",
        "/sys",
        "/sys",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]
    if run_root is None:
        argv.extend(["--chdir", "/"])
    else:
        argv.extend(["--bind", str(run_root), SANDBOX_ROOT, "--chdir", SANDBOX_ROOT])
    return argv


# 沙箱探针注入的内嵌 Python 程序。必须是完整可解析的源码而不是分号拼接的
# 语句串——历史版本曾把 try/except 写成分号行导致 --check 一启动就 SyntaxError，
# 纯单测只 mock 了 _run 没有执行这段代码，因此把程序提为常量并用 ast.parse
# 在单测里直接解析"实际被执行"的这份数据，防止再次出现不可执行探针。
SANDBOX_PROBE_PROGRAM = '''\
import ctypes.util
import json
import os
import pathlib
import socket
import sys
import time

result = {
    "sandboxOk": True,
    "usernsIdentity": None,
    "usernsIdentityDetail": "",
    "loopback": "unmeasured",
    "loopbackDetail": "",
}

# 隔离前提逐项断言；任何一项不成立都直接失败关闭，输出也不会声称 sandbox-ok。
assert not os.path.exists("/home"), "/home must be hidden"
assert not os.path.exists("/mnt"), "/mnt must be hidden"
assert os.access("/tmp", os.W_OK), "/tmp must be writable"
os.makedirs("/tmp/.X11-unix", mode=0o700, exist_ok=True)
assert ctypes.util.find_library("X11"), "libX11 must be visible"
assert ctypes.util.find_library("Xtst"), "libXtst must be visible"
assert os.path.isfile(
    "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json"
), "lavapipe ICD must be visible"

# 用户命名空间判定：不做 uid_map 形状猜测（每行恒为 3 字段，旧逻辑按全文
# split 后的 4 字段判定，条件恒假导致 userns 恒为真），改为输出本进程
# /proc/self/ns/user 的命名空间身份 (st_dev, st_ino)，由宿主在启动 bwrap 前
# 读取自身身份并比较：不同即证明新建了用户命名空间，相同即证明没有隔离，
# 任一侧读不到都按未知失败关闭。宿主本身已在容器或用户命名空间内时同样
# 成立，因为比较的是命名空间身份而不是映射形状。
try:
    ns_info = os.stat("/proc/self/ns/user")
    result["usernsIdentity"] = [ns_info.st_dev, ns_info.st_ino]
except OSError as error:
    result["usernsIdentity"] = None
    result["usernsIdentityDetail"] = str(error)

# 回环必须完整走一次 TCP 自连：新 netns 里 lo 默认 DOWN，bind/listen 都可能
# 在接口未启用时"成功"，只有 connect+accept 成功才能证明 127.0.0.1 启动路径
# 可用。每一步限时，任何一步失败都记录阶段与 errno，绝不把部分成功报成可用。
listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
client = None
accepted = None
try:
    listener.bind(("127.0.0.1", 0))
    listener.settimeout(5.0)
    listener.listen(1)
    client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    client.settimeout(5.0)
    client.connect(listener.getsockname())
    client.settimeout(5.0)
    client.sendall(b"kerminal-loopback-probe")
    accepted, _ = listener.accept()
    accepted.settimeout(5.0)
    echoed = accepted.recv(64)
    if echoed == b"kerminal-loopback-probe":
        result["loopback"] = "usable"
    else:
        result["loopback"] = "unverified-echo"
        result["loopbackDetail"] = f"echo mismatch: {echoed!r}"
except OSError as error:
    result["loopback"] = f"unavailable:{error.errno}"
    result["loopbackDetail"] = str(error)
finally:
    # 逐项关闭并吞掉关闭期错误；清理失败不影响已经测量到的结论。
    for resource in (accepted, client):
        if resource is not None:
            try:
                resource.close()
            except OSError:
                pass
    try:
        listener.close()
    except OSError:
        pass

# 只输出一行 JSON 标记，宿主解析失败即证据缺失，按失败关闭处理。
print("SANDBOX_PROBE " + json.dumps(result, sort_keys=True))
'''

# 探针输出前缀；解析不到该前缀或 JSON 不合法都视为探针未完成。
SANDBOX_PROBE_MARKER = "SANDBOX_PROBE "

# 回环自连的端到端预算；超过即认为回环不可用，NO-GO 而不是等待更久。
SANDBOX_PROBE_TIMEOUT_SECONDS = 30.0


def _read_userns_identity() -> Optional[tuple[int, int]]:
    """读取调用进程的 /proc/self/ns/user 身份 (st_dev, st_ino)。

    比较命名空间身份而不是 uid_map 映射形状：宿主本身已在容器或用户命名空间
    内时映射形状不可预期，但"探针的 userns 是否等于宿主启动器所在的 userns"
    始终可判。读不到（非 Linux、procfs 不可用）返回 None 表示未测，调用方
    必须失败关闭而不是猜测。inode 号在 nsfs 中按命名空间类型全局分配，
    用于区分不同用户命名空间是可靠的。
    """
    try:
        info = os.stat("/proc/self/ns/user")
    except OSError:
        return None
    return (info.st_dev, info.st_ino)


def _parse_sandbox_probe_output(
    output: str,
) -> tuple[bool, Optional[tuple[int, int]], Optional[bool], str]:
    """解析探针一行 JSON 标记；返回 (探针是否完成, userns 身份, 回环可用, 细节)。

    解析不到标记、JSON 损坏或字段缺失都按未完成处理——证据缺失时必须失败
    关闭，不能把"没看到"当成"通过"。usernsIdentity 缺失或畸形返回 None
    （未测），由调用方与宿主身份比较后决定是否阻塞。纯函数供单测覆盖。
    """
    for line in reversed(output.splitlines()):
        stripped = line.strip()
        if not stripped.startswith(SANDBOX_PROBE_MARKER):
            continue
        payload = stripped[len(SANDBOX_PROBE_MARKER) :]
        try:
            data = json.loads(payload)
        except json.JSONDecodeError:
            return False, None, None, f"unparseable probe payload: {payload[:120]}"
        if not isinstance(data, dict) or data.get("sandboxOk") is not True:
            return False, None, None, f"probe did not pass: {payload[:120]}"
        loopback = data.get("loopback")
        usable = loopback == "usable"
        detail = str(data.get("loopbackDetail", ""))
        if not usable:
            detail = f"loopback={loopback} {detail}".strip()
        identity_raw = data.get("usernsIdentity")
        identity: Optional[tuple[int, int]] = None
        if (
            isinstance(identity_raw, list)
            and len(identity_raw) == 2
            and all(isinstance(value, int) and not isinstance(value, bool) for value in identity_raw)
        ):
            identity = (identity_raw[0], identity_raw[1])
        else:
            userns_detail = str(data.get("usernsIdentityDetail", ""))
            detail = f"userns identity missing/malformed {userns_detail}".strip() + (
                f"; {detail}" if detail else ""
            )
        return True, identity, usable, detail
    return False, None, None, "no sandbox probe marker in output"


def _sandbox_probe(bwrap: str, report: DependencyReport) -> None:
    """用真实挂载形状做只读探针，证明隔离前提而不是假设它们成立。

    检查项：/home 与 /mnt 不可见、私有 /tmp 可写、X11/XTest 依赖可见、lavapipe
    ICD 可见；并实测 (a) 用户命名空间是否真的建立——在启动 bwrap 前读取宿主
    自身 /proc/self/ns/user 身份，与探针报告的自身身份比较（不同才证明新建，
    相同即未隔离，任一侧读不到即未测），(b) 新网络命名空间里 127.0.0.1 回环
    TCP 能否完整自连（bind/listen/connect/accept/echo）。
    bind 成功不算数：lo 未启用时 bind 也会成功，只有端到端回环可用才证明应用的
    127.0.0.1 启动路径成立；不可用就 NO-GO，不弱化网络命名空间、不假装已验证。
    """
    # 必须在启动 bwrap 前取样宿主身份：这是"探针的 userns 是否不同于宿主"
    # 判定的比较基准，宿主本身在容器/userns 内时依然成立。
    host_identity = _read_userns_identity()
    probe = _sandbox_prefix(bwrap, None)
    probe.extend(["--setenv", "PATH", "/usr/bin:/bin"])
    probe.extend(["/usr/bin/python3", "-I", "-c", SANDBOX_PROBE_PROGRAM])
    try:
        result = _run(probe, timeout=SANDBOX_PROBE_TIMEOUT_SECONDS)
    except (OSError, subprocess.TimeoutExpired) as error:
        report.blocker(f"bubblewrap sandbox probe failed: {error}")
        return
    output = result.stdout.strip()
    # 逐项解析探测输出；解析不到就是证据缺失，按失败关闭而不是默认通过。
    probe_completed, probe_identity, loopback_usable, probe_detail = (
        _parse_sandbox_probe_output(output)
    )
    userns_measured = host_identity is not None and probe_identity is not None
    # 只有两侧身份都测到且不同，才证明 bwrap 真的新建了用户命名空间。
    userns_proven = userns_measured and host_identity != probe_identity
    sandbox_checks_ok = probe_completed and userns_proven
    checks_text = (
        "/home and /mnt hidden, /tmp writable, private /tmp/.X11-unix creatable, "
        "libX11/libXtst and lavapipe ICD visible, user namespace identity compared "
        "against the host (must differ; unmeasurable identity fails closed), "
        "loopback TCP self-connect (bind/listen/connect/accept/echo) measured "
        "in the new network namespace"
    )
    report.sandbox = {
        "argv": probe,
        "exitCode": result.returncode,
        "output": output[:400],
        "checks": checks_text,
        "probeCompleted": probe_completed,
        "namespaceUnshareAccepted": sandbox_checks_ok,
        "privateTmpfs": sandbox_checks_ok,
        # 语义：仅当判明"探针 userns 与宿主不同"才为 True；未测为 None，相等为 False。
        "userNamespaceMeasured": userns_proven if userns_measured else None,
        "userNamespaceIdentity": {
            "host": list(host_identity) if host_identity is not None else None,
            "probe": list(probe_identity) if probe_identity is not None else None,
        },
        "loopbackMeasured": probe_completed,
        "loopbackUsable": bool(loopback_usable) if probe_completed else False,
        # 探针只使用 127.0.0.1 自连（无特权端口回环收发），从不触达外部网络。
        "networkExercised": "loopback-only self-connect inside the probe namespace",
    }
    if not sandbox_checks_ok:
        if not probe_completed:
            report.blocker(
                "bubblewrap sandbox probe did not complete its isolation checks "
                f"({probe_detail}); refusing a weaker filesystem/namespace fallback"
            )
        elif not userns_measured:
            report.blocker(
                "user namespace identity could not be read on both sides "
                f"(host={host_identity}, probe={probe_identity}); unmeasurable "
                "isolation is NO-GO, refusing to guess from mapping shape"
            )
        else:
            report.blocker(
                "probe user namespace identity equals the host launcher's "
                f"({probe_identity}); bwrap did not create a new user namespace"
            )
        return
    # 回环是应用 127.0.0.1 启动路径的硬前提；测不出可用就 NO-GO。
    if not loopback_usable:
        report.blocker(
            "loopback TCP is not proven usable inside the unshared network namespace "
            f"({probe_detail}); the app's 127.0.0.1 startup path cannot be satisfied "
            "without weakening network isolation, which this script refuses to do"
        )


def check_dependencies(*, need_app: Optional[pathlib.Path] = None) -> DependencyReport:
    """静态检查全部运行前置，不启动 Xvfb、不启动应用、不创建 run root。"""
    report = DependencyReport()
    if not sys.platform.startswith("linux"):
        report.blocker("native check must run on Linux (WSL2 Ubuntu-22.04)")
        return report
    if os.geteuid() == 0:
        report.blocker("run as the unprivileged acceptance user, not root")
    for tool in ("Xvfb", "bwrap", "scrot", "xdpyinfo", "xauth", "setxkbmap"):
        path = _which(tool)
        report.tools[tool] = path
        if not path:
            report.blocker(f"missing tool: {tool}")
    if report.tools.get("bwrap"):
        try:
            report.versions["bwrap"] = _run(
                [report.tools["bwrap"], "--version"], timeout=5.0
            ).stdout.strip()
        except (OSError, subprocess.TimeoutExpired) as error:
            report.blocker(f"cannot query bubblewrap version: {error}")
        _sandbox_probe(report.tools["bwrap"], report)
    if report.tools.get("scrot"):
        try:
            report.versions["scrot"] = _run(
                [report.tools["scrot"], "--version"], timeout=5.0
            ).stdout.strip().splitlines()[0]
        except (OSError, subprocess.TimeoutExpired, IndexError) as error:
            report.blocker(f"cannot query scrot version: {error}")

    _load_xlibs(report)

    icd = pathlib.Path("/usr/share/vulkan/icd.d/lvp_icd.x86_64.json")
    report.vulkan_icd = str(icd) if icd.is_file() else ""
    if not report.vulkan_icd:
        report.blocker("lavapipe Vulkan ICD (lvp_icd.x86_64.json) is missing")

    try:
        import sqlite3 as sqlite_module

        report.sqlite_version = sqlite_module.sqlite_version
    except Exception as error:  # pragma: no cover - 环境特定
        report.blocker(f"Python sqlite3 is unavailable: {error}")

    report.parent_display_present = bool(os.environ.get("DISPLAY"))
    report.parent_wayland_present = bool(os.environ.get("WAYLAND_DISPLAY"))
    report.parent_dbus_present = bool(os.environ.get("DBUS_SESSION_BUS_ADDRESS"))

    if need_app is not None:
        app = need_app.resolve()
        if not app.is_file():
            report.blocker(f"app binary does not exist: {app}")
        elif not os.access(app, os.X_OK):
            report.blocker(f"app binary is not executable: {app}")
        else:
            report.versions["appSha256"] = _sha256_file(app)

    report.ready = not report.blockers
    return report


def _print_json(value: object) -> None:
    """统一输出 UTF-8 JSON，便于主 agent 直接读取和保存证据。"""
    print(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True))


def _plan() -> dict[str, object]:
    """打印执行计划与失败边界，供审查阶段核对而不触发任何原生副作用。"""
    return {
        "schemaVersion": SCHEMA_VERSION,
        "scope": "linux-native-right-rail-same-tab-split-switch",
        "executionRequiresApproval": True,
        "window": {
            "width": WINDOW_WIDTH,
            "height": WINDOW_HEIGHT,
            "depth": SCREEN_DEPTH,
            "dpi": SCREEN_DPI,
            "windowManager": "none (Xvfb root, GPUI client-side decorations)",
        },
        "isolation": {
            "outer": "one bubblewrap --unshare-all namespace runs controller, Xvfb and app",
            "filesystem": "tmpfs /; read-only /usr,/etc,/sys; private tmpfs /tmp; run root rw at /kerminal-run",
            "home": "hidden by sandbox; probe asserts /home and /mnt are absent",
            "display": "Xvfb inside the private mount namespace; socket lives in private tmpfs /tmp/.X11-unix",
            "displayNetwork": "-nolisten tcp",
            "displayAuthorization": "private MIT-MAGIC-COOKIE-1 Xauthority file under the run root",
            "pidNamespace": "unshared; --die-with-parent; descendants die with the controller by kernel design (not measured); inner controller proves it runs inside this launcher's bwrap via pid-1 comm/cmdline + run id before starting anything",
            "userNamespace": "proven by comparing /proc/self/ns/user identity (st_dev, st_ino) sampled from the host launcher before starting bwrap against the probe's own identity; they must differ, equality or an unreadable identity on either side is NO-GO rather than guessed from uid_map shape",
            "networkNamespace": "unshared; loopback TCP self-connect (bind/listen/connect/accept/echo) is measured in --check and must be fully usable, otherwise NO-GO; never bypassed; bind-only success is not accepted because lo can be DOWN",
            "environment": "bwrap --clearenv plus explicit allowlist",
            "configRoot": "KERMINAL_CONFIG_ROOT=<run-root>/config",
            "shell": "<run-root>/bin/bash wrapper exporting SHELL inside the run root",
            "limitations": [
                "read-only /usr,/etc,/sys remain shared system inputs; they are not a private copy",
                "the host launcher is not inside bubblewrap: it only creates the run root and never deletes it",
                "Xvfb is not a compositor; no WM, GPU, DPI or IME path is exercised",
            ],
        },
        "input": {
            "method": "ctypes libXtst XTestFakeMotionEvent/XTestFakeButtonEvent/XTestFakeKeyEvent",
            "shortcuts": {
                "newLocalTab": "Ctrl+Shift+T",
                "splitRight": "Ctrl+Alt+Right",
                "openLogs": "Alt+7",
                "quit": "Ctrl+Shift+Q",
            },
            "paneClickFractions": {"a": list(PANE_A_POINT), "b": list(PANE_B_POINT)},
            "failClosed": [
                "no Kerminal X11 window",
                "shell integration not producing command history",
                "marker history row missing, duplicated, or not user/local",
                "two markers not sharing one tab_id",
                "pane_id/session_id not distinct",
                "screenshot is blank, uniform, mostly transparent, or flat (pixel gate)",
                "right-rail assumed-region pixels fail the anti-blank gate",
                "no observable screen transition after Alt+7",
                "A/B screenshot hashes identical",
                "graceful quit did not finish before timeout",
                "sandbox controller exit code or report missing",
                "controller cannot prove pid-1 is this launcher's bwrap with --unshare-all and this run's id (--inner guard; NSpid depth is not usable because the inner procfs shows a single level)",
            ],
        },
        "markers": {
            "a": "echo kerminal_rr_split_a_<random hex>",
            "b": "echo kerminal_rr_split_b_<random hex>",
            "sensitive": False,
        },
        "sqlite": {
            "path": "<run-root>/config/data/command.sqlite",
            "access": "read-only URI mode=ro with PRAGMA query_only=ON",
            "schemaVersionRequired": 3,
            "assertions": [
                "exactly one row per marker command",
                "source=user and target=local",
                "both rows have non-empty pane_id/session_id/tab_id",
                "pane_id and session_id differ between A and B",
                "tab_id is identical for A and B",
                "A row precedes B row",
            ],
        },
        "screenshots": [
            "01-window-ready.png",
            "02-before-logs.png",
            "03-after-logs.png",
            "10-pane-a.png",
            "20-pane-b.png",
            "30-pane-a-return.png",
        ],
        "screenshotGate": {
            "signatureAndGeometry": "PNG signature + fixed 1600x1000",
            "antiBlank": (
                "per-pixel distinct-RGB >= 8 and luma-range >= 8 on the full frame "
                "and on the right-rail assumed region (fraction 0.72..0.98 x 0.02..0.98); "
                "rejects pure black, solid color, uniform, and mostly transparent frames"
            ),
            "notClaimed": (
                "the gate is anti-blank only; it does not prove right-rail presence, "
                "content, focus follow, or animation — full-frame hash differences are "
                "never reported as right-rail visual/animation pass"
            ),
        },
        "cleanup": {
            "sandbox": "SIGTERM/SIGKILL the outer bwrap process group; private PID namespace removes descendants",
            "xvfb": "inner controller stops only the Xvfb pid it started",
            "runRoot": "always retained, including on success; no auto-delete path exists in this script; human deletion is a manual filesystem action",
            "failure": "preserve the original stage error; cleanup failures are reported separately",
        },
        "claims": {
            "mechanicalPass": "sqlite identity + window presence + anti-blank pixel gate on frames and the assumed right-rail region",
            "notClaimed": [
                "visual focus or animation/opacity pass",
                "right-rail content verified by the script (selector/render scope unverified; screenshots require human review)",
                "fixed-region geometry/transition timing",
                "hardware GPU, DPI scaling, physical IME, compositor or OS WM behavior",
                "network behavior beyond the loopback-only TCP self-connect probe and namespace isolation",
            ],
            "humanReviewRequired": True,
            "evidenceRetention": "run root and all PNGs are never auto-deleted; host report maps sandbox paths to fileHost for human review",
        },
    }


class PrivateXvfb:
    """只拥有本进程启动的 Xvfb；display 号、PID 和日志都限定在私有 /tmp。"""

    def __init__(self, paths: RunPaths):
        self.paths = paths
        self.process: Optional[subprocess.Popen[bytes]] = None
        self.display = ""
        self.screen = 0
        self.log_handle = None

    def _pick_display(self) -> int:
        """随机起点加顺序探测，避免同一命名空间里的第二次启动相撞。"""
        start = random.SystemRandom().randint(120, 220)
        for offset in range(0, 80):
            number = 120 + ((start - 120 + offset) % 80)
            socket = pathlib.Path(f"/tmp/.X11-unix/X{number}")
            lock = pathlib.Path(f"/tmp/.X{number}-lock")
            if not socket.exists() and not lock.exists():
                return number
        raise PreflightError("no free private Xvfb display number in 120..199")

    def start(self) -> str:
        """启动私有 Xvfb 并等 xdpyinfo 成功；失败时不复用半启动进程。"""
        socket_dir = pathlib.Path("/tmp/.X11-unix")
        socket_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(socket_dir, 0o700)
        number = self._pick_display()
        self.display = f":{number}"
        self.screen = number
        xauth = _which("xauth")
        if not xauth:
            raise PreflightError("xauth is required to protect the private display")
        cookie = secrets.token_hex(16)
        env = {
            "PATH": BASE_ENVIRONMENT["PATH"],
            "HOME": str(self.paths.home),
            "XAUTHORITY": str(self.paths.xauthority),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
        }
        auth = _run(
            [
                xauth,
                "-f",
                str(self.paths.xauthority),
                "add",
                self.display,
                "MIT-MAGIC-COOKIE-1",
                cookie,
            ],
            env=env,
            timeout=5.0,
        )
        if auth.returncode != 0:
            raise PreflightError("cannot create the private Xauthority cookie")
        os.chmod(self.paths.xauthority, 0o600)
        self.log_handle = self.paths.xvfb_log.open("wb")
        argv = [
            _which("Xvfb") or "/usr/bin/Xvfb",
            self.display,
            "-screen",
            "0",
            f"{WINDOW_WIDTH}x{WINDOW_HEIGHT}x{SCREEN_DEPTH}",
            "-dpi",
            str(SCREEN_DPI),
            "-nolisten",
            "tcp",
            "-auth",
            str(self.paths.xauthority),
        ]
        try:
            self.process = subprocess.Popen(
                argv,
                stdin=subprocess.DEVNULL,
                stdout=self.log_handle,
                stderr=subprocess.STDOUT,
                env=env,
                cwd=str(self.paths.root),
                start_new_session=True,
            )
        except OSError as error:
            raise PreflightError(f"cannot start private Xvfb: {error}") from error
        deadline = time.monotonic() + 15.0
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise PreflightError(
                    f"private Xvfb exited before readiness (exit {self.process.returncode})"
                )
            try:
                probe = _run(
                    ["xdpyinfo", "-display", self.display], env=env, timeout=3.0
                )
            except (OSError, subprocess.TimeoutExpired):
                probe = None
            if probe is not None and probe.returncode == 0:
                return self.display
            time.sleep(0.2)
        raise PreflightError("private Xvfb did not answer xdpyinfo before timeout")

    def stop(self) -> dict[str, object]:
        """只终止本实例 PID，并报告它自己未清理的 lock/socket 残余。"""
        record: dict[str, object] = {"pid": self.process.pid if self.process else None}
        if self.process is not None and self.process.poll() is None:
            try:
                self.process.send_signal(signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                self.process.wait(timeout=PROCESS_TERM_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                try:
                    self.process.kill()
                except ProcessLookupError:
                    pass
                try:
                    self.process.wait(timeout=PROCESS_KILL_TIMEOUT_SECONDS)
                except subprocess.TimeoutExpired:
                    raise CleanupError("private Xvfb did not exit after SIGKILL")
        record["exitCode"] = self.process.returncode if self.process else None
        if self.log_handle is not None:
            try:
                self.log_handle.close()
            except OSError:
                pass
            self.log_handle = None
        record["lockLeft"] = pathlib.Path(f"/tmp/.X{self.screen}-lock").exists()
        record["socketLeft"] = pathlib.Path(f"/tmp/.X11-unix/X{self.screen}").exists()
        record["display"] = self.display
        return record


class X11Session:
    """外部 X11/XTest 操作；不依赖应用内部 selector 或 render_to_image。"""

    def __init__(self, x11: ctypes.CDLL, xtst: ctypes.CDLL, display: str):
        self.x11 = x11
        self.xtst = xtst
        self.dpy = x11.XOpenDisplay(display.encode())
        if not self.dpy:
            raise PreflightError(f"cannot open private display {display}")
        self.screen = x11.XDefaultScreen(self.dpy)
        self.root = x11.XDefaultRootWindow(self.dpy)
        self.net_wm_name = x11.XInternAtom(self.dpy, b"_NET_WM_NAME", 0)
        self.utf8_string = x11.XInternAtom(self.dpy, b"UTF8_STRING", 0)
        self.window: Optional[int] = None

    def close(self) -> None:
        """关闭本实例唯一 display 连接，避免迟到事件落到下一轮。"""
        if self.dpy:
            self.x11.XCloseDisplay(self.dpy)
            self.dpy = None

    def _children(self, window: int) -> list[int]:
        """读取并立即释放 XQueryTree 分配的 child 数组。"""
        root_return = ctypes.c_ulong()
        parent_return = ctypes.c_ulong()
        children = ctypes.POINTER(ctypes.c_ulong)()
        count = ctypes.c_uint()
        if not self.x11.XQueryTree(
            self.dpy,
            window,
            ctypes.byref(root_return),
            ctypes.byref(parent_return),
            ctypes.byref(children),
            ctypes.byref(count),
        ):
            return []
        try:
            return [int(children[index]) for index in range(count.value)]
        finally:
            if children:
                self.x11.XFree(children)

    def _title(self, window: int) -> str:
        """优先读取 UTF-8 _NET_WM_NAME，退回 WM_NAME；空标题不参与匹配。"""
        actual_type = ctypes.c_ulong()
        actual_format = ctypes.c_int()
        nitems = ctypes.c_ulong()
        bytes_after = ctypes.c_ulong()
        prop = ctypes.POINTER(ctypes.c_ubyte)()
        status = self.x11.XGetWindowProperty(
            self.dpy,
            window,
            self.net_wm_name,
            0,
            256,
            0,
            self.utf8_string,
            ctypes.byref(actual_type),
            ctypes.byref(actual_format),
            ctypes.byref(nitems),
            ctypes.byref(bytes_after),
            ctypes.byref(prop),
        )
        if status == 0 and prop and actual_format.value == 8:
            try:
                return ctypes.string_at(prop, nitems.value).decode("utf-8", "replace")
            finally:
                self.x11.XFree(prop)
        name = ctypes.c_char_p()
        if self.x11.XFetchName(self.dpy, window, ctypes.byref(name)) and name.value:
            try:
                return name.value.decode("utf-8", "replace")
            finally:
                self.x11.XFree(ctypes.cast(name, ctypes.c_void_p))
        return ""

    def _attributes(self, window: int) -> Optional[XWindowAttributes]:
        """读取窗口可见性与几何；失败窗口直接忽略。"""
        attributes = XWindowAttributes()
        if not self.x11.XGetWindowAttributes(self.dpy, window, ctypes.byref(attributes)):
            return None
        return attributes

    def find_window(self, timeout: float = 60.0) -> WindowInfo:
        """在 root 子树上等待真实可见的 Kerminal 顶层窗口，并按面积选最大者。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            candidates: list[WindowInfo] = []
            stack = [(child, 0) for child in self._children(self.root)]
            while stack:
                window, depth = stack.pop()
                attributes = self._attributes(window)
                if attributes is None:
                    continue
                if attributes.map_state == IS_VIEWABLE:
                    title = self._title(window)
                    if "kerminal" in title.lower():
                        candidates.append(
                            WindowInfo(
                                window_id=window,
                                title=title,
                                width=attributes.width,
                                height=attributes.height,
                                x=attributes.x,
                                y=attributes.y,
                            )
                        )
                if depth < 3:
                    stack.extend((child, depth + 1) for child in self._children(window))
            if candidates:
                best = max(candidates, key=lambda item: item.width * item.height)
                self.window = best.window_id
                return best
            time.sleep(0.25)
        raise PreflightError("Kerminal X11 window was not observed before timeout")

    def prepare_window(self) -> WindowInfo:
        """把窗口放到固定 root 几何并夺取 X 输入焦点，使 XTest 事件可归因于本轮窗口。"""
        if self.window is None:
            raise PreflightError("window must be discovered before prepare_window")
        self.x11.XMoveResizeWindow(
            self.dpy, self.window, 0, 0, WINDOW_WIDTH, WINDOW_HEIGHT
        )
        self.x11.XMapRaised(self.dpy, self.window)
        self.x11.XSetInputFocus(self.dpy, self.window, REVERT_TO_PARENT, CURRENT_TIME)
        self.x11.XSync(self.dpy, 0)
        time.sleep(0.8)
        attributes = self._attributes(self.window)
        if attributes is None or attributes.map_state != IS_VIEWABLE:
            raise PreflightError("prepared Kerminal window is no longer viewable")
        if (attributes.width, attributes.height) != (WINDOW_WIDTH, WINDOW_HEIGHT):
            raise PreflightError("Kerminal window did not accept the fixed geometry")
        if (attributes.x, attributes.y) != (0, 0):
            raise PreflightError(
                "Kerminal window is not at the Xvfb root origin; click coordinates would be ambiguous"
            )
        return WindowInfo(
            window_id=self.window,
            title=self._title(self.window),
            width=attributes.width,
            height=attributes.height,
            x=attributes.x,
            y=attributes.y,
        )

    def focus_window(self) -> None:
        """每个输入阶段前重新确认顶层 X 焦点，避免无 WM 环境下焦点漂到 root。"""
        if self.window is None:
            raise PreflightError("window is not prepared")
        self.x11.XSetInputFocus(self.dpy, self.window, REVERT_TO_PARENT, CURRENT_TIME)
        self.x11.XSync(self.dpy, 0)
        focused = ctypes.c_ulong()
        revert = ctypes.c_int()
        self.x11.XGetInputFocus(self.dpy, ctypes.byref(focused), ctypes.byref(revert))
        if focused.value != self.window:
            raise AcceptanceError(
                "X input focus did not remain on the prepared Kerminal window"
            )

    def click_fraction(self, x_fraction: float, y_fraction: float) -> None:
        """按窗口比例点击 terminal 画布；不依赖内部 selector 或标题文本。"""
        if not 0.0 < x_fraction < 1.0 or not 0.0 < y_fraction < 1.0:
            raise AcceptanceError("pane click fraction is outside the window")
        x = int(WINDOW_WIDTH * x_fraction)
        y = int(WINDOW_HEIGHT * y_fraction)
        self.xtst.XTestFakeMotionEvent(self.dpy, self.screen, x, y, 0)
        self.xtst.XTestFakeButtonEvent(self.dpy, 1, 1, 0)
        self.xtst.XTestFakeButtonEvent(self.dpy, 1, 0, 0)
        self.x11.XSync(self.dpy, 0)
        time.sleep(0.25)

    def _keycode(self, keysym: int) -> int:
        """keysym 必须能映射到当前 XKB 键盘，否则键入会变成不可归因的静默失败。"""
        keycode = self.x11.XKeysymToKeycode(self.dpy, keysym)
        if not keycode:
            raise AcceptanceError("keysym has no keycode on the private XKB map")
        return int(keycode)

    def _tap(self, keycode: int, shift: bool = False) -> None:
        """按下/释放单个键；Shift 只包裹字符键，不和组合键修饰符混用。"""
        shift_code = self._keycode(KEY_SHIFT) if shift else 0
        if shift:
            self.xtst.XTestFakeKeyEvent(self.dpy, shift_code, 1, 0)
        self.xtst.XTestFakeKeyEvent(self.dpy, keycode, 1, 0)
        self.xtst.XTestFakeKeyEvent(self.dpy, keycode, 0, 0)
        if shift:
            self.xtst.XTestFakeKeyEvent(self.dpy, shift_code, 0, 0)
        self.x11.XSync(self.dpy, 0)
        time.sleep(0.012)

    def type_text(self, text: str) -> None:
        """逐字符查 XKB 两级映射；要求 Shift 或无法表示时失败关闭而不是猜布局。"""
        for character in text:
            name = CHAR_KEYSYMS.get(character, character)
            keysym = int(self.x11.XStringToKeysym(name.encode("utf-8")))
            if not keysym:
                raise AcceptanceError(f"X11 cannot resolve character {character!r}")
            keycode = self._keycode(keysym)
            level0 = int(self.x11.XkbKeycodeToKeysym(self.dpy, keycode, 0, 0))
            level1 = int(self.x11.XkbKeycodeToKeysym(self.dpy, keycode, 0, 1))
            if level0 == keysym:
                shift = False
            elif level1 == keysym:
                shift = True
            else:
                raise AcceptanceError(
                    f"character {character!r} is not representable on the current XKB map"
                )
            self._tap(keycode, shift=shift)

    def press_enter(self) -> None:
        """使用独立 Enter 键码提交命令行，让 UI 的历史捕获路径看到明确提交流程。"""
        keysym = int(self.x11.XStringToKeysym(b"Return"))
        self._tap(self._keycode(keysym))

    def chord(self, modifiers: Iterable[int], key_name: str) -> None:
        """按修饰符顺序按下、点击主键、逆序释放，模拟真实组合键而非直接注入文本。"""
        modifier_codes = [self._keycode(keysym) for keysym in modifiers]
        key_keysym = int(self.x11.XStringToKeysym(key_name.encode("utf-8")))
        if not key_keysym:
            raise AcceptanceError(f"X11 cannot resolve chord key {key_name!r}")
        key_code = self._keycode(key_keysym)
        for code in modifier_codes:
            self.xtst.XTestFakeKeyEvent(self.dpy, code, 1, 0)
        self.xtst.XTestFakeKeyEvent(self.dpy, key_code, 1, 0)
        self.xtst.XTestFakeKeyEvent(self.dpy, key_code, 0, 0)
        for code in reversed(modifier_codes):
            self.xtst.XTestFakeKeyEvent(self.dpy, code, 0, 0)
        self.x11.XSync(self.dpy, 0)
        time.sleep(0.12)


# 右栏假定区域：GPUI 客户端装饰、无 WM 的 Xvfb root 上窗口即全屏，右栏按宽度
# 从右侧切出（右边 28%、全高、留边）。这是几何假设而不是 selector 证明，报告
# 中必须保持 "assumed-region" 口径；窗口几何已在 prepare_window 强制为固定值。
RIGHT_RAIL_REGION_FRACTION = (0.72, 0.02, 0.98, 0.98)


def _screenshot(
    display: str, paths: RunPaths, name: str
) -> dict[str, object]:
    """用私有 DISPLAY 的 scrot 抓取 root 画面并做签名、尺寸与像素内容门禁。

    历史版本曾带第四个 sandbox_root 形参，六个调用点把沙箱根路径常量与截图
    文件名成对交换，导致 scrot 目标路径落到沙箱根而不是 evidence 目录；该形参
    在函数体内从未被使用，直接删除，让"目录来自 paths、文件名是唯一可变项"
    成为不可能再交换的单一来源。
    整帧与右栏假定区域分别统计 distinct RGB / 亮度跨度，拒绝纯黑、纯色、均匀
    和透明帧；这不是右栏内容证明，只是让后续哈希比较至少发生在真实画面上。
    file 记录沙箱内路径，fileHost 记录宿主侧可打开的对应路径，供人工复核。
    """
    path = paths.evidence / name
    env = {
        "DISPLAY": display,
        "PATH": BASE_ENVIRONMENT["PATH"],
        "HOME": str(paths.home),
        "XAUTHORITY": str(paths.xauthority),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
    }
    try:
        result = _run(["scrot", "-o", str(path)], env=env, timeout=15.0, cwd=paths.root)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise AcceptanceError(f"screenshot {name} failed: {error}") from error
    if result.returncode != 0 or not path.is_file():
        raise AcceptanceError(f"screenshot {name} failed (scrot exit {result.returncode})")
    data = path.read_bytes()
    if len(data) < 24 or data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise AcceptanceError(f"screenshot {name} is not a valid PNG")
    width = int.from_bytes(data[16:20], "big")
    height = int.from_bytes(data[20:24], "big")
    if (width, height) != (WINDOW_WIDTH, WINDOW_HEIGHT):
        raise AcceptanceError(
            f"screenshot {name} geometry is {width}x{height}, expected {WINDOW_WIDTH}x{WINDOW_HEIGHT}"
        )
    _, _, pixels = _decode_png_pixels(data)
    frame_stats = _pixel_stats(pixels, 0, 0, width, height)
    _gate_pixel_stats(frame_stats, name)
    fx0, fy0, fx1, fy1 = RIGHT_RAIL_REGION_FRACTION
    region_stats = _pixel_stats(
        pixels,
        int(width * fx0),
        int(height * fy0),
        int(width * fx1),
        int(height * fy1),
    )
    _gate_pixel_stats(region_stats, f"{name} right-rail region")
    # 沙箱内路径与宿主 run root 的映射由宿主启动器统一改写（见 launch_native），
    # 控制器不知道宿主路径，也不应该知道。
    return {
        "file": str(path),
        "sha256": hashlib.sha256(data).hexdigest(),
        "width": width,
        "height": height,
        "frameStats": {
            "distinctRgb": frame_stats.distinct_rgb,
            "lumaRange": frame_stats.luma_range,
            "opaqueSamples": frame_stats.opaque_samples,
            "totalSamples": frame_stats.total_samples,
        },
        "rightRailRegionStats": {
            "regionFraction": list(RIGHT_RAIL_REGION_FRACTION),
            "regionKind": "assumed-geometry-not-selector-proven",
            "distinctRgb": region_stats.distinct_rgb,
            "lumaRange": region_stats.luma_range,
            "opaqueSamples": region_stats.opaque_samples,
            "totalSamples": region_stats.total_samples,
        },
        "pixelGate": "distinctRgb/lumaRange/opacity anti-blank guard; not a right-rail content proof",
    }


def _prepare_run_root(base: pathlib.Path) -> RunPaths:
    """建立独占目录树并逐项核验仍在 run root 内，任何符号链接逃逸都直接失败。"""
    if not base.is_absolute() or not base.is_dir():
        raise PreflightError("run base must be an existing absolute directory")
    if _is_within(base, _real_home()):
        raise PreflightError("run base must not be inside the real user HOME")
    repo_root = _repo_root()
    if repo_root is not None and _is_within(base, repo_root):
        raise PreflightError("run base must not be inside the repository worktree")
    base_text = str(base.resolve())
    if base_text == "/" or base_text == "/mnt" or base_text.startswith("/mnt/"):
        raise PreflightError(
            "run base must be on the Linux filesystem, not / or the Windows /mnt mount"
        )
    root = pathlib.Path(
        tempfile.mkdtemp(prefix="kerminal-rr-split-linux-", dir=str(base))
    )
    os.chmod(root, 0o700)
    if root.is_symlink() or root.resolve().parent != base.resolve():
        raise PreflightError("run root escaped the requested run base")
    paths = RunPaths(
        root=root,
        home=root / "home",
        config=root / "config",
        evidence=root / "evidence",
        app=root / "app",
        shell=root / "bin" / "bash",
        xauthority=root / "xauthority",
        controller=root / "controller.py",
        xvfb_log=root / "xvfb.log",
        app_log=root / "app.log",
        sandbox_log=root / "sandbox.log",
        inner_report_file=root / "evidence" / "inner-report.json",
        report_file=root / "evidence" / "right-rail-split-linux-native-report.json",
    )
    for directory in (
        paths.home,
        paths.home / "xdg" / "config",
        paths.home / "xdg" / "data",
        paths.home / "xdg" / "cache",
        paths.home / "xdg" / "state",
        paths.home / "runtime",
        paths.home / "tmp",
        paths.config,
        paths.evidence,
        paths.app,
        paths.shell.parent,
    ):
        directory.mkdir(parents=True, exist_ok=True)
        os.chmod(directory, 0o700)
        if directory.is_symlink() or not _is_within(directory, root):
            raise PreflightError(f"run directory escaped the run root: {directory}")
    paths.xauthority.write_bytes(b"")
    os.chmod(paths.xauthority, 0o600)
    # 只改变 SHELL 的可见取值，不复制系统 shell；wrapper 让应用把 pty 参数原样交给 /bin/bash。
    paths.shell.write_text('#!/bin/sh\nexec /bin/bash "$@"\n', encoding="utf-8")
    os.chmod(paths.shell, 0o700)
    return paths


def _prepare_existing_paths(root: pathlib.Path) -> RunPaths:
    """为已存在的 run root 重建路径视图，不创建目录；用于沙箱内控制器和显式删除。"""
    if root.is_symlink() or not root.is_dir():
        raise CleanupError(f"run root is not a verified directory: {root}")
    return RunPaths(
        root=root,
        home=root / "home",
        config=root / "config",
        evidence=root / "evidence",
        app=root / "app",
        shell=root / "bin" / "bash",
        xauthority=root / "xauthority",
        controller=root / "controller.py",
        xvfb_log=root / "xvfb.log",
        app_log=root / "app.log",
        sandbox_log=root / "sandbox.log",
        inner_report_file=root / "evidence" / "inner-report.json",
        report_file=root / "evidence" / "right-rail-split-linux-native-report.json",
    )


def _stage_app(source: pathlib.Path, paths: RunPaths) -> dict[str, str]:
    """把候选二进制复制到 run root 再执行，避免在 /mnt 上直接执行并只暴露该文件。"""
    source = source.resolve()
    if not source.is_file() or not os.access(source, os.X_OK):
        raise PreflightError("app binary is missing or not executable")
    destination = paths.app / "kerminal-desktop"
    shutil.copy2(source, destination)
    os.chmod(destination, 0o700)
    source_hash = _sha256_file(source)
    staged_hash = _sha256_file(destination)
    if source_hash != staged_hash:
        raise PreflightError("staged app binary hash differs from the source binary")
    return {
        "sourcePath": str(source),
        "stagedPath": str(destination),
        "sha256": staged_hash,
    }


def _stage_controller(paths: RunPaths) -> str:
    """把本脚本复制进 run root，使沙箱内控制器只读取本轮受控文件。"""
    shutil.copy2(pathlib.Path(__file__).resolve(), paths.controller)
    os.chmod(paths.controller, 0o700)
    return _sha256_file(paths.controller)


def _app_environment(display: str, paths: RunPaths) -> dict[str, str]:
    """为应用构造白名单环境；不继承宿主 DISPLAY、DBUS、SSH 或用户 XDG 路径。"""
    return {
        "DISPLAY": display,
        "HOME": str(paths.home),
        "XDG_CONFIG_HOME": str(paths.home / "xdg" / "config"),
        "XDG_DATA_HOME": str(paths.home / "xdg" / "data"),
        "XDG_CACHE_HOME": str(paths.home / "xdg" / "cache"),
        "XDG_STATE_HOME": str(paths.home / "xdg" / "state"),
        "XDG_RUNTIME_DIR": str(paths.home / "runtime"),
        "TMPDIR": str(paths.home / "tmp"),
        "SHELL": str(paths.shell),
        "HISTFILE": str(paths.home / ".bash_history"),
        "KERMINAL_CONFIG_ROOT": str(paths.config),
        "PATH": BASE_ENVIRONMENT["PATH"],
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "XAUTHORITY": str(paths.xauthority),
        "XDG_SESSION_TYPE": "x11",
        "LIBGL_ALWAYS_SOFTWARE": "1",
        "VK_ICD_FILENAMES": "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json",
        "WGPU_BACKEND": "vulkan",
        "NO_AT_BRIDGE": "1",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "RUST_BACKTRACE": "0",
    }


def _sandbox_environment(run_id: str) -> dict[str, str]:
    """控制器只需要解释器与隔离路径；root 使用沙箱内固定路径，不携带宿主环境。"""
    return {
        "HOME": f"{SANDBOX_ROOT}/home",
        "TMPDIR": f"{SANDBOX_ROOT}/home/tmp",
        "PATH": BASE_ENVIRONMENT["PATH"],
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONNOUSERSITE": "1",
        "XAUTHORITY": f"{SANDBOX_ROOT}/xauthority",
        RUN_ID_ENV: run_id,
    }

def _read_db_rows(
    database: pathlib.Path, commands: tuple[str, str]
) -> tuple[int, list[dict[str, str]]]:
    """以只读 URI 打开本轮 command.sqlite；绝不创建、迁移或清理历史库。

    只读口径的实测限制：数据库为 WAL 模式时，mode=ro 连接仍会在库旁创建
    -shm/-wal sidecar 文件（SQLite 需要共享内存索引才能读取 WAL 内容），
    这不是本函数的写操作；本函数不使用 immutable=1，因为该选项会忽略未
    checkpoint 的 WAL 数据、在 marker 刚写入时静默读到旧快照，等待失真。
    sidecar 由 SQLite 在连接关闭后自动清理。
    """
    if not database.is_file():
        raise FileNotFoundError(str(database))
    uri = f"file:{quote(str(database))}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=2.0)
    try:
        connection.execute("PRAGMA query_only = ON")
        version = int(connection.execute("PRAGMA user_version").fetchone()[0])
        cursor = connection.execute(
            "SELECT rowid, id, command, source, target, session_id, pane_id, tab_id, "
            "profile_id, created_at FROM command_history "
            "WHERE command IN (?, ?) ORDER BY rowid",
            commands,
        )
        columns = [description[0] for description in cursor.description]
        rows = [dict(zip(columns, row)) for row in cursor.fetchall()]
        return version, rows
    finally:
        connection.close()


def _wait_marker(
    database: pathlib.Path, marker: str, *, timeout: float
) -> dict[str, str]:
    """等待精确 marker 的唯一 user/local 行；重复行立即失败，避免用最近一条假装唯一。"""
    deadline = time.monotonic() + timeout
    last_error = ""
    while time.monotonic() < deadline:
        try:
            version, rows = _read_db_rows(database, (marker, marker))
        except (FileNotFoundError, sqlite3.OperationalError) as error:
            last_error = str(error)
            time.sleep(0.25)
            continue
        if version != 3:
            raise AcceptanceError(
                f"command.sqlite schema version is {version}, expected 3"
            )
        matching = [row for row in rows if row["command"] == marker]
        if len(matching) == 1:
            row = matching[0]
            if row["source"] != "user" or row["target"] != "local":
                raise AcceptanceError(
                    "marker history row is not a user/local terminal command"
                )
            return {key: "" if value is None else str(value) for key, value in row.items()}
        if len(matching) > 1:
            raise AcceptanceError("marker produced duplicate command history rows")
        time.sleep(0.25)
    raise AcceptanceError(
        f"marker history row was not observed before timeout ({last_error or 'no error'})"
    )


def _verify_history(a: dict[str, str], b: dict[str, str]) -> dict[str, bool]:
    """只核对可机械证明的身份关系；不把 tab_id 相等夸大成视觉或动画通过。"""
    return {
        "commandsDistinct": a.get("command") != b.get("command"),
        "sourceAndTarget": a.get("source") == b.get("source") == "user"
        and a.get("target") == b.get("target") == "local",
        "identitiesPresent": all(
            value
            for value in (
                a.get("pane_id"),
                a.get("session_id"),
                a.get("tab_id"),
                b.get("pane_id"),
                b.get("session_id"),
                b.get("tab_id"),
            )
        ),
        "sameTab": bool(a.get("tab_id")) and a.get("tab_id") == b.get("tab_id"),
        "distinctPane": bool(a.get("pane_id"))
        and bool(b.get("pane_id"))
        and a.get("pane_id") != b.get("pane_id"),
        "distinctSession": bool(a.get("session_id"))
        and bool(b.get("session_id"))
        and a.get("session_id") != b.get("session_id"),
        "aBeforeB": int(a.get("rowid", "0")) < int(b.get("rowid", "0")),
    }


def _write_report(path: pathlib.Path, report: dict[str, object]) -> pathlib.Path:
    """报告始终写入本轮 evidence；失败路径也保留同一文件，便于主 agent 复核。"""
    payload = json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True).encode(
        "utf-8"
    )
    temporary = path.parent / f".{path.name}.tmp"
    temporary.write_bytes(payload)
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)
    return path


def _focus_and_click(
    session: X11Session, point: tuple[float, float]
) -> None:
    """把焦点确认和 pane 中心点击固定为一个外部手势，便于失败日志定位阶段。"""
    session.focus_window()
    session.click_fraction(*point)


def _parse_sandbox_identity(
    pid1_comm: str, pid1_cmdline: str, status_text: str, run_id: str
) -> dict[str, object]:
    """从 pid-1 的 comm/cmdline 与 /proc/self/status 提取沙箱身份证据（纯函数）。

    设计原因：bwrap --unshare-all 的新 PID namespace 里 pid 1 就是 bwrap 本身，
    /proc/1/comm 与 /proc/1/cmdline 是内核维护的事实，宿主上 pid 1 是 init 不可能
    满足；cmdline 还携带宿主启动器注入的本轮唯一 run id，把"处于某种隔离"收紧为
    "处于本脚本启动器发起的这次 bwrap"。注意 NSpid 门禁被明确放弃：procfs 由最
    内层 PID namespace 挂载（bwrap --proc /proc 正是如此）时 NSpid 只显示单层，
    已在 Linux 5.15 + bwrap 0.6.1 实测，按多级判定会错误拒绝合法沙箱运行。
    CapEff 必须实际存在且为零，缺失即失败关闭，绝不默认为 0。
    """
    identity: dict[str, object] = {}
    comm = pid1_comm.strip()
    if comm != "bwrap":
        raise PreflightError(
            "sandbox identity unproven: pid 1 is not bubblewrap "
            f"(comm={comm!r}); refusing to start Xvfb or the app outside the "
            "launcher's sandbox"
        )
    identity["pid1Comm"] = comm
    argv = [token for token in pid1_cmdline.split("\0") if token]
    if "bwrap" not in pathlib.PurePath(argv[0] if argv else "").name:
        raise PreflightError("sandbox identity unproven: pid 1 argv is not bwrap")
    if "--unshare-all" not in argv:
        raise PreflightError(
            "sandbox identity unproven: bwrap was not started with --unshare-all"
        )
    if not run_id:
        raise PreflightError(
            "sandbox identity unproven: run id is empty; the controller cannot "
            "attribute this namespace to its own launcher"
        )
    if run_id not in argv:
        raise PreflightError(
            "sandbox identity unproven: pid 1 cmdline does not carry this run's "
            "id; the namespace was not created by this run's launcher"
        )
    identity["pid1ArgvTokens"] = len(argv)
    identity["unshareAllProven"] = True
    identity["runIdMatchedOnPid1Cmdline"] = True
    fields: dict[str, str] = {}
    for line in status_text.splitlines():
        key, _, value = line.partition(":")
        fields[key.strip()] = value.strip()
    # CapEff 缺失或不可解析都失败关闭：默认 0 会把"读不到能力"当成"能力已清零"。
    cap_eff = fields.get("CapEff")
    if not cap_eff:
        raise PreflightError(
            "sandbox identity unproven: CapEff is missing from /proc/self/status"
        )
    try:
        cap_zero = int(cap_eff, 16) == 0
    except ValueError as error:
        raise PreflightError(f"CapEff is not parseable: {error}") from error
    if not cap_zero:
        raise PreflightError(
            "sandbox identity unproven: capabilities were not dropped (CapEff != 0)"
        )
    identity["capEffZero"] = True
    # NSpid 只记录观测值用于报告，不做门禁：内层 procfs 视角下单层是正常形态。
    nspid = fields.get("NSpid", "")
    identity["nspidObserved"] = nspid
    return identity


def _verify_sandbox_identity(run_id: str) -> dict[str, object]:
    """用 /proc 内核证据证明当前进程运行在本脚本启动器发起的 bwrap 命名空间里。

    读取而不是假设：pid 1 必须是带 --unshare-all 且携带本轮 run id 的 bwrap、
    CapEff 必须存在且为零、/tmp 必须是独立挂载、/home 与 /mnt 不可见。任何一项
    读不到或不成立都抛 PreflightError——这保证在宿主直接执行 --inner 时先失败
    关闭，不会在宿主 /kerminal-run 恰好存在时启动 Xvfb 或应用。已实测限制：procfs
    由最内层 PID namespace 挂载时 NSpid 只有单层，所以不按 NSpid 深度判定。
    """
    pid = os.getpid()
    identity: dict[str, object] = {"pid": pid}
    try:
        pid1_comm = pathlib.Path("/proc/1/comm").read_text(encoding="utf-8")
        pid1_cmdline = pathlib.Path("/proc/1/cmdline").read_bytes().decode(
            "utf-8", "replace"
        )
        status_text = pathlib.Path("/proc/self/status").read_text(encoding="utf-8")
    except OSError as error:
        raise PreflightError(f"procfs identity files are not readable: {error}") from error
    identity.update(_parse_sandbox_identity(pid1_comm, pid1_cmdline, status_text, run_id))
    try:
        root_stat = os.stat("/")
        tmp_stat = os.stat("/tmp")
        identity["tmpIsPrivateMount"] = tmp_stat.st_dev != root_stat.st_dev
    except OSError as error:
        raise PreflightError(f"/tmp is not statable: {error}") from error
    if not identity["tmpIsPrivateMount"]:
        raise PreflightError(
            "sandbox identity unproven: /tmp is not a separate mount; "
            "the X socket would land on a shared filesystem"
        )
    if pathlib.Path("/home").exists() or pathlib.Path("/mnt").exists():
        raise PreflightError("sandbox did not hide /home or /mnt")
    identity["homeHidden"] = True
    identity["mntHidden"] = True
    return identity


def run_inner() -> int:
    """沙箱内控制器：启动 Xvfb 与应用，执行外部输入流程并写出报告。

    它运行在宿主启动的唯一 bubblewrap 命名空间里，因此 /tmp 是私有 tmpfs，
    X socket 不落到宿主；私有 PID 命名空间保证控制器退出后无残留后代。
    """
    run_id = os.environ.get(RUN_ID_ENV, "")
    if not run_id:
        print("run id is missing from the sandbox environment", file=sys.stderr)
        return 1
    # run root 路径准备显式接住类型化失败：它发生在主 try 之前，而主 try 的
    # except/finally 都依赖 paths 变量写报告，把准备挪进去会在失败时变成
    # NameError；此前直接调用则让 CleanupError 以裸 traceback 逃出控制器，
    # 沙箱内既无证据报告也没有可归因的退出信息。
    try:
        paths = _prepare_existing_paths(pathlib.Path(SANDBOX_ROOT))
    except AcceptanceError as error:
        print(f"run root is not usable: {error}", file=sys.stderr)
        return 1
    report: dict[str, object] = {
        "schemaVersion": SCHEMA_VERSION,
        "scope": "linux-native-right-rail-same-tab-split-switch",
        "status": "running",
        "startedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "runId": run_id,
        "humanReviewRequired": True,
        "claims": {
            "mechanicalPassOnly": True,
            "visualPass": False,
            "visualSplitVerified": False,
            "rightRailContentVerified": False,
            "animationObserved": False,
            "fixedRegionsVerified": False,
            "hardwareGpuVerified": False,
            "dpiScalingVerified": False,
            "physicalImeVerified": False,
        },
    }
    xvfb: Optional[PrivateXvfb] = None
    session: Optional[X11Session] = None
    app_process: Optional[subprocess.Popen[bytes]] = None
    app_log_handle = None
    stage = "inner-start"
    cleanup: dict[str, object] = {}
    original_error: Optional[BaseException] = None
    status = 1

    try:
        report["isolation"] = {
            "controllerInsideSandbox": True,
            "privateTmp": True,
            "displaySocketDirectory": "/tmp/.X11-unix (private tmpfs)",
            "environmentAllowlist": sorted(_sandbox_environment(run_id)),
            **_verify_sandbox_identity(run_id),
        }

        stage = "start-xvfb"
        xvfb = PrivateXvfb(paths)
        display = xvfb.start()
        os.environ["DISPLAY"] = display
        os.environ["XAUTHORITY"] = str(paths.xauthority)
        report["display"] = {
            "name": display,
            "screen": f"{WINDOW_WIDTH}x{WINDOW_HEIGHT}x{SCREEN_DEPTH}",
            "dpi": SCREEN_DPI,
            "network": "-nolisten tcp",
            "authorization": "MIT-MAGIC-COOKIE-1 under run root",
            "windowManager": "none",
            "socketPath": f"/tmp/.X11-unix/X{xvfb.screen}",
        }
        try:
            layout = _run(
                ["setxkbmap", "-display", display, "-query"],
                env={
                    "DISPLAY": display,
                    "PATH": BASE_ENVIRONMENT["PATH"],
                    "HOME": str(paths.home),
                    "XAUTHORITY": str(paths.xauthority),
                },
                timeout=5.0,
            )
            report["keyboardLayout"] = layout.stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            report["keyboardLayout"] = "unknown"

        stage = "start-app"
        app_log_handle = paths.app_log.open("wb")
        app_process = subprocess.Popen(
            [str(paths.app / "kerminal-desktop")],
            stdin=subprocess.DEVNULL,
            stdout=app_log_handle,
            stderr=subprocess.STDOUT,
            env=_app_environment(display, paths),
            cwd=str(paths.root),
            start_new_session=True,
        )
        report["appPid"] = app_process.pid

        stage = "find-window"
        time.sleep(1.0)
        if app_process.poll() is not None:
            raise AcceptanceError(
                f"application exited before opening a window (exit {app_process.returncode})"
            )
        x11, xtst = _load_xlibs(DependencyReport())
        if x11 is None or xtst is None:
            raise PreflightError("X11/XTest libraries disappeared after preflight")
        session = X11Session(x11, xtst, display)
        session.find_window(timeout=60.0)
        window = session.prepare_window()
        report["window"] = {
            "id": window.window_id,
            "title": window.title,
            "width": window.width,
            "height": window.height,
            "x": window.x,
            "y": window.y,
        }
        screenshots = [_screenshot(display, paths, "01-window-ready.png")]
        report["screenshots"] = screenshots

        marker_nonce = secrets.token_hex(6)
        marker_a = f"echo kerminal_rr_split_a_{marker_nonce}"
        marker_b = f"echo kerminal_rr_split_b_{marker_nonce}"
        report["markers"] = {"a": marker_a, "b": marker_b, "sensitive": False}
        database = paths.config / "data" / "command.sqlite"

        stage = "create-initial-tab"
        session.focus_window()
        session.chord([KEY_CTRL, KEY_SHIFT], "t")
        time.sleep(SHELL_SETTLE_SECONDS)

        stage = "type-marker-a"
        _focus_and_click(session, PANE_A_POINT)
        session.focus_window()
        session.type_text(marker_a)
        session.press_enter()
        row_a = _wait_marker(database, marker_a, timeout=MARKER_TIMEOUT_SECONDS)
        report["historyA"] = row_a

        stage = "split-right"
        session.focus_window()
        session.chord([KEY_CTRL, KEY_ALT], "Right")
        time.sleep(SPLIT_SETTLE_SECONDS)

        stage = "type-marker-b"
        _focus_and_click(session, PANE_B_POINT)
        session.focus_window()
        session.type_text(marker_b)
        session.press_enter()
        row_b = _wait_marker(database, marker_b, timeout=MARKER_TIMEOUT_SECONDS)
        report["historyB"] = row_b
        checks = _verify_history(row_a, row_b)
        report["historyChecks"] = checks
        if not all(checks.values()):
            failed = [name for name, passed in checks.items() if not passed]
            raise AcceptanceError(
                "command history identity checks failed: " + ", ".join(failed)
            )

        stage = "open-logs"
        session.focus_window()
        before_logs = _screenshot(display, paths, "02-before-logs.png")
        screenshots.append(before_logs)
        session.chord([KEY_ALT], "7")
        time.sleep(PANEL_SETTLE_SECONDS)
        after_logs = _screenshot(display, paths, "03-after-logs.png")
        screenshots.append(after_logs)
        if before_logs["sha256"] == after_logs["sha256"]:
            raise AcceptanceError(
                "screen did not change after Alt+7; no observable UI transition"
            )

        stage = "switch-a"
        _focus_and_click(session, PANE_A_POINT)
        time.sleep(PANEL_SETTLE_SECONDS)
        shot_a = _screenshot(display, paths, "10-pane-a.png")
        screenshots.append(shot_a)

        stage = "switch-b"
        _focus_and_click(session, PANE_B_POINT)
        time.sleep(PANEL_SETTLE_SECONDS)
        shot_b = _screenshot(display, paths, "20-pane-b.png")
        screenshots.append(shot_b)
        if shot_a["sha256"] == shot_b["sha256"]:
            raise AcceptanceError(
                "screen did not change between pane A and pane B clicks"
            )

        stage = "switch-a-return"
        _focus_and_click(session, PANE_A_POINT)
        time.sleep(PANEL_SETTLE_SECONDS)
        screenshots.append(_screenshot(display, paths, "30-pane-a-return.png"))

        stage = "quit"
        session.focus_window()
        session.chord([KEY_CTRL, KEY_SHIFT], "q")
        try:
            app_process.wait(timeout=QUIT_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired as error:
            raise AcceptanceError(
                "application did not finish graceful quit before timeout"
            ) from error
        if app_process.returncode != 0:
            raise AcceptanceError(
                f"application exited with {app_process.returncode} during graceful quit"
            )

        stage = "verify-persisted-history"
        final_version, final_rows = _read_db_rows(database, (marker_a, marker_b))
        if final_version != 3:
            raise AcceptanceError(
                f"persisted history schema version is {final_version}, expected 3"
            )
        final_by_command = {row["command"]: row for row in final_rows}
        if (
            final_by_command.get(marker_a, {}).get("id") != row_a.get("id")
            or final_by_command.get(marker_b, {}).get("id") != row_b.get("id")
        ):
            raise AcceptanceError("persisted marker history changed after graceful quit")

        report["status"] = "mechanical-pass-human-visual-review-required"
        report["mechanicalAssertions"] = {
            "windowObserved": True,
            "markersRecordedOnce": True,
            "sameTabDistinctPaneAndSession": True,
            "screenChangedAfterAlt7": True,
            "screenChangedBetweenPaneClicks": True,
            "hashChecksAreLivenessOnly": True,
            "gracefulQuit": True,
            "historyPersisted": True,
        }
        report["evidenceLimits"] = [
            "XTest clicks prove coordinates were dispatched; GPUI internal focus and right-rail content require human screenshot review",
            "command.sqlite proves same tab_id and distinct pane_id/session_id, not visual animation or opacity",
            "screenshot hash changes are liveness guards only; cursor blink or unrelated redraw can change pixels and do not prove logs content",
            "the right-rail pixel gate is an anti-blank/uniform-frame guard on an assumed geometry region (fraction 0.72..0.98 x 0.02..0.98), not a selector-based or content proof of the right rail",
            "screenshots come from Xvfb root via scrot; no compositor, OS window manager, or hardware GPU path is exercised",
            "client-side decorations, software Vulkan/lavapipe, C.UTF-8 locale, and unavailable system CJK fonts limit font/IME/DPI claims",
            "OS-level input is synthetic XTest against Xvfb, not a physical keyboard, IME, or compositor event stream",
            "bubblewrap unshares the network namespace; --check measured a loopback-only TCP self-connect inside a probe sandbox; the application's network behavior itself is not exercised by this run",
            "no in-process event or selector instrumentation is used; input is external XTest only",
            "app SHA-256 is recorded independently; binary-to-HEAD correspondence and debug/release mode are not proven",
            "fixed-region geometry, transition frames, fade duration, and system appearance following are not claimed",
            "the run root with all PNG evidence is retained; nothing is auto-deleted, including on success",
        ]
        report["humanReviewChecklist"] = [
            "10-pane-a.png and 30-pane-a-return.png show pane A focused and right rail history containing only the A marker",
            "20-pane-b.png shows pane B focused and right rail history containing only the B marker",
            "the window is genuinely split into two panes inside one tab; the right rail card/toolbar do not jump",
            "screenshots are raw evidence, not a visual pass; animation and fixed-region claims remain unverified",
            "review each screenshot's fileHost path on the host; screenshots have passed only an anti-blank pixel gate",
        ]
        status = 0
    except BaseException as error:  # 保留原始阶段错误，清理失败只追加证据
        original_error = error
        report["status"] = "failed"
        report["failure"] = {
            "stage": stage,
            "type": type(error).__name__,
            "message": str(error),
        }
        if paths.app_log.is_file():
            try:
                tail = paths.app_log.read_text(encoding="utf-8", errors="replace")[-2000:]
                if tail:
                    report["appLogTail"] = tail
            except OSError:
                pass
        status = 1
    finally:
        cleanup_errors: list[str] = []
        if app_process is not None and app_process.poll() is None:
            try:
                os.killpg(app_process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                app_process.wait(timeout=PROCESS_TERM_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(app_process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    app_process.wait(timeout=PROCESS_KILL_TIMEOUT_SECONDS)
                except subprocess.TimeoutExpired:
                    cleanup_errors.append("application process survived SIGKILL")
        cleanup["appExitCode"] = (
            app_process.returncode if app_process is not None else None
        )
        if app_log_handle is not None:
            try:
                app_log_handle.close()
            except OSError as error:
                cleanup_errors.append(f"app log close: {error}")
        if session is not None:
            try:
                session.close()
            except BaseException as error:
                cleanup_errors.append(f"X11 close: {error}")
        if xvfb is not None:
            try:
                cleanup["xvfb"] = xvfb.stop()
            except BaseException as error:
                cleanup_errors.append(f"xvfb cleanup: {error}")
        cleanup["pidNamespaceDiesWithController"] = "by-kernel-design-not-measured"
        cleanup["evidenceRetained"] = True
        report["cleanup"] = cleanup
        report["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
        if cleanup_errors:
            report["cleanupErrors"] = cleanup_errors
            if original_error is None:
                report["status"] = "failed"
                report["failure"] = {
                    "stage": "cleanup",
                    "type": "CleanupError",
                    "message": "; ".join(cleanup_errors),
                }
            # 清理失败时退出码必须与失败报告一致：此前只把报告改成 failed 而
            # 保留 status=0，内层成功返回 0 却携带失败报告，形成自相矛盾的证据；
            # 宿主虽按报告失败关闭，但返回值本身必须同样保真。
            status = 1
        # 证据报告写不出来属于证据缺失：退出码必须如实反映（报告是宿主失败
        # 关闭与人工复核的唯一机器可读载体），否则内层返回"成功"却没有任何
        # 可读证据，与主体失败同样不可接受。这里不能简单与主体失败合并改写
        # status——主体失败时退出码已是 1；只有主体成功（status==0）而报告
        # 没写出来时才需要降级，避免覆盖更具体的失败信息。
        try:
            report["reportPath"] = str(_write_report(paths.inner_report_file, report))
        except OSError as error:
            print(f"failed to write evidence report: {error}", file=sys.stderr)
            if status == 0:
                status = 1
    # 退出码在 finally 之后统一决定：主体失败为 1，主体成功但清理失败降级为 1，
    # 任何情况下都与写入报告的 status 一致，避免内层返回值与失败报告互相矛盾。
    return status


def _remap_screenshot_paths(
    screenshots: object, host_root: pathlib.Path
) -> list[dict[str, object]]:
    """把 inner 报告里的沙箱截图路径改写为宿主可打开的 run root 路径。

    每个条目都必须出现在返回值里：结构异常的条目保留原样并标记
    remapStatus=malformed-entry，绝不静默丢弃；沙箱路径改写时校验映射结果
    仍在宿主 run root 内，出现逃逸按 remapStatus=error 标记而不产出可打开的
    越界路径；宿主文件缺失标记 fileHostExists=false。人工复核因此总能看到
    沙箱内原始 file 路径，也能一眼区分成功映射与可疑条目。
    """
    if not isinstance(screenshots, list):
        # 非列表结构整体异常：返回带状态说明的空结果，不假装映射成功。
        return [
            {
                "remapStatus": "error",
                "error": "screenshots value is not a list; nothing was remapped",
            }
        ]
    remapped: list[dict[str, object]] = []
    host_root_resolved = host_root.resolve()
    for entry in screenshots:
        if not isinstance(entry, dict):
            remapped.append(
                {
                    "remapStatus": "malformed-entry",
                    "originalEntry": entry,
                }
            )
            continue
        record = dict(entry)
        file_path = record.get("file")
        if not isinstance(file_path, str) or not file_path:
            record["remapStatus"] = "malformed-entry"
            remapped.append(record)
            continue
        if not file_path.startswith(SANDBOX_ROOT + "/"):
            record["remapStatus"] = "unmapped"
            remapped.append(record)
            continue
        relative = file_path[len(SANDBOX_ROOT) + 1 :]
        host_file = host_root_resolved / relative
        try:
            resolved = host_file.resolve()
        except OSError:
            record["remapStatus"] = "error"
            record["error"] = "host path cannot be resolved"
            remapped.append(record)
            continue
        if resolved != host_root_resolved and not _is_within(resolved, host_root_resolved):
            # 映射结果逃出 run root 属于证据异常：标记错误而不是给出可打开路径。
            record["remapStatus"] = "error"
            record["error"] = "mapped path escapes the run root"
            remapped.append(record)
            continue
        record["fileHost"] = str(resolved)
        record["fileHostExists"] = resolved.is_file()
        record["remapStatus"] = "ok" if record["fileHostExists"] else "host-file-missing"
        remapped.append(record)
    return remapped


def launch_native(
    app_source: pathlib.Path, run_base: pathlib.Path
) -> tuple[int, dict[str, object]]:
    """宿主侧启动器：只创建 run root、启动唯一沙箱、读取沙箱报告并处理清理。

    宿主不直接启动 Xvfb 或应用，因此不会碰到用户 DISPLAY、桌面或用户配置；
    接触 UI 的进程全部在私有挂载/PID/网络命名空间内。
    """
    report: dict[str, object] = {
        "schemaVersion": SCHEMA_VERSION,
        "scope": "linux-native-right-rail-same-tab-split-switch",
        "status": "running",
        "startedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "humanReviewRequired": True,
    }
    paths: Optional[RunPaths] = None
    process: Optional[subprocess.Popen[bytes]] = None
    log_handle = None
    run_id = secrets.token_hex(16)
    stage = "preflight"
    status = 1

    try:
        dependencies = check_dependencies(need_app=app_source)
        report["dependencies"] = {
            "ready": dependencies.ready,
            "blockers": dependencies.blockers,
            "tools": dependencies.tools,
            "libraries": dependencies.libraries,
            "versions": dependencies.versions,
            "sandbox": dependencies.sandbox,
            "vulkanIcd": dependencies.vulkan_icd,
            "sqliteVersion": dependencies.sqlite_version,
        }
        if not dependencies.ready:
            raise PreflightError(
                "static preflight failed: " + "; ".join(dependencies.blockers)
            )

        stage = "prepare-run-root"
        paths = _prepare_run_root(run_base)
        app_info = _stage_app(app_source, paths)
        controller_hash = _stage_controller(paths)
        report["runRoot"] = str(paths.root)
        report["runId"] = run_id
        report["app"] = app_info
        report["controller"] = {
            "path": str(paths.controller),
            "sha256": controller_hash,
        }
        report["isolation"] = {
            "method": "bubblewrap",
            "outer": "one --unshare-all namespace runs controller, Xvfb and app",
            "filesystem": "tmpfs /; read-only /usr,/etc,/sys; private tmpfs /tmp; run root rw at /kerminal-run",
            "homeHidden": True,
            "privateDisplaySocket": True,
            "networkNamespace": "unshared; loopback TCP self-connect was measured once by the --check probe; the run itself never opens network connections",
            "userNamespace": "unshared; the --check probe compared /proc/self/ns/user identity between the host launcher and the probe sandbox and they differed (see dependencies.sandbox)",
            "pidNamespace": "unshared; --die-with-parent; descendants die with the controller by kernel design (not measured)",
            "hostParentDisplayPresent": dependencies.parent_display_present,
            "hostParentWaylandPresent": dependencies.parent_wayland_present,
            "hostParentDbusPresent": dependencies.parent_dbus_present,
            "launcherLimits": (
                "the host launcher is not inside bubblewrap; it only creates the run root "
                "(never deletes it) and passes an explicit argv"
            ),
            "environmentAllowlist": sorted(_sandbox_environment(run_id)),
        }

        stage = "start-sandbox"
        argv = _sandbox_prefix(dependencies.tools["bwrap"], paths.root)
        for key, value in sorted(_sandbox_environment(run_id).items()):
            argv.extend(["--setenv", key, value])
        argv.extend(["/usr/bin/python3", "-I", SANDBOX_CONTROLLER, "--inner"])
        report["sandboxArgv"] = argv
        log_handle = paths.sandbox_log.open("wb")
        process = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            env=dict(BASE_ENVIRONMENT),
            cwd=str(paths.root),
            start_new_session=True,
        )
        report["sandboxPid"] = process.pid
        try:
            returncode = process.wait(timeout=SANDBOX_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired as error:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            raise AcceptanceError(
                f"sandbox did not finish within {int(SANDBOX_TIMEOUT_SECONDS)}s"
            ) from error
        report["sandboxExitCode"] = returncode
        if log_handle is not None:
            log_handle.close()
            log_handle = None
        stage = "read-report"
        inner: dict[str, object] = {}
        if paths.inner_report_file.is_file():
            inner = json.loads(paths.inner_report_file.read_text(encoding="utf-8"))
            for key in (
                "isolation",
                "display",
                "keyboardLayout",
                "appPid",
                "window",
                "markers",
                "historyA",
                "historyB",
                "historyChecks",
                "screenshots",
                "mechanicalAssertions",
                "evidenceLimits",
                "humanReviewChecklist",
                "cleanup",
                "cleanupErrors",
                "failure",
                "finishedAt",
                "appLogTail",
                "status",
            ):
                if key in inner:
                    report[key] = inner[key]
            # 截图 file 是沙箱内路径；宿主报告必须给出人可打开的 run root 路径。
            if "screenshots" in report:
                report["screenshots"] = _remap_screenshot_paths(
                    report["screenshots"], paths.root
                )
        if returncode != 0 or inner.get("status") == "failed":
            if "failure" not in report:
                report["failure"] = {
                    "stage": "sandbox",
                    "type": "AcceptanceError",
                    "message": f"sandbox exited with {returncode} and wrote no failure report",
                }
            report["status"] = "failed"
            status = 1
        elif inner.get("status") != "mechanical-pass-human-visual-review-required":
            report["status"] = "failed"
            report["failure"] = {
                "stage": "report",
                "type": "AcceptanceError",
                "message": "sandbox report did not reach the expected mechanical-pass status",
            }
            status = 1
        else:
            report["status"] = "mechanical-pass-human-visual-review-required"
            status = 0
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {
            "stage": stage,
            "type": type(error).__name__,
            "message": str(error),
        }
        if paths is not None:
            try:
                tail = paths.sandbox_log.read_text(
                    encoding="utf-8", errors="replace"
                )[-2000:]
                if tail:
                    report["sandboxLogTail"] = tail
            except OSError:
                pass
        status = 1
    finally:
        if log_handle is not None:
            try:
                log_handle.close()
            except OSError:
                pass
        if process is not None and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=PROCESS_KILL_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                report["cleanupErrors"] = list(report.get("cleanupErrors", [])) + [
                    "outer sandbox process did not exit after SIGKILL"
                ]
                report["status"] = "failed"
                status = 1
        report["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
        if paths is not None:
            # 与 run_inner 的证据报告同理：宿主最终报告写不出来属于证据缺失，
            # 即使沙箱判定成功也必须以退出码 1 交付，如实体现证据不可持久化；
            # 报告对象本身仍打印到 stdout，供调用方留存（行为与内层不同：宿主
            # 报告是主输出，内层报告只落盘，所以宿主不必合并失败细节，只降级
            # 退出码并保留 stderr 提示）。
            try:
                _write_report(paths.report_file, report)
            except OSError as error:
                print(f"failed to write evidence report: {error}", file=sys.stderr)
                if status == 0:
                    status = 1
    return status, report


def main(argv: Optional[list[str]] = None) -> int:
    """解析四种模式：计划、静态检查、宿主启动器、沙箱内控制器。"""
    parser = argparse.ArgumentParser(
        description="Linux native acceptance for the right-rail same-tab split switch"
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--check",
        action="store_true",
        help="dependency check + one real bubblewrap --unshare-all probe (no GUI/app/run root; a loopback TCP self-connect must succeed fully or NO-GO)",
    )
    mode.add_argument("--plan", action="store_true", help="print the execution plan")
    mode.add_argument("--run", action="store_true", help="run the native acceptance")
    mode.add_argument("--inner", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--app", type=pathlib.Path, help="prebuilt kerminal-desktop binary")
    parser.add_argument(
        "--run-base",
        type=pathlib.Path,
        default=pathlib.Path("/tmp"),
        help="existing directory for the exclusive run root (default: /tmp)",
    )
    args = parser.parse_args(argv)

    if args.plan:
        _print_json(_plan())
        return 0

    if args.check:
        report = check_dependencies(need_app=args.app)
        _print_json(
            {
                "schemaVersion": SCHEMA_VERSION,
                "mode": "check",
                "sideEffects": "executes one bubblewrap --unshare-all probe (python3 -I inside); no GUI, no app, no run root",
                "ready": report.ready,
                "blockers": report.blockers,
                "tools": report.tools,
                "libraries": report.libraries,
                "versions": report.versions,
                "sandbox": report.sandbox,
                "vulkanIcd": report.vulkan_icd,
                "sqliteVersion": report.sqlite_version,
                "parentDisplayPresent": report.parent_display_present,
                "parentWaylandPresent": report.parent_wayland_present,
                "parentDbusPresent": report.parent_dbus_present,
            }
        )
        return 0 if report.ready else 2

    if args.inner:
        return run_inner()

    if not args.app:
        parser.error("--run requires --app with a prebuilt kerminal-desktop binary")
    status, report = launch_native(args.app, args.run_base)
    _print_json(report)
    # run root（含全部截图证据）一律保留供人工复核；删除须人工在文件系统层面进行，
    # 脚本不提供任何自动或半自动删除路径。
    return status


if __name__ == "__main__":
    sys.exit(main())
