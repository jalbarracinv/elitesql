//! ClawDB stable C ABI.
//!
//! Conventions (see include/clawdb.h for the authoritative header):
//! - Every function returns a `uint32_t` status: 0 = OK, otherwise the
//!   stable error codes from `clawdb_core::Error::code()`. 100 = internal
//!   panic (a bug; the engine never unwinds across the FFI boundary).
//! - Output strings are heap-allocated UTF-8, returned via out-params; the
//!   caller frees them with `clawdb_free_string`.
//! - `clawdb_last_error()` returns a thread-local message for the last
//!   failing call on this thread (valid until the next failing call).
//! - Payloads are JSON: query results use the shape produced by
//!   `clawdb_core::jsonio` (tagged values for blob/timestamp/date/time/
//!   json/vector). This keeps the ABI small and binding-friendly.

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use clawdb_core::{jsonio, Db, DbOptions, Durability, Error};

pub struct ClawDb {
    db: Db,
}

pub struct ClawSnapshot {
    snap: clawdb_core::Snapshot,
}

const PANIC_CODE: u32 = 100;
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

fn set_error(msg: &str) {
    let clean = msg.replace('\0', " ");
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = CString::new(clean).unwrap_or_default();
    });
}

fn fail(e: &Error) -> u32 {
    set_error(&e.to_string());
    e.code()
}

/// Wraps every entry point: catches panics, maps errors to codes.
fn guard(f: impl FnOnce() -> Result<(), Error>) -> u32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(&e),
        Err(_) => {
            set_error("internal panic in clawdb (please report)");
            PANIC_CODE
        }
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Result<&'a str, Error> {
    if p.is_null() {
        return Err(Error::InvalidArgument("null pointer".into()));
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| Error::InvalidArgument("invalid utf8".into()))
}

fn out_string(out: *mut *mut c_char, s: String) -> Result<(), Error> {
    if out.is_null() {
        return Err(Error::InvalidArgument("null output pointer".into()));
    }
    let c = CString::new(s.replace('\0', " "))
        .map_err(|_| Error::InvalidArgument("interior nul".into()))?;
    unsafe { *out = c.into_raw() };
    Ok(())
}

unsafe fn db_ref<'a>(db: *mut ClawDb) -> Result<&'a ClawDb, Error> {
    if db.is_null() {
        return Err(Error::InvalidArgument("null database handle".into()));
    }
    Ok(unsafe { &*db })
}

/// Version string of the library. Static; do not free.
#[no_mangle]
pub extern "C" fn clawdb_version() -> *const c_char {
    VERSION.as_ptr() as *const c_char
}

/// Thread-local message for the last failing call. Do not free; valid until
/// the next failing call on this thread.
#[no_mangle]
pub extern "C" fn clawdb_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

/// # Safety
/// `s` must be a string returned by this library, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn clawdb_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// Open (creating if missing) a database. `options_json` may be NULL or a
/// JSON object: {"durability": "safe"|"balanced"|"fast"}.
///
/// # Safety
/// `path`/`options_json` must be valid NUL-terminated strings; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_open(
    path: *const c_char,
    options_json: *const c_char,
    out: *mut *mut ClawDb,
) -> u32 {
    guard(|| {
        let path = unsafe { cstr(path)? };
        let mut opts = DbOptions::default();
        if !options_json.is_null() {
            let raw = unsafe { cstr(options_json)? };
            if !raw.trim().is_empty() {
                let j: serde_json::Value = serde_json::from_str(raw)
                    .map_err(|e| Error::InvalidArgument(format!("options: {e}")))?;
                match j.get("durability").and_then(|d| d.as_str()) {
                    None => {}
                    Some("safe") => opts.durability = Durability::Safe,
                    Some("balanced") => opts.durability = Durability::Balanced,
                    Some("fast") => opts.durability = Durability::Fast,
                    Some(other) => {
                        return Err(Error::InvalidArgument(format!(
                            "unknown durability '{other}'"
                        )))
                    }
                }
                if let Some(ro) = j.get("read_only").and_then(|r| r.as_bool()) {
                    opts.read_only = ro;
                }
            }
        }
        let db = if opts.read_only {
            Db::open_with(path, opts)?
        } else {
            Db::open_or_create_with(path, opts)?
        };
        if out.is_null() {
            return Err(Error::InvalidArgument("null output pointer".into()));
        }
        unsafe { *out = Box::into_raw(Box::new(ClawDb { db })) };
        Ok(())
    })
}

/// Close and free a database handle. The handle is invalid afterwards.
///
/// # Safety
/// `db` must be a handle returned by `clawdb_open`, closed at most once.
#[no_mangle]
pub unsafe extern "C" fn clawdb_close(db: *mut ClawDb) -> u32 {
    guard(|| {
        if !db.is_null() {
            unsafe { drop(Box::from_raw(db)) };
        }
        Ok(())
    })
}

/// Execute one SQL statement. On success `*result_json` holds the result:
/// {"columns":[...],"rows":[[...]]} | {"inserted":[ids]} | {"affected":n} |
/// {"ok":true}. Free it with `clawdb_free_string`.
///
/// # Safety
/// `db` valid handle; `sql` valid NUL-terminated UTF-8; `result_json` valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_query(
    db: *mut ClawDb,
    sql: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let sql = unsafe { cstr(sql)? };
        let out = handle.db.query(sql)?;
        out_string(result_json, jsonio::output_to_json(&out).to_string())
    })
}

/// ANN search. `params_json`: {"table","column","vector":[...],"top_k",
/// "ef_search"?, "filter"?: {col: value}}. Result: {"hits":[{"id","distance",
/// "record":{...}}]}.
///
/// # Safety
/// `db` valid handle; `params_json` valid NUL-terminated UTF-8; out valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_search_vector(
    db: *mut ClawDb,
    params_json: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let raw = unsafe { cstr(params_json)? };
        let p: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| Error::InvalidArgument(format!("params: {e}")))?;
        let result = jsonio::search_vector_json(&handle.db, &p)?;
        out_string(result_json, result.to_string())
    })
}

/// Create an ANN index over a vector column. `params_json`:
/// {"table","column","metric"?: "cosine"|"dot"|"l2","mode"?: "sync"|"async",
/// "m"?, "ef_construction"?}.
///
/// # Safety
/// `db` valid handle; `params_json` valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn clawdb_create_vector_index(
    db: *mut ClawDb,
    params_json: *const c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let raw = unsafe { cstr(params_json)? };
        let p: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| Error::InvalidArgument(format!("params: {e}")))?;
        jsonio::create_vector_index_json(&handle.db, &p)?;
        Ok(())
    })
}

/// Create a full-text index. `params_json`: {"table","column"}.
///
/// # Safety
/// `db` valid handle; `params_json` valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn clawdb_create_text_index(
    db: *mut ClawDb,
    params_json: *const c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let raw = unsafe { cstr(params_json)? };
        let p: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| Error::InvalidArgument(format!("params: {e}")))?;
        jsonio::create_text_index_json(&handle.db, &p)?;
        Ok(())
    })
}

/// BM25 full-text search. `params_json`: {"table","column","query","top_k"?,
/// "filter"?}. Result: {"hits":[{"id","score","record"}]}.
///
/// # Safety
/// `db` valid handle; strings valid NUL-terminated UTF-8; out valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_search_text(
    db: *mut ClawDb,
    params_json: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let raw = unsafe { cstr(params_json)? };
        let p: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| Error::InvalidArgument(format!("params: {e}")))?;
        let result = jsonio::search_text_json(&handle.db, &p)?;
        out_string(result_json, result.to_string())
    })
}

/// Hybrid (RRF) search. `params_json`: {"table","top_k"?,"ef_search"?,
/// "filter"?, "text"?: {"column","query"}, "vector"?: {"column","vector"}}.
///
/// # Safety
/// `db` valid handle; strings valid NUL-terminated UTF-8; out valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_search_hybrid(
    db: *mut ClawDb,
    params_json: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let raw = unsafe { cstr(params_json)? };
        let p: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| Error::InvalidArgument(format!("params: {e}")))?;
        let result = jsonio::search_hybrid_json(&handle.db, &p)?;
        out_string(result_json, result.to_string())
    })
}

/// Take a stable read snapshot. Close it with `clawdb_snapshot_close`;
/// compaction preserves the versions it needs while it is open.
///
/// # Safety
/// `db` valid handle; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_snapshot_open(
    db: *mut ClawDb,
    out: *mut *mut ClawSnapshot,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        if out.is_null() {
            return Err(Error::InvalidArgument("null output pointer".into()));
        }
        let snap = handle.db.snapshot();
        unsafe { *out = Box::into_raw(Box::new(ClawSnapshot { snap })) };
        Ok(())
    })
}

/// # Safety
/// `snap` must be a handle from `clawdb_snapshot_open`, closed at most once.
#[no_mangle]
pub unsafe extern "C" fn clawdb_snapshot_close(snap: *mut ClawSnapshot) -> u32 {
    guard(|| {
        if !snap.is_null() {
            unsafe { drop(Box::from_raw(snap)) };
        }
        Ok(())
    })
}

/// Read one record as of the snapshot. Result: {"record": {...} | null}.
///
/// # Safety
/// Handles valid; strings valid NUL-terminated UTF-8; out valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_snapshot_get(
    db: *mut ClawDb,
    snap: *mut ClawSnapshot,
    table: *const c_char,
    id: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        if snap.is_null() {
            return Err(Error::InvalidArgument("null snapshot handle".into()));
        }
        let snapshot = unsafe { &(*snap).snap };
        let table = unsafe { cstr(table)? };
        let id = unsafe { cstr(id)? };
        let record = handle.db.get_at(snapshot, table, id)?;
        let j = match record {
            Some(r) => serde_json::json!({"record": jsonio::record_to_json(&r)}),
            None => serde_json::json!({"record": null}),
        };
        out_string(result_json, j.to_string())
    })
}

/// Scan a table as of the snapshot. Result: {"rows": [{...}]}.
///
/// # Safety
/// Handles valid; strings valid NUL-terminated UTF-8; out valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_snapshot_scan(
    db: *mut ClawDb,
    snap: *mut ClawSnapshot,
    table: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        if snap.is_null() {
            return Err(Error::InvalidArgument("null snapshot handle".into()));
        }
        let snapshot = unsafe { &(*snap).snap };
        let table = unsafe { cstr(table)? };
        let rows = handle.db.scan_at(snapshot, table)?;
        let rows_json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(_, r)| jsonio::record_to_json(r))
            .collect();
        out_string(result_json, serde_json::json!({"rows": rows_json}).to_string())
    })
}

/// Drain the WAL into a segment and publish a fresh manifest.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn clawdb_checkpoint(db: *mut ClawDb) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        handle.db.checkpoint()
    })
}

/// Rewrite segments dropping dead versions (respects live snapshots).
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn clawdb_compact(db: *mut ClawDb) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        handle.db.compact()
    })
}

/// Offline integrity check of a database directory (do not run while the
/// database is open elsewhere). Result: {"ok":bool,"errors":[..],
/// "warnings":[..],"used_manifest_prev":bool}.
///
/// # Safety
/// `path` valid NUL-terminated UTF-8; `report_json` valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_check(
    path: *const c_char,
    report_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let path = unsafe { cstr(path)? };
        let report = clawdb_core::check(path)?;
        let j = serde_json::json!({
            "ok": report.is_ok(),
            "errors": report.errors,
            "warnings": report.warnings,
            "used_manifest_prev": report.used_manifest_prev,
        });
        out_string(report_json, j.to_string())
    })
}

/// Salvage every recoverable record from `src` into a fresh database at
/// `dst`. Result: a JSON report; nothing is ever discarded silently.
///
/// # Safety
/// `src`/`dst` valid NUL-terminated UTF-8; `report_json` valid.
#[no_mangle]
pub unsafe extern "C" fn clawdb_repair(
    src: *const c_char,
    dst: *const c_char,
    report_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let src = unsafe { cstr(src)? };
        let dst = unsafe { cstr(dst)? };
        let report = clawdb_core::salvage(src, dst)?;
        let j = serde_json::json!({
            "tables": report.tables,
            "recovered_records": report.recovered_records,
            "deleted_records": report.deleted_records,
            "skipped": report.skipped,
            "segments_scanned": report.segments_scanned,
            "wal_files_scanned": report.wal_files_scanned,
            "notes": report.notes,
        });
        out_string(report_json, j.to_string())
    })
}
