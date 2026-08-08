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

use elitesql_core::{jsonio, Db};
use serde_json::{json, Value as J};

/// Protocol-level error code for authentication, outside the engine's range
/// (`Error::code()` currently returns 1..=16) so clients can tell them apart.
const AUTH_ERROR_CODE: u32 = 20;

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
    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let response = dispatch(&db, &line, &auth, &mut authenticated);
        if writeln!(writer, "{response}").is_err() || writer.flush().is_err() {
            return;
        }
    }
}

/// Applies the connection's auth state, then runs the request. Everything
/// before a successful `auth` on an untrusted transport is refused, so no
/// unauthenticated caller reaches the engine — not even `ping`.
fn dispatch(db: &Db, line: &str, auth: &Auth, authenticated: &mut bool) -> J {
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
    handle_request(db, request, id)
}

fn handle_request(db: &Db, request: J, id: Option<J>) -> J {
    let op = request
        .get("op")
        .and_then(|o| o.as_str())
        .unwrap_or_default();
    let result = match op {
        "ping" => Ok(json!("pong")),
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
