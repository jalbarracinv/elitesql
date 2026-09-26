"""User latency must retain the correlation between query time and pool wait."""
import csv
import gzip
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import runner


class LatencyTests(unittest.TestCase):
    def test_p99_combines_each_requests_wait_before_sorting(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            sample = stage / "samples-00.csv.gz"
            with gzip.open(sample, "wt", newline="") as output:
                writer = csv.writer(output)
                writer.writerow(runner.SAMPLE_FIELDS)
                # Every request takes 100 us total, but the two marginal
                # p99s are each 100 us. Adding them would incorrectly report 200.
                for index in range(100):
                    latency, wait = (100, 0) if index < 50 else (0, 100)
                    writer.writerow(["browse", index, latency, wait, "ok", 0])
            Path(str(sample) + ".meta.json").write_text(json.dumps({"errors": {}}))
            cfg = runner.StageConfig(transport="sqlite", db_path="unused", users=1,
                                     duration=1, ramp=0, warmup=0, keep_samples=True)
            summary = runner.aggregate(stage, cfg, [], {}, (0, 0))
            self.assertEqual(summary["ops_total_window"], 100)
            self.assertEqual(summary["latency"]["p99_us"], 100)
            self.assertEqual(summary["pool_wait"]["p99_us"], 100)
            self.assertEqual(summary["user_latency_p99_us"], 100)


if __name__ == "__main__":
    unittest.main()
