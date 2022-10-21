tokio-tasks
=============
Task managment for tokio

[![Crates.io](https://img.shields.io/crates/v/tokio-tasks.svg)](https://crates.io/crates/tokio-tasks)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE-MIT)
[![Build status](https://img.shields.io/github/workflow/status/antialize/tokio-tasks/RUST%20Continuous%20integration)](https://github.com/antialize/tokio-tasks/actions)
[![Docs](https://img.shields.io/badge/docs-latest-blue.svg)](https://docs.rs/tokio-tasks)

```rust
use tokio_tasks::{TaskBuilder, run_tasks, shutdown, cancelable, RunToken};

// Main task, program will shut down if when finishes
async fn main_task(run_token: RunToken) -> Result<(), String> {
    println!("Main task start");
    match cancelable(&run_token, tokio::time::sleep(std::time::Duration::from_secs(10))).await {
       Ok(()) => println!("Main task finished"),
       Err(_) => println!("Main task cancled"),
    }
    Ok(())
}

// Critical task, program will shut down if this finished with an error
async fn critical_task(run_token: RunToken) -> Result<(), String> {
    println!("Critical task start");
    match cancelable(&run_token, tokio::time::sleep(std::time::Duration::from_secs(1))).await {
       Ok(()) => println!("Critical task finished"),
       Err(_) => println!("Critical task cancled"),
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    TaskBuilder::new("main_task")
        .main()
        .shutdown_order(1)
        .create(|rt| main_task(rt));

    TaskBuilder::new("critical_task")
        .critical()
        .shutdown_order(2)
        .create(|rt| critical_task(rt));

    // Shutdown the application on ctrl+c
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.unwrap();
        shutdown("ctrl+c".to_string());
    });

    // Run until all tasks stop
    run_tasks().await;
}
 ```

