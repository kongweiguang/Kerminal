//! 主机列表读取的可重复文件 I/O 基准。
//!
//! 侧栏主机导航按串行链读取主机目录：远程、扩展、本地三类列表共享同一配置根，
//! 并可能在之后继续读取分组和 profile。这里在隔离临时根中按固定规模合成主机文件，
//! 排除初始化、目录创建和文件写入耗时，只测量仓储与用例层的列表读取。
//!
//! 使用 `harness = false` 直接运行，并以 dev profile 构建，才能和既有 debug
//! 测试二进制保持同一优化级别；release/bench profile 的数字不能直接与基线比较。
//!
//! 运行方式（仅手动）：
//! `cargo bench -p kerminal-infrastructure --bench host_list_io --profile dev`
//!
//! @author kongweiguang

use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use kerminal_application::connections::management::{
    HostManagementRepository, HostManagementService,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
};
use tempfile::tempdir;

/// 基准迭代次数；保留预热后样本，避免把首次文件缓存与运行时启动算进结果。
const ITERATIONS: usize = 5;

/// 合成可被正式 validator 接受的 schema v3 SSH 主机，避免基准只测无效文件解析。
fn host_source(id: &str, index: usize) -> String {
    format!(
        "schema_version = 3\n\
         protocol = \"ssh\"\n\
         id = \"{id}\"\n\
         name = \"Synthetic host {index}\"\n\
         host = \"198.51.100.1\"\n\
         port = 22\n\
         username = \"bench\"\n\
         auth_type = \"agent\"\n\
         sort_order = {index}\n\
         created_at = \"1\"\n\
         updated_at = \"1\"\n"
    )
}

/// 直接写入合法主机文档，模拟已有数据目录；预热时只读一次以计入文件系统缓存。
fn seed_hosts(root: &Path, count: usize) {
    let directory = root.join("hosts");
    fs::create_dir_all(&directory).expect("创建基准主机目录");
    for index in 0..count {
        let id = format!("bench-host-{index:04}");
        fs::write(
            directory.join(format!("{id}.toml")),
            host_source(&id, index),
        )
        .expect("写入基准主机");
    }
}

/// 记录单次操作的墙钟耗时；返回全部样本供报告最小值和中位数。
fn timed(samples: &mut Vec<Duration>, started: Instant) {
    samples.push(started.elapsed());
}

/// 单独计时当前远程主机列表，便于与旧版本同一公开入口的样本比较。
async fn measure_remote(
    repository: &HostConfigurationRepository,
    expected: usize,
    iterations: usize,
) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let hosts = repository.list_hosts().await.expect("读取远程主机列表");
        assert_eq!(hosts.len(), expected, "远程列表条数必须保持完整");
        timed(&mut samples, started);
    }
    samples
}

/// 逐次读取本地主机列表；合成数据全是 SSH，因此正确结果应为空但不允许报错。
async fn measure_local(
    repository: &HostConfigurationRepository,
    iterations: usize,
) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let hosts = repository
            .list_local_hosts()
            .await
            .expect("读取本地主机列表");
        assert!(hosts.is_empty(), "SSH 主机不能混入本地列表");
        timed(&mut samples, started);
    }
    samples
}

/// 按侧栏原调用顺序执行当前的三个批量列表入口，以隔离合并扫描本身的收益。
async fn measure_serial_chain(
    service: &HostManagementService,
    expected: usize,
    iterations: usize,
) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let remote = service.list_hosts().await.expect("串行读取远程列表");
        let plugins = service.list_plugin_hosts().await.expect("串行读取扩展列表");
        let local = service.list_local_hosts().await.expect("串行读取本地列表");
        assert_eq!(remote.len(), expected, "远程列表条数必须保持完整");
        assert!(plugins.is_empty(), "SSH 主机不能混入扩展列表");
        assert!(local.is_empty(), "SSH 主机不能混入本地列表");
        timed(&mut samples, started);
    }
    samples
}

/// 一次目录快照取得侧栏三类主机，衡量新入口相对原串行链的收益。
async fn measure_catalog(
    service: &HostManagementService,
    expected: usize,
    iterations: usize,
) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let catalog = service.list_catalog().await.expect("一次读取主机目录");
        assert_eq!(catalog.hosts.len(), expected, "远程列表条数必须保持完整");
        assert!(catalog.plugin_hosts.is_empty(), "SSH 主机不能混入扩展列表");
        assert!(catalog.local_hosts.is_empty(), "SSH 主机不能混入本地列表");
        timed(&mut samples, started);
    }
    samples
}

/// 以微秒输出原始样本与中位数，报告可直接对照修改前后，不做跨机器归一化。
fn report(scale: usize, label: &str, samples: &[Duration]) {
    let mut micros: Vec<u128> = samples.iter().map(Duration::as_micros).collect();
    micros.sort_unstable();
    let median = micros[micros.len() / 2];
    let min = micros[0];
    let max = micros[micros.len() - 1];
    eprintln!(
        "HOST_LIST_BENCH hosts={scale} {label} min_us={min} median_us={median} max_us={max} samples_us={micros:?}"
    );
}

/// 单一入口覆盖 10/100/500 台主机的单列表、串行链和目录快照；初始化与建文件不计时。
///
/// 保留断言是为了避免基准在错误结果上给出漂亮数字；任何列表分类或条数异常都会
/// 直接终止运行，不会静默产出不可比较的样本。
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    for scale in [10usize, 100, 500] {
        let root = tempdir().expect("创建隔离配置根");
        seed_hosts(root.path(), scale);
        let store = ConfigurationStore::initialize(root.path()).expect("初始化隔离配置仓储");
        let repository = HostConfigurationRepository::new(store);
        let service = HostManagementService::new(Arc::new(repository.clone()));

        // 预热一次，避免把首次读取的元数据缓存成本计入稳态样本。
        let _ = repository.list_hosts().await.expect("预热远程列表");
        let _ = repository.list_local_hosts().await.expect("预热本地列表");
        let _ = service.list_plugin_hosts().await.expect("预热扩展列表");
        let _ = service.list_catalog().await.expect("预热目录快照");

        let remote = measure_remote(&repository, scale, ITERATIONS).await;
        report(scale, "remote", &remote);
        let local = measure_local(&repository, ITERATIONS).await;
        report(scale, "local", &local);
        let chain = measure_serial_chain(&service, scale, ITERATIONS).await;
        report(scale, "serial_remote_plugin_local", &chain);
        let catalog = measure_catalog(&service, scale, ITERATIONS).await;
        report(scale, "catalog", &catalog);
    }
}
