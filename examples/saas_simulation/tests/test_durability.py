"""Guard the durability contract of benchmark connections, not just labels."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import runner
from saas.drivers import SqliteConnection


class DurabilityTests(unittest.TestCase):
    def test_profiles_apply_and_read_back_pragmas(self):
        with tempfile.TemporaryDirectory() as directory:
            for profile, sync in (("fast", 0), ("balanced", 1), ("safe", 2)):
                with self.subTest(profile=profile):
                    conn = SqliteConnection(str(Path(directory) / f"{profile}.sqlite"),
                                            durability=profile)
                    try:
                        actual = conn.durability_settings
                        self.assertEqual(actual["journal_mode"], "wal")
                        self.assertEqual(actual["synchronous"], sync)
                        self.assertEqual(actual["fullfsync"], int(profile == "safe"))
                        self.assertEqual(actual["checkpoint_fullfsync"], int(profile == "safe"))
                    finally:
                        conn.close()

    def test_stage_connections_honor_safe(self):
        with tempfile.TemporaryDirectory() as directory:
            cfg = runner.StageConfig(transport="sqlite", users=1,
                                     db_path=str(Path(directory) / "stage.sqlite"),
                                     durability="safe")
            conn = runner._connect(cfg)
            try:
                self.assertEqual(conn.durability_settings["synchronous"], 2)
                self.assertEqual(conn.durability_settings["fullfsync"], 1)
                conn.execute("CREATE TABLE proof (id INTEGER PRIMARY KEY)")
                tx = conn.transaction()
                tx.execute("INSERT INTO proof VALUES (?)", [1])
                tx.commit()
                self.assertEqual(conn.execute("SELECT count(*) FROM proof").scalar, 1)
            finally:
                conn.close()

    def test_default_keeps_existing_normal_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            conn = SqliteConnection(str(Path(directory) / "default.sqlite"))
            try:
                self.assertEqual(conn.durability_settings["synchronous"], 1)
            finally:
                conn.close()

    def test_invalid_profile_fails_before_creating_database(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "invalid.sqlite"
            with self.assertRaises(ValueError):
                SqliteConnection(str(path), durability="typo")
            self.assertFalse(path.exists())


if __name__ == "__main__":
    unittest.main()
