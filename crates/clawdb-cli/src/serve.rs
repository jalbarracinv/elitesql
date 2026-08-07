//! `clawdb serve` — the sidecar mode for multi-worker deployments.
//!
//! One process owns the embedded engine; clients (gunicorn workers, PHP-FPM
//! pools, other processes) connect over a Unix domain socket and speak a
//! line-delimited JSON protocol. Concurrency comes from the engine itself:
//! readers never block writers, writers only meet at commit — each
//! connection is served by its own thread over one shared `Db`.
//!
//! Protocol (one JSON object per line, response per request in order):
//!   -> {"op":"query","sql":"SELECT ..."}
//!   -> {"op":"search_vector","table":...,"column":...,"vector":[...],...}
//!   -> {"op":"checkpoint"} | {"op":"compact"} | {"op":"ping"}
//!   <- {"ok":true,"result":...}
//!   <- {"ok":false,"code":N,"error":"..."}

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use clawdb_core::{jsonio, Db};
use serde_json::{json, Value as J};

pub fn serve(db: Db, socket_path: &str) -> Result<(), String> {
    // A stale socket file from a previous run would block the bind.
    if std::fs::metadata(socket_path).is_ok() {
        std::fs::remove_file(socket_path).map_err(|e| format!("cannot remove stale socket: {e}"))?;
    }
    let listener = UnixListener::bind(socket_path)
        .map_err(|e| format!("cannot bind {socket_path}: {e}"))?;
    eprintln!("clawdb serve: listening on {socket_path}");
    let db = Arc::new(db);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let db = db.clone();
                std::thread::spawn(move || handle_connection(db, stream));
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_connection(db: Arc<Db>, stream: UnixStream) {
    let reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = BufWriter::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let response = handle_request(&db, &line);
        if writeln!(writer, "{response}").is_err() || writer.flush().is_err() {
            return;
        }
    }
}

fn handle_request(db: &Db, line: &str) -> J {
    let request: J = match serde_json::from_str(line) {
        Ok(j) => j,
        Err(e) => return json!({"ok": false, "code": 8, "error": format!("bad request json: {e}")}),
    };
    let id = request.get("id").cloned();
    let op = request.get("op").and_then(|o| o.as_str()).unwrap_or_default();
    let result = match op {
        "ping" => Ok(json!("pong")),
        "query" => match request.get("sql").and_then(|s| s.as_str()) {
            Some(sql) => db.query(sql).map(|out| jsonio::output_to_json(&out)),
            None => Err(clawdb_core::Error::InvalidArgument("missing 'sql'".into())),
        },
        "search_vector" => jsonio::search_vector_json(db, &request),
        "create_vector_index" => jsonio::create_vector_index_json(db, &request),
        "search_text" => jsonio::search_text_json(db, &request),
        "create_text_index" => jsonio::create_text_index_json(db, &request),
        "search_hybrid" => jsonio::search_hybrid_json(db, &request),
        "checkpoint" => db.checkpoint().map(|()| json!(true)),
        "compact" => db.compact().map(|()| json!(true)),
        other => Err(clawdb_core::Error::InvalidArgument(format!(
            "unknown op '{other}'"
        ))),
    };
    let mut response = match result {
        Ok(value) => json!({"ok": true, "result": value}),
        Err(e) => json!({"ok": false, "code": e.code(), "error": e.to_string()}),
    };
    if let (Some(id), Some(obj)) = (id, response.as_object_mut()) {
        obj.insert("id".into(), id);
    }
    response
}
