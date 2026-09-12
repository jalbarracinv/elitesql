//! Durability barriers. `fsync` on macOS only pushes data to the drive, not
//! through its volatile write cache; `F_FULLFSYNC` does. It is an order of
//! magnitude slower, so it is opt-in (`DbOptions::full_fsync`), as in SQLite.
//! The setting is process-wide because every durability point of every open
//! database in this process must use the same barrier.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};

static FULL_FSYNC: AtomicBool = AtomicBool::new(false);

/// Request full drive-cache flushes for every later barrier in this process.
/// Once enabled it stays enabled: a weaker barrier must never follow a
/// stronger one that an application already relied on.
pub(crate) fn enable_full_fsync() {
    FULL_FSYNC.store(true, Ordering::Release);
}

pub(crate) fn full_fsync_enabled() -> bool {
    FULL_FSYNC.load(Ordering::Acquire)
}

#[cfg(target_os = "macos")]
fn full_fsync(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_FULLFSYNC: i32 = 51;
    // SAFETY: fcntl(F_FULLFSYNC) takes no further arguments and only acts on
    // the open descriptor owned by `file`.
    if unsafe { fcntl(file.as_raw_fd(), F_FULLFSYNC) } == 0 {
        Ok(())
    } else {
        // Some filesystems (network mounts, FAT) reject F_FULLFSYNC; fall
        // back to the ordinary barrier rather than failing the commit.
        file.sync_all()
    }
}

#[cfg(not(target_os = "macos"))]
fn full_fsync(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

/// Data-only barrier for append-only logs.
pub(crate) fn sync_data(file: &File) -> std::io::Result<()> {
    if full_fsync_enabled() {
        full_fsync(file)
    } else {
        file.sync_data()
    }
}

/// Data-and-metadata barrier for files published by rename.
pub(crate) fn sync_all(file: &File) -> std::io::Result<()> {
    if full_fsync_enabled() {
        full_fsync(file)
    } else {
        file.sync_all()
    }
}
