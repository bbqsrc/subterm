# Subterm

A Rust library for efficiently managing pools of interactive subprocesses with async support.

[![Crates.io](https://img.shields.io/crates/v/subterm.svg)](https://crates.io/crates/subterm)
[![Documentation](https://docs.rs/subterm/badge.svg)](https://docs.rs/subterm)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/subterm.svg)](#license)

## Features

- **Efficient Process Pool Management**: Maintain a pool of reusable subprocess instances
- **Async/Await Support**: Built on tokio for asynchronous operation
- **Fair Queue Management**: Uses a channel-based queuing system for fair process allocation
- **Interactive Process Control**: Read from and write to subprocess stdin/stdout
- **Automatic Process Recovery**: Dead processes are automatically replaced
- **Resource Control**: Limit the maximum number of concurrent processes

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
subterm = "0.0.1"
```

### Using the Process Pool

```rust
use subterm::{SubprocessPool, Result};
use tokio::process::Command;

#[tokio::main]
async fn main() -> Result<()> {
    // Create a pool of 4 processes
    let pool = SubprocessPool::new(|| {
        let mut cmd = Command::new("your_interactive_program");
        cmd.arg("--some-flag");
        cmd
    }, 4).await?;

    // Acquire a process from the pool
    let mut process = pool.acquire().await?;
    
    // Interact with the process
    process.write_line("some input").await?;
    let response = process.read_line().await?;
    println!("Got response: {}", response);
    
    Ok(())
}
```

### Direct Subprocess Usage

You can also create and manage subprocesses directly without using the pool:

```rust
use std::sync::Arc;
use subterm::{Subprocess, Result};
use tokio::process::Command;

#[tokio::main]
async fn main() -> Result<()> {
    let mut process = Subprocess::new(|| {
        let mut cmd = Command::new("your_interactive_program");
        cmd.arg("--some-flag");
        cmd
    })?;
    
    // Interact with the process
    process.write_line("hello").await?;
    let response = process.read_line().await?;
    println!("Got response: {}", response);
    
    // Read until a specific delimiter
    process.write_bytes(b"part1:part2\n").await?;
    let response = process.read_bytes_until(b':').await?;
    
    // Close stdin when done
    process.close_stdin();
    
    Ok(())
}
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
