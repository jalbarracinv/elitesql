import datetime as dt
import ctypes
import platform
import sys
import tempfile
import threading
import unittest
from pathlib import Path


PYTHON_BINDING = Path(__file__).resolve().parents[1]
REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(PYTHON_BINDING))

from elitesql import EliteSQL, EliteSQLError, _encode_params  # noqa: E402


LIB_NAME = {
    "Darwin": "libelitesql.dylib",
    "Linux": "libelitesql.so",
}.get(platform.system(), "elitesql.dll")
LIB_PATH = REPO / "target" / "debug" / LIB_NAME


class ParameterEncodingTests(unittest.TestCase):
    def test_python_values_are_encoded_with_explicit_types(self):
        encoded = _encode_params(
            [
                b"\x00\xff",
                dt.date(2026, 8, 8),
                dt.time(12, 34, 56, 7),
                dt.datetime(2026, 8, 8, 12, 34, 56, 7, tzinfo=dt.timezone.utc),
                {"nested": [1, True, None]},
                [0.25, -1.0, 3.5],
            ]
        )
        self.assertEqual(encoded[0], {"$t": "blob", "hex": "00ff"})
        self.assertEqual(encoded[1]["$t"], "date")
        self.assertEqual(encoded[2], {"$t": "time", "us": 45_296_000_007})
        self.assertEqual(encoded[3]["$t"], "timestamp")
        self.assertEqual(encoded[4]["$t"], "json")
        self.assertEqual(encoded[5], {"$t": "json", "v": [0.25, -1.0, 3.5]})

    def test_parameter_container_must_be_sequence_or_mapping(self):
        with self.assertRaises(TypeError):
            _encode_params("not-a-parameter-sequence")

    def test_close_waits_for_active_native_handle_leases(self):
        closed = threading.Event()

        class FakeLib:
            @staticmethod
            def elitesql_close(_handle):
                closed.set()
                return 0

        db = EliteSQL.__new__(EliteSQL)
        db._lib = FakeLib()
        db._handle = ctypes.c_void_p(1)
        db._lifecycle = threading.Condition()
        db._active_calls = 0
        db._closing = False
        entered = threading.Event()
        release = threading.Event()

        def use_handle():
            with db._lease():
                entered.set()
                release.wait(2)

        user = threading.Thread(target=use_handle)
        user.start()
        self.assertTrue(entered.wait(2))
        closer = threading.Thread(target=db.close)
        closer.start()
        with db._lifecycle:
            self.assertTrue(db._lifecycle.wait_for(lambda: db._closing, timeout=2))
        self.assertFalse(closed.is_set())
        release.set()
        user.join(2)
        closer.join(2)
        self.assertTrue(closed.is_set())
        self.assertIsNone(db._handle)


@unittest.skipUnless(LIB_PATH.is_file(), f"build {LIB_PATH} first")
class EmbeddedParameterTests(unittest.TestCase):
    def test_roundtrip_named_limit_and_injection_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            with EliteSQL(Path(directory) / "params.esql", lib_path=str(LIB_PATH)) as db:
                db.query(
                    "CREATE TABLE docs (name text NOT NULL, payload blob NOT NULL, day date NOT NULL, metadata json NOT NULL, embedding vector(3) NOT NULL)"
                )
                hostile = "x' OR TRUE --"
                db.query(
                    "INSERT INTO docs (name, payload, day, metadata, embedding) VALUES (%s, %s, %s, %s, %s)",
                    [
                        hostile,
                        b"\x00\xff",
                        dt.date(2026, 8, 8),
                        {"safe": True},
                        [0.25, -1.0, 3.5],
                    ],
                )
                result = db.query(
                    "SELECT name, payload, day, metadata, embedding FROM docs WHERE name = %(name)s LIMIT %(limit)s",
                    {"name": hostile, "limit": 1},
                )
                self.assertEqual(result["rows"][0][0], hostile)
                self.assertEqual(result["rows"][0][1], b"\x00\xff")
                self.assertEqual(result["rows"][0][2], dt.date(2026, 8, 8))
                self.assertEqual(result["rows"][0][3], {"safe": True})
                self.assertEqual(result["rows"][0][4], [0.25, -1.0, 3.5])

                with self.assertRaises(EliteSQLError):
                    db.query("SELECT * FROM docs", ["unused"])

    def test_structured_transaction_is_atomic_and_returns_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            with EliteSQL(Path(directory) / "txn.esql", lib_path=str(LIB_PATH)) as db:
                db.query(
                    "CREATE TABLE docs (doc_id int AUTO_INCREMENT, title text NOT NULL, done bool NOT NULL)"
                )
                with db.transaction() as tx:
                    inserted = tx.execute(
                        "INSERT INTO docs (title, done) VALUES (%s, %s) RETURNING id, doc_id",
                        ["contract", False],
                    ).fetchone()
                    self.assertEqual(inserted[1], 1)
                    changed = tx.execute(
                        "UPDATE docs SET done = %s WHERE id = %s", [True, inserted[0]]
                    )
                    self.assertEqual(changed.rowcount, 1)
                    self.assertTrue(tx.get("docs", inserted[0])["done"])
                self.assertEqual(
                    db.query("SELECT doc_id, done FROM docs")["rows"], [[1, True]]
                )

                with self.assertRaises(RuntimeError):
                    with db.transaction() as tx:
                        tx.insert("docs", {"title": "rolled back", "done": False})
                        raise RuntimeError("abort")
                self.assertEqual(db.query("SELECT count(*) AS n FROM docs")["rows"], [[1]])

    def test_dbapi_cursor_exposes_integer_lastrowid_and_fetch_methods(self):
        with tempfile.TemporaryDirectory() as directory:
            with EliteSQL(Path(directory) / "cursor.esql", lib_path=str(LIB_PATH)) as db:
                db.query("CREATE TABLE docs (doc_id int AUTO_INCREMENT, title varchar(20) NOT NULL)")
                cursor = db.cursor()
                returned = cursor.execute(
                    "INSERT INTO docs (title) VALUES (%s)", ["contract"]
                )
                self.assertIs(returned, cursor)
                self.assertEqual(cursor.lastrowid, 1)
                self.assertEqual(cursor.rowcount, 1)

                cursor.executemany(
                    "INSERT INTO docs (title) VALUES (%s)",
                    [["second"], ["third"]],
                )
                self.assertEqual(cursor.rowcount, 2)
                self.assertEqual(cursor.lastrowid, 3)
                cursor.execute("SELECT doc_id, title FROM docs ORDER BY doc_id")
                self.assertEqual(cursor.fetchone(), [1, "contract"])
                self.assertEqual(cursor.fetchmany(1), [[2, "second"]])
                self.assertEqual(cursor.fetchall(), [[3, "third"]])
                self.assertIsNone(cursor.fetchone())


if __name__ == "__main__":
    unittest.main()
