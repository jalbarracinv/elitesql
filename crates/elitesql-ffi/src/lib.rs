//! EliteSQL stable C ABI.
//!
//! Conventions (see include/elitesql.h for the authoritative header):
//! - Every function returns a `uint32_t` status: 0 = OK, otherwise the
//!   stable error codes from `elitesql_core::Error::code()`. 100 = internal
//!   panic (a bug; the engine never unwinds across the FFI boundary).
//! - Output strings are heap-allocated UTF-8, returned via out-params; the
//!   caller frees them with `elitesql_free_string`.
//! - `elitesql_last_error()` returns a thread-local message for the last
//!   failing call on this thread (valid until the next failing call).
//! - Payloads are JSON: query results use the shape produced by
//!   `elitesql_core::jsonio` (tagged values for blob/timestamp/date/time/
//!   json/vector). This keeps the ABI small and binding-friendly.

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard};

use elitesql_core::{jsonio, Db, DbOptions, Durability, Error};

pub struct EliteSql {
    db: Db,
}

pub struct EliteSqlSnapshot {
    snap: elitesql_core::Snapshot,
}

pub struct EliteSqlTxn {
    txn: Mutex<Option<elitesql_core::Txn>>,
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

fn optional_usize(value: &serde_json::Value, key: &str) -> Result<Option<usize>, Error> {
    let Some(raw) = value.get(key) else {
        return Ok(None);
    };
    let number = raw
        .as_u64()
        .ok_or_else(|| Error::InvalidArgument(format!("memory.{key} must be an integer")))?;
    usize::try_from(number)
        .map(Some)
        .map_err(|_| Error::InvalidArgument(format!("memory.{key} is too large")))
}

/// Wraps every entry point: catches panics, maps errors to codes.
fn guard(f: impl FnOnce() -> Result<(), Error>) -> u32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(&e),
        Err(_) => {
            set_error("internal panic in elitesql (please report)");
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

unsafe fn db_ref<'a>(db: *mut EliteSql) -> Result<&'a EliteSql, Error> {
    if db.is_null() {
        return Err(Error::InvalidArgument("null database handle".into()));
    }
    Ok(unsafe { &*db })
}

unsafe fn txn_guard<'a>(
    txn: *mut EliteSqlTxn,
) -> Result<MutexGuard<'a, Option<elitesql_core::Txn>>, Error> {
    if txn.is_null() {
        return Err(Error::InvalidArgument("null transaction handle".into()));
    }
    Ok(unsafe { &*txn }
        .txn
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()))
}

/// Version string of the library. Static; do not free.
#[no_mangle]
pub extern "C" fn elitesql_version() -> *const c_char {
    VERSION.as_ptr() as *const c_char
}

/// Thread-local message for the last failing call. Do not free; valid until
/// the next failing call on this thread.
#[no_mangle]
pub extern "C" fn elitesql_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

/// # Safety
/// `s` must be a string returned by this library, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn elitesql_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// Open (creating if missing) a database. `options_json` may be NULL or a
/// JSON object: {"durability": "safe"|"balanced"|"fast", "memory"?: {...}}.
///
/// # Safety
/// `path`/`options_json` must be valid NUL-terminated strings; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn elitesql_open(
    path: *const c_char,
    options_json: *const c_char,
    out: *mut *mut EliteSql,
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
                if let Some(memory) = j.get("memory") {
                    if !memory.is_object() {
                        return Err(Error::InvalidArgument(
                            "memory options must be an object".into(),
                        ));
                    }
                    macro_rules! set_memory_usize {
                        ($field:ident) => {
                            if let Some(value) = optional_usize(memory, stringify!($field))? {
                                opts.memory.$field = value;
                            }
                        };
                    }
                    set_memory_usize!(total_memory_bytes);
                    set_memory_usize!(query_pool_bytes);
                    set_memory_usize!(query_working_bytes);
                    set_memory_usize!(index_delta_pool_bytes);
                    set_memory_usize!(maintenance_pool_bytes);
                    set_memory_usize!(reserved_memory_bytes);
                    set_memory_usize!(scan_batch_rows);
                    if let Some(directory) = memory.get("spill_directory") {
                        opts.memory.spill_directory = if directory.is_null() {
                            None
                        } else {
                            Some(std::path::PathBuf::from(directory.as_str().ok_or_else(
                                || {
                                    Error::InvalidArgument(
                                        "memory.spill_directory must be a string or null".into(),
                                    )
                                },
                            )?))
                        };
                    }
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
        unsafe { *out = Box::into_raw(Box::new(EliteSql { db })) };
        Ok(())
    })
}

/// Close and free a database handle. The handle is invalid afterwards.
///
/// # Safety
/// `db` must be a handle returned by `elitesql_open`, closed at most once.
#[no_mangle]
pub unsafe extern "C" fn elitesql_close(db: *mut EliteSql) -> u32 {
    guard(|| {
        if !db.is_null() {
            unsafe { drop(Box::from_raw(db)) };
        }
        Ok(())
    })
}

/// Execute one SQL statement. On success `*result_json` holds the result:
/// {"columns":[...],"rows":[[...]]} | {"inserted":[ids]} | {"affected":n} |
/// {"ok":true}. Free it with `elitesql_free_string`.
///
/// # Safety
/// `db` valid handle; `sql` valid NUL-terminated UTF-8; `result_json` valid.
#[no_mangle]
pub unsafe extern "C" fn elitesql_query(
    db: *mut EliteSql,
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

/// Execute SQL with parameters supplied separately as a JSON array
/// (positional `?`/`%s`) or object (named `%(name)s`). Native JSON scalars and
/// the tagged EliteSQL value representation preserve parameter types.
///
/// # Safety
/// All pointers must be valid; strings must be NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn elitesql_query_params(
    db: *mut EliteSql,
    sql: *const c_char,
    params_json: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        let sql = unsafe { cstr(sql)? };
        let raw = unsafe { cstr(params_json)? };
        let params: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| Error::InvalidArgument(format!("SQL params: {error}")))?;
        let out = jsonio::query_with_params_json(&handle.db, sql, &params)?;
        out_string(result_json, jsonio::output_to_json(&out).to_string())
    })
}

/// Begin a multi-operation transaction. The returned handle owns a stable
/// snapshot and must be committed, rolled back or closed exactly once.
#[no_mangle]
/// Begin a transaction and return an owned handle.
///
/// # Safety
/// `db` must be a live database handle and `out` must be writable. Closing
/// either handle must not overlap a call using that same handle.
pub unsafe extern "C" fn elitesql_txn_begin(db: *mut EliteSql, out: *mut *mut EliteSqlTxn) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        if out.is_null() {
            return Err(Error::InvalidArgument("null output pointer".into()));
        }
        unsafe {
            *out = Box::into_raw(Box::new(EliteSqlTxn {
                txn: Mutex::new(Some(handle.db.begin())),
            }))
        };
        Ok(())
    })
}

/// Execute one parameterized SQL statement against a transaction's staged
/// view. `params_json` is an array, object or null, exactly like
/// `elitesql_query_params`.
#[no_mangle]
/// Execute parameterized SQL inside a transaction.
///
/// # Safety
/// `txn` must be live, input pointers must name valid NUL-terminated UTF-8,
/// and `result_json` must be writable.
pub unsafe extern "C" fn elitesql_txn_query_params(
    txn: *mut EliteSqlTxn,
    sql: *const c_char,
    params_json: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let sql = unsafe { cstr(sql)? };
        let raw = unsafe { cstr(params_json)? };
        let params: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| Error::InvalidArgument(format!("SQL params: {error}")))?;
        let mut guard = unsafe { txn_guard(txn)? };
        let transaction = guard
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction is already closed".into()))?;
        let out = jsonio::query_txn_with_params_json(transaction, sql, &params)?;
        out_string(result_json, jsonio::output_to_json(&out).to_string())
    })
}

/// Insert one record inside a transaction. Result contains both the physical
/// ULID and the normalized record, including any generated identity column.
#[no_mangle]
/// Stage an insert in a transaction.
///
/// # Safety
/// `txn` must be live, input pointers must name valid NUL-terminated UTF-8,
/// and `result_json` must be writable.
pub unsafe extern "C" fn elitesql_txn_insert(
    txn: *mut EliteSqlTxn,
    table: *const c_char,
    record_json: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let mut guard = unsafe { txn_guard(txn)? };
        let txn = guard
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction is already closed".into()))?;
        let table = unsafe { cstr(table)? };
        let raw = unsafe { cstr(record_json)? };
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| Error::InvalidArgument(format!("record: {error}")))?;
        let id = txn.insert(table, jsonio::json_to_record(&value)?)?;
        let record = txn
            .get(table, &id)?
            .expect("a staged insert is visible in its transaction");
        out_string(
            result_json,
            serde_json::json!({"id": id, "record": jsonio::record_to_json(&record)}).to_string(),
        )
    })
}

#[no_mangle]
/// Read one record through a transaction's snapshot and staged changes.
///
/// # Safety
/// `txn` must be live, `table` and `id` must be valid NUL-terminated UTF-8,
/// and `result_json` must be writable.
pub unsafe extern "C" fn elitesql_txn_get(
    txn: *mut EliteSqlTxn,
    table: *const c_char,
    id: *const c_char,
    result_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let mut guard = unsafe { txn_guard(txn)? };
        let txn = guard
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction is already closed".into()))?;
        let record = txn.get(unsafe { cstr(table)? }, unsafe { cstr(id)? })?;
        out_string(
            result_json,
            serde_json::json!({"record": record.as_ref().map(jsonio::record_to_json)}).to_string(),
        )
    })
}

#[no_mangle]
/// Stage an update in a transaction.
///
/// # Safety
/// `txn` must be live and all input pointers must name valid NUL-terminated
/// UTF-8 strings.
pub unsafe extern "C" fn elitesql_txn_update(
    txn: *mut EliteSqlTxn,
    table: *const c_char,
    id: *const c_char,
    patch_json: *const c_char,
) -> u32 {
    guard(|| {
        let mut guard = unsafe { txn_guard(txn)? };
        let txn = guard
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction is already closed".into()))?;
        let value: serde_json::Value = serde_json::from_str(unsafe { cstr(patch_json)? })
            .map_err(|error| Error::InvalidArgument(format!("patch: {error}")))?;
        txn.update(
            unsafe { cstr(table)? },
            unsafe { cstr(id)? },
            jsonio::json_to_record(&value)?,
        )
    })
}

#[no_mangle]
/// Stage a delete in a transaction.
///
/// # Safety
/// `txn` must be live, `table` and `id` must be valid NUL-terminated UTF-8,
/// and `deleted` must be writable.
pub unsafe extern "C" fn elitesql_txn_delete(
    txn: *mut EliteSqlTxn,
    table: *const c_char,
    id: *const c_char,
    deleted: *mut bool,
) -> u32 {
    guard(|| {
        if deleted.is_null() {
            return Err(Error::InvalidArgument("null output pointer".into()));
        }
        let mut guard = unsafe { txn_guard(txn)? };
        let transaction = guard
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction is already closed".into()))?;
        let value = transaction.delete(unsafe { cstr(table)? }, unsafe { cstr(id)? })?;
        unsafe { *deleted = value };
        Ok(())
    })
}

#[no_mangle]
/// Commit and consume the transaction's active state.
///
/// # Safety
/// `txn` must be a live handle and `committed_version` must be writable.
pub unsafe extern "C" fn elitesql_txn_commit(
    txn: *mut EliteSqlTxn,
    committed_version: *mut u64,
) -> u32 {
    guard(|| {
        if txn.is_null() || committed_version.is_null() {
            return Err(Error::InvalidArgument("null pointer".into()));
        }
        let transaction = unsafe { txn_guard(txn)? }
            .take()
            .ok_or_else(|| Error::InvalidArgument("transaction is already closed".into()))?;
        unsafe { *committed_version = transaction.commit()? };
        Ok(())
    })
}

#[no_mangle]
/// Roll back and consume the transaction's active state.
///
/// # Safety
/// `txn` must be a live handle. Closing it must not overlap this call.
pub unsafe extern "C" fn elitesql_txn_rollback(txn: *mut EliteSqlTxn) -> u32 {
    guard(|| {
        if txn.is_null() {
            return Err(Error::InvalidArgument("null transaction handle".into()));
        }
        if let Some(transaction) = unsafe { txn_guard(txn)? }.take() {
            transaction.rollback();
        }
        Ok(())
    })
}

/// Free the transaction handle. An open transaction is rolled back by drop.
#[no_mangle]
/// Destroy a transaction handle, rolling back if it is still active.
///
/// # Safety
/// `txn` must be a live handle returned by this library, closed exactly once,
/// with no overlapping operation on the same handle.
pub unsafe extern "C" fn elitesql_txn_close(txn: *mut EliteSqlTxn) -> u32 {
    guard(|| {
        if !txn.is_null() {
            unsafe { drop(Box::from_raw(txn)) };
        }
        Ok(())
    })
}

/// ANN search. `params_json`: {"table","column","vector":[...],"top_k",
/// "ef_search"?, "filter"?: {col: value}}. Result: {"hits":[{"id","distance",
/// "record":{...}}]}.
///
/// # Safety
/// `db` valid handle; `params_json` valid NUL-terminated UTF-8; out valid.
#[no_mangle]
pub unsafe extern "C" fn elitesql_search_vector(
    db: *mut EliteSql,
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
pub unsafe extern "C" fn elitesql_create_vector_index(
    db: *mut EliteSql,
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
pub unsafe extern "C" fn elitesql_create_text_index(
    db: *mut EliteSql,
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
pub unsafe extern "C" fn elitesql_search_text(
    db: *mut EliteSql,
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
pub unsafe extern "C" fn elitesql_search_hybrid(
    db: *mut EliteSql,
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

/// Take a stable read snapshot. Close it with `elitesql_snapshot_close`;
/// compaction preserves the versions it needs while it is open.
///
/// # Safety
/// `db` valid handle; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn elitesql_snapshot_open(
    db: *mut EliteSql,
    out: *mut *mut EliteSqlSnapshot,
) -> u32 {
    guard(|| {
        let handle = unsafe { db_ref(db)? };
        if out.is_null() {
            return Err(Error::InvalidArgument("null output pointer".into()));
        }
        let snap = handle.db.snapshot();
        unsafe { *out = Box::into_raw(Box::new(EliteSqlSnapshot { snap })) };
        Ok(())
    })
}

/// # Safety
/// `snap` must be a handle from `elitesql_snapshot_open`, closed at most once.
#[no_mangle]
pub unsafe extern "C" fn elitesql_snapshot_close(snap: *mut EliteSqlSnapshot) -> u32 {
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
pub unsafe extern "C" fn elitesql_snapshot_get(
    db: *mut EliteSql,
    snap: *mut EliteSqlSnapshot,
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
pub unsafe extern "C" fn elitesql_snapshot_scan(
    db: *mut EliteSql,
    snap: *mut EliteSqlSnapshot,
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
        out_string(
            result_json,
            serde_json::json!({"rows": rows_json}).to_string(),
        )
    })
}

/// Drain the WAL into a segment and publish a fresh manifest.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn elitesql_checkpoint(db: *mut EliteSql) -> u32 {
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
pub unsafe extern "C" fn elitesql_compact(db: *mut EliteSql) -> u32 {
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
pub unsafe extern "C" fn elitesql_check(path: *const c_char, report_json: *mut *mut c_char) -> u32 {
    guard(|| {
        let path = unsafe { cstr(path)? };
        let report = elitesql_core::check(path)?;
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
pub unsafe extern "C" fn elitesql_repair(
    src: *const c_char,
    dst: *const c_char,
    report_json: *mut *mut c_char,
) -> u32 {
    guard(|| {
        let src = unsafe { cstr(src)? };
        let dst = unsafe { cstr(dst)? };
        let report = elitesql_core::salvage(src, dst)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ffi_query_params_binds_tagged_values() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "elitesql-ffi-params-{}-{unique}",
            std::process::id()
        ));
        let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut db = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_open(path_c.as_ptr(), std::ptr::null(), &mut db) },
            0
        );

        let create = CString::new("CREATE TABLE docs (name text, payload blob)").unwrap();
        let mut output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query(db, create.as_ptr(), &mut output) },
            0
        );
        unsafe { elitesql_free_string(output) };

        let insert = CString::new("INSERT INTO docs (name, payload) VALUES (%s, %s)").unwrap();
        let params = CString::new(r#"["x' OR TRUE --",{"$t":"blob","hex":"00ff"}]"#).unwrap();
        output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query_params(db, insert.as_ptr(), params.as_ptr(), &mut output) },
            0
        );
        unsafe { elitesql_free_string(output) };

        let select = CString::new("SELECT name, payload FROM docs WHERE name = %(name)s").unwrap();
        let params = CString::new(r#"{"name":"x' OR TRUE --"}"#).unwrap();
        output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query_params(db, select.as_ptr(), params.as_ptr(), &mut output) },
            0
        );
        let result = unsafe { CStr::from_ptr(output) }.to_str().unwrap();
        assert!(result.contains("x' OR TRUE --"), "{result}");
        assert!(result.contains(r#""hex":"00ff""#), "{result}");
        unsafe { elitesql_free_string(output) };

        assert_eq!(unsafe { elitesql_close(db) }, 0);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn ffi_transaction_is_atomic_and_returns_generated_identity() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("elitesql-ffi-txn-{}-{unique}", std::process::id()));
        let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut db = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_open(path_c.as_ptr(), std::ptr::null(), &mut db) },
            0
        );
        let create = CString::new(
            "CREATE TABLE docs (doc_id int AUTO_INCREMENT, title text NOT NULL, done bool NOT NULL)",
        )
        .unwrap();
        let mut output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query(db, create.as_ptr(), &mut output) },
            0
        );
        unsafe { elitesql_free_string(output) };

        let mut txn = std::ptr::null_mut();
        assert_eq!(unsafe { elitesql_txn_begin(db, &mut txn) }, 0);
        let table = CString::new("docs").unwrap();
        let insert_sql =
            CString::new("INSERT INTO docs (title, done) VALUES (%s, %s) RETURNING id, doc_id")
                .unwrap();
        let insert_params = CString::new(r#"["contract",false]"#).unwrap();
        output = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                elitesql_txn_query_params(
                    txn,
                    insert_sql.as_ptr(),
                    insert_params.as_ptr(),
                    &mut output,
                )
            },
            0
        );
        let inserted: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(output) }.to_str().unwrap()).unwrap();
        assert_eq!(inserted["rows"][0][1], 1);
        let id = CString::new(inserted["rows"][0][0].as_str().unwrap()).unwrap();
        unsafe { elitesql_free_string(output) };

        let patch = CString::new(r#"{"done":true}"#).unwrap();
        assert_eq!(
            unsafe { elitesql_txn_update(txn, table.as_ptr(), id.as_ptr(), patch.as_ptr()) },
            0
        );
        let mut version = 0;
        assert_eq!(unsafe { elitesql_txn_commit(txn, &mut version) }, 0);
        assert!(version > 0);
        assert_eq!(unsafe { elitesql_txn_close(txn) }, 0);

        let select = CString::new("SELECT doc_id, done FROM docs").unwrap();
        output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query(db, select.as_ptr(), &mut output) },
            0
        );
        let selected: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(output) }.to_str().unwrap()).unwrap();
        assert_eq!(selected["rows"][0], serde_json::json!([1, true]));
        unsafe { elitesql_free_string(output) };

        assert_eq!(unsafe { elitesql_close(db) }, 0);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn ffi_serializes_concurrent_calls_on_one_transaction_handle() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "elitesql-ffi-shared-txn-{}-{unique}",
            std::process::id()
        ));
        let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut db = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_open(path_c.as_ptr(), std::ptr::null(), &mut db) },
            0
        );
        let create = CString::new("CREATE TABLE docs (name text NOT NULL)").unwrap();
        let mut output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query(db, create.as_ptr(), &mut output) },
            0
        );
        unsafe { elitesql_free_string(output) };

        let mut txn = std::ptr::null_mut();
        assert_eq!(unsafe { elitesql_txn_begin(db, &mut txn) }, 0);
        let txn_address = txn as usize;
        let workers: Vec<_> = (0..8)
            .map(|index| {
                std::thread::spawn(move || {
                    let table = CString::new("docs").unwrap();
                    let record = CString::new(format!(r#"{{"name":"n-{index}"}}"#)).unwrap();
                    let mut result = std::ptr::null_mut();
                    let status = unsafe {
                        elitesql_txn_insert(
                            txn_address as *mut EliteSqlTxn,
                            table.as_ptr(),
                            record.as_ptr(),
                            &mut result,
                        )
                    };
                    if !result.is_null() {
                        unsafe { elitesql_free_string(result) };
                    }
                    status
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), 0);
        }
        let mut version = 0;
        assert_eq!(unsafe { elitesql_txn_commit(txn, &mut version) }, 0);
        assert_eq!(unsafe { elitesql_txn_close(txn) }, 0);

        let select = CString::new("SELECT COUNT(*) FROM docs").unwrap();
        output = std::ptr::null_mut();
        assert_eq!(
            unsafe { elitesql_query(db, select.as_ptr(), &mut output) },
            0
        );
        let selected: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(output) }.to_str().unwrap()).unwrap();
        assert_eq!(selected["rows"][0][0], 8);
        unsafe { elitesql_free_string(output) };
        assert_eq!(unsafe { elitesql_close(db) }, 0);
        std::fs::remove_dir_all(path).unwrap();
    }
}
