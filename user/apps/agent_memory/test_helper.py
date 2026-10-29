#!/usr/bin/env python3
"""Host unit/error-path tests; these do not claim successful guest cgroup execution."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).with_name("agent_memory_guest.c").resolve()


class HelperTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="agent-memory-host-")
        cls.root = Path(cls.temp.name)
        cls.binary = cls.root / "helper"
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
                        "-O2", str(SOURCE), "-o", str(cls.binary)], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_invalid_options_do_not_start_workload(self):
        for value in ("-1", "18446744073709551616", "0", "100000"):
            with self.subTest(value=value):
                result = subprocess.run([str(self.binary), "--output",
                                         str(self.root / "never-created"),
                                         "--interval-ms", value, "--", "/bin/true"],
                                        capture_output=True)
                self.assertEqual(result.returncode, 2)
                self.assertFalse((self.root / "never-created").exists())

    def test_existing_output_is_never_reused(self):
        with tempfile.TemporaryDirectory(dir=self.root) as output:
            marker = Path(output) / "marker"
            marker.write_text("preserve")
            result = subprocess.run([str(self.binary), "--output", output,
                                     "--", "/bin/true"], capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(marker.read_text(), "preserve")
            self.assertFalse((Path(output) / "result.json").exists())

    def test_missing_cgroup_rejects_exec_and_publishes_private_result(self):
        with tempfile.TemporaryDirectory(dir=self.root) as directory:
            directory = Path(directory)
            output = directory / "output"
            marker = directory / "must-not-execute"
            result = subprocess.run([
                str(self.binary), "--output", str(output), "--cgroup-parent",
                str(directory / "absent"), "--settle-seconds", "0",
                "--", "/bin/touch", str(marker)], capture_output=True)
            self.assertEqual(result.returncode, 1)
            self.assertFalse(marker.exists())
            report = json.loads((output / "result.json").read_text())
            self.assertEqual(report["schema"], 1)
            self.assertEqual(report["status"], "sample_failed")
            self.assertIsNone(report["child_pid"])
            self.assertIsNone(report["exit_code"])
            self.assertIsNone(report["memory_peak_bytes"])
            for field in ("workload_start_ns", "leader_exit_ns", "descendants_empty_ns", "task_elapsed_ns"):
                self.assertIsNone(report[field])
            self.assertGreater(report["sample_errors"], 0)
            self.assertFalse(report["descendants_empty"])
            self.assertFalse((output / "result.json.tmp").exists())
            self.assertEqual(output.stat().st_mode & 0o777, 0o700)
            for name in ("result.json", "stdout.log", "stderr.log", "metrics.jsonl"):
                self.assertEqual((output / name).stat().st_mode & 0o777, 0o600)

    def test_parsers_reopen_and_cleanup_identity(self):
        source = SOURCE.with_name("test_helper_unit.c")
        binary = self.root / "parser-test"
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2",
                        str(source), "-o", str(binary)], check=True)
        subprocess.run([str(binary), str(self.root)], check=True)


if __name__ == "__main__":
    unittest.main()
