//! Database-wide admission control for engine-owned working memory.
//!
//! The governor deliberately accounts estimated operator/index memory rather
//! than attempting to replace Rust's allocator. Each pool has an independent
//! ceiling and all pools, plus the emergency reserve, fit under one configured
//! database budget. Query and maintenance reservations are RAII permits;
//! retained index deltas are reconciled after every publish/consolidation.

use crate::error::{Error, Result};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub(crate) struct MemoryLimits {
    pub total: usize,
    pub query: usize,
    pub index_delta: usize,
    pub maintenance: usize,
    pub reserve: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalMemoryStats {
    pub total_bytes: u64,
    pub reserved_bytes: u64,
    pub query_capacity_bytes: u64,
    pub query_in_use_bytes: u64,
    pub query_peak_bytes: u64,
    pub query_waits: u64,
    pub index_delta_capacity_bytes: u64,
    pub index_delta_bytes: u64,
    pub index_delta_peak_bytes: u64,
    pub index_consolidations: u64,
    pub maintenance_capacity_bytes: u64,
    pub maintenance_in_use_bytes: u64,
    pub maintenance_peak_bytes: u64,
    pub maintenance_waits: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MemoryPool {
    Query,
    Maintenance,
}

#[derive(Debug, Default)]
struct PoolState {
    used: usize,
    peak: usize,
    waits: u64,
}

#[derive(Debug, Default)]
struct GovernorState {
    query: PoolState,
    maintenance: PoolState,
    index_delta: usize,
    index_delta_peak: usize,
    index_consolidations: u64,
}

#[derive(Debug)]
pub(crate) struct MemoryGovernor {
    limits: MemoryLimits,
    state: Mutex<GovernorState>,
    available: Condvar,
}

impl MemoryGovernor {
    pub(crate) fn new(limits: MemoryLimits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: Mutex::new(GovernorState::default()),
            available: Condvar::new(),
        })
    }

    pub(crate) fn acquire(self: &Arc<Self>, pool: MemoryPool, bytes: usize) -> MemoryPermit {
        self.acquire_with_deadline(pool, bytes, None)
            .expect("validated maintenance reservation")
    }

    pub(crate) fn acquire_timeout(
        self: &Arc<Self>,
        pool: MemoryPool,
        bytes: usize,
        timeout: Duration,
    ) -> Result<MemoryPermit> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            Error::InvalidArgument("query admission timeout is out of range".into())
        })?;
        self.acquire_with_deadline(pool, bytes, Some(deadline))
    }

    fn acquire_with_deadline(
        self: &Arc<Self>,
        pool: MemoryPool,
        bytes: usize,
        deadline: Option<Instant>,
    ) -> Result<MemoryPermit> {
        let capacity = self.capacity(pool);
        let control = matches!(pool, MemoryPool::Query)
            .then(crate::QueryControl::current)
            .flatten();
        if bytes > capacity {
            return Err(Error::MemoryLimit(
                "reservation exceeds its memory pool".into(),
            ));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut counted_wait = false;
        loop {
            if let Some(control) = &control {
                control.check()?;
            }
            let target = match pool {
                MemoryPool::Query => &mut state.query,
                MemoryPool::Maintenance => &mut state.maintenance,
            };
            if target.used.saturating_add(bytes) <= capacity {
                target.used += bytes;
                target.peak = target.peak.max(target.used);
                return Ok(MemoryPermit {
                    governor: self.clone(),
                    pool,
                    bytes,
                });
            }
            if !counted_wait {
                target.waits = target.waits.saturating_add(1);
                counted_wait = true;
            }
            state = if let Some(deadline) = deadline {
                let mut remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::MemoryLimit(
                        "query memory admission timed out".into(),
                    ));
                }
                if let Some(control) = &control {
                    remaining = remaining
                        .min(control.remaining().unwrap_or(Duration::from_millis(50)))
                        .min(Duration::from_millis(50));
                }
                self.available
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0
            } else {
                self.available
                    .wait(state)
                    .unwrap_or_else(|poison| poison.into_inner())
            };
        }
    }

    /// Account `bytes` that already exist without waiting for room. A frozen
    /// heap moving out of the index-delta pool is real memory whether or not
    /// the maintenance pool has capacity left, so blocking here would only
    /// delay the consolidation that frees it. `used` may exceed the capacity
    /// until the permit drops; blocking reservations wait for that.
    pub(crate) fn acquire_unbounded(
        self: &Arc<Self>,
        pool: MemoryPool,
        bytes: usize,
    ) -> MemoryPermit {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let target = match pool {
            MemoryPool::Query => &mut state.query,
            MemoryPool::Maintenance => &mut state.maintenance,
        };
        target.used = target.used.saturating_add(bytes);
        target.peak = target.peak.max(target.used);
        MemoryPermit {
            governor: self.clone(),
            pool,
            bytes,
        }
    }

    pub(crate) fn index_would_exceed(&self, additional: usize) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.index_delta.saturating_add(additional) > self.limits.index_delta
    }

    pub(crate) fn set_index_delta_bytes(&self, bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.index_delta = bytes;
        state.index_delta_peak = state.index_delta_peak.max(bytes);
        self.available.notify_all();
    }

    pub(crate) fn add_index_delta_bytes(&self, bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.index_delta = state.index_delta.saturating_add(bytes);
        state.index_delta_peak = state.index_delta_peak.max(state.index_delta);
    }

    pub(crate) fn record_index_consolidation(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.index_consolidations = state.index_consolidations.saturating_add(1);
    }

    pub(crate) fn index_capacity(&self) -> usize {
        self.limits.index_delta
    }

    pub(crate) fn stats(&self) -> GlobalMemoryStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        GlobalMemoryStats {
            total_bytes: self.limits.total as u64,
            reserved_bytes: self.limits.reserve as u64,
            query_capacity_bytes: self.limits.query as u64,
            query_in_use_bytes: state.query.used as u64,
            query_peak_bytes: state.query.peak as u64,
            query_waits: state.query.waits,
            index_delta_capacity_bytes: self.limits.index_delta as u64,
            index_delta_bytes: state.index_delta as u64,
            index_delta_peak_bytes: state.index_delta_peak as u64,
            index_consolidations: state.index_consolidations,
            maintenance_capacity_bytes: self.limits.maintenance as u64,
            maintenance_in_use_bytes: state.maintenance.used as u64,
            maintenance_peak_bytes: state.maintenance.peak as u64,
            maintenance_waits: state.maintenance.waits,
        }
    }

    fn capacity(&self, pool: MemoryPool) -> usize {
        match pool {
            MemoryPool::Query => self.limits.query,
            MemoryPool::Maintenance => self.limits.maintenance,
        }
    }

    fn release(&self, pool: MemoryPool, bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let target = match pool {
            MemoryPool::Query => &mut state.query,
            MemoryPool::Maintenance => &mut state.maintenance,
        };
        target.used = target.used.saturating_sub(bytes);
        self.available.notify_all();
    }
}

#[derive(Debug)]
pub(crate) struct MemoryPermit {
    governor: Arc<MemoryGovernor>,
    pool: MemoryPool,
    bytes: usize,
}

impl MemoryPermit {
    pub(crate) fn shrink_to(&mut self, bytes: usize) {
        assert!(bytes <= self.bytes);
        self.governor.release(self.pool, self.bytes - bytes);
        self.bytes = bytes;
    }
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.governor.release(self.pool, self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn admission_expires_and_transferred_memory_is_not_clamped() {
        let governor = MemoryGovernor::new(MemoryLimits {
            total: 100,
            query: 40,
            index_delta: 20,
            maintenance: 30,
            reserve: 10,
        });
        let transferred = governor.acquire_unbounded(MemoryPool::Maintenance, 75);
        assert_eq!(governor.stats().maintenance_in_use_bytes, 75);
        drop(transferred);
        assert_eq!(governor.stats().maintenance_in_use_bytes, 0);
        let held = governor.acquire(MemoryPool::Query, 40);
        assert!(matches!(
            governor.acquire_timeout(MemoryPool::Query, 1, Duration::from_millis(20)),
            Err(Error::MemoryLimit(_))
        ));
        assert_eq!(governor.stats().query_in_use_bytes, 40);
        drop(held);
        assert!(governor
            .acquire_timeout(MemoryPool::Query, 40, Duration::ZERO)
            .is_ok());
    }

    #[test]
    fn query_permits_wait_and_wake() {
        let governor = MemoryGovernor::new(MemoryLimits {
            total: 100,
            query: 40,
            index_delta: 20,
            maintenance: 30,
            reserve: 10,
        });
        let held = governor.acquire(MemoryPool::Query, 40);
        let next = governor.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = next.acquire(MemoryPool::Query, 40);
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
        assert_eq!(governor.stats().query_waits, 1);
    }
}
