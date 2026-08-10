//! `elitesql serve` — the sidecar mode for multi-worker deployments.
//!
//! One process owns the embedded engine; clients (gunicorn workers, PHP-FPM
//! pools, other processes, or an app on another host) connect over a Unix
//! domain socket or TCP and speak a line-delimited JSON protocol. Concurrency
//! comes from the engine itself: readers never block writers, writers only meet
//! at commit — each connection is served by its own thread over one shared `Db`.
//!
//! Protocol (one JSON object per line, response per request in order):
//!   -> {"op":"auth","token":"..."}            (TCP only; must come first)
//!   -> {"op":"query","sql":"SELECT ...","params"?: [...] | {...}}
//!   -> {"op":"search_vector","table":...,"column":...,"vector":[...],...}
//!   -> {"op":"checkpoint"} | {"op":"compact"} | {"op":"ping"}
//!   <- {"ok":true,"result":...}
//!   <- {"ok":false,"code":N,"error":"..."}
//!
//! Transport and trust. A Unix socket is authenticated by filesystem
//! permissions, so its connections start trusted. A TCP port is reachable by
//! anyone who can route to it, so it requires a shared token and every request
//! before a successful `auth` is refused. The protocol is **not encrypted**:
//! run TCP over a loopback bind, a private network, an SSH tunnel or a VPN, and
//! treat the token as a credential that travels in cleartext otherwise.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use elitesql_core::{jsonio, Db, Error, Txn};
use serde_json::{json, Value as J};

/// Protocol-level error code for authentication, outside the engine's range
/// (`Error::code()` currently returns 1..=16) so clients can tell them apart.
const AUTH_ERROR_CODE: u32 = 20;
const SIDECAR_TXN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ServeOptions {
    /// Unix socket path to bind, when serving locally.
    pub socket_path: Option<String>,
    /// `host:port` to bind, when serving over the network.
    pub tcp: Option<String>,
    /// Shared secret. Required for TCP, ignored for a Unix socket.
    pub token: Option<String>,
    /// Upper bound on concurrent connections, since each one costs a thread.
    pub max_connections: usize,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            socket_path: None,
            tcp: None,
            token: None,
            max_connections: 128,
        }
    }
}

/// Whether a connection still has to prove who it is.
#[derive(Clone)]
enum Auth {
    /// Unix socket: the filesystem already decided who may connect.
    Trusted,
    Token(Arc<String>),
}

/// Compares in time independent of how many leading bytes match, so a caller
/// cannot discover the token one byte at a time.
fn token_matches(expected: &str, supplied: &str) -> bool {
    let (a, b) = (expected.as_bytes(), supplied.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// The two listeners differ only in how a connection is duplicated for the
/// reader half, so the serving loop is written once over this.
trait Stream: Read + Write + Send + Sized + 'static {
    fn duplicate(&self) -> std::io::Result<Self>;
    fn peer(&self) -> String;
}

impl Stream for UnixStream {
    fn duplicate(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn peer(&self) -> String {
        "unix".into()
    }
}

impl Stream for TcpStream {
    fn duplicate(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn peer(&self) -> String {
        self.peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "tcp".into())
    }
}

pub fn serve(db: Db, options: ServeOptions) -> Result<(), String> {
    let db = Arc::new(db);
    let live = Arc::new(AtomicUsize::new(0));
    match (&options.socket_path, &options.tcp) {
        (Some(path), None) => serve_unix(db, path, &options, live),
        (None, Some(addr)) => serve_tcp(db, addr, &options, live),
        (Some(_), Some(_)) => Err("choose one transport: a socket path or --tcp, not both".into()),
        (None, None) => Err("serve needs a socket path or --tcp <host:port>".into()),
    }
}

fn serve_unix(
    db: Arc<Db>,
    socket_path: &str,
    options: &ServeOptions,
    live: Arc<AtomicUsize>,
) -> Result<(), String> {
    // A stale socket file from a previous run would block the bind.
    if std::fs::metadata(socket_path).is_ok() {
        std::fs::remove_file(socket_path)
            .map_err(|e| format!("cannot remove stale socket: {e}"))?;
    }
    let listener =
        UnixListener::bind(socket_path).map_err(|e| format!("cannot bind {socket_path}: {e}"))?;
    eprintln!("elitesql serve: listening on {socket_path}");
    accept_loop(listener.incoming(), db, Auth::Trusted, options, live);
    Ok(())
}

fn serve_tcp(
    db: Arc<Db>,
    addr: &str,
    options: &ServeOptions,
    live: Arc<AtomicUsize>,
) -> Result<(), String> {
    // A reachable port with no credential would publish the database to
    // everyone who can route to it, so this is refused rather than warned about.
    let Some(token) = options.token.clone() else {
        return Err(
            "--tcp requires a token: set ELITESQL_TOKEN or pass --token-file <path>".into(),
        );
    };
    if token.trim().is_empty() {
        return Err("the token is empty; refusing to serve TCP without a credential".into());
    }
    let listener = TcpListener::bind(addr).map_err(|e| format!("cannot bind {addr}: {e}"))?;
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.to_string());
    eprintln!("elitesql serve: listening on tcp://{local}");
    if !is_loopback(&listener) {
        eprintln!(
            "warning: {local} is not loopback and the protocol is not encrypted. \
             Put it behind an SSH tunnel, a VPN or a private network."
        );
    }
    accept_loop(
        listener.incoming(),
        db,
        Auth::Token(Arc::new(token)),
        options,
        live,
    );
    Ok(())
}

fn is_loopback(listener: &TcpListener) -> bool {
    listener
        .local_addr()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false)
}

fn accept_loop<S: Stream>(
    incoming: impl Iterator<Item = std::io::Result<S>>,
    db: Arc<Db>,
    auth: Auth,
    options: &ServeOptions,
    live: Arc<AtomicUsize>,
) {
    let max = options.max_connections.max(1);
    for conn in incoming {
        match conn {
            Ok(mut stream) => {
                // Each connection costs a thread, so an unbounded accept loop
                // would let anyone who can reach the port exhaust the process.
                if live.load(Ordering::Relaxed) >= max {
                    let refusal = json!({
                        "ok": false,
                        "code": AUTH_ERROR_CODE,
                        "error": format!("too many connections (limit {max})"),
                    });
                    let _ = writeln!(stream, "{refusal}");
                    let _ = stream.flush();
                    eprintln!("refused {}: connection limit {max} reached", stream.peer());
                    continue;
                }
                live.fetch_add(1, Ordering::Relaxed);
                let db = db.clone();
                let auth = auth.clone();
                let live = live.clone();
                std::thread::spawn(move || {
                    handle_connection(db, stream, auth);
                    live.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

fn handle_connection<S: Stream>(db: Arc<Db>, stream: S, auth: Auth) {
    let reader = BufReader::new(match stream.duplicate() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = BufWriter::new(stream);
    let mut authenticated = matches!(auth, Auth::Trusted);
    let mut txn = None;
    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let response = dispatch_with_txn(&db, &line, &auth, &mut authenticated, &mut txn);
        if writeln!(writer, "{response}").is_err() || writer.flush().is_err() {
            return;
        }
    }
}

/// Applies the connection's auth state, then runs the request. Everything
/// before a successful `auth` on an untrusted transport is refused, so no
/// unauthenticated caller reaches the engine — not even `ping`.
#[cfg(test)]
fn dispatch(db: &Db, line: &str, auth: &Auth, authenticated: &mut bool) -> J {
    let mut txn = None;
    dispatch_with_txn(db, line, auth, authenticated, &mut txn)
}

fn dispatch_with_txn(
    db: &Db,
    line: &str,
    auth: &Auth,
    authenticated: &mut bool,
    txn: &mut Option<Txn>,
) -> J {
    let request: J = match serde_json::from_str(line) {
        Ok(j) => j,
        Err(e) => return json!({"ok": false, "code": 8, "error": format!("bad request json: {e}")}),
    };
    let id = request.get("id").cloned();
    let op = request
        .get("op")
        .and_then(|o| o.as_str())
        .unwrap_or_default();

    let auth_response = |ok: bool, message: &str| {
        let mut response = if ok {
            json!({"ok": true, "result": true})
        } else {
            json!({"ok": false, "code": AUTH_ERROR_CODE, "error": message})
        };
        if let (Some(id), Some(obj)) = (id.clone(), response.as_object_mut()) {
            obj.insert("id".into(), id);
        }
        response
    };

    if op == "auth" {
        let Auth::Token(expected) = auth else {
            // Answering "ok" would let a client believe it authenticated
            // against something. Say plainly that this transport has no token.
            return auth_response(false, "this connection does not use token auth");
        };
        let supplied = request.get("token").and_then(|t| t.as_str()).unwrap_or("");
        if token_matches(expected, supplied) {
            *authenticated = true;
            return auth_response(true, "");
        }
        // Do not distinguish "missing" from "wrong": both are one message.
        return auth_response(false, "invalid token");
    }

    if !*authenticated {
        return auth_response(
            false,
            "authentication required: send {\"op\":\"auth\",\"token\":...} first",
        );
    }
    handle_request(db, request, id, txn)
}

fn handle_request(db: &Db, request: J, id: Option<J>, txn: &mut Option<Txn>) -> J {
    let op = request
        .get("op")
        .and_then(|o| o.as_str())
        .unwrap_or_default();
    let required_string = |key: &str| {
        request
            .get(key)
            .and_then(|value| value.as_str())
            .ok_or_else(|| Error::InvalidArgument(format!("missing '{key}'")))
    };
    if txn
        .as_ref()
        .is_some_and(|transaction| transaction.elapsed() > SIDECAR_TXN_TIMEOUT)
    {
        // Dropping Txn is rollback: no WAL entry has been published.
        txn.take();
        let mut response = json!({
            "ok": false,
            "code": 8,
            "error": "transaction exceeded the 30 second sidecar deadline and was rolled back"
        });
        if let (Some(id), Some(object)) = (id, response.as_object_mut()) {
            object.insert("id".into(), id);
        }
        return response;
    }
    let result = match op {
        "ping" => Ok(json!("pong")),
        "query" if txn.is_some() => Err(Error::InvalidArgument(
            "an explicit transaction is active; use query_in_txn or commit/rollback".into(),
        )),
        "query" => match request.get("sql").and_then(|s| s.as_str()) {
            Some(sql) => request
                .get("params")
                .map_or_else(
                    || db.query(sql),
                    |params| jsonio::query_with_params_json(db, sql, params),
                )
                .map(|out| jsonio::output_to_json(&out)),
            None => Err(elitesql_core::Error::InvalidArgument(
                "missing 'sql'".into(),
            )),
        },
        "search_vector" => jsonio::search_vector_json(db, &request),
        "create_vector_index" => jsonio::create_vector_index_json(db, &request),
        "search_text" => jsonio::search_text_json(db, &request),
        "create_text_index" => jsonio::create_text_index_json(db, &request),
        "search_hybrid" => jsonio::search_hybrid_json(db, &request),
        "begin" => {
            if txn.is_some() {
                Err(Error::InvalidArgument(
                    "a transaction is already active on this connection".into(),
                ))
            } else {
                *txn = Some(db.begin());
                Ok(json!(true))
            }
        }
        "query_in_txn" => (|| {
            let sql = required_string("sql")?;
            let transaction = txn
                .as_mut()
                .ok_or_else(|| Error::InvalidArgument("no active transaction".into()))?;
            let output = match request.get("params") {
                Some(params) => jsonio::query_txn_with_params_json(transaction, sql, params),
                None => transaction.query(sql),
            }?;
            Ok(jsonio::output_to_json(&output))
        })(),
        "txn_insert" => (|| {
            let table = required_string("table")?;
            let record = request
                .get("record")
                .ok_or_else(|| Error::InvalidArgument("missing 'record'".into()))?;
            let transaction = txn
                .as_mut()
                .ok_or_else(|| Error::InvalidArgument("no active transaction".into()))?;
            let id = transaction.insert(table, jsonio::json_to_record(record)?)?;
            let inserted = transaction
                .get(table, &id)?
                .expect("staged insert is visible in its transaction");
            Ok(json!({"id": id, "record": jsonio::record_to_json(&inserted)}))
        })(),
        "txn_get" => (|| {
            let transaction = txn
                .as_mut()
                .ok_or_else(|| Error::InvalidArgument("no active transaction".into()))?;
            let record = transaction.get(required_string("table")?, required_string("id")?)?;
            Ok(json!({"record": record.as_ref().map(jsonio::record_to_json)}))
        })(),
        "txn_update" => (|| {
            let patch = request
                .get("patch")
                .ok_or_else(|| Error::InvalidArgument("missing 'patch'".into()))?;
            txn.as_mut()
                .ok_or_else(|| Error::InvalidArgument("no active transaction".into()))?
                .update(
                    required_string("table")?,
                    required_string("id")?,
                    jsonio::json_to_record(patch)?,
                )?;
            Ok(json!(true))
        })(),
        "txn_delete" => (|| {
            let deleted = txn
                .as_mut()
                .ok_or_else(|| Error::InvalidArgument("no active transaction".into()))?
                .delete(required_string("table")?, required_string("id")?)?;
            Ok(json!(deleted))
        })(),
        "commit" => match txn.take() {
            Some(transaction) => transaction.commit().map(|version| json!(version)),
            None => Err(Error::InvalidArgument("no active transaction".into())),
        },
        "rollback" => match txn.take() {
            Some(transaction) => {
                transaction.rollback();
                Ok(json!(true))
            }
            None => Err(Error::InvalidArgument("no active transaction".into())),
        },
        "checkpoint" => db.checkpoint().map(|()| json!(true)),
        "compact" => db.compact().map(|()| json!(true)),
        other => Err(elitesql_core::Error::InvalidArgument(format!(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_call(writer: &mut UnixStream, reader: &mut BufReader<UnixStream>, request: J) -> J {
        writeln!(writer, "{request}").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// A request on a Unix-socket-style connection, which starts trusted.
    fn trusted(db: &Db, request: J) -> J {
        let mut authenticated = true;
        dispatch(db, &request.to_string(), &Auth::Trusted, &mut authenticated)
    }

    #[test]
    fn sidecar_query_binds_positional_and_named_parameters() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("sidecar-params.esql")).unwrap();
        db.query("CREATE TABLE docs (name text NOT NULL, payload blob NOT NULL)")
            .unwrap();

        let inserted = trusted(
            &db,
            json!({
                "op": "query",
                "sql": "INSERT INTO docs (name, payload) VALUES (%s, %s)",
                "params": ["x' OR TRUE --", {"$t": "blob", "hex": "00ff"}]
            }),
        );
        assert_eq!(inserted["ok"], true);

        let selected = trusted(
            &db,
            json!({
                "op": "query",
                "sql": "SELECT name, payload FROM docs WHERE name = %(name)s LIMIT %(limit)s",
                "params": {"name": "x' OR TRUE --", "limit": 1}
            }),
        );
        assert_eq!(selected["ok"], true);
        assert_eq!(selected["result"]["rows"][0][0], "x' OR TRUE --");
        assert_eq!(selected["result"]["rows"][0][1]["$t"], "blob");
    }

    #[test]
    fn sidecar_connection_transaction_is_atomic_and_returns_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("sidecar-txn.esql")).unwrap();
        db.query(
            "CREATE TABLE docs (doc_id int AUTO_INCREMENT, title text NOT NULL, done bool NOT NULL)",
        )
        .unwrap();
        let mut authenticated = true;
        let mut txn = None;
        let call = |request: J, authenticated: &mut bool, txn: &mut Option<Txn>| {
            dispatch_with_txn(
                &db,
                &request.to_string(),
                &Auth::Trusted,
                authenticated,
                txn,
            )
        };

        assert_eq!(
            call(json!({"op": "begin"}), &mut authenticated, &mut txn)["ok"],
            true
        );
        let inserted = call(
            json!({
                "op": "query_in_txn",
                "sql": "INSERT INTO docs (title, done) VALUES (%s, %s) RETURNING id, doc_id",
                "params": ["contract", false]
            }),
            &mut authenticated,
            &mut txn,
        );
        assert_eq!(inserted["result"]["rows"][0][1], 1);
        let id = inserted["result"]["rows"][0][0]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            call(
                json!({
                    "op": "txn_update", "table": "docs", "id": id,
                    "patch": {"done": true}
                }),
                &mut authenticated,
                &mut txn,
            )["ok"],
            true
        );
        assert!(
            db.scan("docs").unwrap().is_empty(),
            "uncommitted row leaked"
        );
        assert_eq!(
            call(json!({"op": "commit"}), &mut authenticated, &mut txn)["ok"],
            true
        );
        let selected = db.query("SELECT doc_id, done FROM docs").unwrap();
        assert_eq!(
            jsonio::output_to_json(&selected)["rows"][0],
            json!([1, true])
        );

        call(json!({"op": "begin"}), &mut authenticated, &mut txn);
        call(
            json!({
                "op": "txn_insert", "table": "docs",
                "record": {"title": "rollback", "done": false}
            }),
            &mut authenticated,
            &mut txn,
        );
        call(json!({"op": "rollback"}), &mut authenticated, &mut txn);
        assert_eq!(db.scan("docs").unwrap().len(), 1);
    }

    #[test]
    fn sidecar_disconnect_rolls_back_active_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("disconnect.esql")).unwrap());
        db.query("CREATE TABLE docs (title text NOT NULL)").unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        let served = db.clone();
        let worker = std::thread::spawn(move || handle_connection(served, server, Auth::Trusted));
        let mut responses = BufReader::new(client.try_clone().unwrap());

        writeln!(client, "{}", json!({"op": "begin"})).unwrap();
        client.flush().unwrap();
        let mut line = String::new();
        responses.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<J>(&line).unwrap()["ok"], true);

        writeln!(
            client,
            "{}",
            json!({
                "op": "query_in_txn",
                "sql": "INSERT INTO docs (title) VALUES ('must rollback')"
            })
        )
        .unwrap();
        client.flush().unwrap();
        line.clear();
        responses.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<J>(&line).unwrap()["ok"], true);
        drop(responses);
        drop(client);
        worker.join().unwrap();
        assert!(db.scan("docs").unwrap().is_empty());
    }

    #[test]
    fn four_sidecar_workers_keep_credit_and_document_commits_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            Db::create_with(
                dir.path().join("workers.esql"),
                elitesql_core::DbOptions {
                    durability: elitesql_core::Durability::Fast,
                    ..elitesql_core::DbOptions::default()
                },
            )
            .unwrap(),
        );
        db.query("CREATE TABLE users (user_id int AUTO_INCREMENT, credits int NOT NULL)")
            .unwrap();
        db.query("CREATE TABLE docs (owner_id int REFERENCES users(user_id), worker int NOT NULL)")
            .unwrap();
        db.query("INSERT INTO users (credits) VALUES (1000)")
            .unwrap();

        let mut app_workers = Vec::new();
        let mut server_workers = Vec::new();
        for worker_id in 0..4 {
            let (server, mut client) = UnixStream::pair().unwrap();
            let served = db.clone();
            server_workers.push(std::thread::spawn(move || {
                handle_connection(served, server, Auth::Trusted)
            }));
            app_workers.push(std::thread::spawn(move || {
                let mut reader = BufReader::new(client.try_clone().unwrap());
                let mut committed = 0usize;
                while committed < 250 {
                    assert_eq!(
                        socket_call(&mut client, &mut reader, json!({"op": "begin"}))["ok"],
                        true
                    );
                    let charged = socket_call(
                        &mut client,
                        &mut reader,
                        json!({
                            "op": "query_in_txn",
                            "sql": "UPDATE users SET credits = credits - 1 WHERE user_id = 1 AND credits >= 1"
                        }),
                    );
                    assert_eq!(charged["result"]["affected"], 1);
                    let inserted = socket_call(
                        &mut client,
                        &mut reader,
                        json!({
                            "op": "query_in_txn",
                            "sql": "INSERT INTO docs (owner_id, worker) VALUES (1, %s)",
                            "params": [worker_id]
                        }),
                    );
                    assert_eq!(inserted["ok"], true);
                    let commit = socket_call(
                        &mut client,
                        &mut reader,
                        json!({"op": "commit"}),
                    );
                    if commit["ok"] == true {
                        committed += 1;
                    } else {
                        assert_eq!(commit["code"], 9, "unexpected commit error: {commit}");
                    }
                }
            }));
        }
        for worker in app_workers {
            worker.join().unwrap();
        }
        for worker in server_workers {
            worker.join().unwrap();
        }

        assert_eq!(db.scan("docs").unwrap().len(), 1000);
        assert_eq!(
            db.scan("users").unwrap()[0].1["credits"],
            elitesql_core::Value::Int64(0)
        );
    }

    fn token_db() -> (tempfile::TempDir, Db, Auth) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("sidecar-auth.esql")).unwrap();
        db.query("CREATE TABLE t (n int64)").unwrap();
        (dir, db, Auth::Token(Arc::new("s3cr3t".to_string())))
    }

    /// Nothing reaches the engine before a successful `auth` — not even `ping`,
    /// which would otherwise confirm a live database to an unauthenticated peer.
    #[test]
    fn every_op_is_refused_before_authentication() {
        let (_d, db, auth) = token_db();
        let mut authenticated = false;
        for request in [
            json!({"op": "ping"}),
            json!({"op": "query", "sql": "SELECT n FROM t"}),
            json!({"op": "checkpoint"}),
            json!({"op": "compact"}),
        ] {
            let response = dispatch(&db, &request.to_string(), &auth, &mut authenticated);
            assert_eq!(response["ok"], false, "for {request}");
            assert_eq!(response["code"], AUTH_ERROR_CODE, "for {request}");
        }
        assert!(!authenticated);
    }

    #[test]
    fn a_wrong_token_neither_authenticates_nor_leaks_which_part_was_wrong() {
        let (_d, db, auth) = token_db();
        let mut authenticated = false;
        for supplied in [
            json!({"op": "auth"}),
            json!({"op": "auth", "token": "nope"}),
        ] {
            let response = dispatch(&db, &supplied.to_string(), &auth, &mut authenticated);
            assert_eq!(response["ok"], false);
            assert_eq!(response["error"], "invalid token");
            assert!(!authenticated, "a failed auth must not open the connection");
        }
    }

    #[test]
    fn the_right_token_opens_the_connection_for_later_requests() {
        let (_d, db, auth) = token_db();
        let mut authenticated = false;
        let ok = dispatch(
            &db,
            &json!({"op": "auth", "token": "s3cr3t"}).to_string(),
            &auth,
            &mut authenticated,
        );
        assert_eq!(ok["ok"], true);
        assert!(authenticated);
        let pong = dispatch(
            &db,
            &json!({"op": "ping"}).to_string(),
            &auth,
            &mut authenticated,
        );
        assert_eq!(pong["result"], "pong");
    }

    /// A Unix socket has no token to check, so `auth` there is an error rather
    /// than a success that would imply a credential was verified.
    #[test]
    fn auth_on_a_trusted_transport_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("sidecar-trusted.esql")).unwrap();
        let response = trusted(&db, json!({"op": "auth", "token": "anything"}));
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"], "this connection does not use token auth");
    }

    #[test]
    fn the_request_id_is_echoed_on_auth_responses_too() {
        let (_d, db, auth) = token_db();
        let mut authenticated = false;
        let denied = dispatch(
            &db,
            &json!({"op": "ping", "id": 7}).to_string(),
            &auth,
            &mut authenticated,
        );
        assert_eq!(denied["id"], 7, "a client must match errors to requests");
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_value() {
        assert!(token_matches("s3cr3t", "s3cr3t"));
        assert!(!token_matches("s3cr3t", "s3cr3"));
        assert!(!token_matches("s3cr3t", "s3cr3t "));
        assert!(!token_matches("s3cr3t", ""));
        assert!(!token_matches("", "x"));
        assert!(token_matches("", ""));
    }

    #[test]
    fn tcp_without_a_token_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("sidecar-notoken.esql")).unwrap();
        let err = serve(
            db,
            ServeOptions {
                tcp: Some("127.0.0.1:0".into()),
                ..ServeOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("requires a token"), "{err}");
    }

    #[test]
    fn a_transport_must_be_chosen_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("sidecar-transport.esql")).unwrap();
        let err = serve(db, ServeOptions::default()).unwrap_err();
        assert!(err.contains("needs a socket path or --tcp"), "{err}");

        let db = Db::open_or_create(dir.path().join("sidecar-transport.esql")).unwrap();
        let err = serve(
            db,
            ServeOptions {
                socket_path: Some("/tmp/x.sock".into()),
                tcp: Some("127.0.0.1:0".into()),
                token: Some("t".into()),
                ..ServeOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("one transport"), "{err}");
    }
}
