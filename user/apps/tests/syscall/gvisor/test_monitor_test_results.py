#!/usr/bin/env python3
"""Host regression tests for the monitor's actual boot-marker predicate.

Run: python3 user/apps/tests/syscall/gvisor/test_monitor_test_results.py
Only the function under test is executed: sourcing the monitor would start its
process-management loop and could terminate an unrelated QEMU instance.
"""

from pathlib import Path
import re
import subprocess
import tempfile
import unittest


class BootMarkerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        source = Path(__file__).with_name("monitor_test_results.sh").read_text()
        match = re.search(r"^check_boot_complete\(\) \{\n.*?^\}", source, re.M | re.S)
        if match is None:
            raise AssertionError("actual check_boot_complete function not found")
        cls.function = match.group(0)

    def check_log(self, contents, expected):
        with tempfile.TemporaryDirectory(prefix="gvisor-monitor-test-") as directory:
            path = Path(directory) / "serial_opt.txt"
            if contents is not None:
                path.write_bytes(contents)
            result = subprocess.run(
                ["sh", "-c", self.function + '\nSERIAL_FILE="$1"\ncheck_boot_complete',
                 "monitor-fixture", str(path)],
                capture_output=True, timeout=5,
            )
            self.assertEqual(result.returncode, 0 if expected else 1, result.stderr)

    def test_real_rcs_marker(self):
        self.check_log(b"[rcS] Running system init script...\n", True)

    def test_rcs_marker_with_nul_and_crlf(self):
        self.check_log(b"early boot\x00\r\n[rcS] Running system init script...\r\n", True)

    def test_banner_alone_is_not_a_boot_marker(self):
        self.check_log(b"DragonOS - Lightweight Cloud-Native Kernel\r\n", False)

    def test_regex_lookalike_is_not_the_literal_marker(self):
        self.check_log(b"r Running system init scriptXYZ\n", False)

    def test_gvisor_start_marker(self):
        self.check_log("开始运行gvisor系统调用测试\r\n".encode(), True)

    def test_missing_serial_file(self):
        self.check_log(None, False)


if __name__ == "__main__":
    unittest.main(verbosity=2)
