"""EliteSQL Python binding.

Two ways to use it:

- ``EliteSQL``: embedded, in-process, over the C ABI (libelitesql) via ctypes.
  ctypes releases the GIL during every foreign call, so DB operations from
  multiple Python threads genuinely run in parallel inside the Rust engine.
  One process owns the database (lock file); use threads or async within it.

- ``SidecarClient``: connects to a ``elitesql serve <db> <socket>`` process
  over a Unix socket. This is the deployment mode for gunicorn/uwsgi with
  multiple workers: every worker talks to the single engine process.

Values: scalars map to Python natively; dates/times/timestamps arrive as
``datetime.date`` / ``datetime.time`` / ``datetime.datetime`` (UTC), blobs as
``bytes``, vectors as ``list[float]``.
"""

from __future__ import annotations

import ctypes
import datetime as _dt
import json
import math
import os
import socket
import threading
from pathlib import Path
from typing import Any, Iterable, Optional

__all__ = ["EliteSQL", "SidecarClient", "Snapshot", "EliteSQLError", "check", "repair"]


class EliteSQLError(Exception):
    """Engine error; ``code`` carries the stable elitesql status code."""

    def __init__(self, code: int, message: str):
        super().__init__(f"[elitesql:{code}] {message}")
        self.code = code

    CONFLICT_RETRY = 9  # retry the transaction/operation


# --- library loading -------------------------------------------------------

_LIB = None
_LIB_LOCK = threading.Lock()


def _candidate_paths() -> Iterable[Path]:
    env = os.environ.get("ELITESQL_LIB")
    if env:
        yield Path(env)
    here = Path(__file__).resolve()
    names = {
        "darwin": "libelitesql.dylib",
        "linux": "libelitesql.so",
    }.get(os.uname().sysname.lower() if hasattr(os, "uname") else "linux", "libelitesql.so")
    for base in [here.parent, *here.parents]:
        for sub in ("", "target/release", "target/debug"):
            yield base / sub / names
        if (base / "Cargo.toml").exists():
            break


def _load_lib(explicit: Optional[str] = None) -> ctypes.CDLL:
    global _LIB
    with _LIB_LOCK:
        if _LIB is not None and explicit is None:
            return _LIB
        paths = [Path(explicit)] if explicit else list(_candidate_paths())
        last_err = None
        for p in paths:
            if not p.is_file():
                continue
            try:
                lib = ctypes.CDLL(str(p))
            except OSError as e:  # pragma: no cover
                last_err = e
                continue
            _configure(lib)
            _LIB = lib
            return lib
        raise EliteSQLError(
            8,
            "libelitesql not found; build it with `cargo build --release -p elitesql-ffi` "
            "or set ELITESQL_LIB=/path/to/libelitesql.dylib"
            + (f" (last error: {last_err})" if last_err else ""),
        )


def _configure(lib: ctypes.CDLL) -> None:
    lib.elitesql_open.restype = ctypes.c_uint32
    lib.elitesql_open.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    lib.elitesql_close.restype = ctypes.c_uint32
    lib.elitesql_close.argtypes = [ctypes.c_void_p]
    lib.elitesql_query.restype = ctypes.c_uint32
    lib.elitesql_query.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    lib.elitesql_query_params.restype = ctypes.c_uint32
    lib.elitesql_query_params.argtypes = [
        ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.elitesql_search_vector.restype = ctypes.c_uint32
    lib.elitesql_search_vector.argtypes = [
        ctypes.c_void_p,
        ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.elitesql_create_vector_index.restype = ctypes.c_uint32
    lib.elitesql_create_vector_index.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    lib.elitesql_create_text_index.restype = ctypes.c_uint32
    lib.elitesql_create_text_index.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    lib.elitesql_search_text.restype = ctypes.c_uint32
    lib.elitesql_search_text.argtypes = [
        ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.elitesql_search_hybrid.restype = ctypes.c_uint32
    lib.elitesql_search_hybrid.argtypes = [
        ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.elitesql_snapshot_open.restype = ctypes.c_uint32
    lib.elitesql_snapshot_open.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p)]
    lib.elitesql_snapshot_close.restype = ctypes.c_uint32
    lib.elitesql_snapshot_close.argtypes = [ctypes.c_void_p]
    lib.elitesql_snapshot_get.restype = ctypes.c_uint32
    lib.elitesql_snapshot_get.argtypes = [
        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.elitesql_snapshot_scan.restype = ctypes.c_uint32
    lib.elitesql_snapshot_scan.argtypes = [
        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.elitesql_checkpoint.restype = ctypes.c_uint32
    lib.elitesql_checkpoint.argtypes = [ctypes.c_void_p]
    lib.elitesql_compact.restype = ctypes.c_uint32
    lib.elitesql_compact.argtypes = [ctypes.c_void_p]
    lib.elitesql_check.restype = ctypes.c_uint32
    lib.elitesql_check.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    lib.elitesql_repair.restype = ctypes.c_uint32
    lib.elitesql_repair.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    lib.elitesql_last_error.restype = ctypes.c_char_p
    lib.elitesql_free_string.restype = None
    lib.elitesql_free_string.argtypes = [ctypes.c_void_p]
    lib.elitesql_version.restype = ctypes.c_char_p


def _raise_if(lib: ctypes.CDLL, status: int) -> None:
    if status != 0:
        msg = lib.elitesql_last_error()
        raise EliteSQLError(status, msg.decode("utf-8", "replace") if msg else "unknown error")


def _take_string(lib: ctypes.CDLL, ptr: ctypes.c_void_p) -> str:
    try:
        return ctypes.string_at(ptr.value).decode("utf-8")
    finally:
        lib.elitesql_free_string(ptr)


# --- value decoding ---------------------------------------------------------

_EPOCH_DATE = _dt.date(1970, 1, 1)


def _decode_value(v: Any) -> Any:
    if isinstance(v, dict) and "$t" in v:
        t = v["$t"]
        if t == "date":
            return _EPOCH_DATE + _dt.timedelta(days=v["days"])
        if t == "time":
            us = v["us"]
            return _dt.time(
                us // 3_600_000_000,
                (us // 60_000_000) % 60,
                (us // 1_000_000) % 60,
                us % 1_000_000,
            )
        if t == "timestamp":
            return _dt.datetime.fromtimestamp(v["us"] / 1_000_000, tz=_dt.timezone.utc)
        if t == "blob":
            return bytes.fromhex(v["hex"])
        if t == "vector":
            return list(v["v"])
        if t == "json":
            return v["v"]
        if t == "float64":
            return float(v["repr"])
    return v


def _decode_result(result: Any) -> Any:
    if isinstance(result, dict) and "rows" in result and "columns" in result:
        result = dict(result)
        result["rows"] = [[_decode_value(c) for c in row] for row in result["rows"]]
    return result


def _decode_record(record: dict) -> dict:
    return {k: _decode_value(v) for k, v in record.items()}


def _json_native(value: Any) -> Any:
    """Validate/normalize a value intended for an EliteSQL JSON parameter."""
    if value is None or isinstance(value, (bool, str)):
        return value
    if isinstance(value, int):
        if not -(2 ** 63) <= value < 2 ** 63:
            raise OverflowError("JSON integer parameter is out of int64 range")
        return value
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError("JSON parameters require finite numbers")
        return value
    if isinstance(value, (list, tuple)):
        return [_json_native(item) for item in value]
    if isinstance(value, dict):
        if not all(isinstance(key, str) for key in value):
            raise TypeError("JSON parameter object keys must be strings")
        return {key: _json_native(item) for key, item in value.items()}
    raise TypeError(f"unsupported nested JSON parameter type: {type(value).__name__}")


def _encode_param(value: Any) -> Any:
    if value is None or isinstance(value, (bool, str)):
        return value
    if isinstance(value, int):
        if not -(2 ** 63) <= value < 2 ** 63:
            raise OverflowError("EliteSQL int64 parameter is out of range")
        return value
    if isinstance(value, float):
        if math.isfinite(value):
            return value
        representation = "NaN" if math.isnan(value) else ("inf" if value > 0 else "-inf")
        return {"$t": "float64", "repr": representation}
    if isinstance(value, (bytes, bytearray, memoryview)):
        return {"$t": "blob", "hex": bytes(value).hex()}
    if isinstance(value, _dt.datetime):
        if value.tzinfo is None:
            value = value.replace(tzinfo=_dt.timezone.utc)
        else:
            value = value.astimezone(_dt.timezone.utc)
        delta = value - _dt.datetime(1970, 1, 1, tzinfo=_dt.timezone.utc)
        micros = delta.days * 86_400_000_000 + delta.seconds * 1_000_000 + delta.microseconds
        return {"$t": "timestamp", "us": micros}
    if isinstance(value, _dt.date):
        return {"$t": "date", "days": (value - _EPOCH_DATE).days}
    if isinstance(value, _dt.time):
        if value.tzinfo is not None and value.utcoffset() is not None:
            raise TypeError("EliteSQL time parameters cannot carry a timezone")
        micros = (
            value.hour * 3_600_000_000
            + value.minute * 60_000_000
            + value.second * 1_000_000
            + value.microsecond
        )
        return {"$t": "time", "us": micros}
    if isinstance(value, (dict, list, tuple)):
        return {"$t": "json", "v": _json_native(value)}
    raise TypeError(f"unsupported EliteSQL parameter type: {type(value).__name__}")


def _encode_params(params: Any) -> list[Any] | dict[str, Any]:
    if isinstance(params, dict):
        if not all(isinstance(key, str) for key in params):
            raise TypeError("named SQL parameter keys must be strings")
        return {key: _encode_param(value) for key, value in params.items()}
    if isinstance(params, (list, tuple)):
        return [_encode_param(value) for value in params]
    raise TypeError("SQL params must be a sequence or mapping")


# --- embedded (C ABI) --------------------------------------------------------


class EliteSQL:
    """Embedded EliteSQL over the C ABI. Thread-safe; use as a context manager."""

    def __init__(self, path: str | os.PathLike, durability: Optional[str] = None,
                 read_only: bool = False, lib_path: Optional[str] = None,
                 memory: Optional[dict[str, Any]] = None):
        self._lib = _load_lib(lib_path)
        handle = ctypes.c_void_p()
        opts: dict[str, Any] = {}
        if durability:
            opts["durability"] = durability
        if read_only:
            opts["read_only"] = True
        if memory is not None:
            opts["memory"] = memory
        options = json.dumps(opts) if opts else None
        status = self._lib.elitesql_open(
            str(path).encode(), options.encode() if options else None, ctypes.byref(handle)
        )
        _raise_if(self._lib, status)
        self._handle: Optional[ctypes.c_void_p] = handle

    # -- lifecycle
    def close(self) -> None:
        if self._handle is not None:
            _raise_if(self._lib, self._lib.elitesql_close(self._handle))
            self._handle = None

    def __enter__(self) -> "EliteSQL":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    def __del__(self):  # best effort
        try:
            self.close()
        except Exception:
            pass

    def _h(self) -> ctypes.c_void_p:
        if self._handle is None:
            raise EliteSQLError(8, "database is closed")
        return self._handle

    # -- operations
    def query(self, sql: str, params: Any = None) -> Any:
        """Execute one SQL statement.

        Returns {"columns", "rows"} for SELECT (values decoded to Python
        types), {"inserted": [ids]}, {"affected": n} or {"ok": True}.
        ``params`` is a sequence for ``?``/``%s`` placeholders or a mapping
        for ``%(name)s`` placeholders. Values are bound, never interpolated.
        """
        out = ctypes.c_void_p()
        if params is None:
            status = self._lib.elitesql_query(self._h(), sql.encode(), ctypes.byref(out))
        else:
            encoded = json.dumps(_encode_params(params), separators=(",", ":")).encode()
            status = self._lib.elitesql_query_params(
                self._h(), sql.encode(), encoded, ctypes.byref(out)
            )
        _raise_if(self._lib, status)
        return _decode_result(json.loads(_take_string(self._lib, out)))

    def search_vector(self, table: str, column: str, vector: list[float], top_k: int = 10,
                      ef_search: Optional[int] = None, filter: Optional[dict] = None) -> list[dict]:
        params: dict[str, Any] = {
            "table": table, "column": column, "vector": vector, "top_k": top_k,
        }
        if ef_search is not None:
            params["ef_search"] = ef_search
        if filter is not None:
            params["filter"] = filter
        out = ctypes.c_void_p()
        status = self._lib.elitesql_search_vector(
            self._h(), json.dumps(params).encode(), ctypes.byref(out)
        )
        _raise_if(self._lib, status)
        hits = json.loads(_take_string(self._lib, out))["hits"]
        for h in hits:
            h["record"] = _decode_record(h["record"])
        return hits

    def create_vector_index(self, table: str, column: str, metric: str = "cosine",
                            mode: str = "sync", m: Optional[int] = None,
                            ef_construction: Optional[int] = None,
                            quantized: bool = False) -> None:
        params: dict[str, Any] = {"table": table, "column": column, "metric": metric, "mode": mode}
        if m is not None:
            params["m"] = m
        if ef_construction is not None:
            params["ef_construction"] = ef_construction
        if quantized:
            params["quantized"] = True
        status = self._lib.elitesql_create_vector_index(self._h(), json.dumps(params).encode())
        _raise_if(self._lib, status)

    def create_text_index(self, table: str, column: str) -> None:
        params = json.dumps({"table": table, "column": column}).encode()
        _raise_if(self._lib, self._lib.elitesql_create_text_index(self._h(), params))

    def search_text(self, table: str, column: str, query: str, top_k: int = 10,
                    filter: Optional[dict] = None) -> list[dict]:
        params: dict[str, Any] = {
            "table": table, "column": column, "query": query, "top_k": top_k,
        }
        if filter is not None:
            params["filter"] = filter
        out = ctypes.c_void_p()
        status = self._lib.elitesql_search_text(self._h(), json.dumps(params).encode(),
                                              ctypes.byref(out))
        _raise_if(self._lib, status)
        hits = json.loads(_take_string(self._lib, out))["hits"]
        for h in hits:
            h["record"] = _decode_record(h["record"])
        return hits

    def search_hybrid(self, table: str, text: Optional[tuple[str, str]] = None,
                      vector: Optional[tuple[str, list[float]]] = None, top_k: int = 10,
                      ef_search: Optional[int] = None,
                      filter: Optional[dict] = None) -> list[dict]:
        """RRF fusion of BM25 text and ANN vector rankings.

        text=(column, query); vector=(column, embedding). One or both.
        """
        params: dict[str, Any] = {"table": table, "top_k": top_k}
        if text is not None:
            params["text"] = {"column": text[0], "query": text[1]}
        if vector is not None:
            params["vector"] = {"column": vector[0], "vector": vector[1]}
        if ef_search is not None:
            params["ef_search"] = ef_search
        if filter is not None:
            params["filter"] = filter
        out = ctypes.c_void_p()
        status = self._lib.elitesql_search_hybrid(self._h(), json.dumps(params).encode(),
                                                ctypes.byref(out))
        _raise_if(self._lib, status)
        hits = json.loads(_take_string(self._lib, out))["hits"]
        for h in hits:
            h["record"] = _decode_record(h["record"])
        return hits

    def snapshot(self) -> "Snapshot":
        """A stable read position; use as a context manager."""
        handle = ctypes.c_void_p()
        _raise_if(self._lib, self._lib.elitesql_snapshot_open(self._h(), ctypes.byref(handle)))
        return Snapshot(self, handle)

    def checkpoint(self) -> None:
        _raise_if(self._lib, self._lib.elitesql_checkpoint(self._h()))

    def compact(self) -> None:
        _raise_if(self._lib, self._lib.elitesql_compact(self._h()))

    @property
    def version(self) -> str:
        return self._lib.elitesql_version().decode()


class Snapshot:
    """Stable reads: `get`/`scan` see the database as of snapshot time,
    while other writers keep committing. Close (or use `with`) to let
    compaction reclaim old versions."""

    def __init__(self, db: "EliteSQL", handle: ctypes.c_void_p):
        self._db = db
        self._handle: Optional[ctypes.c_void_p] = handle

    def _h(self) -> ctypes.c_void_p:
        if self._handle is None:
            raise EliteSQLError(8, "snapshot is closed")
        return self._handle

    def get(self, table: str, id: str) -> Optional[dict]:
        out = ctypes.c_void_p()
        status = self._db._lib.elitesql_snapshot_get(
            self._db._h(), self._h(), table.encode(), id.encode(), ctypes.byref(out)
        )
        _raise_if(self._db._lib, status)
        record = json.loads(_take_string(self._db._lib, out))["record"]
        return _decode_record(record) if record is not None else None

    def scan(self, table: str) -> list[dict]:
        out = ctypes.c_void_p()
        status = self._db._lib.elitesql_snapshot_scan(
            self._db._h(), self._h(), table.encode(), ctypes.byref(out)
        )
        _raise_if(self._db._lib, status)
        rows = json.loads(_take_string(self._db._lib, out))["rows"]
        return [_decode_record(r) for r in rows]

    def close(self) -> None:
        if self._handle is not None:
            self._db._lib.elitesql_snapshot_close(self._handle)
            self._handle = None

    def __enter__(self) -> "Snapshot":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    def __del__(self):  # best effort
        try:
            self.close()
        except Exception:
            pass


def check(path: str | os.PathLike, lib_path: Optional[str] = None) -> dict:
    """Offline integrity check (do not run while the db is open elsewhere)."""
    lib = _load_lib(lib_path)
    out = ctypes.c_void_p()
    _raise_if(lib, lib.elitesql_check(str(path).encode(), ctypes.byref(out)))
    return json.loads(_take_string(lib, out))


def repair(src: str | os.PathLike, dst: str | os.PathLike,
           lib_path: Optional[str] = None) -> dict:
    """Salvage every recoverable record from src into a fresh db at dst."""
    lib = _load_lib(lib_path)
    out = ctypes.c_void_p()
    _raise_if(lib, lib.elitesql_repair(str(src).encode(), str(dst).encode(), ctypes.byref(out)))
    return json.loads(_take_string(lib, out))


# --- sidecar client -----------------------------------------------------------


class SidecarClient:
    """Client for ``elitesql serve`` — the multi-worker and remote-host mode.

    Two transports, matching the two ways the server can listen::

        SidecarClient("/tmp/elitesql.sock")             # Unix socket, same host
        SidecarClient(host="db", port=7070, token=tok)  # TCP, needs the token

    A Unix socket is authenticated by filesystem permissions. TCP is not, so a
    token is required and is sent as the first request on every connection,
    including after a reconnect. The protocol is not encrypted: reach another
    host through an SSH tunnel, a VPN or a private network.

    Each client owns one connection; it is thread-safe (a lock serializes
    request/response pairs). Create one per worker process.
    """

    def __init__(self, socket_path: str | os.PathLike | None = None, *,
                 host: Optional[str] = None, port: Optional[int] = None,
                 token: Optional[str] = None, timeout: Optional[float] = None):
        if (socket_path is None) == (host is None):
            raise ValueError("pass either socket_path or host= and port=, not both")
        if host is not None and port is None:
            raise ValueError("host= also needs port=")
        if host is not None and not token:
            raise ValueError("a TCP sidecar requires token=")
        self._path = str(socket_path) if socket_path is not None else None
        self._host = host
        self._port = port
        self._token = token
        self._timeout = timeout
        self._lock = threading.Lock()
        self._connect()

    def _connect(self) -> None:
        if self._path is not None:
            self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            if self._timeout is not None:
                self._sock.settimeout(self._timeout)
            self._sock.connect(self._path)
            self._file = self._sock.makefile("rwb")
            return
        self._sock = socket.create_connection((self._host, self._port), timeout=self._timeout)
        # Nagle would add latency to the small request/response pairs this
        # protocol is made of.
        self._sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._file = self._sock.makefile("rwb")
        self._authenticate()

    def _authenticate(self) -> None:
        """Sent before anything else; the server refuses every other op first."""
        self._call({"op": "auth", "token": self._token}, _locked=False)

    def close(self) -> None:
        try:
            self._file.close()
            self._sock.close()
        except OSError:
            pass

    def __enter__(self) -> "SidecarClient":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    def _call(self, request: dict, _locked: bool = True) -> Any:
        payload = (json.dumps(request) + "\n").encode()
        if _locked:
            with self._lock:
                self._file.write(payload)
                self._file.flush()
                line = self._file.readline()
        else:
            # The auth handshake runs inside _connect(), which already owns the
            # connection; taking the lock again would deadlock a reconnect.
            self._file.write(payload)
            self._file.flush()
            line = self._file.readline()
        if not line:
            raise EliteSQLError(1, "sidecar closed the connection")
        response = json.loads(line)
        if not response.get("ok"):
            raise EliteSQLError(int(response.get("code", 1)), str(response.get("error", "unknown")))
        return response.get("result")

    def ping(self) -> bool:
        return self._call({"op": "ping"}) == "pong"

    def query(self, sql: str, params: Any = None) -> Any:
        request: dict[str, Any] = {"op": "query", "sql": sql}
        if params is not None:
            request["params"] = _encode_params(params)
        return _decode_result(self._call(request))

    def search_vector(self, table: str, column: str, vector: list[float], top_k: int = 10,
                      ef_search: Optional[int] = None, filter: Optional[dict] = None) -> list[dict]:
        request: dict[str, Any] = {
            "op": "search_vector", "table": table, "column": column,
            "vector": vector, "top_k": top_k,
        }
        if ef_search is not None:
            request["ef_search"] = ef_search
        if filter is not None:
            request["filter"] = filter
        hits = self._call(request)["hits"]
        for h in hits:
            h["record"] = _decode_record(h["record"])
        return hits

    def create_vector_index(self, table: str, column: str, metric: str = "cosine",
                            mode: str = "sync", m: Optional[int] = None,
                            ef_construction: Optional[int] = None,
                            quantized: bool = False) -> None:
        request: dict[str, Any] = {
            "op": "create_vector_index", "table": table, "column": column,
            "metric": metric, "mode": mode,
        }
        if m is not None:
            request["m"] = m
        if ef_construction is not None:
            request["ef_construction"] = ef_construction
        if quantized:
            request["quantized"] = True
        self._call(request)

    def create_text_index(self, table: str, column: str) -> None:
        self._call({"op": "create_text_index", "table": table, "column": column})

    def search_text(self, table: str, column: str, query: str, top_k: int = 10,
                    filter: Optional[dict] = None) -> list[dict]:
        request: dict[str, Any] = {
            "op": "search_text", "table": table, "column": column,
            "query": query, "top_k": top_k,
        }
        if filter is not None:
            request["filter"] = filter
        hits = self._call(request)["hits"]
        for h in hits:
            h["record"] = _decode_record(h["record"])
        return hits

    def search_hybrid(self, table: str, text: Optional[tuple[str, str]] = None,
                      vector: Optional[tuple[str, list[float]]] = None, top_k: int = 10,
                      ef_search: Optional[int] = None,
                      filter: Optional[dict] = None) -> list[dict]:
        request: dict[str, Any] = {"op": "search_hybrid", "table": table, "top_k": top_k}
        if text is not None:
            request["text"] = {"column": text[0], "query": text[1]}
        if vector is not None:
            request["vector"] = {"column": vector[0], "vector": vector[1]}
        if ef_search is not None:
            request["ef_search"] = ef_search
        if filter is not None:
            request["filter"] = filter
        hits = self._call(request)["hits"]
        for h in hits:
            h["record"] = _decode_record(h["record"])
        return hits

    def checkpoint(self) -> None:
        self._call({"op": "checkpoint"})

    def compact(self) -> None:
        self._call({"op": "compact"})
