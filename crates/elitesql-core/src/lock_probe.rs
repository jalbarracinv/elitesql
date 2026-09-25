//! The state lock, and the read view published with it.
//!
//! Every release of the write lock publishes a view of the state built while
//! the lock is still held (`ProbedRwLock::view`), so a reader that only needs
//! committed data takes an `Arc` from a mutex held for a pointer copy and
//! never queues behind a writer. The lock itself remains for writers and for
//! the reads that still need it.
//!
//! With `ELITESQL_LOCK_PROBE=<file>` set, every acquisition is attributed to
//! its call site: holds, hold time, wait time, and "blame", the part of a
//! read hold that overlapped a writer waiting for the lock. A background
//! thread rewrites `<file>` every two seconds.
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{
    Arc, LockResult, Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::time::{Duration, Instant};

#[derive(Default, Clone, Copy)]
struct Stat {
    count: u64,
    hold_ns: u64,
    max_hold_ns: u64,
    wait_ns: u64,
    blame_ns: u64,
}

type Site = (&'static Location<'static>, bool);
type Stats = Arc<Mutex<HashMap<Site, Stat>>>;

struct Probe {
    epoch: Instant,
    enabled: bool,
    threads: Mutex<Vec<Stats>>,
    /// Nanoseconds since `epoch` at which the current writer started to
    /// wait, or 0.
    writer_waiting_since: AtomicU64,
}

fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(|| {
        let path = std::env::var_os("ELITESQL_LOCK_PROBE");
        let probe = Probe {
            epoch: Instant::now(),
            enabled: path.is_some(),
            threads: Mutex::new(Vec::new()),
            writer_waiting_since: AtomicU64::new(0),
        };
        if let Some(path) = path {
            // One file per process: the offline check a sweep runs at the
            // end would otherwise overwrite the server's figures.
            let mut path = path;
            path.push(format!(".{}", std::process::id()));
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                dump(&path);
            });
        }
        probe
    })
}

thread_local! {
    static LOCAL: Stats = {
        let stats: Stats = Arc::default();
        probe().threads.lock().unwrap().push(stats.clone());
        stats
    };
}

fn now_ns(probe: &Probe) -> u64 {
    probe.epoch.elapsed().as_nanos() as u64
}

fn record(site: Site, acquired: Instant, wait: Duration) {
    let probe = probe();
    let hold = acquired.elapsed();
    let mut blame = 0u64;
    if !site.1 {
        let since = probe.writer_waiting_since.load(Ordering::Acquire);
        if since != 0 {
            let acquired_ns = now_ns(probe).saturating_sub(hold.as_nanos() as u64);
            blame = now_ns(probe).saturating_sub(since.max(acquired_ns));
        }
    }
    LOCAL.with(|stats| {
        let mut stats = stats.lock().unwrap();
        let stat = stats.entry(site).or_default();
        stat.count += 1;
        stat.hold_ns += hold.as_nanos() as u64;
        stat.max_hold_ns = stat.max_hold_ns.max(hold.as_nanos() as u64);
        stat.wait_ns += wait.as_nanos() as u64;
        stat.blame_ns += blame;
    });
}

fn dump(path: &std::ffi::OsStr) {
    let mut total: HashMap<Site, Stat> = HashMap::new();
    for stats in probe().threads.lock().unwrap().iter() {
        for (site, stat) in stats.lock().unwrap().iter() {
            let entry = total.entry(*site).or_default();
            entry.count += stat.count;
            entry.hold_ns += stat.hold_ns;
            entry.max_hold_ns = entry.max_hold_ns.max(stat.max_hold_ns);
            entry.wait_ns += stat.wait_ns;
            entry.blame_ns += stat.blame_ns;
        }
    }
    let mut rows: Vec<_> = total.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.blame_ns
            .cmp(&a.1.blame_ns)
            .then(b.1.hold_ns.cmp(&a.1.hold_ns))
    });
    let mut out =
        String::from("kind\tcount\ttotal_hold_us\tmax_hold_us\ttotal_wait_us\tblame_us\tsite\n");
    for ((location, write), stat) in rows {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}:{}\n",
            if write { "W" } else { "R" },
            stat.count,
            stat.hold_ns / 1000,
            stat.max_hold_ns / 1000,
            stat.wait_ns / 1000,
            stat.blame_ns / 1000,
            location.file(),
            location.line(),
        ));
    }
    let _ = std::fs::write(path, out);
}

pub(crate) struct ProbedRwLock<T, V> {
    inner: RwLock<T>,
    /// The view built from the state at the last write release. Readers
    /// share it, so taking the view only contends with the brief pointer
    /// swap of a publication; a mutex here put every read of every thread
    /// in one queue.
    view: RwLock<Arc<V>>,
    build: fn(&T) -> V,
}

pub(crate) struct ProbedReadGuard<'a, T> {
    guard: RwLockReadGuard<'a, T>,
    probe: Option<(Site, Instant, Duration)>,
}

pub(crate) struct ProbedWriteGuard<'a, T, V> {
    guard: RwLockWriteGuard<'a, T>,
    probe: Option<(Site, Instant, Duration)>,
    lock: &'a ProbedRwLock<T, V>,
}

impl<T, V> ProbedRwLock<T, V> {
    pub(crate) fn new(value: T, build: fn(&T) -> V) -> Self {
        let view = RwLock::new(Arc::new(build(&value)));
        Self {
            inner: RwLock::new(value),
            view,
            build,
        }
    }

    /// The view published at the last write release.
    pub(crate) fn view(&self) -> Arc<V> {
        self.view
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    #[track_caller]
    pub(crate) fn read(&self) -> LockResult<ProbedReadGuard<'_, T>> {
        let location = Location::caller();
        let started = probe().enabled.then(Instant::now);
        let wrap = |guard| ProbedReadGuard {
            guard,
            probe: started.map(|started| ((location, false), Instant::now(), started.elapsed())),
        };
        match self.inner.read() {
            Ok(guard) => Ok(wrap(guard)),
            Err(poison) => Err(PoisonError::new(wrap(poison.into_inner()))),
        }
    }

    #[track_caller]
    pub(crate) fn write(&self) -> LockResult<ProbedWriteGuard<'_, T, V>> {
        let location = Location::caller();
        let probe = probe();
        let enabled = probe.enabled;
        let started = enabled.then(Instant::now);
        if enabled {
            let _ = probe.writer_waiting_since.compare_exchange(
                0,
                now_ns(probe).max(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        let result = self.inner.write();
        if enabled {
            probe.writer_waiting_since.store(0, Ordering::Release);
        }
        let wrap = |guard| ProbedWriteGuard {
            guard,
            probe: started.map(|started| ((location, true), Instant::now(), started.elapsed())),
            lock: self,
        };
        match result {
            Ok(guard) => Ok(wrap(guard)),
            Err(poison) => Err(PoisonError::new(wrap(poison.into_inner()))),
        }
    }
}

impl<T> Deref for ProbedReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> Drop for ProbedReadGuard<'_, T> {
    fn drop(&mut self) {
        if let Some((site, acquired, wait)) = self.probe {
            record(site, acquired, wait);
        }
    }
}

impl<T, V> Deref for ProbedWriteGuard<'_, T, V> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T, V> DerefMut for ProbedWriteGuard<'_, T, V> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T, V> Drop for ProbedWriteGuard<'_, T, V> {
    fn drop(&mut self) {
        // Published while the write lock is still held: no reader of the
        // lock can observe a state newer than the view.
        let view = Arc::new((self.lock.build)(&self.guard));
        let previous = std::mem::replace(
            &mut *self
                .lock
                .view
                .write()
                .unwrap_or_else(|poison| poison.into_inner()),
            view,
        );
        // Freed, when this was its last holder, outside the view mutex.
        drop(previous);

        if let Some((site, acquired, wait)) = self.probe {
            record(site, acquired, wait);
        }
    }
}
