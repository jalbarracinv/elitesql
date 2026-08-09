//! Measure how the memory configuration affects vector-index persistence and
//! restart time. This intentionally skips recall and Criterion search loops so
//! memory profiles can be compared on the same host with less unrelated work.

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::time::Instant;

use elitesql_core::{
    AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Durability, MemoryOptions, Record,
    TableSchema, Value, VectorIndexOptions, VectorSearchOptions,
};

const DIM: usize = 64;
const BATCH: usize = 10_000;
const MIB: usize = 1024 * 1024;

#[derive(Debug)]
struct Args {
    rows: usize,
    total_mib: usize,
    index_mib: usize,
    maintenance_mib: usize,
    memtable_mib: usize,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = env::args().skip(1);
        let mut out = Self {
            rows: 100_000,
            total_mib: 384,
            index_mib: 128,
            maintenance_mib: 128,
            memtable_mib: 64,
        };
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("missing value after {flag}"))?
                .parse::<usize>()
                .map_err(|_| format!("invalid integer after {flag}"))?;
            match flag.as_str() {
                "--rows" => out.rows = value,
                "--total-mib" => out.total_mib = value,
                "--index-mib" => out.index_mib = value,
                "--maintenance-mib" => out.maintenance_mib = value,
                "--memtable-mib" => out.memtable_mib = value,
                _ => return Err(format!("unknown option: {flag}")),
            }
        }
        if out.rows == 0 {
            return Err("--rows must be positive".into());
        }
        Ok(out)
    }

    fn db_options(&self) -> DbOptions {
        DbOptions {
            durability: Durability::Fast,
            memtable_max_bytes: (self.memtable_mib * MIB) as u64,
            auto_compaction: AutoCompactionOptions::disabled(),
            memory: MemoryOptions {
                total_memory_bytes: self.total_mib * MIB,
                query_pool_bytes: 64 * MIB,
                query_working_bytes: 16 * MIB,
                index_delta_pool_bytes: self.index_mib * MIB,
                maintenance_pool_bytes: self.maintenance_mib * MIB,
                reserved_memory_bytes: 8 * MIB,
                scan_batch_rows: 512,
                spill_directory: None,
            },
            ..DbOptions::default()
        }
    }
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn unit_f32(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0 - 0.5
    }
}

struct Clustered {
    centers: Vec<Vec<f32>>,
    rng: XorShift,
}

impl Clustered {
    fn new(seed: u64) -> Self {
        let mut rng = XorShift(seed);
        let centers = (0..1024)
            .map(|_| (0..DIM).map(|_| rng.unit_f32()).collect())
            .collect();
        Self { centers, rng }
    }

    fn vector(&mut self) -> Vec<f32> {
        let center = &self.centers[(self.rng.next() % self.centers.len() as u64) as usize];
        center
            .iter()
            .map(|value| value + self.rng.unit_f32() * 0.6)
            .collect()
    }
}

fn persisted_base(db_path: &std::path::Path) -> Result<(u64, u32, u64), String> {
    let path = fs::read_dir(db_path.join("vectors"))
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "vidx")
        })
        .ok_or_else(|| "no durable .vidx base found".to_owned())?;
    let bytes = fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .len();
    let mut header = [0_u8; 36];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| error.to_string())?;
    let dump_version = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let nodes = u32::from_le_bytes(header[32..36].try_into().unwrap());
    Ok((dump_version, nodes, bytes))
}

fn run(args: Args) -> Result<(), String> {
    let temp = tempfile::tempdir().map_err(|error| error.to_string())?;
    let path = temp.path().join("vec.esql");
    let options = args.db_options();
    eprintln!("config: {args:?}");

    let started = Instant::now();
    let db = Db::create_with(&path, options.clone()).map_err(|error| error.to_string())?;
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("n", ColumnType::Int64),
            Column::vector("embedding", DIM),
        ],
    ))
    .map_err(|error| error.to_string())?;
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
        .map_err(|error| error.to_string())?;

    let mut generator = Clustered::new(0xABCDEF);
    let mut inserted = 0;
    let mut committed_version = 0;
    while inserted < args.rows {
        let end = (inserted + BATCH).min(args.rows);
        let mut transaction = db.begin();
        for row in inserted..end {
            let mut record = Record::new();
            record.insert("id".into(), Value::Text(format!("d-{row:06}")));
            record.insert("n".into(), Value::Int64(row as i64));
            record.insert("embedding".into(), Value::Vector(generator.vector()));
            transaction
                .insert("docs", record)
                .map_err(|error| error.to_string())?;
        }
        committed_version = transaction.commit().map_err(|error| error.to_string())?;
        inserted = end;
        eprintln!("loaded {inserted}/{}", args.rows);
    }
    db.wait_vector_indexing();
    let build_seconds = started.elapsed().as_secs_f64();
    let memory = db.global_memory_stats();
    let maintenance = db.maintenance_stats();

    let close_started = Instant::now();
    drop(db);
    let close_seconds = close_started.elapsed().as_secs_f64();
    let (base_version, base_nodes, base_bytes) = persisted_base(&path)?;

    let open_started = Instant::now();
    let reopened = Db::open_with(&path, options).map_err(|error| error.to_string())?;
    let open_seconds = open_started.elapsed().as_secs_f64();
    let mut query_generator = Clustered::new(0x5EED_0000_0001);
    let query = query_generator.vector();
    let search_started = Instant::now();
    let hits = reopened
        .search_vector(
            "docs",
            "embedding",
            &query,
            10,
            &VectorSearchOptions::default(),
        )
        .map_err(|error| error.to_string())?;
    let cold_search_ms = search_started.elapsed().as_secs_f64() * 1000.0;
    if hits.len() != 10 {
        return Err(format!("expected 10 search hits, got {}", hits.len()));
    }
    drop(reopened);

    println!(
        "RESULT rows={} total_mib={} index_mib={} maintenance_mib={} memtable_mib={} \
         committed_version={} base_version={} base_nodes={} catch_up_rows={} base_bytes={} \
         build_s={build_seconds:.3} close_s={close_seconds:.3} open_s={open_seconds:.3} \
         cold_search_ms={cold_search_ms:.3} index_peak_bytes={} index_consolidations={} \
         maintenance_peak_bytes={} checkpoints={}",
        args.rows,
        args.total_mib,
        args.index_mib,
        args.maintenance_mib,
        args.memtable_mib,
        committed_version,
        base_version,
        base_nodes,
        args.rows.saturating_sub(base_nodes as usize),
        base_bytes,
        memory.index_delta_peak_bytes,
        memory.index_consolidations,
        memory.maintenance_peak_bytes,
        maintenance.checkpoints,
    );
    Ok(())
}

fn main() {
    if let Err(error) = Args::parse().and_then(run) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}
