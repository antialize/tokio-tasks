//! Task manager for tokio. Facilitates clean shutdow of applications, and async cancelation of tasks
//!
//! Example
//! =======
//! ```
//! use tokio_tasks::{TaskBuilder, run_tasks, shutdown, cancelable, RunToken};
//!
//! // Main task, program will shut down if when finishes
//! async fn main_task(run_token: RunToken) -> Result<(), String> {
//!     println!("Main task start");
//!     match cancelable(&run_token, tokio::time::sleep(std::time::Duration::from_secs(10))).await {
//!        Ok(()) => println!("Main task finished"),
//!        Err(_) => println!("Main task cancled"),
//!     }
//!     Ok(())
//! }
//!
//! // Critical task, program will shut down if this finished with an error
//! async fn critical_task(run_token: RunToken) -> Result<(), String> {
//!     println!("Critical task start");
//!     match cancelable(&run_token, tokio::time::sleep(std::time::Duration::from_secs(1))).await {
//!        Ok(()) => println!("Critical task finished"),
//!        Err(_) => println!("Critical task cancled"),
//!     }
//!     Ok(())
//! }
//!
//! # tokio::runtime::Builder::new_current_thread()
//! #        .enable_all()
//! #        .build()
//! #        .unwrap()
//! #        .block_on(async {
//! TaskBuilder::new("main_task")
//!     .main()
//!     .shutdown_order(1)
//!     .create(|rt| main_task(rt));
//!
//! TaskBuilder::new("critical_task")
//!     .critical()
//!     .shutdown_order(2)
//!     .create(|rt| critical_task(rt));
//!
//! // Normally one would register the program to stop on ctrl+c
//! // tokio::spawn(async {
//! //    tokio::signal::ctrl_c().await.unwrap();
//! //    shutdown("ctrl+c".to_string());
//! // });
//!
//! // We just shut down after a little while
//! tokio::spawn(async {
//!    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
//!    shutdown("timeout".to_string());
//! });
//! run_tasks().await;
//! # })
//! ```

mod run_token;
mod scope_guard;
mod task;

pub use run_token::RunToken;
pub use run_token::WaitForCancellationFuture;
#[cfg(feature = "pause")]
pub use run_token::WaitForPauseFuture;
pub use task::cancelable;
#[cfg(feature = "ordered-locks")]
pub use task::cancelable_checked;
pub use task::list_tasks;
pub use task::run_tasks;
pub use task::shutdown;
pub use task::try_list_tasks_for;
pub use task::BoxFuture;
pub use task::CancelledError;
pub use task::FinishState;
pub use task::Task;
pub use task::TaskBase;
pub use task::TaskBuilder;
pub use task::WaitError;
