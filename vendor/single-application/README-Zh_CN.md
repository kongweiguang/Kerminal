# single-application

`single-application` 是一个轻量的 Rust 单实例应用守卫库。它通过操作系统本地套接字确保同一应用同一时刻只运行一个主实例，而后续启动的次实例可将其携带的消息转发给主实例。


[![crates.io](https://img.shields.io/crates/v/single-application.svg)](https://crates.io/crates/single-application)
[![docs.rs](https://docs.rs/single-application/badge.svg)](https://docs.rs/single-application)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)


## 特性

- 基于本地套接字实现单实例约束，跨平台。
- 次实例自动将消息发送给主实例。
- 消息类型泛型化：任何实现 `Serialize + DeserializeOwned` 的类型均可。
- 支持两种运行时：
  - 默认的阻塞式 `std` 接口；
  - 通过 `tokio` feature 启用的异步接口。

## 安装

```toml
[dependencies]
single-application = "0.1"
# 如需异步接口，启用 tokio feature
single-application = { version = "0.1", features = ["tokio"] }
```

## 快速开始

### Blocking (default)

```rust
use single_application::SingleApplication;

fn main() {
    let app = SingleApplication::<String>::new("{EA4AEAE0-2B98-441D-8610-7EAE283329EC}");

    match app {
        SingleApplication::PrimaryInstance(primary) => {
            println!("Primary instance is running.");
            while let Ok(data) = primary.recv() {
                println!("Received from secondary: {data}");
            }
        }
        SingleApplication::SecondaryInstance(secondary) => {
            println!("Secondary instance started, forwarding message.");
            secondary.send_message("Hello from secondary".to_string());
        }
    }
}
```

### Async (with the `tokio` feature)

```rust
use single_application::SingleApplication;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let app = SingleApplication::<String>::new("{EA4AEAE0-2B98-441D-8610-7EAE283329EC}");

    match app {
        SingleApplication::PrimaryInstance(mut primary) => {
            println!("Primary instance is running.");
            while let Some(data) = primary.recv().await {
                println!("Received from secondary: {data}");
            }
        }
        SingleApplication::SecondaryInstance(secondary) => {
            println!("Secondary instance started, forwarding message.");
            secondary.send_message("Hello from secondary".to_string()).await;
        }
    }
}
```

## 许可证

基于 [MIT 许可证](LICENSE) 发布。
