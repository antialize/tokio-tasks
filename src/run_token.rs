
//! Defines a run token that can be used to cancel async tasks
use futures_util::Future;

use std::{
    mem::MaybeUninit,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll, Waker},
};

use std::sync::atomic::AtomicPtr;
#[cfg(feature = "runtoken-id")]
use std::sync::atomic::AtomicU64;

#[cfg(feature = "ordered-locks")]
use ordered_locks::{L0, LockToken};

/// Next id used for run token ids
#[cfg(feature = "runtoken-id")]
static IDC: AtomicU64 = AtomicU64::new(0);

/// Intrusive circular linked list of T's
pub struct IntrusiveList<T> {
    /// Pointer to the first element in the list, the last element can be found
    /// as first->prev
    first: *mut ListNode<T>,
}

impl<T> Default for IntrusiveList<T> {
    fn default() -> Self {
        Self {
            first: std::ptr::null_mut(),
        }
    }
}

impl<T> IntrusiveList<T> {
    /// Add node to the list with the content v
    ///
    /// Safety:
    /// - node should be a valid pointer.
    /// - node should only be accessed by us
    /// - node should not be in a list
    /// - node should not have value added to it
    ///
    /// This will write a value to the node
    unsafe fn push_back(&mut self, node: *mut ListNode<T>, v: T) {
        unsafe {
            assert!((*node).next.is_null());
            (*node).data.write(v);
            if self.first.is_null() {
                (*node).next = node;
                (*node).prev = node;
                self.first = node;
            } else {
                (*node).prev = (*self.first).prev;
                (*node).next = self.first;
                (*(*node).prev).next = node;
                (*(*node).next).prev = node;
            }
        }
    }

    /// Remove node to the list returning its content
    ///
    /// Safety:
    /// - node should be a valid pointer.
    /// - node should only be accessed by us
    /// - node should be in a list
    ///
    /// The value will be read out from the node
    unsafe fn remove(&mut self, node: *mut ListNode<T>) -> T {
        unsafe {
            assert!(!(*node).next.is_null());
            let v = (*node).data.as_mut_ptr().read();
            if (*node).next == node {
                self.first = std::ptr::null_mut();
            } else {
                if self.first == node {
                    self.first = (*node).next;
                }
                (*(*node).next).prev = (*node).prev;
                (*(*node).prev).next = (*node).next;
            }
            (*node).next = std::ptr::null_mut();
            (*node).prev = std::ptr::null_mut();
            v
        }
    }

    /// Remove all entries from this list
    fn drain(&mut self, v: impl Fn(T)) {
        unsafe {
            if self.first.is_null() {
                return;
            }
            let mut cur = self.first;
            loop {
                v((*cur).data.as_mut_ptr().read());
                let next = (*cur).next;
                (*cur).next = std::ptr::null_mut();
                (*cur).prev = std::ptr::null_mut();
                if next == self.first {
                    break;
                }
                cur = next;
            }
            self.first = std::ptr::null_mut();
        }
    }

    /// Check if the node is in a list
    ///
    /// Safety:
    /// - node should be a valid pointer
    /// - No one should modify node while we access it
    unsafe fn in_list(&self, node: *mut ListNode<T>) -> bool {
        unsafe { !(*node).next.is_null() }
    }
}

/// Node uned in the linked list
pub struct ListNode<T> {
    /// The previous element in the list
    prev: *mut ListNode<T>,
    /// The next element in the node
    next: *mut ListNode<T>,
    /// The data contained in this node
    data: std::mem::MaybeUninit<T>,
    /// Make sure we do not implement unpin
    _pin: std::marker::PhantomPinned,
}

impl<T> Default for ListNode<T> {
    fn default() -> Self {
        Self {
            prev: std::ptr::null_mut(),
            next: std::ptr::null_mut(),
            data: MaybeUninit::uninit(),
            _pin: Default::default(),
        }
    }
}

/// The state a [RunToken] is in
enum State {
    /// The [RunToken] is running
    Run,
    /// The [RunToken] has been canceled
    Cancel,
    /// The task should paused
    #[cfg(feature = "pause")]
    Pause,
}

/// Inner content of a [RunToken] behind a Mutex
struct Content {
    /// The state of the run token
    state: State,
    /// Wakers to wake when cancelling the [RunToken]
    cancel_wakers: IntrusiveList<Waker>,
    /// Wakers to wake when unpausing the [RunToken]
    run_wakers: IntrusiveList<Waker>,
}

unsafe impl Send for Content {}

impl Content {
    /// Wake waker when the [RunToken] is cancelled
    ///
    /// Safety:
    /// - node must be a valid pointer
    /// - node must not contain a value if it is not in a list
    unsafe fn add_cancel_waker(&mut self, node: *mut ListNode<Waker>, waker: &Waker) {
        unsafe {
            if !self.cancel_wakers.in_list(node) {
                self.cancel_wakers.push_back(node, waker.clone())
            }
        }
    }

    /// Wake waker when the [RunToken] is unpaused
    ///
    /// Safety:
    /// - node must be a valid pointer
    /// - node must not contain a value if it is not in a list
    #[cfg(feature = "pause")]
    unsafe fn add_run_waker(&mut self, node: *mut ListNode<Waker>, waker: &Waker) {
        unsafe {
            if !self.run_wakers.in_list(node) {
                self.run_wakers.push_back(node, waker.clone())
            }
        }
    }

    /// Remove node from the list of nodes to be woken when the run token is
    /// cancelled
    ///
    /// Safety:
    /// - node must be a valid pointer
    /// - if the node is in a list, it mut be the cancel_wakers list
    unsafe fn remove_cancel_waker(&mut self, node: *mut ListNode<Waker>) {
        unsafe {
            if self.cancel_wakers.in_list(node) {
                self.cancel_wakers.remove(node);
            }
        }
    }

    /// Remove node from the list of nodes to be woken when the run token is
    /// unpaused
    ///
    /// Safety:
    /// - node must be a valid pointer
    /// - if the node is in a list, it mut be the run_wakers list
    #[cfg(feature = "pause")]
    unsafe fn remove_run_waker(&mut self, node: *mut ListNode<Waker>) {
        unsafe {
            if self.run_wakers.in_list(node) {
                self.run_wakers.remove(node);
            }
        }
    }
}

/// Inner content of a [RunToken] not behind a mutex
struct Inner {
    /// Condition notified on cancel and unpause
    cond: std::sync::Condvar,
    /// Inner content of this [RunToken] that must be accessed exclusively
    content: std::sync::Mutex<Content>,
    /// The id unique of this run token
    #[cfg(feature = "runtoken-id")]
    id: u64,
    /// The location last set on this run-token, mut be a valid pointer to a
    /// &' static str of the form "file:line" or null
    location_file_line: AtomicPtr<u8>,
}
/// Similar to a [`tokio_util::sync::CancellationToken`],
/// the RunToken encapsulates the possibility of canceling an async command.
/// However it also allows pausing and resuming the async command,
/// and it is possible to wait in both a blocking fashion and an asynchronous fashion.
#[derive(Clone)]
pub struct RunToken(Arc<Inner>);

impl RunToken {
    /// Construct a new paused run token
    #[cfg(feature = "pause")]
    pub fn new_paused() -> Self {
        Self(Arc::new(Inner {
            cond: std::sync::Condvar::new(),
            content: std::sync::Mutex::new(Content {
                state: State::Pause,
                cancel_wakers: Default::default(),
                run_wakers: Default::default(),
            }),
            location_file_line: Default::default(),
            #[cfg(feature = "runtoken-id")]
            id: IDC.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        }))
    }

    /// Construct a new running run token
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            cond: std::sync::Condvar::new(),
            content: std::sync::Mutex::new(Content {
                state: State::Run,
                cancel_wakers: Default::default(),
                run_wakers: Default::default(),
            }),
            location_file_line: Default::default(),
            #[cfg(feature = "runtoken-id")]
            id: IDC.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        }))
    }

    /// Cancel computation
    pub fn cancel(&self) {
        let mut content = self.0.content.lock().unwrap();
        if matches!(content.state, State::Cancel) {
            return;
        }
        content.state = State::Cancel;

        content.run_wakers.drain(|w| w.wake());
        content.cancel_wakers.drain(|w| w.wake());
        self.0.cond.notify_all();
    }

    /// Pause computation if we are running
    #[cfg(feature = "pause")]
    pub fn pause(&self) {
        let mut content = self.0.content.lock().unwrap();
        if !matches!(content.state, State::Run) {
            return;
        }
        content.state = State::Pause;
    }

    /// Resume computation if we are paused
    #[cfg(feature = "pause")]
    pub fn resume(&self) {
        let mut content = self.0.content.lock().unwrap();
        if !matches!(content.state, State::Pause) {
            return;
        }
        content.state = State::Run;
        content.run_wakers.drain(|w| w.wake());
        self.0.cond.notify_all();
    }

    /// Return true iff we are canceled
    pub fn is_cancelled(&self) -> bool {
        matches!(self.0.content.lock().unwrap().state, State::Cancel)
    }

    /// Return true iff we are paused
    #[cfg(feature = "pause")]
    pub fn is_paused(&self) -> bool {
        matches!(self.0.content.lock().unwrap().state, State::Pause)
    }

    /// Return true iff we are runnig
    #[cfg(feature = "pause")]
    pub fn is_running(&self) -> bool {
        matches!(self.0.content.lock().unwrap().state, State::Run)
    }

    /// Block the thread until we are not paused, and then return true if we are canceled or false if we are running
    #[cfg(feature = "pause")]
    pub fn wait_paused_check_cancelled_sync(&self) -> bool {
        let mut content = self.0.content.lock().unwrap();
        loop {
            match &content.state {
                State::Run => return false,
                State::Cancel => return true,
                State::Pause => {
                    content = self.0.cond.wait(content).unwrap();
                }
            }
        }
    }

    /// Suspend the async coroutine until we are not paused, and then return true if we are canceled or false if we are running
    #[cfg(feature = "pause")]
    pub fn wait_paused_check_cancelled(&self) -> WaitForPauseFuture<'_> {
        WaitForPauseFuture {
            token: self,
            waker: Default::default(),
        }
    }

    /// Suspend the async coroutine until cancel() is called
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        WaitForCancellationFuture {
            token: self,
            waker: Default::default(),
        }
    }

    /// Suspend the async coroutine until cancel() is called, checking that we do not hold
    /// a too deep lock
    #[cfg(feature = "ordered-locks")]
    pub fn cancelled_checked(
        &self,
        _lock_token: LockToken<'_, L0>,
    ) -> WaitForCancellationFuture<'_> {
        WaitForCancellationFuture {
            token: self,
            waker: Default::default(),
        }
    }

    /// Store a file line location in the run_token
    /// The string must be on the form "file:line\0"
    ///
    /// You probably want to call the [set_location] macro instead
    #[inline]
    pub fn set_location_file_line(&self, file_line_str: &'static str) {
        assert!(file_line_str.ends_with('\0'));
        self.0
            .location_file_line
            .store(file_line_str.as_ptr() as *mut u8, Ordering::Relaxed);
    }

    /// Retrieve the stored file,live location in the run_token
    pub fn location(&self) -> Option<(&'static str, u32)> {
        let location_file_line = self.0.location_file_line.load(Ordering::Relaxed) as *const u8;
        if location_file_line.is_null() {
            return None;
        }
        let mut len = 0;
        // SAFETY: File points to a utf-8 string ending with "\0"
        // This is checked by _set_location
        let location_file_line = unsafe {
            while *location_file_line.add(len) != b'0' {
                len += 1;
            }
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(location_file_line, len))
        };
        let (file, line) = location_file_line
            .rsplit_once(":")
            .expect(": in location_file_line");
        let line = line.parse().expect("Line number after :");
        Some((file, line))
    }

    /// The unique incremental id of this run token
    #[cfg(feature = "runtoken-id")]
    #[inline]
    pub fn id(&self) -> u64 {
        self.0.id
    }
}

/// Update the location stored in a run token to the current file:line
#[macro_export]
macro_rules! set_location {
    ($run_token: expr) => {
        $run_token.set_location_file_line(concat!(file!(), ":", line!(), "\0"));
    };
}

impl Default for RunToken {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for RunToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_tuple("RunToken");
        match self.0.content.lock().unwrap().state {
            State::Run => d.field(&"Running"),
            State::Cancel => d.field(&"Canceled"),
            #[cfg(feature = "pause")]
            State::Pause => d.field(&"Paused"),
        };
        d.finish()
    }
}

/// Wait until task cancellation is completed
///
/// Note: [std::mem::forget]ting this future may crash your application,
/// a pointer to the content of this future is stored in the RunToken
#[must_use = "futures do nothing unless polled"]
pub struct WaitForCancellationFuture<'a> {
    /// The token to wait for
    token: &'a RunToken,
    /// Entry in the cancel_wakers list of the RunToken
    waker: ListNode<Waker>,
}

impl<'a> core::fmt::Debug for WaitForCancellationFuture<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WaitForCancellationFuture").finish()
    }
}

impl<'a> Future for WaitForCancellationFuture<'a> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut content = self.token.0.content.lock().unwrap();
        match content.state {
            State::Cancel => Poll::Ready(()),
            State::Run => {
                unsafe {
                    content.add_cancel_waker(&mut Pin::get_unchecked_mut(self).waker, cx.waker());
                }
                Poll::Pending
            }
            #[cfg(feature = "pause")]
            State::Pause => {
                unsafe {
                    content.add_cancel_waker(&mut Pin::get_unchecked_mut(self).waker, cx.waker());
                }
                Poll::Pending
            }
        }
    }
}

impl<'a> Drop for WaitForCancellationFuture<'a> {
    fn drop(&mut self) {
        unsafe {
            self.token
                .0
                .content
                .lock()
                .unwrap()
                .remove_cancel_waker(&mut self.waker);
        }
    }
}

unsafe impl<'a> Send for WaitForCancellationFuture<'a> {}

/// Wait until task is not paused
///
/// Note: [std::mem::forget]ting this future may crash your application,
/// a pointer to the content of this future is stored in the RunToken
#[cfg(feature = "pause")]
#[must_use = "futures do nothing unless polled"]
pub struct WaitForPauseFuture<'a> {
    /// The run toke to wait to unpause
    token: &'a RunToken,
    /// Entry in the run_wakers list of the RunToken
    waker: ListNode<Waker>,
}

#[cfg(feature = "pause")]
impl<'a> core::fmt::Debug for WaitForPauseFuture<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WaitForPauseFuture").finish()
    }
}

#[cfg(feature = "pause")]
impl<'a> Future for WaitForPauseFuture<'a> {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let mut content = self.token.0.content.lock().unwrap();
        match content.state {
            State::Cancel => Poll::Ready(true),
            State::Run => Poll::Ready(false),
            State::Pause => {
                unsafe {
                    content.add_run_waker(&mut Pin::get_unchecked_mut(self).waker, cx.waker());
                }
                Poll::Pending
            }
        }
    }
}

#[cfg(feature = "pause")]
impl<'a> Drop for WaitForPauseFuture<'a> {
    fn drop(&mut self) {
        unsafe {
            self.token
                .0
                .content
                .lock()
                .unwrap()
                .remove_run_waker(&mut self.waker);
        }
    }
}

#[cfg(feature = "pause")]
unsafe impl<'a> Send for WaitForPauseFuture<'a> {}
