"""Database adapters for the mini-SaaS service.

Three backends share one tiny interface so the *same* service code and the
same workload run against each of them:

- ``embedded``: one ``EliteSQL`` handle shared by every thread of a process.
- ``sidecar``:  one ``SidecarClient`` per connection to ``elitesql serve``.
- ``sqlite``:   one ``sqlite3`` connection per pool slot (WAL, configurable sync),
                the baseline every EliteSQL benchmark in this repo compares to.

A *connection* exposes ``execute``, ``transaction``, ``search_vector`` and
``search_text``; ``execute`` returns a ``Result`` with ``rows``, ``lastrowid``
and ``affected``. SQL is written once with ``?`` placeholders and integer
epoch-milliseconds instead of engine-specific time types.
"""

from __future__ import annotations

import json
import re
import sqlite3
import threading
from collections import deque
from dataclasses import dataclass, field
from typing import Any, Callable, Optional


class Conflict(Exception):
    """A retry-safe optimistic-commit conflict (or SQLite busy)."""


class NotSupported(Exception):
    """The backend cannot run this operation (e.g. vectors on SQLite)."""


@dataclass
class Result:
    rows: list = field(default_factory=list)
    lastrowid: Optional[int] = None
    affected: int = 0

    @property
    def one(self):
        return self.rows[0] if self.rows else None

    @property
    def scalar(self):
        return self.rows[0][0] if self.rows else None


# ---------------------------------------------------------------- EliteSQL --


def _elitesql():
    import elitesql  # imported lazily: the SQLite baseline does not need it
    return elitesql


def _wrap_result(raw: Any) -> Result:
    if isinstance(raw, dict):
        if "rows" in raw:
            return Result(rows=raw["rows"])
        if "lastrowid" in raw or "inserted" in raw:
            inserted = raw.get("inserted") or []
            return Result(lastrowid=raw.get("lastrowid"), affected=len(inserted))
        if "affected" in raw:
            return Result(affected=int(raw["affected"]))
    return Result()


def _translate(error: Exception) -> Exception:
    """Map retry-safe engine errors onto ``Conflict``."""
    code = getattr(error, "code", None)
    if code in (9, 21):  # CONFLICT_RETRY, TRANSACTION_EXPIRED
        return Conflict(str(error))
    return error


class _EliteTx:
    def __init__(self, tx):
        self._tx = tx

    def execute(self, sql: str, params=None) -> Result:
        try:
            return _wrap_result(self._tx.query(sql, params))
        except Exception as error:  # noqa: BLE001 - translated below
            raise _translate(error) from error

    def commit(self) -> None:
        try:
            self._tx.commit()
        except Exception as error:  # noqa: BLE001
            raise _translate(error) from error

    def rollback(self) -> None:
        try:
            self._tx.rollback()
        except Exception:  # noqa: BLE001 - best effort
            pass


class EliteConnection:
    """Wraps either an ``EliteSQL`` handle or a ``SidecarClient``."""

    supports_vectors = True
    supports_fulltext = True

    def __init__(self, handle):
        self._h = handle

    def execute(self, sql: str, params=None) -> Result:
        try:
            return _wrap_result(self._h.query(sql, params))
        except Exception as error:  # noqa: BLE001
            raise _translate(error) from error

    def transaction(self) -> _EliteTx:
        try:
            return _EliteTx(self._h.transaction())
        except Exception as error:  # noqa: BLE001
            raise _translate(error) from error

    @staticmethod
    def _hits(hits: list[dict]) -> list[dict]:
        # The engine reports the physical ULID as ``id``; the application
        # thinks in declared integer ids, which travel inside ``record``.
        return [{"id": h["record"]["id"], "record": h["record"]} for h in hits]

    def search_vector(self, table, column, vector, top_k, filter=None) -> list[dict]:
        try:
            return self._hits(self._h.search_vector(table, column, vector, top_k=top_k, filter=filter))
        except Exception as error:  # noqa: BLE001
            raise _translate(error) from error

    def search_text(self, table, column, query, top_k) -> list[dict]:
        try:
            # The caller ranks and then reads the declared id, so a hit needs
            # that column and nothing else. SQLite's driver below returns
            # rowids for the same reason; asking for the whole row here would
            # have been comparing two different questions.
            return self._hits(self._h.search_text(table, column, query, top_k=top_k,
                                                  columns=["id"]))
        except Exception as error:  # noqa: BLE001
            raise _translate(error) from error

    def create_vector_index(self, table, column) -> None:
        self._h.create_vector_index(table, column, metric="cosine")

    def create_text_index(self, table, column) -> None:
        self._h.create_text_index(table, column)

    def checkpoint(self) -> None:
        self._h.checkpoint()

    def close(self) -> None:
        self._h.close()


def open_embedded(path: str, durability: str = "balanced") -> EliteConnection:
    return EliteConnection(_elitesql().EliteSQL(path, durability=durability))


def open_sidecar(socket_path: str, timeout: Optional[float] = None) -> EliteConnection:
    return EliteConnection(_elitesql().SidecarClient(socket_path, timeout=timeout))


# ------------------------------------------------------------------ SQLite --

_AUTOINC = re.compile(r"\bint AUTO_INCREMENT PRIMARY KEY\b", re.I)
_VECTOR_LINE = re.compile(r",\s*\n\s*\w+ vector\(\d+\)", re.I)
_TIMESTAMP = re.compile(r"\btimestamp\b", re.I)
_ANON_INDEX = re.compile(r"CREATE (UNIQUE )?INDEX ON (\w+) \(([\w, ]+)\)", re.I)


def sqlite_ddl(sql: str) -> str:
    """Rewrite the portable DDL into SQLite's spelling."""
    sql = _AUTOINC.sub("INTEGER PRIMARY KEY AUTOINCREMENT", sql)
    sql = _VECTOR_LINE.sub("", sql)
    sql = _TIMESTAMP.sub("TEXT", sql)
    match = _ANON_INDEX.match(sql.strip())
    if match:
        unique, table, cols = match.groups()
        name = f"idx_{table}_{cols.replace(', ', '_').replace(' ', '')}"
        sql = f"CREATE {unique or ''}INDEX {name} ON {table} ({cols})"
    return sql


class _SqliteTx:
    def __init__(self, conn: sqlite3.Connection):
        self._c = conn
        try:
            self._c.execute("BEGIN IMMEDIATE")
        except sqlite3.OperationalError as error:
            raise Conflict(str(error)) from error

    def execute(self, sql: str, params=None) -> Result:
        try:
            cur = self._c.execute(sql, params or ())
        except sqlite3.OperationalError as error:
            if "locked" in str(error) or "busy" in str(error):
                raise Conflict(str(error)) from error
            raise
        rows = cur.fetchall() if cur.description else []
        return Result(rows=[list(r) for r in rows], lastrowid=cur.lastrowid,
                      affected=max(cur.rowcount, 0))

    def commit(self) -> None:
        try:
            self._c.execute("COMMIT")
        except sqlite3.OperationalError as error:
            self.rollback()
            if "locked" in str(error) or "busy" in str(error):
                raise Conflict(str(error)) from error
            raise

    def rollback(self) -> None:
        try:
            self._c.execute("ROLLBACK")
        except sqlite3.OperationalError:
            pass


class SqliteConnection:
    supports_vectors = False
    supports_fulltext = True  # FTS5

    def __init__(self, path: str, timeout_s: float = 5.0, durability: str = "balanced"):
        synchronous = {"fast": "OFF", "balanced": "NORMAL", "safe": "FULL"}
        if durability not in synchronous:
            raise ValueError(f"unknown durability: {durability}")
        self._c = sqlite3.connect(path, timeout=timeout_s, isolation_level=None,
                                  check_same_thread=False)
        self._c.execute("PRAGMA journal_mode=WAL")
        self._c.execute(f"PRAGMA synchronous={synchronous[durability]}")
        # EliteSQL Safe uses F_FULLFSYNC on macOS. SQLite's default fsync
        # would give it a weaker power-loss contract on the same machine.
        strict = 1 if durability == "safe" else 0
        self._c.execute(f"PRAGMA fullfsync={strict}")
        self._c.execute(f"PRAGMA checkpoint_fullfsync={strict}")
        self._c.execute(f"PRAGMA busy_timeout={int(timeout_s * 1000)}")
        self.durability_settings = {
            name: self._c.execute(f"PRAGMA {name}").fetchone()[0]
            for name in ("journal_mode", "synchronous", "fullfsync",
                         "checkpoint_fullfsync", "wal_autocheckpoint", "busy_timeout")
        }
        expected = {"fast": 0, "balanced": 1, "safe": 2}[durability]
        if (self.durability_settings["journal_mode"] != "wal"
                or self.durability_settings["synchronous"] != expected
                or self.durability_settings["fullfsync"] != strict
                or self.durability_settings["checkpoint_fullfsync"] != strict):
            self._c.close()
            raise RuntimeError(f"SQLite rejected durability settings: {self.durability_settings}")

    def execute(self, sql: str, params=None) -> Result:
        try:
            cur = self._c.execute(sql, params or ())
        except sqlite3.OperationalError as error:
            if "locked" in str(error) or "busy" in str(error):
                raise Conflict(str(error)) from error
            raise
        rows = cur.fetchall() if cur.description else []
        return Result(rows=[list(r) for r in rows], lastrowid=cur.lastrowid,
                      affected=max(cur.rowcount, 0))

    def transaction(self) -> _SqliteTx:
        return _SqliteTx(self._c)

    def search_vector(self, table, column, vector, top_k, filter=None):
        raise NotSupported("SQLite has no vector index")

    def search_text(self, table, column, query, top_k) -> list[dict]:
        rows = self._c.execute(
            f"SELECT rowid FROM {table}_fts WHERE {table}_fts MATCH ? ORDER BY rank LIMIT ?",
            (query, top_k),
        ).fetchall()
        return [{"id": r[0], "record": None} for r in rows]

    def create_vector_index(self, table, column) -> None:
        pass  # unsupported; the service degrades the operation

    def create_text_index(self, table, column) -> None:
        self._c.execute(
            f"CREATE VIRTUAL TABLE {table}_fts USING fts5({column}, content='{table}', content_rowid='id')"
        )
        self._c.execute(f"INSERT INTO {table}_fts({table}_fts) VALUES('rebuild')")

    def checkpoint(self) -> None:
        self._c.execute("PRAGMA wal_checkpoint(PASSIVE)")

    def close(self) -> None:
        self._c.close()


# -------------------------------------------------------------------- Pool --


class ConnectionPool:
    """A bounded pool; ``acquire`` blocks and reports how long it waited.

    Hand-off is strictly FIFO: a released connection goes to the longest
    waiter, and a thread that just released one queues behind everybody else.
    Without that, under the GIL the releasing thread re-takes "its" connection
    immediately and the waiting users starve, which would hide the pool wait
    from the per-operation percentiles.

    For the embedded engine one shared handle serves every thread, so the
    pool is a formality (``shared=True``): every acquire returns instantly.
    """

    def __init__(self, factory: Callable[[], Any], size: int, shared: bool = False):
        self.size = size
        self._factory = factory
        self._shared = shared
        self._lock = threading.Lock()
        self._idle: list = []
        self._waiters: deque = deque()
        self._all: list = []
        if shared:
            self._one = factory()
            self._all.append(self._one)
        else:
            for _ in range(size):
                conn = factory()
                self._idle.append(conn)
                self._all.append(conn)

    def acquire(self):
        if self._shared:
            return self._one
        with self._lock:
            if self._idle and not self._waiters:
                return self._idle.pop()
            slot = _Waiter()
            self._waiters.append(slot)
        slot.event.wait()
        return slot.conn

    def release(self, conn, broken: bool = False) -> None:
        if self._shared:
            return
        if broken:
            # A sidecar connection that timed out or lost its stream refuses
            # further use; put a fresh one in its place.
            try:
                conn.close()
            except Exception:  # noqa: BLE001
                pass
            with self._lock:
                self._all.remove(conn)
            conn = self._factory()
            with self._lock:
                self._all.append(conn)
        with self._lock:
            if self._waiters:
                slot = self._waiters.popleft()
                slot.conn = conn
                slot.event.set()
            else:
                self._idle.append(conn)

    def close(self) -> None:
        for conn in self._all:
            try:
                conn.close()
            except Exception:  # noqa: BLE001
                pass


class _Waiter:
    __slots__ = ("event", "conn")

    def __init__(self):
        self.event = threading.Event()
        self.conn = None
