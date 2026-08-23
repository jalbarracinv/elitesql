//! Isolates whether reserving WAL length changes strict sync latency.
//!
//! The initial resize and sync are outside the measured window. Each sample
//! writes one 4 KiB frame and calls the same `File::sync_data` primitive used
//! by EliteSQL's WAL writer.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const ITERATIONS: usize = 100;
const REPETITIONS: usize = 5;
const FRAME_BYTES: usize = 4 * 1024;
const RESERVED_BYTES: u64 = 64 * 1024 * 1024;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .to_path_buf()
}

fn run(preallocated: bool) -> Result<Vec<u64>, Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("probe.wal");
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)?;
    if preallocated {
        file.set_len(RESERVED_BYTES)?;
        file.sync_data()?;
        file.seek(SeekFrom::Start(0))?;
    }
    let frame = vec![0x5a; FRAME_BYTES];
    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let started = Instant::now();
        file.write_all(&frame)?;
        file.sync_data()?;
        samples.push(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
    }
    samples.sort_unstable();
    Ok(samples)
}

fn percentile_us(samples: &[u64], percentile: usize) -> f64 {
    let index = (samples.len() * percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index] as f64 / 1_000.0
}

fn main() -> Result<(), Box<dyn Error>> {
    let csv_path = std::env::args()
        .skip(1)
        .find(|arg| arg != "--bench")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            workspace_root().join("benchmark-results/wal-preallocation-2026-08-23.csv")
        });
    let mut csv = String::from("mode,repetition,iterations,frame_bytes,p50_us,p95_us,mean_us\n");
    for repetition in 1..=REPETITIONS {
        let order = if repetition % 2 == 1 {
            [false, true]
        } else {
            [true, false]
        };
        for preallocated in order {
            let samples = run(preallocated)?;
            let mode = if preallocated {
                "preallocated"
            } else {
                "growing"
            };
            let mean_us = samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1_000.0;
            let p50_us = percentile_us(&samples, 50);
            let p95_us = percentile_us(&samples, 95);
            println!(
                "{mode:<12} run={repetition} p50={p50_us:.1} us p95={p95_us:.1} us mean={mean_us:.1} us"
            );
            csv.push_str(&format!(
                "{mode},{repetition},{ITERATIONS},{FRAME_BYTES},{p50_us:.3},{p95_us:.3},{mean_us:.3}\n"
            ));
        }
    }
    if let Some(parent) = csv_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&csv_path, csv)?;
    println!("CSV: {}", csv_path.display());
    Ok(())
}
