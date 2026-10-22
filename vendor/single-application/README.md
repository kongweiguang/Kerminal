# single-application

`single-application` is a lightweight single-instance application guard library for Rust. It uses OS local sockets to ensure that only one primary instance of an application runs at a time, while secondary instances launched later can forward their messages to the primary instance.


[![crates.io](https://img.shields.io/crates/v/single-application.svg)](https://crates.io/crates/single-application)
[![docs.rs](https://docs.rs/single-application/badge.svg)](https://docs.rs/single-application)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)


## Features

- Single-instance enforcement via local sockets, cross-platform.
- Secondary instances automatically send messages to the primary instance.
- Generic message type: any type implementing `Serialize + DeserializeOwned` is supported.
- Two runtimes supported:
  - A blocking `std` API by default;
  - An async API enabled via the `tokio` feature.

## Installation

```toml
[dependencies]
single-application = "0.1"
# Enable the tokio feature for the async API
single-application = { version = "0.1", features = ["tokio"] }
```

## Quick start

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

## License

Released under the [MIT License](LICENSE).
