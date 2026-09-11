//! Cooperative cancellation and deadlines at query work boundaries.
use crate::{Error, Result};
use std::cell::RefCell;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct QueryControl {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

thread_local! { static CURRENT: RefCell<Option<QueryControl>> = const { RefCell::new(None) }; }

impl QueryControl {
    pub fn with_timeout(timeout: Duration) -> Result<Self> {
        Ok(Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Some(
                Instant::now().checked_add(timeout).ok_or_else(|| {
                    Error::InvalidArgument("query timeout is out of range".into())
                })?,
            ),
        })
    }
    /// May be called from another thread. Work stops at its next checkpoint.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Error::QueryInterrupted("cancelled".into()));
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::QueryInterrupted("deadline exceeded".into()));
        }
        Ok(())
    }
    pub(crate) fn current() -> Option<Self> {
        CURRENT.with(|current| current.borrow().clone())
    }
    pub(crate) fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
    pub(crate) fn without<T>(execute: impl FnOnce() -> T) -> T {
        let previous = CURRENT.with(|current| current.take());
        let _guard = Scope(previous);
        execute()
    }
    /// Apply this control to synchronous query work in the closure. Nested
    /// scopes restore their previous control, including on error or panic.
    pub fn run<T>(&self, execute: impl FnOnce() -> Result<T>) -> Result<T> {
        self.check()?;
        let previous = CURRENT.with(|current| current.replace(Some(self.clone())));
        let _guard = Scope(previous);
        execute()
    }
}

struct Scope(Option<QueryControl>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

pub(crate) fn check_current() -> Result<()> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(Ok(()), QueryControl::check)
    })
}
