#!/usr/bin/env python3
"""right-rail-split-linux-native.py 的纯单元测试（同目录专用）。

只测纯函数与静态守卫，不运行 --run、--inner、GUI、Xvfb，也不触碰任何真实
数据；全部临时文件都放在 tempfile 提供的独立目录，测试结束后删除。真实
bwrap 只在 Linux 主机的 --check 中执行，不属于纯单测范围；但探针"实际会被
执行的内嵌 Python 代码常量"必须用 ast.parse 直接解析——历史版本把 try/except
写成分号行导致探针必然 SyntaxError，而 mock _run 的单测完全发现不了，因此
这里把"解析被执行的代码"作为回归。回环自连逻辑额外在本机 127.0.0.1 上用
真实 socket 执行一次（不依赖 bwrap、不触达外部网络）。

运行：
    python3 right-rail-split-linux-native.test.py

@author kongweiguang
"""

from __future__ import annotations

import ast
import importlib.util
import json
import os
import pathlib
import socket
import struct
import sys
import tempfile
import threading
import types
import zlib

HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE / "right-rail-split-linux-native.py"

failures: list[str] = []
checks = 0


def check(condition: bool, message: str) -> None:
    """记录断言结果；失败继续执行，最后统一汇总，方便一次看到全部问题。"""
    global checks
    checks += 1
    if not condition:
        failures.append(message)


def load_script():
    """以独立模块加载脚本；脚本在非 Linux 平台也能导入是纯单测的前提。

    dataclass 解析要求模块已注册进 sys.modules，因此先注册再执行。
    """
    spec = importlib.util.spec_from_file_location("rr_split_script", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load script module")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def make_png(
    width: int,
    height: int,
    pixel_fn,
    *,
    color_type: int = 2,
    trns: bytes | None = None,
    palette: bytes | None = None,
) -> bytes:
    """构造 8-bit 非交错 PNG；color_type 2=RGB、3=调色板、6=RGBA。

    trns/palette 允许构造带透明度的真实 PNG 字节，用于验证 tRNS 语义而不是
    伪造的内存对象；pixel_fn(x, y) 返回该格式的原始字节序列。
    """
    bpp = {2: 3, 3: 1, 6: 4}[color_type]
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter type 0（None），已知数据可直接还原
        for x in range(width):
            raw.extend(pixel_fn(x, y))
    compressed = zlib.compress(bytes(raw))

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + kind
            + payload
            + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)
        )

    data = b"\x89PNG\r\n\x1a\n" + chunk(
        b"IHDR", struct.pack(">IIBBBBB", width, height, 8, color_type, 0, 0, 0)
    )
    if palette is not None:
        data += chunk(b"PLTE", palette)
    if trns is not None:
        data += chunk(b"tRNS", trns)
    data += chunk(b"IDAT", compressed) + chunk(b"IEND", b"")
    return data


def test_probe_program_is_real_code(module) -> None:
    """回归：ast.parse 必须能解析"实际会被执行的"探针代码常量。

    历史缺陷是内嵌代码以分号拼接 try/except，运行时必然 SyntaxError，而旧
    单测 mock 了 _run，从不执行这段文本，因此缺陷逃逸。这里直接解析常量本身，
    而不是解析整份脚本源码。
    """
    try:
        ast.parse(module.SANDBOX_PROBE_PROGRAM)
    except SyntaxError as error:
        check(False, f"SANDBOX_PROBE_PROGRAM must be executable Python: {error}")
    else:
        check(True, "probe program parses")
    # 程序必须真的包含回环自连的关键调用，防止未来退化成只 bind 的弱探针。
    program = module.SANDBOX_PROBE_PROGRAM
    for required in (
        "socket.socket(",
        ".bind((",
        ".listen(",
        ".connect(",
        ".accept(",
        '"usable"',
    ):
        check(
            required in program,
            f"probe program must actually self-connect on loopback (missing {required!r})",
        )
    # userns 判定必须是命名空间身份测量，不得退回 uid_map 形状猜测（历史缺陷：
    # uid_map 每行 3 字段，旧逻辑按 split 后 4 字段判定恒假，userns 恒为真）。
    check(
        "ns/user" in program and "st_ino" in program,
        "probe must report its /proc/self/ns/user identity, not guess from uid_map",
    )
    check(
        "uid_map" not in program,
        "probe must not use uid_map shape guessing for the userns verdict",
    )
    # 整份脚本源码也必须保持可解析，这是 --check/--run 能启动的第一前提。
    try:
        ast.parse(SCRIPT.read_text(encoding="utf-8"))
    except SyntaxError as error:
        check(False, f"script source must stay parseable: {error}")


def test_probe_parses_probe_output(module) -> None:
    """探针输出解析：合法标记、损坏 JSON、缺标记与 sandboxOk=False 都要失败关闭。

    usernsIdentity 为 [dev, ino] 列表时解析成二元组；缺失、畸形或布尔值按
    未测（None）处理并保留细节，由上层与宿主身份比较后判定。
    """
    ok_line = "SANDBOX_PROBE "
    ok_line += json.dumps(
        {
            "sandboxOk": True,
            "usernsIdentity": [37, 4026532718],
            "loopback": "usable",
            "loopbackDetail": "",
        },
        sort_keys=True,
    )
    done, identity, usable, detail = module._parse_sandbox_probe_output(ok_line + "\n")
    check(
        done and identity == (37, 4026532718) and usable is True and detail == "",
        f"valid probe output must parse identity and usable: {done} {identity} {usable} {detail!r}",
    )
    done, _, usable, detail = module._parse_sandbox_probe_output(
        "SANDBOX_PROBE {broken json\n"
    )
    check(not done and usable is None and "unparseable" in detail,
          "corrupt probe JSON must fail closed")
    done, _, _, detail = module._parse_sandbox_probe_output("no marker here\n")
    check(not done and "no sandbox probe marker" in detail,
          "missing marker must fail closed")
    fail_line = "SANDBOX_PROBE " + json.dumps(
        {"sandboxOk": True, "usernsIdentity": [1, 2],
         "loopback": "unavailable:101", "loopbackDetail": "Network is unreachable"},
        sort_keys=True,
    )
    done, _, usable, detail = module._parse_sandbox_probe_output(fail_line)
    check(done and usable is False and "unavailable:101" in detail,
          "loopback failure must be reported as not usable, not dropped")
    # 身份缺失/畸形必须解析为 None（未测），不得猜测或当成通过。
    for label, payload in (
        ("missing identity", {"sandboxOk": True, "loopback": "usable"}),
        (
            "malformed identity",
            {"sandboxOk": True, "usernsIdentity": "4026532718", "loopback": "usable"},
        ),
        (
            "wrong arity",
            {"sandboxOk": True, "usernsIdentity": [1, 2, 3], "loopback": "usable"},
        ),
        (
            "bool elements",
            {"sandboxOk": True, "usernsIdentity": [True, 2], "loopback": "usable"},
        ),
    ):
        line = "SANDBOX_PROBE " + json.dumps(payload, sort_keys=True)
        done, identity, usable, detail = module._parse_sandbox_probe_output(line)
        check(
            done and identity is None and "userns identity missing" in detail,
            f"{label} must parse as unmeasured identity with detail, got: {identity} / {detail!r}",
        )


def test_sandbox_probe_no_go_semantics(module) -> None:
    """--check 的 NO-GO 语义：身份不同才通过；相同/未测/畸形/缺标记全部阻塞。

    宿主身份由 _read_userns_identity 打桩注入，覆盖等身份/不同身份/宿主或探针
    侧缺失四种判定；回环不可用仍单独阻塞。userNamespaceMeasured 语义为
    "判明不同"才 True，未测为 None。
    """
    probe_identity = [37, 4026532718]
    host_identity = (37, 4026532718)
    host_other = (37, 1)

    def payload(identity):
        data = {"sandboxOk": True, "loopback": "usable", "loopbackDetail": ""}
        if identity is not None:
            data["usernsIdentity"] = identity
        return "SANDBOX_PROBE " + json.dumps(data, sort_keys=True)

    class FakeResult:
        def __init__(self, stdout: str) -> None:
            self.returncode = 0
            self.stdout = stdout

    original_run = module._run
    original_identity_reader = module._read_userns_identity
    try:
        # 场景 A：探针身份与宿主不同 → 不阻塞，Measured=True，身份进报告。
        report = module.DependencyReport()
        module._run = lambda argv, **kwargs: FakeResult(payload(probe_identity))
        module._read_userns_identity = lambda: host_other
        module._sandbox_probe("/usr/bin/bwrap", report)
        check(
            not report.blockers
            and report.sandbox.get("userNamespaceMeasured") is True
            and report.sandbox.get("userNamespaceIdentity", {}).get("probe") == probe_identity,
            f"different identities must pass and record both sides: {report.sandbox.get('userNamespaceMeasured')} / {report.blockers}",
        )
        # 场景 B：身份相同（bwrap 没建新 userns）→ 必须阻塞且 Measured=False。
        report_same = module.DependencyReport()
        module._run = lambda argv, **kwargs: FakeResult(payload(probe_identity))
        module._read_userns_identity = lambda: host_identity
        module._sandbox_probe("/usr/bin/bwrap", report_same)
        check(
            report_same.blockers
            and report_same.sandbox.get("userNamespaceMeasured") is False,
            "identical host/probe identity must block (no new user namespace)",
        )
        check(
            any("did not create a new user namespace" in blocker for blocker in report_same.blockers),
            f"equal identity must name the namespace failure: {report_same.blockers}",
        )
        # 场景 C：宿主侧身份读不到 → 未测必须失败关闭，不得从映射形状猜。
        report_nohost = module.DependencyReport()
        module._run = lambda argv, **kwargs: FakeResult(payload(probe_identity))
        module._read_userns_identity = lambda: None
        module._sandbox_probe("/usr/bin/bwrap", report_nohost)
        check(
            report_nohost.blockers
            and report_nohost.sandbox.get("userNamespaceMeasured") is None,
            "unreadable host identity must fail closed as unmeasured (None)",
        )
        # 场景 D：探针身份缺失/畸形 → 未测必须失败关闭。
        for label, bad in (
            ("missing", None),
            ("malformed", "4026532718"),
            ("wrong-arity", [1, 2, 3]),
        ):
            report_bad = module.DependencyReport()
            module._run = lambda argv, **kwargs: FakeResult(payload(bad))
            module._read_userns_identity = lambda: host_other
            module._sandbox_probe("/usr/bin/bwrap", report_bad)
            check(
                report_bad.blockers
                and report_bad.sandbox.get("userNamespaceMeasured") is None,
                f"probe identity {label} must fail closed as unmeasured",
            )
        # 回环不可用（身份不同）仍必须单独 NO-GO。
        fail_payload = "SANDBOX_PROBE " + json.dumps(
            {"sandboxOk": True, "usernsIdentity": probe_identity,
             "loopback": "unavailable:101", "loopbackDetail": "Network is unreachable"},
            sort_keys=True,
        )
        report_loop = module.DependencyReport()
        module._run = lambda argv, **kwargs: FakeResult(fail_payload)
        module._read_userns_identity = lambda: host_other
        module._sandbox_probe("/usr/bin/bwrap", report_loop)
        check(
            report_loop.sandbox.get("loopbackUsable") is False,
            "unusable loopback must not be reported as usable",
        )
        check(
            any("loopback" in blocker for blocker in report_loop.blockers),
            "unproven loopback must produce a NO-GO blocker",
        )
        check(
            report_loop.sandbox.get("networkExercised") == "loopback-only self-connect inside the probe namespace",
            "sandbox report must state the probe only exercised loopback self-connect",
        )
        # 探针 argv 必须真的把内嵌程序传给 python3 -c（而不是遗留的损坏拼接）。
        report_argv = module.DependencyReport()
        module._run = lambda argv, **kwargs: FakeResult(payload(probe_identity))
        module._sandbox_probe("/usr/bin/bwrap", report_argv)
        ok_argv = report_argv.sandbox.get("argv", [])
        check(
            ok_argv[-4:] == ["/usr/bin/python3", "-I", "-c", module.SANDBOX_PROBE_PROGRAM],
            "probe argv must execute the SANDBOX_PROBE_PROGRAM constant",
        )
        # 输出缺标记（如探针崩溃）也必须失败关闭。
        report_broken = module.DependencyReport()
        module._run = lambda argv, **kwargs: FakeResult("Traceback (most recent call last)")
        module._sandbox_probe("/usr/bin/bwrap", report_broken)
        check(
            report_broken.sandbox.get("probeCompleted") is False
            and report_broken.blockers,
            "probe crash output must fail closed with a blocker",
        )
    finally:
        module._run = original_run
        module._read_userns_identity = original_identity_reader


def test_loopback_self_connect_real_socket(module) -> None:
    """在本机回环上真实执行探针的连接语义（不依赖 bwrap、不触外部网络）。

    探针程序本体依赖沙箱内的断言（/home 不可见等），无法在宿主直接执行；
    因此这里用等价的 socket 序列验证"bind+listen+connect+accept+echo 全链路
    成功才判 usable"的逻辑，并验证 accept 限时存在。
    """
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    client = None
    accepted = None
    usable = False
    try:
        listener.bind(("127.0.0.1", 0))
        listener.settimeout(5.0)
        listener.listen(1)
        client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        client.settimeout(5.0)
        client.connect(listener.getsockname())
        client.sendall(b"kerminal-loopback-probe")
        accepted, _ = listener.accept()
        accepted.settimeout(5.0)
        usable = accepted.recv(64) == b"kerminal-loopback-probe"
    except OSError as error:
        check(False, f"loopback self-connect must work on this host: {error}")
    finally:
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
    check(usable, "loopback echo must round-trip through bind/listen/connect/accept")


def test_pixel_gates(module) -> None:
    """像素门禁的正反例：全黑、纯色、均匀、透明与真实内容帧。"""
    size = (64, 48)
    solid_black = make_png(*size, lambda x, y: (0, 0, 0))
    solid_gray = make_png(*size, lambda x, y: (128, 128, 128))
    # 水平渐变：RGB 种类多但亮度跨度小，验证亮度门禁独立于颜色门禁生效。
    gradient = make_png(*size, lambda x, y: (100 + x % 5, 100, 100))
    content = make_png(
        *size, lambda x, y: (x * 4 % 256, y * 5 % 256, (x + y) * 3 % 256)
    )
    for label, data in (
        ("solid-black", solid_black),
        ("solid-gray", solid_gray),
        ("flat-gradient", gradient),
    ):
        try:
            width, height, pixels = module._decode_png_pixels(data)
            stats = module._pixel_stats(pixels, 0, 0, width, height)
            module._gate_pixel_stats(stats, label)
        except module.AcceptanceError as error:
            check(
                PIXEL_MIN in str(error) or "blank" in str(error) or "flat" in str(error),
                f"{label} should be rejected by the anti-blank gate, got: {error}",
            )
        else:
            check(False, f"{label} frame must fail the anti-blank gate")
    try:
        width, height, pixels = module._decode_png_pixels(content)
        stats = module._pixel_stats(pixels, 0, 0, width, height)
        module._gate_pixel_stats(stats, "content")
    except module.AcceptanceError as error:
        check(False, f"content-rich frame must pass the gate, got: {error}")
    else:
        check(
            stats.distinct_rgb >= module.PIXEL_MIN_DISTINCT_RGB,
            "content frame stats should reflect many distinct RGB values",
        )


PIXEL_MIN = "distinct RGB"


def test_transparent_rgba_frame_rejected(module) -> None:
    """透明帧门禁：RGBA 大面积 alpha=0 必须被拒绝（scrot 可能产出 RGBA）。"""
    width, height = 32, 32
    data = make_png(
        width,
        height,
        lambda x, y: (10, 20, 30, 0 if y < height // 2 else 255),
        color_type=6,
    )
    _, _, pixels = module._decode_png_pixels(data)
    stats = module._pixel_stats(pixels, 0, 0, width, height)
    try:
        module._gate_pixel_stats(stats, "mostly-transparent")
    except module.AcceptanceError as error:
        check("transparent" in str(error), f"expected transparency rejection, got: {error}")
    else:
        check(False, "mostly transparent frame must fail the gate")


def test_semitransparent_frame_rejected(module) -> None:
    """半透明帧门禁：alpha=1..254 的像素不算不透明，"几乎不可见"的内容层必须拒绝。"""
    width, height = 32, 32
    # 整帧 alpha=3、RGB 花样繁多：若残色被计入颜色/亮度证据，门禁就会被绕过。
    data = make_png(
        width,
        height,
        lambda x, y: (x * 7 % 256, y * 11 % 256, (x + y) * 5 % 256, 3),
        color_type=6,
    )
    _, _, pixels = module._decode_png_pixels(data)
    stats = module._pixel_stats(pixels, 0, 0, width, height)
    check(stats.opaque_samples == 0, "alpha=3 pixels must not count as opaque")
    check(
        stats.distinct_rgb == 0 and stats.luma_range == 0,
        "semitransparent residuals must not be folded into color/luma evidence",
    )
    try:
        module._gate_pixel_stats(stats, "semitransparent")
    except module.AcceptanceError as error:
        check("transparent" in str(error), f"expected transparency rejection, got: {error}")
    else:
        check(False, "fully semi-transparent frame must fail the gate")


def test_palette_trns_transparency(module) -> None:
    """调色板 tRNS：全透明调色板帧不得按不透明绕过门禁；不透明调色板可正常统计。"""
    width, height = 32, 32
    # 8 个全部不同的调色板条目（RGB 多样性足以通过颜色门禁），但 tRNS 全为 0：
    # 若解码把透明条目当不透明，这帧会伪造出足够的颜色/亮度跨度。
    palette = bytes(bytearray(
        channel
        for index in range(8)
        for channel in (index * 30 % 256, index * 17 % 256, index * 53 % 256)
    ))
    transparent = make_png(
        width,
        height,
        lambda x, y: bytes([(y * width + x) % 8]),
        color_type=3,
        palette=palette,
        trns=b"\x00" * 8,
    )
    _, _, pixels = module._decode_png_pixels(transparent)
    stats = module._pixel_stats(pixels, 0, 0, width, height)
    check(stats.opaque_samples == 0, "palette entries with tRNS alpha=0 must stay transparent")
    try:
        module._gate_pixel_stats(stats, "palette-transparent")
    except module.AcceptanceError as error:
        check("transparent" in str(error), f"expected transparency rejection, got: {error}")
    else:
        check(False, "fully transparent palette frame must fail the gate")
    # 对照组：同一调色板不透明（无 tRNS）应通过透明门禁并保留全部颜色。
    # 索引按 (x+y) 变化保证 8 个条目都会出现在画面里。
    opaque = make_png(
        width,
        height,
        lambda x, y: bytes([(x + y) % 8]),
        color_type=3,
        palette=palette,
    )
    _, _, pixels = module._decode_png_pixels(opaque)
    stats = module._pixel_stats(pixels, 0, 0, width, height)
    check(stats.opaque_samples == width * height, "palette without tRNS must be opaque")
    check(stats.distinct_rgb == 8, "all palette colors must be counted")


def test_truecolor_trns_rejected(module) -> None:
    """真彩 tRNS 是颜色键透明不是逐像素 alpha，无法安全近似，必须明确拒绝。"""
    width, height = 16, 16
    data = make_png(width, height, lambda x, y: (1, 2, 3), trns=b"\x00\x01\x02")
    try:
        module._decode_png_pixels(data)
    except module.AcceptanceError as error:
        check("tRNS" in str(error), f"truecolor tRNS must be rejected explicitly: {error}")
    else:
        check(False, "truecolor tRNS frame must fail closed")


def test_malformed_png_fail_closed(module) -> None:
    """畸形 PNG 必须失败关闭：截断扫描线、伪造块长度、超预算解压都不得伪成功。"""
    good = make_png(8, 8, lambda x, y: (1, 2, 3))
    # 在 IDAT 数据中间截断：块声明的长度大于剩余字节，解码必须报错。
    idat_index = good.find(b"IDAT")
    truncated = good[: idat_index + 4 + 4]  # 只留 IDAT 前 4 字节实际数据
    try:
        module._decode_png_pixels(truncated)
    except module.AcceptanceError:
        check(True, "truncated PNG must fail closed")
    except Exception as error:
        check(False, f"truncated PNG must raise AcceptanceError, got {type(error).__name__}")
    else:
        check(False, "truncated PNG must not decode")
    # 伪造超大块长度：不能按伪造长度切片或撑大内存。
    forged = bytearray(make_png(8, 8, lambda x, y: (1, 2, 3)))
    forged[8:12] = struct.pack(">I", 0xFFFFFFF0)
    try:
        module._decode_png_pixels(bytes(forged))
    except module.AcceptanceError as error:
        check("exceeds" in str(error) or "truncated" in str(error),
              f"forged chunk length must be rejected: {error}")
    else:
        check(False, "forged chunk length must not decode")
    # 声称超大数据的炸弹：高压缩比 IDAT 解压超预算必须拒绝。
    bomb_payload = zlib.compress(b"\x00" * (module.PNG_MAX_RAW_BYTES + 4096))
    bomb = bytearray(make_png(module.PNG_MAX_DIMENSION, 4, lambda x, y: (1, 2, 3)))
    idat_index = bomb.find(b"IDAT")
    original_length = struct.unpack(">I", bomb[idat_index - 4 : idat_index])[0]
    bomb[idat_index - 4 : idat_index] = struct.pack(">I", len(bomb_payload))
    bomb[idat_index + 4 : idat_index + 4 + original_length] = bomb_payload
    try:
        module._decode_png_pixels(bytes(bomb))
    except module.AcceptanceError as error:
        check("exceeds" in str(error), f"decompression bomb must be rejected: {error}")
    except Exception as error:
        check(False, f"bomb must raise AcceptanceError, got {type(error).__name__}")
    else:
        check(False, "decompression bomb must not decode")


def test_region_fraction(module) -> None:
    """右栏假定区域分数必须落在窗口内，且区域统计按几何切分。"""
    width, height = 160, 100
    pixels = [
        [(x, y, 60, 255) for x in range(width)] for y in range(height)
    ]
    fx0, fy0, fx1, fy1 = module.RIGHT_RAIL_REGION_FRACTION
    stats = module._pixel_stats(
        pixels, int(width * fx0), int(height * fy0), int(width * fx1), int(height * fy1)
    )
    check(
        0.0 <= fx0 < fx1 <= 1.0 and 0.0 <= fy0 < fy1 <= 1.0,
        "right rail region fraction must be a valid sub-rectangle",
    )
    check(
        stats.total_samples == int(width * (fx1 - fx0)) * int(height * (fy1 - fy0)),
        "region stats should cover exactly the fractional area",
    )


def test_verify_history_pure(module) -> None:
    """身份核对纯函数：同 tab 不同 pane/session 通过；缺失或相等必须失败。"""
    base = {
        "command": "a",
        "source": "user",
        "target": "local",
        "pane_id": "p1",
        "session_id": "s1",
        "tab_id": "t1",
        "rowid": "10",
    }
    other = dict(base, command="b", pane_id="p2", session_id="s2", rowid="11")
    result = module._verify_history(base, other)
    check(all(result.values()), f"valid pair must pass all checks: {result}")
    same_pane = dict(other, pane_id="p1")
    check(
        not module._verify_history(base, same_pane)["distinctPane"],
        "equal pane_id must fail distinctPane",
    )
    same_tab = dict(other, tab_id="t2")
    check(
        not module._verify_history(base, same_tab)["sameTab"],
        "different tab_id must fail sameTab",
    )
    check(
        not module._verify_history(base, dict(other, rowid="5"))["aBeforeB"],
        "B before A must fail aBeforeB",
    )


def test_sandbox_identity_parsing(module) -> None:
    """--inner 身份解析：pid-1 bwrap + run id 通过；单层 NSpid 不再是错误。

    procfs 由最内层 PID namespace 挂载时 NSpid 只显示单层（已在 Linux 5.15 +
    bwrap 0.6.1 实测），因此合法沙箱形态"NSpid: 2"必须通过而不是被拒绝。
    """
    pid1_comm = "bwrap\n"
    pid1_cmdline = (
        "bwrap\0--unshare-all\0--die-with-parent\0--cap-drop\0ALL\0"
        "--setenv\0KERMINAL_LINUX_ACCEPTANCE_RUN_ID\0cafe1234\0"
        "/usr/bin/python3\0-I\0/kerminal-run/controller.py\0--inner\0"
    )
    status_single = "NSpid:\t2\nCapEff:\t0000000000000000\n"
    identity = module._parse_sandbox_identity(pid1_comm, pid1_cmdline, status_single, "cafe1234")
    check(
        identity.get("unshareAllProven") is True and identity.get("capEffZero") is True,
        f"legal sandbox identity must pass: {identity}",
    )
    check(
        identity.get("runIdMatchedOnPid1Cmdline") is True,
        "run id on pid-1 cmdline must be recorded as proven",
    )
    # 宿主形态：pid 1 不是 bwrap，必须失败关闭。
    for label, args in (
        ("host pid-1", ("systemd\n", pid1_cmdline, status_single, "cafe1234")),
        ("missing unshare", ("bwrap\n", "bwrap\0--die-with-parent\0--setenv\0KERMINAL_LINUX_ACCEPTANCE_RUN_ID\0cafe1234\0", status_single, "cafe1234")),
        ("run id mismatch", (pid1_comm, pid1_cmdline, status_single, "deadbeef")),
        ("empty run id", (pid1_comm, pid1_cmdline, status_single, "")),
        ("cap missing", (pid1_comm, pid1_cmdline, "NSpid:\t2\n", "cafe1234")),
        ("cap nonzero", (pid1_comm, pid1_cmdline, "NSpid:\t2\nCapEff:\t00000000000001ff\n", "cafe1234")),
        ("cap unparseable", (pid1_comm, pid1_cmdline, "NSpid:\t2\nCapEff:\tnot-hex\n", "cafe1234")),
    ):
        try:
            module._parse_sandbox_identity(*args)
        except module.PreflightError:
            check(True, f"{label} must fail closed")
        else:
            check(False, f"{label} identity must be rejected")


def test_screenshot_remap_pure(module) -> None:
    """宿主截图路径映射：改写沙箱前缀、标记存在性、保留异常条目、阻止逃逸。"""
    with tempfile.TemporaryDirectory(prefix="rr-test-remap-") as temp:
        host_root = pathlib.Path(temp)
        (host_root / "evidence").mkdir()
        (host_root / "evidence" / "01.png").write_bytes(b"png")
        entries = [
            {"file": "/kerminal-run/evidence/01.png", "sha256": "a"},
            {"file": "/kerminal-run/evidence/missing.png", "sha256": "b"},
            {"file": "unexpected", "sha256": "c"},
            {"sha256": "d"},
            "not-a-dict",
            {"file": "/kerminal-run/../../etc/passwd", "sha256": "e"},
        ]
        remapped = module._remap_screenshot_paths(entries, host_root)
        check(len(remapped) == 6, "all entries must survive remapping")
        check(
            remapped[0]["fileHost"] == str(host_root.resolve() / "evidence" / "01.png")
            and remapped[0]["fileHostExists"] is True
            and remapped[0]["remapStatus"] == "ok",
            "existing host evidence must map with remapStatus ok",
        )
        check(
            remapped[1]["fileHostExists"] is False
            and remapped[1]["remapStatus"] == "host-file-missing",
            "missing host evidence must be flagged, not silently dropped",
        )
        check(
            remapped[2]["remapStatus"] == "unmapped" and "fileHost" not in remapped[2],
            "non-sandbox paths must not be rewritten",
        )
        check(
            remapped[3]["remapStatus"] == "malformed-entry",
            "entry without file must be marked malformed",
        )
        check(
            remapped[4]["remapStatus"] == "malformed-entry"
            and remapped[4]["originalEntry"] == "not-a-dict",
            "non-dict entry must survive as malformed, not be dropped",
        )
        check(
            remapped[5]["remapStatus"] == "error"
            and "fileHost" not in remapped[5],
            "escaping path must be an error without an openable host path",
        )
        # 非列表整体也必须显式报错而不是返回空列表假装成功。
        non_list = module._remap_screenshot_paths({"nope": True}, host_root)
        check(
            len(non_list) == 1 and non_list[0]["remapStatus"] == "error",
            "non-list screenshots value must be reported as an error",
        )


def test_no_auto_delete_path(module) -> None:
    """证据保留：脚本不得包含 run root 删除入口，--remove-run-root 必须消失。"""
    source = SCRIPT.read_text(encoding="utf-8")
    check(
        "remove-run-root" not in source,
        "--remove-run-root must not exist; evidence is never auto-deleted",
    )
    check(
        "rmtree" not in source,
        "no rmtree call may remain in the script",
    )
    tree = ast.parse(source)
    functions = {node.name for node in ast.walk(tree) if isinstance(node, ast.FunctionDef)}
    check(
        "_cleanup_run_root" not in functions,
        "run-root deletion function must be removed",
    )


def test_plan_contract(module) -> None:
    """计划契约：无副作用、如实声明 --check 的沙箱探针副作用与证据保留。"""
    plan = module._plan()
    check(plan["executionRequiresApproval"] is True, "plan must require approval")
    cleanup_text = json.dumps(plan["cleanup"], ensure_ascii=False)
    check(
        "always retained" in cleanup_text,
        "plan must state the run root is always retained",
    )
    check(
        "creates the run root" in plan["isolation"]["limitations"][1],
        "plan must not claim the host launcher deletes the run root",
    )
    claims_text = json.dumps(plan["claims"], ensure_ascii=False)
    check(
        "humanReviewRequired" in claims_text and plan["claims"]["humanReviewRequired"] is True,
        "plan must keep humanReviewRequired",
    )
    gate_text = json.dumps(plan.get("screenshotGate", {}), ensure_ascii=False)
    check(
        "anti-blank" in gate_text and "does not prove right-rail presence" in gate_text,
        "plan must describe the pixel gate scope honestly",
    )
    network_text = plan["isolation"]["networkNamespace"]
    check(
        "NO-GO" in network_text and "bind-only" in network_text,
        "plan must state the loopback self-connect NO-GO rule and reject bind-only",
    )
    pid_text = plan["isolation"]["pidNamespace"]
    check(
        "not measured" in pid_text and "pid-1" in pid_text,
        "plan must be honest about PID-namespace cleanup and the --inner pid-1 proof",
    )


def test_screenshot_call_sites_bind_arguments_correctly(module) -> None:
    """回归：run_inner 的六个截图调用不得把 SANDBOX_ROOT 与文件名成对交换。

    历史缺陷：_screenshot 曾有第四个 sandbox_root 形参，run_inner 的调用写成
    _screenshot(display, paths, SANDBOX_ROOT, "01-....png")，名字与根目录互换。
    pathlib 对绝对路径分量会整体重置，paths.evidence / "/kerminal-run" 实际指向
    /kerminal-run，截图证据落到错误位置。这里按 AST 绑定实际调用参数（而不是
    只测 _screenshot 函数本身），并要求第三参是文件名字面量、形参列表不含
    sandbox_root——修复前的代码在两组断言上都会失败。
    """
    source = SCRIPT.read_text(encoding="utf-8")
    tree = ast.parse(source)
    func = next(
        node for node in ast.walk(tree)
        if isinstance(node, ast.FunctionDef) and node.name == "_screenshot"
    )
    params = [arg.arg for arg in func.args.posonlyargs + func.args.args]
    check(
        params == ["display", "paths", "name"],
        f"_screenshot signature must be (display, paths, name), got: {params}",
    )
    run_inner = next(
        node for node in ast.walk(tree)
        if isinstance(node, ast.FunctionDef) and node.name == "run_inner"
    )
    calls = [
        node for node in ast.walk(run_inner)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "_screenshot"
    ]
    check(len(calls) == 6, f"run_inner must contain six _screenshot calls, got {len(calls)}")
    expected_names = {
        "01-window-ready.png",
        "02-before-logs.png",
        "03-after-logs.png",
        "10-pane-a.png",
        "20-pane-b.png",
        "30-pane-a-return.png",
    }
    seen_names = set()
    for index, call in enumerate(calls):
        check(
            len(call.args) == 3,
            f"screenshot call #{index} must pass exactly 3 args, got {len(call.args)}",
        )
        if len(call.args) != 3:
            continue
        second = call.args[2]
        check(
            isinstance(second, ast.Constant) and isinstance(second.value, str),
            f"screenshot call #{index} must pass a filename literal as arg 3, got: {ast.dump(second)}",
        )
        if isinstance(second, ast.Constant) and isinstance(second.value, str):
            seen_names.add(second.value)
    check(
        seen_names == expected_names,
        f"screenshot filenames must cover all six evidence names, got: {sorted(seen_names)}",
    )
    check(
        "SANDBOX_ROOT" not in source.split("def _screenshot")[1].split("def ")[0]
        if "def _screenshot" in source else False,
        "_screenshot body must not reference SANDBOX_ROOT",
    )
    # 行为层兜底：真实调用路径上，路径必须落在 evidence 目录而不是被绝对路径
    # 重置到沙箱根。用假 PNG 触发 PNG 门禁失败，但失败前 scrot 目标已被记录。
    with tempfile.TemporaryDirectory(prefix="rr-test-shot-") as temp:
        root = pathlib.Path(temp)
        evidence = root / "evidence"
        evidence.mkdir()
        paths = module.RunPaths(
            root=root,
            home=root / "home",
            config=root / "config",
            evidence=evidence,
            app=root / "app",
            shell=root / "bin" / "bash",
            xauthority=root / "xauthority",
            controller=root / "controller.py",
            xvfb_log=root / "xvfb.log",
            app_log=root / "app.log",
            sandbox_log=root / "sandbox.log",
            inner_report_file=root / "evidence" / "inner-report.json",
            report_file=root / "evidence" / "report.json",
        )
        scrot_calls: list[list[str]] = []

        def fake_scrot(argv, **kwargs):
            scrot_calls.append(argv)
            target = pathlib.Path(argv[argv.index("-o") + 1])
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"not a png")
            return types.SimpleNamespace(returncode=0)

        original_run = module._run
        try:
            module._run = fake_scrot
            try:
                module._screenshot(":99", paths, "01-window-ready.png")
            except module.AcceptanceError as error:
                check(
                    "not a valid PNG" in str(error),
                    f"fake scrot output must fail the PNG gate, got: {error}",
                )
            except Exception as error:
                check(False, f"_screenshot must fail typed, got {type(error).__name__}: {error}")
            else:
                check(False, "fake PNG must not pass the gate")
            check(
                len(scrot_calls) == 1
                and pathlib.Path(scrot_calls[0][scrot_calls[0].index("-o") + 1]).parent == evidence,
                f"scrot target must live under the evidence directory, argv: {scrot_calls}",
            )
        finally:
            module._run = original_run


def test_short_ihdr_chunk_fails_typed(module) -> None:
    """回归：长度不足 13 字节的 IHDR 必须抛 AcceptanceError 而不是裸 IndexError。"""
    good = make_png(8, 8, lambda x, y: (1, 2, 3))
    # 正常 IHDR 块长是 13；把长度字段改成 8 后该块只剩 width+height，
    # 解码器在 chunk[9]/chunk[12] 处本会越界，现在必须被长度校验拦下。
    # 文件总长保持合法（>= 57 字节），确保触发的是 IHDR 分支而不是入口长度门。
    forged = bytearray(good)
    forged[8:12] = struct.pack(">I", 8)
    try:
        module._decode_png_pixels(bytes(forged))
    except module.AcceptanceError as error:
        check("IHDR" in str(error), f"short IHDR must be rejected explicitly: {error}")
    except Exception as error:
        check(
            False,
            f"short IHDR must raise AcceptanceError, got {type(error).__name__}: {error}",
        )
    else:
        check(False, "short IHDR chunk must not decode")


def test_run_inner_fails_closed_without_run_root(module) -> None:
    """回归：SANDBOX_ROOT 不存在时 run_inner 必须干净退出 1，不得裸 traceback。

    _prepare_existing_paths 在主 try 之前执行，失败曾是未捕获的 CleanupError；
    修复后控制器打印原因并以退出码 1 结束，宿主按"sandbox 无报告退出"失败关闭。
    沙箱根指向本测试私有 TemporaryDirectory 下一个刻意未创建的子目录：目录由
    测试独占创建，不引用全局固定路径，避免他人恰好建了同名可用目录时读到他人
    内容甚至写报告。
    """
    original_root = module.SANDBOX_ROOT
    original_env = os.environ.get(module.RUN_ID_ENV)
    try:
        with tempfile.TemporaryDirectory(prefix="rr-inner-gone-") as temp:
            module.SANDBOX_ROOT = str(pathlib.Path(temp) / "missing-run-root")
            check(
                not pathlib.Path(module.SANDBOX_ROOT).exists(),
                "the simulated sandbox root must not exist inside the private temp dir",
            )
            os.environ[module.RUN_ID_ENV] = "test-run-id"
            code = module.run_inner()
    finally:
        # 环境与模块状态必须在异常路径下也恢复，否则污染后续用例。
        module.SANDBOX_ROOT = original_root
        if original_env is None:
            os.environ.pop(module.RUN_ID_ENV, None)
        else:
            os.environ[module.RUN_ID_ENV] = original_env
    check(code == 1, f"missing run root must exit 1, got: {code}")


def test_run_inner_exit_code_matches_cleanup_report(module) -> None:
    """回归：主体成功但清理失败时，退出码必须与失败报告一致降级为 1。

    历史缺陷：finally 中 cleanup 失败把 report.status 改成 failed 却保留
    status=0，内层返回 0 同时携带失败报告，证据自相矛盾（宿主按报告失败关闭，
    但内层退出码不保真）。用隔离注入驱动真实 run_inner 控制流：沙箱身份与
    /proc 读取打桩、Xvfb/app/X11 打桩为成功路径，X11 close 注入异常模拟清理
    失败，验证返回 1、报告 status=failed、stage=cleanup；对照组验证成功路径
    返回 0 且无 cleanup 错误，以及先前主体失败时报告保真不被 cleanup 覆盖。
    """
    with tempfile.TemporaryDirectory(prefix="rr-inner-cleanup-") as temp:
        root = pathlib.Path(temp)
        (root / "evidence").mkdir()
        paths = module.RunPaths(
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
            report_file=root / "evidence" / "report.json",
        )

        class FakeXvfb:
            """最小 Xvfb 替身：start 返回固定 display，stop 可注入清理异常。"""

            def __init__(self, stop_error: Exception | None = None) -> None:
                self.screen = 0
                self.stop_error = stop_error

            def start(self) -> str:
                return ":99"

            def stop(self) -> dict[str, object]:
                if self.stop_error is not None:
                    raise self.stop_error
                return {"pid": 1}

        class FakeSession:
            """最小 X11 会话替身；close 可注入异常模拟清理失败。"""

            def __init__(self, close_error: Exception | None = None) -> None:
                self.close_error = close_error

            def find_window(self, timeout: float = 60.0) -> None:
                return None

            def prepare_window(self) -> object:
                return types.SimpleNamespace(
                    window_id=1, title="kerminal", width=1, height=1, x=0, y=0
                )

            def focus_window(self) -> None:
                return None

            def click_fraction(self, x_fraction: float, y_fraction: float) -> None:
                return None

            def chord(self, modifiers, key_name: str) -> None:
                return None

            def type_text(self, text: str) -> None:
                return None

            def press_enter(self) -> None:
                return None

            def close(self) -> None:
                if self.close_error is not None:
                    raise self.close_error

        def run_with(
            capture_report, session_factory, xvfb_factory
        ) -> tuple[int, dict[str, object]]:
            """打桩后执行真实 run_inner，经 capture_report 读回最终报告。

            打桩范围只覆盖 run_inner 引用的模块级名字：Xvfb/X11 替身、沙箱身份
            与 /proc 探测、截图与 marker 等待、以及应用启动（subprocess.Popen）
            和全部 settle 等待（time.sleep），既避免启动真实进程，也让主体在
            毫秒级走完成功路径。app_process 替身提供 wait/poll/returncode/kill
            语义，使 quit 与 finally 清理分支按真实代码执行。
            """
            class FakeAppProcess:
                """最小应用进程替身：模拟运行中进程，供 wait 与清理分支使用。

                poll 在 wait 之前返回 None（仍在运行）以通过启动检查；wait 返回
                0 并记录 returncode，使 finally 清理按"已优雅退出"跳过信号路径；
                kill 吞掉调用，避免触达真实进程语义。
                """

                pid = 4242
                returncode = None

                def poll(self):
                    return self.returncode

                def wait(self, timeout=None):
                    self.returncode = 0
                    return 0

                def kill(self):
                    return None

            originals = {
                name: getattr(module, name)
                for name in (
                    "PrivateXvfb",
                    "X11Session",
                    "_load_xlibs",
                    "_verify_sandbox_identity",
                    "_sandbox_environment",
                    "_prepare_existing_paths",
                    "_app_environment",
                    "_screenshot",
                    "_wait_marker",
                    "_read_db_rows",
                    "_write_report",
                    "subprocess",
                    "time",
                    "os",
                )
            }

            class StubSubprocess:
                """subprocess 替身：透传异常类型，只替换 Popen；setxkbmap
                探测的 CompletedProcess 由 stub_run 假成功返回。"""

                DEVNULL = -3
                TimeoutExpired = originals["subprocess"].TimeoutExpired
                CompletedProcess = originals["subprocess"].CompletedProcess
                PIPE = -1
                STDOUT = -2

                @staticmethod
                def Popen(argv, **kwargs):
                    return FakeAppProcess()

                @staticmethod
                def run(argv, **kwargs):
                    return originals["subprocess"].CompletedProcess(argv, 0, "")

            class StubTime:
                """time 替身：sleep 短路；monotonic/strftime 透传真实实现。"""

                @staticmethod
                def sleep(seconds):
                    return None

                @staticmethod
                def monotonic():
                    return originals["time"].monotonic()

                @staticmethod
                def strftime(fmt):
                    return originals["time"].strftime(fmt)

            class StubOs:
                """os 替身：killpg 在 Windows 上不存在，替身为记录调用的空操作；
                environ 透传真实实现，保证 RUN_ID_ENV 读写仍走真实环境。"""

                killpg_calls: list[object] = []
                environ = os.environ

                @staticmethod
                def killpg(pid, sig):
                    StubOs.killpg_calls.append((pid, sig))
                    return None

            try:
                module._prepare_existing_paths = lambda root: paths
                module.PrivateXvfb = xvfb_factory
                module.X11Session = session_factory
                module._load_xlibs = lambda report: (object(), object())
                module._verify_sandbox_identity = lambda run_id: {"pid": 1}
                module._sandbox_environment = lambda run_id: {}
                module._app_environment = lambda display, p: {}
                # 截图哈希按文件名区分，通过 A/B 与 Alt+7 前后的变化断言。
                module._screenshot = lambda display, p, name: {"sha256": name}
                # 持久化历史读回：按传入 marker 命中，schema 版本固定 3。
                # marker 内容决定身份字段，使 A/B 产生不同 pane/session 且 A 先于 B，
                # 通过 _verify_history 的全部身份断言。
                def stub_history_row(command: str) -> dict[str, str]:
                    ordinal = 1 if "rr_split_a_" in command else 2
                    return {
                        "command": command,
                        "id": command,
                        "rowid": str(ordinal),
                        "pane_id": f"pane-{ordinal}",
                        "session_id": f"session-{ordinal}",
                        "tab_id": "tab-1",
                        "source": "user",
                        "target": "local",
                    }

                module._wait_marker = lambda database, marker, timeout: stub_history_row(
                    marker
                )

                def fake_read_db_rows(database, commands):
                    return 3, [stub_history_row(command) for command in commands]

                module._read_db_rows = fake_read_db_rows
                module._write_report = capture_report
                module.subprocess = StubSubprocess
                module.time = StubTime
                module.os = StubOs
                os.environ[module.RUN_ID_ENV] = "cleanup-test-run"
                code = module.run_inner()
                return code, capture_report.last_report
            finally:
                for name, value in originals.items():
                    setattr(module, name, value)
                os.environ.pop(module.RUN_ID_ENV, None)

        class ReportCapture:
            """记录 run_inner 最后一次写报告的载体，供断言读取最终状态。"""

            def __init__(self) -> None:
                self.last_report: dict[str, object] = {}

            def __call__(self, path, report) -> object:
                self.last_report = report
                return path

        # 场景 1：主体成功 + X11 close 抛错 → 退出码 1、报告 failed、stage=cleanup。
        capture1 = ReportCapture()
        code, report = run_with(
            capture1,
            lambda x11, xtst, display: FakeSession(
                close_error=OSError("x11 close failed")
            ),
            lambda p: FakeXvfb(),
        )
        check(
            code == 1,
            f"cleanup failure after successful body must exit 1, got: {code}",
        )
        check(
            report.get("status") == "failed"
            and report.get("failure", {}).get("stage") == "cleanup",
            f"cleanup failure must produce failed report with cleanup stage, got: {report.get('status')} / {report.get('failure')}",
        )
        # 场景 2：主体成功 + 全部清理成功 → 退出码 0、报告达到 mechanical-pass。
        capture2 = ReportCapture()
        code_ok, report_ok = run_with(
            capture2,
            lambda x11, xtst, display: FakeSession(),
            lambda p: FakeXvfb(),
        )
        check(
            code_ok == 0,
            f"fully successful body with clean cleanup must exit 0, got: {code_ok}",
        )
        check(
            report_ok.get("status") == "mechanical-pass-human-visual-review-required"
            and "cleanupErrors" not in report_ok,
            f"success path must reach mechanical-pass without cleanup errors, got: {report_ok.get('status')} / {report_ok.get('cleanupErrors')}",
        )
        # 场景 3：主体失败 + 清理也失败 → 报告必须保留主体阶段错误，不被覆盖。
        class FailingSession(FakeSession):
            """prepare_window 抛错模拟主体失败；close 也失败模拟清理叠加。"""

            def prepare_window(self) -> object:
                raise module.AcceptanceError("body stage failure")

        capture3 = ReportCapture()
        code_fail, report_fail = run_with(
            capture3,
            lambda x11, xtst, display: FailingSession(
                close_error=OSError("x11 close failed")
            ),
            lambda p: FakeXvfb(stop_error=OSError("xvfb stop failed")),
        )
        check(
            code_fail == 1,
            f"body failure must exit 1 regardless of cleanup, got: {code_fail}",
        )
        check(
            report_fail.get("failure", {}).get("stage") == "find-window",
            f"body failure stage must be preserved, got: {report_fail.get('failure', {}).get('stage')}",
        )
        check(
            report_fail.get("cleanupErrors"),
            f"cleanup errors must still be recorded alongside body failure: {report_fail.get('cleanupErrors')}",
        )
        # 场景 4：主体成功但证据报告写不出（OSError）→ 退出码必须降级为 1。
        # 内层报告只落盘不打印，写失败即全部证据缺失，退出码 0 会伪成功。
        # sink 抛错时 run_inner 无报告可读，断言只看退出码。
        def failing_report_sink(path, report) -> object:
            raise OSError("disk full")

        code_no_report, _ = run_with(
            failing_report_sink,
            lambda x11, xtst, display: FakeSession(),
            lambda p: FakeXvfb(),
        )
        check(
            code_no_report == 1,
            f"successful body with unwritable evidence report must exit 1, got: {code_no_report}",
        )
        # 场景 5：主体失败 + 报告也写不出 → 退出码保持 1，不被"成功"掩盖。
        code_fail_no_report, _ = run_with(
            FailingReportSink(),
            lambda x11, xtst, display: FailingSession(),
            lambda p: FakeXvfb(),
        )
        check(
            code_fail_no_report == 1,
            f"body failure with unwritable report must stay exit 1, got: {code_fail_no_report}",
        )


def test_launch_native_report_write_failure_degrades_exit(module) -> None:
    """回归：宿主报告写不出时 launch_native 必须降级退出码并如实打印。

    与内层不同，宿主报告同时是 main 的 stdout 主输出（调用方仍能读到内容），
    但落盘证据缺失必须体现在退出码上：沙箱判定成功（status==0）时写报告
    OSError 要把退出码改为 1；先前已失败时保持 1。打桩到最小依赖集合，不
    启动沙箱、不触碰文件系统之外的资源。
    """
    with tempfile.TemporaryDirectory(prefix="rr-launch-report-") as temp:
        root = pathlib.Path(temp)
        paths = module.RunPaths(
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
            report_file=root / "evidence" / "host-report.json",
        )

        class FakeSandboxProcess:
            """沙箱进程替身：立即以给定退出码结束，供 wait 与清理分支使用。"""

            pid = 777
            returncode = 0

            def poll(self):
                return self.returncode

            def wait(self, timeout=None):
                return self.returncode

            def kill(self):
                return None

        class FailingReportSink:
            """_write_report 替身：抛 OSError 模拟写入失败。"""

            def __call__(self, path, report) -> object:
                raise OSError("disk full")

        # inner 报告文件不存在 → 宿主走"sandbox 无失败报告"分支，status 本为 1；
        # 再叠加写报告失败，验证保持 1 而不是被改写为 0。
        originals = {
            name: getattr(module, name)
            for name in (
                "check_dependencies",
                "_prepare_run_root",
                "_stage_app",
                "_stage_controller",
                "_sandbox_prefix",
                "_sandbox_environment",
                "_write_report",
                "subprocess",
                "time",
                "secrets",
            )
        }

        class StubSubprocess:
            """subprocess 替身：Popen 返回立即退出的沙箱进程替身。"""

            DEVNULL = -3
            TimeoutExpired = originals["subprocess"].TimeoutExpired
            CompletedProcess = originals["subprocess"].CompletedProcess

            @staticmethod
            def Popen(argv, **kwargs):
                process = FakeSandboxProcess()
                process.returncode = 0
                return process

        class StubTime:
            """time 替身：sleep 短路；wait 超时路径不触发。"""

            @staticmethod
            def sleep(seconds):
                return None

            @staticmethod
            def strftime(fmt):
                return originals["time"].strftime(fmt)

        try:
            module.check_dependencies = lambda need_app=None: (
                lambda: (
                    setattr(deps := module.DependencyReport(), "ready", True),
                    deps.tools.update({"bwrap": "/usr/bin/bwrap"}),
                    deps,
                )[-1]
            )()
            module._prepare_run_root = lambda base: paths
            module._stage_app = lambda source, p: {"sha256": "x"}
            module._stage_controller = lambda p: "y"
            module._sandbox_prefix = lambda bwrap, root: [bwrap]
            module._sandbox_environment = lambda run_id: {}
            module._write_report = FailingReportSink()
            module.subprocess = StubSubprocess
            module.time = StubTime
            module.secrets = types.SimpleNamespace(token_hex=lambda n: "feedface")

            status, report = module.launch_native(
                pathlib.Path("/nonexistent/app"), root
            )
            check(
                status == 1,
                f"missing inner report must already fail closed, got status {status}",
            )
            check(
                report.get("status") == "failed",
                f"missing inner report must produce failed host report, got: {report.get('status')}",
            )

            # 让 inner 报告存在且成功，验证宿主本应成功时写报告失败会降级退出码。
            paths.inner_report_file.parent.mkdir(parents=True, exist_ok=True)
            paths.inner_report_file.write_text(
                json.dumps({"status": "mechanical-pass-human-visual-review-required"}),
                encoding="utf-8",
            )
            status_ok, report_ok = module.launch_native(
                pathlib.Path("/nonexistent/app"), root
            )
            check(
                status_ok == 1,
                f"successful verdict with unwritable host report must exit 1, got: {status_ok}",
            )
            check(
                report_ok.get("status") == "mechanical-pass-human-visual-review-required",
                "host report content must keep the verdict; only the exit code degrades",
            )
        finally:
            for name, value in originals.items():
                setattr(module, name, value)


def main() -> int:
    """运行全部纯单测；不产生任何持久副作用，失败时列出全部断言消息。"""
    module = load_script()
    test_probe_program_is_real_code(module)
    test_probe_parses_probe_output(module)
    test_sandbox_probe_no_go_semantics(module)
    test_loopback_self_connect_real_socket(module)
    test_pixel_gates(module)
    test_transparent_rgba_frame_rejected(module)
    test_semitransparent_frame_rejected(module)
    test_palette_trns_transparency(module)
    test_truecolor_trns_rejected(module)
    test_malformed_png_fail_closed(module)
    test_region_fraction(module)
    test_verify_history_pure(module)
    test_sandbox_identity_parsing(module)
    test_screenshot_remap_pure(module)
    test_screenshot_call_sites_bind_arguments_correctly(module)
    test_short_ihdr_chunk_fails_typed(module)
    test_run_inner_fails_closed_without_run_root(module)
    test_run_inner_exit_code_matches_cleanup_report(module)
    test_launch_native_report_write_failure_degrades_exit(module)
    test_no_auto_delete_path(module)
    test_plan_contract(module)
    if failures:
        print(f"FAILED {len(failures)}/{checks}:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(f"right-rail-split-linux-native pure tests passed ({checks} checks).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
