//! 插件归档的路径、条目边界与流式摘要验证回归。
//! @author kongweiguang

use flate2::{Compression, write::GzEncoder};
use kerminal_application::AppError;
use std::io::{Cursor, Read, Write};
use tokio_util::sync::CancellationToken;

use super::{ArchiveExpectation, unpack_stream, unpack_verified};

/// 仅测试使用的无期望值解压入口：生产安装链一律走 `unpack_verified`，把该入口
/// 留在测试模块里，既不把未认证路径编进产物，也避免 src 出现 cfg(test) 项。
fn unpack(
    archive_path: &std::path::Path,
    destination: &std::path::Path,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| AppError::infrastructure_with_source("读取插件安装包失败", e))?;
    unpack_stream(file, destination, cancellation, None)
}

/// 构造原始 header 的未压缩 tar，允许测试正常 builder 会主动拒绝的危险路径。
fn raw_tar(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, kind, bytes) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(*kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        let name_bytes = name.as_bytes();
        header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
        header.set_cksum();
        tar.append(&header, Cursor::new(*bytes)).expect("tar entry");
    }
    tar.into_inner().expect("tar finish")
}

/// 把一段字节经 gzip 压缩为一个完整成员。
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

/// 构造单成员 gzip tar 归档，与既有用例的输入形态保持一致。
fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    gzip(&raw_tar(entries))
}

/// 多成员 gzip：同一份合法 tar 拆成前后两段分别压缩，两段都是成员的一部分。
///
/// 拆分点必须落在条目数据区，绝不能切进末尾 1024 字节结束块：tar 迭代在遇到
/// 第一个零块时消费它并结束，尾检要求其后仍有完整零块；若结束块被切成 512+512
/// 两块，仍可满足，但若切出不足 512 字节的残块就会把合法归档误判为缺结束块。
/// 这里按“去掉结束块后的正文长度”对半取块对齐点，保证结束块完整留在第二成员。
fn multi_member_gzip(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let raw = raw_tar(entries);
    // raw_tar 末尾固定 1024 字节结束块，正文 = 总长 - 1024，按 512 对齐取中点。
    let body = raw.len().saturating_sub(1024);
    let split = (body / 2 / 512) * 512;
    assert!(split > 0 && split < body, "样本正文应足够分成两个非空成员");
    let mut bytes = gzip(&raw[..split]);
    bytes.extend_from_slice(&gzip(&raw[split..]));
    bytes
}

/// 每个样本使用隔离目录，防止残留文件改变后续碰撞结果。
fn attempt(bytes: &[u8]) -> bool {
    let root = tempfile::tempdir().expect("temporary root");
    let source = root.path().join("package.kplugin");
    std::fs::write(&source, bytes).expect("write archive");
    let destination = root.path().join("extract");
    std::fs::create_dir(&destination).expect("extract directory");
    unpack(&source, &destination, &CancellationToken::new()).is_ok()
}

/// 计算字节的 SHA-256 并输出小写十六进制，与签名目录落盘格式一致。
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// unpack_verified 的便捷封装：按给定字节构造期望值并返回完整错误供精确断言。
fn run_verified(bytes: &[u8], size: u64, sha256: &str, cancelled: bool) -> Result<(), AppError> {
    let root = tempfile::tempdir().expect("temporary root");
    let source = root.path().join("package.kplugin");
    std::fs::write(&source, bytes).expect("write archive");
    let destination = root.path().join("extract");
    std::fs::create_dir(&destination).expect("extract directory");
    let cancellation = CancellationToken::new();
    if cancelled {
        cancellation.cancel();
    }
    let expected = ArchiveExpectation {
        size,
        sha256: sha256.to_string(),
    };
    unpack_verified(&source, &destination, &cancellation, &expected)
}

/// 便捷封装：只关心成败的用例使用。
fn attempt_verified(bytes: &[u8], size: u64, sha256: &str, cancelled: bool) -> bool {
    run_verified(bytes, size, sha256, cancelled).is_ok()
}

/// 受控 reader：每次 `read` 调用后回调一次，供测试在读取间隙精确触发取消。
struct StepReader<'a> {
    data: Cursor<Vec<u8>>,
    after_read: &'a dyn Fn(usize),
}
impl Read for StepReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.data.read(buf)?;
        (self.after_read)(read);
        Ok(read)
    }
}

/// 直接驱动共享核心，允许注入受控压缩源与指定取消令牌，供读取间隙取消回归使用。
fn run_stream_with<R: Read>(
    source: R,
    size: u64,
    sha256: &str,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    let root = tempfile::tempdir().expect("temporary root");
    let destination = root.path().join("extract");
    std::fs::create_dir(&destination).expect("extract directory");
    let expected = ArchiveExpectation {
        size,
        sha256: sha256.to_string(),
    };
    unpack_stream(source, &destination, cancellation, Some(&expected))
}

/// 普通嵌套文件应能解压，危险条目则不能依赖平台文件系统自行拒绝。
#[test]
fn archive_rejects_traversal_links_and_case_collisions() {
    assert!(attempt(&archive(&[(
        "views/editor.json",
        tar::EntryType::Regular,
        b"{}"
    )])));
    for entries in [
        vec![("../escape", tar::EntryType::Regular, b"x".as_slice())],
        vec![("folder\\escape", tar::EntryType::Regular, b"x".as_slice())],
        vec![("CON.txt", tar::EntryType::Regular, b"x".as_slice())],
        vec![("link", tar::EntryType::Symlink, b"".as_slice())],
        vec![
            ("Foo/a", tar::EntryType::Regular, b"a".as_slice()),
            ("foo/b", tar::EntryType::Regular, b"b".as_slice()),
        ],
        vec![
            ("file", tar::EntryType::Regular, b"a".as_slice()),
            ("file/child", tar::EntryType::Regular, b"b".as_slice()),
        ],
    ] {
        assert!(!attempt(&archive(&entries)));
    }
}

/// 未读到 gzip 校验尾部不能把截断数据当成已经解压成功。
#[test]
fn archive_rejects_truncated_gzip() {
    let mut bytes = archive(&[("data.txt", tar::EntryType::Regular, b"data")]);
    bytes.truncate(bytes.len() - 5);
    assert!(!attempt(&bytes));
}

/// 内容与期望完全一致时验证解压必须成功，证明正向路径可用。
#[test]
fn verified_accepts_matching_archive() {
    let bytes = archive(&[
        ("plugin.json", tar::EntryType::Regular, b"{}".as_slice()),
        (
            "bin/tool.sh",
            tar::EntryType::Regular,
            b"#!/bin/sh\n".as_slice(),
        ),
    ]);
    assert!(attempt_verified(
        &bytes,
        bytes.len() as u64,
        &digest(&bytes),
        false
    ));
}

/// 摘要按压缩字节流计算：内容正确但期望摘要错误必须拒绝，不能返回成功。
#[test]
fn verified_rejects_wrong_digest() {
    let bytes = archive(&[("data.txt", tar::EntryType::Regular, b"payload")]);
    let mut wrong = digest(&bytes);
    // 翻转首字符构造必然不同且格式合法的摘要。
    wrong.replace_range(0..1, if wrong.starts_with('0') { "1" } else { "0" });
    assert!(!attempt_verified(&bytes, bytes.len() as u64, &wrong, false));
}

/// 期望大小与压缩流长度不一致必须拒绝，防止签名目录与实际包错配。
#[test]
fn verified_rejects_wrong_size() {
    let bytes = archive(&[("data.txt", tar::EntryType::Regular, b"payload")]);
    assert!(!attempt_verified(
        &bytes,
        bytes.len() as u64 + 1,
        &digest(&bytes),
        false
    ));
    assert!(!attempt_verified(
        &bytes,
        bytes.len() as u64 - 1,
        &digest(&bytes),
        false
    ));
}

/// 截断的归档无论期望值如何都不能通过：gzip CRC 或 tar 尾检先失败。
#[test]
fn verified_rejects_truncated_stream() {
    let mut bytes = archive(&[("data.txt", tar::EntryType::Regular, b"data")]);
    bytes.truncate(bytes.len() - 5);
    // 用被截断数据自身作期望也必须失败，防止攻击者按损坏流伪造期望值。
    assert!(!attempt_verified(
        &bytes,
        bytes.len() as u64,
        &digest(&bytes),
        false
    ));
}

/// gzip 成员之后附加的坏尾部必须在尾检阶段被拒绝，摘要比对不能先于尾检放行。
#[test]
fn verified_rejects_corrupt_trailing_garbage() {
    let mut bytes = archive(&[("data.txt", tar::EntryType::Regular, b"data")]);
    bytes.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert!(!attempt_verified(
        &bytes,
        bytes.len() as u64,
        &digest(&bytes),
        false
    ));
}

/// 同一 gzip 成员内、tar 结束块之后的非零解压尾部必须被拒绝。
///
/// 与“gzip 成员之后附加垃圾”不同：这里把非零字节拼在解压后的 tar 流里再整体
/// 压缩，压缩层与 CRC 完全合法，只有 tar 尾检能发现未声明内容，因此精确命中
/// “插件归档尾部含未声明内容”分支；期望值按实际压缩字节计算，排除摘要干扰。
#[test]
fn verified_rejects_nonzero_tail_inside_member() {
    // 合法条目 + 结束块，再追加一个非零块，全部放进同一个 gzip 成员。
    let mut raw = raw_tar(&[("data.txt", tar::EntryType::Regular, b"data")]);
    raw.extend_from_slice(&[0x7fu8; 512]);
    let bytes = gzip(&raw);
    let error = run_verified(&bytes, bytes.len() as u64, &digest(&bytes), false)
        .expect_err("成员内非零尾部必须失败");
    match &error {
        AppError::Invalid { context } => {
            assert_eq!(*context, "插件归档尾部含未声明内容");
        }
        other => panic!("非零尾部必须报尾部内容错误，实际：{other:?}"),
    }
}

/// 块对齐但缺少 tar 结束块的归档必须被拒绝，即便末尾 1024 字节恰好全零。
///
/// 构造 1024 字节全零条目数据且不写结束块：解压流 = 512 字节头 + 1024 字节零，
/// 最后 1024 字节全为零，旧尾检（只验“最后 1024 字节为零”）会误判为合法。
/// 期望 size/SHA 按实际压缩字节给出，证明拒绝来自格式判定而非摘要；对应的正向
/// 样本（同样数据 + 结束块）必须通过。
#[test]
fn verified_rejects_block_aligned_archive_without_end_blocks() {
    // 1024 字节零数据：无结束块时迭代在数据末尾直接 EOF，尾部无剩余零块。
    let payload = [0u8; 1024];
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.as_mut_bytes()[..9].copy_from_slice(b"zero.bin\0");
    header.set_cksum();
    builder
        .append(&header, Cursor::new(payload))
        .expect("tar entry");
    // 用 into_inner 会补写结束块，这里取未 finish 的原始字节再 gzip。
    let raw = builder.get_ref().clone();
    // 前提自检：解压流最后 1024 字节确实全零，旧检查会被骗过。
    assert_eq!(raw.len(), 1536);
    assert!(raw[raw.len() - 1024..].iter().all(|byte| *byte == 0));
    let bytes = gzip(&raw);
    let error = run_verified(&bytes, bytes.len() as u64, &digest(&bytes), false)
        .expect_err("缺少结束块必须失败");
    match &error {
        AppError::Invalid { context } => {
            assert_eq!(*context, "插件归档结束块缺失");
        }
        other => panic!("缺少结束块必须报结束块缺失，实际：{other:?}"),
    }
    // 正向对照：同样条目数据补上结束块后必须通过，证明拒绝只针对结束块缺失。
    let with_end = gzip(&raw_tar(&[("zero.bin", tar::EntryType::Regular, &payload)]));
    assert!(attempt_verified(
        &with_end,
        with_end.len() as u64,
        &digest(&with_end),
        false
    ));
}

/// 多成员 gzip（concatenation）应被完整展开并整体验证，正向必须成功。
///
/// 两段压缩取自同一份合法 tar，第二段不含独立 tar 结束块，整体尾检应通过。
#[test]
fn verified_accepts_multi_member_gzip() {
    let bytes = multi_member_gzip(&[
        ("a.txt", tar::EntryType::Regular, b"first".as_slice()),
        ("b.txt", tar::EntryType::Regular, b"second".as_slice()),
    ]);
    assert!(attempt_verified(
        &bytes,
        bytes.len() as u64,
        &digest(&bytes),
        false
    ));
}

/// 第二个 gzip 成员的 CRC 损坏必须在解码阶段失败，不能靠摘要比对兜底放行。
#[test]
fn verified_rejects_corrupt_second_member_crc() {
    let mut bytes = multi_member_gzip(&[
        ("a.txt", tar::EntryType::Regular, b"first".as_slice()),
        ("b.txt", tar::EntryType::Regular, b"second".as_slice()),
    ]);
    // gzip 成员尾部 8 字节是 CRC32(4)+ISIZE(4)；翻转倒数第 5 字节（CRC32 末字节）
    // 只破坏第二成员的 CRC 校验，不波及压缩数据、ISIZE 或 tar 结构。
    let last = bytes.len() - 5;
    bytes[last] ^= 0xff;
    assert!(!attempt_verified(
        &bytes,
        bytes.len() as u64,
        &digest(&bytes),
        false
    ));
}

/// 取消令牌在解压前已触发时，验证解压必须以明确的 Cancelled 错误失败。
#[test]
fn verified_rejects_cancelled() {
    let bytes = archive(&[("data.txt", tar::EntryType::Regular, b"data")]);
    let digest = digest(&bytes);
    let error = run_verified(&bytes, bytes.len() as u64, &digest, true)
        .expect_err("已取消的解压不能返回成功");
    assert!(
        matches!(error, AppError::Cancelled { .. }),
        "取消必须报告 Cancelled 类别，实际：{error:?}"
    );
    // 未取消时同样输入必须成功，证明上面的失败来自取消而非摘要。
    assert!(attempt_verified(&bytes, bytes.len() as u64, &digest, false));
}

/// 压缩输入超过期望大小必须即时失败：大量空 gzip 成员几乎不产生解压字节，
/// 若只等 EOF 才比对，压缩输入可以在解压限额之外被无界撑大；限流须读取中生效。
///
/// 生产限额是 expected.size+1，预算耗尽后还有 1 字节试探读，因此底层交付恰好
/// 停在 expected_size+2：用受控 reader 累计交付字节数并断言该精确值，同时断言
/// 错误是携带固定预算文案的 Invalid 类别，确保失败确实命中预算而非任意解码错误。
#[test]
fn verified_rejects_compressed_input_over_limit() {
    use std::cell::Cell;
    // 单个空成员约 20 余字节，拼 64 个把输入撑到 1KB 以上，而期望大小只有 1。
    let empty_member = gzip(&[]);
    let mut bytes = Vec::new();
    for _ in 0..64 {
        bytes.extend_from_slice(&empty_member);
    }
    let expected_size = 1u64;
    assert!(bytes.len() > 512, "样本输入应明显超过期望大小");
    let delivered = Cell::new(0u64);
    let reader = StepReader {
        data: Cursor::new(bytes.clone()),
        after_read: &|read| delivered.set(delivered.get() + read as u64),
    };
    let cancellation = CancellationToken::new();
    let error = run_stream_with(reader, expected_size, &digest(&bytes), &cancellation)
        .expect_err("压缩输入超限必须失败");
    // 错误必须是预算超限的 Invalid 且携带固定文案，不能是取消或其他任意类别。
    match &error {
        AppError::Invalid { context } => {
            assert_eq!(*context, "插件归档压缩输入超过限额");
        }
        other => panic!("超限必须报 Invalid 预算错误，实际：{other:?}"),
    }
    // 底层交付精确停在 expected_size+2：expected+1 的预算 + 1 字节超限试探读。
    assert_eq!(
        delivered.get(),
        expected_size + 2,
        "底层交付应精确命中预算边界"
    );
}

/// 普通解码错误必须保持 Infrastructure 类别，与预算超限互不混淆。
#[test]
fn verified_reports_plain_decode_error_as_infrastructure() {
    // 非 gzip 的垃圾数据：MultiGzDecoder 首读解析头部即失败，属普通解码错误。
    let bytes = vec![0u8; 1024];
    let cancellation = CancellationToken::new();
    let error = run_stream_with(
        Cursor::new(bytes.clone()),
        bytes.len() as u64,
        &digest(&bytes),
        &cancellation,
    )
    .expect_err("垃圾数据必须失败");
    match &error {
        AppError::Infrastructure { context, .. } => {
            assert_eq!(*context, "读取插件归档条目失败");
        }
        other => panic!("普通解码错误不得伪装成预算或取消，实际：{other:?}"),
    }
}

/// EOF 后才触发的取消必须被最终复核捕获：解码与 tar 迭代全程未取消，
/// 读到 EOF 的那一刻取消，随后任何出口都不能返回成功。
#[test]
fn verified_cancels_after_eof_before_success() {
    let bytes = archive(&[("data.txt", tar::EntryType::Regular, b"data")]);
    let cancellation = CancellationToken::new();
    // 首次读到 0 字节（EOF）即取消：此后进入尾读循环，入口检查必须命中。
    let reader = StepReader {
        data: Cursor::new(bytes.clone()),
        after_read: &|read| {
            if read == 0 {
                cancellation.cancel();
            }
        },
    };
    let error = run_stream_with(reader, bytes.len() as u64, &digest(&bytes), &cancellation)
        .expect_err("EOF 后取消不能返回成功");
    assert!(
        matches!(error, AppError::Cancelled { .. }),
        "EOF 后取消必须报 Cancelled，实际：{error:?}"
    );
}

/// 压缩源读取间隙的取消必须被识别：MultiGzDecoder 单次 read 可能连续消费大量空
/// gzip 成员而不产出解压字节，外层 entry/tail 循环等不到检查点；取消错误按
/// Cancelled 类别精确浮出，且不能在取消后返回成功。
#[test]
fn verified_cancels_between_source_reads() {
    // 多个空成员：解码器要读完整个输入才能结束，取消必然发生在读取间隙。
    let empty_member = gzip(&[]);
    let mut bytes = Vec::new();
    for _ in 0..64 {
        bytes.extend_from_slice(&empty_member);
    }
    let cancellation = CancellationToken::new();
    // 第一次底层读取返回后立即取消：下一次读取边界的检查必须命中。
    let reader = StepReader {
        data: Cursor::new(bytes),
        after_read: &|_| cancellation.cancel(),
    };
    let error = run_stream_with(reader, u64::MAX, "0".repeat(64).as_str(), &cancellation)
        .expect_err("读取间隙取消不能返回成功");
    assert!(
        matches!(error, AppError::Cancelled { .. }),
        "读取间隙取消必须报 Cancelled，实际：{error:?}"
    );
}

/// 解压开始前已取消时必须在首次读取边界失败。
///
/// 注意实际命中点：`MultiGzDecoder::new` 构造时立即解析 gzip 头，触发
/// `HashingReader` 的首次读取；预取消在这里就被标记错误捕获，并未走到
/// entry 循环或尾读入口。entry 循环内与 EOF 后的取消由其他用例分别覆盖。
#[test]
fn verified_rejects_pre_cancelled_at_first_read() {
    // 只含 gzip 头和一个空 deflate 块的合法空成员，解压后是 0 字节即空 tar。
    let bytes = gzip(&[]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = run_stream_with(Cursor::new(bytes), u64::MAX, &"0".repeat(64), &cancellation)
        .expect_err("预取消不能返回成功");
    assert!(
        matches!(error, AppError::Cancelled { .. }),
        "预取消必须报 Cancelled，实际：{error:?}"
    );
}

/// 大条目读取过程中的取消必须被识别，覆盖 entry 循环内的晚阶段读取。
///
/// 条目内容必须不可压缩：全零/重复字节经 deflate 后只有几百字节，落在解码器
/// 32KB 预读内，根本不会有第二次源读取。这里用确定性伪随机字节，压缩后仍远超
/// 64KB；累计交付超过 64KB 时取消，此时 tar 正在读取条目内容，取消由
/// `HashingReader` 的读取边界捕获，经 entry 读取路径映射为 Cancelled。
#[test]
fn verified_cancels_during_large_entry_read() {
    use std::cell::Cell;
    // 确定性伪随机：线性同余序列，压缩率接近 1，保证输入远大于 64KB。
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let payload: Vec<u8> = (0..200 * 1024)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect();
    let bytes = archive(&[("big.bin", tar::EntryType::Regular, &payload)]);
    assert!(
        bytes.len() > 128 * 1024,
        "压缩后仍应远超 64KB，实际 {} 字节",
        bytes.len()
    );
    let cancellation = CancellationToken::new();
    let delivered = Cell::new(0u64);
    let reader = StepReader {
        data: Cursor::new(bytes.clone()),
        after_read: &|read| {
            delivered.set(delivered.get() + read as u64);
            if delivered.get() > 64 * 1024 {
                cancellation.cancel();
            }
        },
    };
    let error = run_stream_with(reader, bytes.len() as u64, &digest(&bytes), &cancellation)
        .expect_err("条目读取中取消不能返回成功");
    assert!(
        matches!(error, AppError::Cancelled { .. }),
        "条目读取中取消必须报 Cancelled，实际：{error:?}"
    );
    assert!(
        delivered.get() > 64 * 1024,
        "取消应发生在多次源读取之后，实际交付 {} 字节",
        delivered.get()
    );
}

/// 模拟预检后暂存文件被替换：期望值取自原包，实际解压读到替换后的包必须被拒绝。
///
/// 实现顺序是先解压写文件、尾检通过后才比对摘要，因此合法替换包的成员文件会写入
/// 隔离目的目录；本测试固定“摘要不匹配返回错误、不提交”这一归档层行为，暂存与
/// 解压材料的清理由调用方负责。安装编排只在成功返回后继续校验与提交。
#[test]
fn verified_rejects_swapped_input_after_preflight() {
    let original = archive(&[("data.txt", tar::EntryType::Regular, b"original")]);
    let replaced = archive(&[("data.txt", tar::EntryType::Regular, b"replaced")]);
    let root = tempfile::tempdir().expect("temporary root");
    let source = root.path().join("package.kplugin");
    let destination = root.path().join("extract");
    std::fs::create_dir(&destination).expect("extract directory");
    // 预检阶段按原包计算 size/摘要（等价于签名目录里登记的值）。
    let expected = ArchiveExpectation {
        size: original.len() as u64,
        sha256: digest(&original),
    };
    // 预检之后、解压之前，攻击者把暂存文件原位替换成另一份合法归档。
    std::fs::write(&source, &replaced).expect("write swapped archive");
    let cancellation = CancellationToken::new();
    let result = unpack_verified(&source, &destination, &cancellation, &expected);
    let error = result.expect_err("替换后的包不得通过验证");
    assert!(
        matches!(error, AppError::Invalid { .. }),
        "摘要不匹配应报 Invalid 类别，实际：{error:?}"
    );
    // 解压材料可能已写入目的目录；未提交的决定权在调用方，归档层只保证失败。
    assert!(destination.join("data.txt").exists());
}
