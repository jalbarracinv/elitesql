import datetime as dt
import platform
import sys
import tempfile
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


if __name__ == "__main__":
    unittest.main()
