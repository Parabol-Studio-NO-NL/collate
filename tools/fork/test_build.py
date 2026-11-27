"""Regression tests for the local Cargo build monitor."""

from contextlib import redirect_stdout
import io
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

import build


class BuildMonitorTests(unittest.TestCase):
    cache = Path(__file__).resolve().parent
    count = b"1024\tcache\n"

    def missing_rmeta(self):
        # du can emit a partial count and still exit 1 when a file disappears.
        return subprocess.CalledProcessError(
            1, ["du", "-sk", self.cache], output=self.count,
            stderr=b"du: cache/tmp.rmeta: No such file or directory\n",
        )

    def run_monitor(self, cargo_status, samples, final_space=10 * build.GIB):
        output = io.StringIO()
        with (
            patch.object(sys, "argv", ["build.py", "check"]),
            patch.object(build, "cache_paths", return_value=[self.cache]),
            patch.object(build, "run_cargo", return_value=cargo_status),
            patch.object(build, "ensure_space", side_effect=[10 * build.GIB, final_space]),
            patch.object(subprocess, "check_output", side_effect=samples),
            redirect_stdout(output),
        ):
            status = build.main()
        return status, output.getvalue()

    def test_disappearing_rmeta_preserves_cargo_status(self):
        for cargo_status in (0, 101):
            for missing_index in (0, 1):
                with self.subTest(cargo_status=cargo_status, missing_index=missing_index):
                    samples = [self.count, self.count]
                    samples[missing_index] = self.missing_rmeta()
                    status, output = self.run_monitor(cargo_status, samples)
                    self.assertEqual(status, cargo_status)
                    self.assertIn("unavailable", output)
                    self.assertIn("Least free space after Cargo", output)
                    self.assertNotRegex(output, r"\([+-][\d.]+ GiB\)")

    def test_failed_final_sample_preserves_reserve_error(self):
        with self.assertRaisesRegex(RuntimeError, "reserve"):
            self.run_monitor(0, [self.count, self.missing_rmeta()],
                             RuntimeError("below free-space reserve"))


if __name__ == "__main__":
    unittest.main()
