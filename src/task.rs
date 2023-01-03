use crate::{scope_guard::scope_guard, RunToken};
use futures_util::{
    future::{self},
    pin_mut, Future, FutureExt,
};
use lazy_static::lazy_static;
use log::{debug, error, info};
use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use std::{collections::HashMap, sync::atomic::AtomicBool};
use std::{fmt::Display, sync::Mutex};
use std::{pin::Pin, task::Poll};
use tokio::{
    sync::Notify,
    task::{JoinError, JoinHandle},
};

#[cfg(feature = "ordered-locks")]
use ordered_locks::{CleanLockToken, LockToken, L0};

lazy_static! {
    static ref TASKS: Mutex<HashMap<usize, Arc<dyn TaskBase>>> = Mutex::new(HashMap::new());
    static ref SHUTDOWN_NOTIFY: Notify = Notify::new();
}
static TASK_ID_COUNT: AtomicUsize = AtomicUsize::new(0);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Error returned by [cancelable] when c was canceled before the future returned
#[derive(Debug)]
pub struct CancelledError {}
impl Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CancelledError")
    }
}
impl std::error::Error for CancelledError {}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Return result from fut, unless run_token is canceled before fut is done
pub async fn cancelable<T, F: Future<Output = T>>(
    run_token: &RunToken,
    fut: F,
) -> Result<T, CancelledError> {
    let c = run_token.cancelled();
    pin_mut!(fut, c);
    let f = future::select(c, fut).await;
    match f {
        future::Either::Right((v, _)) => Ok(v),
        future::Either::Left(_) => Err(CancelledError {}),
    }
}

/// Return result from fut, unless run_token is canceled before fut is done
#[cfg(feature = "ordered-locks")]
pub async fn cancelable_checked<T, F: Future<Output = T>>(
    run_token: &RunToken,
    lock_token: LockToken<'_, L0>,
    fut: F,
) -> Result<T, CancelledError> {
    let c = run_token.cancelled_checked(lock_token);
    pin_mut!(fut, c);
    let f = future::select(c, fut).await;
    match f {
        future::Either::Right((v, _)) => Ok(v),
        future::Either::Left(_) => Err(CancelledError {}),
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub enum FinishState<'a> {
    Success,
    Drop,
    JoinError(JoinError),
    Failure(&'a (dyn std::fmt::Debug + Sync + Send)),
}

/// Builder to create a new task
pub struct TaskBuilder {
    id: usize,
    name: Cow<'static, str>,
    run_token: RunToken,
    critical: bool,
    main: bool,
    abort: bool,
    shutdown_order: i32,
}

impl TaskBuilder {
    /// Stract the construction of a new task with the given name
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            id: TASK_ID_COUNT.fetch_add(1, Ordering::SeqCst),
            name: name.into(),
            run_token: Default::default(),
            critical: false,
            main: false,
            abort: false,
            shutdown_order: 0,
        }
    }

    /// Unique id of the task we are creating
    pub fn id(&self) -> usize {
        self.id
    }

    /// Set the run_token for the task. It is sometimes nessesary to
    /// know the run_token of a task before it is created
    pub fn set_run_token(self, run_token: RunToken) -> Self {
        Self { run_token, ..self }
    }

    /// If the task fails the whole application should be stopped
    pub fn critical(self) -> Self {
        Self {
            critical: true,
            ..self
        }
    }

    /// If the task stops, the whole application should be stopped
    pub fn main(self) -> Self {
        Self { main: true, ..self }
    }

    /// Cancel the task by dropping the future, instead of only setting the cancel token
    pub fn abort(self) -> Self {
        Self {
            abort: true,
            ..self
        }
    }

    /// Tasks with a lower shutdown order are stopped earlier on shutdown
    pub fn shutdown_order(self, shutdown_order: i32) -> Self {
        Self {
            shutdown_order,
            ..self
        }
    }

    /// Create the new task
    pub fn create<
        T: 'static + Send + Sync,
        E: std::fmt::Debug + Sync + Send + 'static,
        Fu: Future<Output = Result<T, E>> + Send + 'static,
        F: FnOnce(RunToken) -> Fu,
    >(
        self,
        fun: F,
    ) -> Arc<Task<T, E>> {
        let fut = fun(self.run_token.clone());
        let id = self.id;
        //Lock here so we do not try to remove before inserting
        let mut tasks = TASKS.lock().unwrap();
        debug!("Started task {} ({})", self.name, id);
        let join_handle = tokio::spawn(async move {
            let g = scope_guard(|| {
                if let Some(t) = TASKS.lock().unwrap().remove(&id) {
                    t._internal_handle_finished(FinishState::Drop);
                }
            });
            let r = fut.await;
            let s = match &r {
                Ok(_) => FinishState::Success,
                Err(e) => FinishState::Failure(e),
            };
            g.release();
            if let Some(t) = TASKS.lock().unwrap().remove(&id) {
                t._internal_handle_finished(s);
            }
            r
        });
        let task = Arc::new(Task {
            id: self.id,
            name: self.name,
            critical: self.critical,
            main: self.main,
            abort: self.abort,
            shutdown_order: self.shutdown_order,
            run_token: self.run_token,
            start_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
            join_handle: Mutex::new(Some(join_handle)),
        });
        tasks.insert(self.id, task.clone());
        task
    }

    /// Create the new task also giving it a clean lock token
    #[cfg(feature = "ordered-locks")]
    pub fn create_with_lock_token<
        T: 'static + Send + Sync,
        E: std::fmt::Debug + Sync + Send + 'static,
        Fu: Future<Output = Result<T, E>> + Send + 'static,
        F: FnOnce(RunToken, CleanLockToken) -> Fu,
    >(
        self,
        fun: F,
    ) -> Arc<Task<T, E>> {
        self.create(|run_token| fun(run_token, unsafe { CleanLockToken::new() }))
    }
}

/// Base trait for all tasks, that is independent of the return type
pub trait TaskBase: Send + Sync {
    #[doc(hidden)]
    fn _internal_handle_finished(&self, state: FinishState);
    /// Return the shutdown order of this task as defined by the [TaskBuilder]
    fn shutdown_order(&self) -> i32;
    /// Return the name of this task as defined by the [TaskBuilder]
    fn name(&self) -> &str;
    /// Return the unique id of this task
    fn id(&self) -> usize;
    /// If true the application will shut down with an error if this task returns
    fn main(&self) -> bool;
    /// If this is true the task will be cancled by dropping the future instead of signaling the run token
    fn abort(&self) -> bool;
    /// If true the application will shut down with an error if this task returns with an error
    fn critical(&self) -> bool;
    /// Unixtimestamp of when the task started
    fn start_time(&self) -> f64;
    /// Cantle the task, return futer that returns when the task is done
    fn cancel(self: Arc<Self>) -> BoxFuture<'static, ()>;
    /// Get the run token associated with the task
    fn run_token(&self) -> &RunToken;
}

/// A possible running task, with a return value of `Result<T, E>`
pub struct Task<T: Send + Sync, E: Sync + Sync> {
    id: usize,
    name: Cow<'static, str>,
    critical: bool,
    main: bool,
    abort: bool,
    shutdown_order: i32,
    run_token: RunToken,
    start_time: f64,
    join_handle: Mutex<Option<JoinHandle<Result<T, E>>>>,
}

impl<T: Send + Sync + 'static, E: Send + Sync + 'static> TaskBase for Task<T, E> {
    fn shutdown_order(&self) -> i32 {
        self.shutdown_order
    }

    fn name(&self) -> &str {
        self.name.as_ref()
    }

    fn id(&self) -> usize {
        self.id
    }

    fn _internal_handle_finished(&self, state: FinishState) {
        match state {
            FinishState::Success => {
                if !self.main
                    || !shutdown(format!(
                        "Main task {} ({}) finished unexpected",
                        self.name, self.id
                    ))
                {
                    debug!("Finished task {} ({})", self.name, self.id);
                }
            }
            FinishState::Drop => {
                if self.main || self.critical {
                    if shutdown(format!("Critical task {} ({}) dropped", self.name, self.id)) {
                    } else if !self.abort {
                        // Task was dropped, but it is not allowed to be dropped
                        error!("Critical task {} ({}) dropped", self.name, self.id);
                    } else {
                        debug!("Critical task {} ({}) dropped", self.name, self.id)
                    }
                } else if !self.abort {
                    // Task was dropped, but it is not allowed to be dropped
                    error!("Task {} ({}) dropped", self.name, self.id);
                } else {
                    debug!("Task {} ({}) dropped", self.name, self.id)
                }
            }
            FinishState::JoinError(e) => {
                if (!self.main && !self.critical)
                    || !shutdown(format!(
                        "Join error in critical task {} ({}): {:?}",
                        self.name, self.id, e
                    ))
                {
                    error!("Join error in task {} ({}): {:?}", self.name, self.id, e);
                }
            }
            FinishState::Failure(e) => {
                if (!self.main && !self.critical)
                    || !shutdown(format!(
                        "Failure in critical task {} ({}) @ {:?}: {:?}",
                        self.name,
                        self.id,
                        self.run_token().location(),
                        e
                    ))
                {
                    let location = self.run_token().location();
                    error!(
                        "Failure in task {} ({}) @ {:?}: {:?}",
                        self.name, self.id, location, e
                    );
                }
            }
        }
    }

    fn cancel(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(self.cancel())
    }

    fn main(&self) -> bool {
        self.main
    }

    fn abort(&self) -> bool {
        self.abort
    }

    fn critical(&self) -> bool {
        self.critical
    }

    fn start_time(&self) -> f64 {
        self.start_time
    }

    fn run_token(&self) -> &RunToken {
        &self.run_token
    }
}

/// Error return while waiting for a task
#[derive(Debug)]
pub enum WaitError<E: Send + Sync> {
    /// The task has allready been sucessfully awaited
    HandleUnset(String),
    /// A join error happened while waiting for the task
    JoinError(tokio::task::JoinError),
    /// The task failed with error E
    TaskFailure(E),
}

impl<E: std::fmt::Display + Send + Sync> std::fmt::Display for WaitError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitError::HandleUnset(v) => write!(f, "Handle unset: {}", v),
            WaitError::JoinError(v) => write!(f, "Join Error: {}", v),
            WaitError::TaskFailure(v) => write!(f, "Task Failure: {}", v),
        }
    }
}

impl<E: std::error::Error + Send + Sync> std::error::Error for WaitError<E> {}

struct TaskJoinHandleBorrow<'a, T: Send + Sync, E: Send + Sync> {
    task: &'a Arc<Task<T, E>>,
    jh: Option<JoinHandle<Result<T, E>>>,
}

impl<'a, T: Send + Sync, E: Send + Sync> TaskJoinHandleBorrow<'a, T, E> {
    fn new(task: &'a Arc<Task<T, E>>) -> Self {
        let jh = task.join_handle.lock().unwrap().take();
        Self { task, jh }
    }
}

impl<'a, T: Send + Sync, E: Send + Sync> Drop for TaskJoinHandleBorrow<'a, T, E> {
    fn drop(&mut self) {
        *self.task.join_handle.lock().unwrap() = self.jh.take();
    }
}

impl<T: Send + Sync, E: Send + Sync> Task<T, E> {
    /// Cancel the task, either by setting the cancel_token or by aborting it.
    /// Wait for it to finish
    /// Note that this function fill fail t
    pub async fn cancel(self: Arc<Self>) {
        let mut b = TaskJoinHandleBorrow::new(&self);
        self.run_token.cancel();
        if let Some(jh) = &mut b.jh {
            if self.abort {
                jh.abort();
            }
            if let Err(e) = jh.await {
                info!("Unable to join task {:?}", e);
                if let Some(t) = TASKS.lock().unwrap().remove(&self.id) {
                    t._internal_handle_finished(FinishState::JoinError(e));
                }
            }
        }
        if !SHUTTING_DOWN.load(Ordering::SeqCst) {
            info!("  canceled {} ({})", self.name, self.id);
        }
        std::mem::forget(b);
    }

    /// Wait for the task to finish.
    pub async fn wait(self: Arc<Self>) -> Result<T, WaitError<E>> {
        let mut b = TaskJoinHandleBorrow::new(&self);
        let r = match &mut b.jh {
            None => Err(WaitError::HandleUnset(self.name.to_string())),
            Some(jh) => match jh.await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(WaitError::TaskFailure(e)),
                Err(e) => Err(WaitError::JoinError(e)),
            },
        };
        std::mem::forget(b);
        r
    }
}
struct WaitTasks<'a, Sleep, Fut>(Sleep, &'a mut Vec<(String, usize, Fut, RunToken)>);
impl<'a, Sleep: Unpin, Fut: Unpin> Unpin for WaitTasks<'a, Sleep, Fut> {}
impl<'a, Sleep: Future + Unpin, Fut: Future + Unpin> Future for WaitTasks<'a, Sleep, Fut> {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<bool> {
        if self.0.poll_unpin(cx).is_ready() {
            return Poll::Ready(false);
        }

        self.1
            .retain_mut(|(_, _, f, _)| !matches!(f.poll_unpin(cx), Poll::Ready(_)));

        if self.1.is_empty() {
            Poll::Ready(true)
        } else {
            Poll::Pending
        }
    }
}

/// Cancel all tasks in shutdown order
pub fn shutdown(message: String) -> bool {
    if SHUTTING_DOWN
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // Already in the process of shutting down
        return false;
    }
    info!("Shutting down: {}", message);
    tokio::spawn(async move {
        let mut shutdown_tasks: Vec<Arc<dyn TaskBase>> = Vec::new();
        loop {
            for (_, task) in TASKS.lock().unwrap().iter() {
                if let Some(t) = shutdown_tasks.get(0) {
                    if t.shutdown_order() < task.shutdown_order() {
                        continue;
                    }
                    if t.shutdown_order() > task.shutdown_order() {
                        shutdown_tasks.clear();
                    }
                }
                shutdown_tasks.push(task.clone());
            }
            if shutdown_tasks.is_empty() {
                break;
            }
            info!(
                "shutting down {} tasks with order {}",
                shutdown_tasks.len(),
                shutdown_tasks[0].shutdown_order()
            );
            let mut stop_futures: Vec<(String, usize, _, RunToken)> = shutdown_tasks
                .iter()
                .map(|t| (t.name().to_string(), t.id(), t.clone().cancel(), t.run_token().clone()))
                .collect();
            while !WaitTasks(
                Box::pin(tokio::time::sleep(tokio::time::Duration::from_secs(30))),
                &mut stop_futures,
            )
            .await
            {
                info!("still waiting for {} tasks", stop_futures.len(),);
                for (name, id, _, rt) in &stop_futures {
                    if let Some((file, line)) = rt.location() {
                        info!("  {} ({}) at {}:{}", name, id, file, line);
                    } else {
                        info!("  {} ({})", name, id);
                    }
                }
            }
            shutdown_tasks.clear();
        }
        info!("shutdown done");
        SHUTDOWN_NOTIFY.notify_waiters();
    });
    true
}

/// Wait until all tasks are done or shutdown has been called
pub async fn run_tasks() {
    SHUTDOWN_NOTIFY.notified().await
}

/// Return a list of all currently running tasks
pub fn list_tasks() -> Vec<Arc<dyn TaskBase>> {
    TASKS.lock().unwrap().values().cloned().collect()
}

/// Try to return a list of all currently running tasks,
/// if we cannot acquire the lock for the tasks before duration has passed return None
pub fn try_list_tasks_for(duration: std::time::Duration) -> Option<Vec<Arc<dyn TaskBase>>> {
    let tries = 50;
    for _ in 0..tries {
        if let Ok(tasks) = TASKS.try_lock() {
            return Some(tasks.values().cloned().collect());
        }
        std::thread::sleep(duration / tries);
    }
    if let Ok(tasks) = TASKS.try_lock() {
        return Some(tasks.values().cloned().collect());
    }
    None
}
