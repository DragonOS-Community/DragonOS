#!/usr/bin/env python3
"""Check filesystem wrappers against the unmodified binary package on Linux."""
import os
from pathlib import Path
import re
import resource
import signal
import subprocess
import sys
import tempfile


def limit_output():
    # Trace/output files are bounded even if the benchmark misbehaves.
    resource.setrlimit(resource.RLIMIT_FSIZE, (65536, 65536))


binary = Path(sys.argv[1]).resolve()
suite = Path(__file__).resolve().parents[1]
shell = os.environ.get("TEST_SHELL", "/bin/sh")
errors = []
with tempfile.TemporaryDirectory(prefix="lmbench-fs-test.") as tmp:
    root = Path(tmp)
    env = dict(os.environ, ENOUGH="10000", TIMING_O="0", LOOP_O="0")
    env["LMBENCH_BIN_DIR"] = str(binary.parent)
    env["LMBENCH_RUN_TMP"] = str(root)
    env["LMBENCH_SH"] = shell
    for name, expected in [
        ("ext4_create_delete_files_0k_ops", ["0", "1", "4", "10"]),
        ("ramfs_create_delete_files_0k_ops", ["0", "1", "4", "10"]),
        ("ext4_create_delete_files_10k_ops", ["10"]),
        ("ramfs_create_delete_files_10k_ops", ["10"]),
    ]:
        target = root / name
        target.mkdir()
        env["LMBENCH_EXT4_DIR"] = env["LMBENCH_TMP_DIR"] = str(target)
        # Deliberately wrong incoming TMPDIR: each wrapper must select its fixture.
        env["TMPDIR"] = str(root / "wrong-directory")
        trace = root / (name + ".trace")
        output = root / (name + ".output")
        with output.open("wb") as stream:
            proc = subprocess.Popen(
                ["strace", "-f", "-e", "trace=mkdir", "-o", str(trace),
                 shell, str(suite / "runner/test_cases" / (name + ".sh"))],
                stdout=stream, stderr=stream, env=env,
                start_new_session=True, preexec_fn=limit_output,
            )
            try:
                rc = proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
                errors.append(name + ": timeout")
                continue
        text = output.read_text()
        sizes = re.findall(r"^(\d+)k\s", text, re.M)
        paths = re.findall(r'mkdir\("([^\"]+)",[^\n]*\) += 0', trace.read_text())
        if rc != 0 or sizes != expected:
            errors.append(f"{name}: rc={rc}, sizes={sizes}, expected={expected}")
        if not paths or any(not p.startswith(str(target) + "/") for p in paths):
            errors.append(name + ": benchmark did not use the requested directory")
        if list(target.iterdir()):
            errors.append(name + ": benchmark left fixtures behind")
if errors:
    raise SystemExit("\n".join(errors))
print("lat_fs binary regression: PASS (four wrappers, original size sweep, target directory, cleanup)")
