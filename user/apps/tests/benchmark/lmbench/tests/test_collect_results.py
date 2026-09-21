#!/usr/bin/env python3
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


SUITE = Path(__file__).resolve().parent.parent
COLLECTOR = SUITE / "orchestrator" / "collect_results.py"


def metric(name="process_getppid_lat", complete=True):
    value = {
        "name": name,
        "category": "process",
        "metric_type": "latency",
        "unit": "microseconds",
        "bigger_is_better": False,
        "status": "ok",
    }
    if complete:
        value.update({
            "samples": [1.25],
            "stats": {"count": 1, "mean": 1.25, "median": 1.25,
                      "stddev": 0.0, "min": 1.25, "max": 1.25, "cv": 0.0},
        })
    return value


def frame(metrics, summary=None, end=True):
    if summary is None:
        summary = {"total": len(metrics), "ok": len(metrics), "failed": 0, "skipped": 0}
    lines = [
        "===LMBENCH_RUN_BEGIN===",
        'LMBENCH_META ' + json.dumps({"suite": "lmbench", "suite_version": "3.0-a9",
                                      "samples": 1, "timeout_sec": 3, "warmup": 0}),
    ]
    lines.extend("LMBENCH_JSON " + json.dumps(item) for item in metrics)
    lines.append("LMBENCH_SUMMARY " + json.dumps(summary))
    if end:
        lines.append("===LMBENCH_RUN_END===")
    return "\n".join(lines) + "\n"


class CollectorProtocolTest(unittest.TestCase):
    def run_collector(self, serial_text):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        serial = root / "serial.log"
        outdir = root / "out"
        serial.write_text(serial_text, encoding="utf-8")
        proc = subprocess.run(
            ["python3", str(COLLECTOR), "--serial", str(serial), "--outdir", str(outdir),
             "--root", str(SUITE)],
            text=True, capture_output=True,
        )
        return proc, outdir

    def assert_no_outputs(self, outdir):
        self.assertFalse((outdir / "history.jsonl").exists())
        self.assertFalse((outdir / "github-benchmark" / "data.json").exists())
        self.assertFalse((outdir / "x86_64").exists())

    def test_complete_frame_is_persisted(self):
        proc, outdir = self.run_collector(frame([metric()]))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue((outdir / "history.jsonl").is_file())
        self.assertEqual(len(list((outdir / "x86_64").glob("*.json"))), 1)

    def test_truncated_last_frame_does_not_fall_back_to_old_complete_frame(self):
        serial = frame([metric("first")]) + frame([metric("last")], end=False)
        proc, outdir = self.run_collector(serial)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("incomplete", proc.stderr.lower())
        self.assert_no_outputs(outdir)

    def test_summary_counts_must_match_metric_statuses(self):
        wrong = {"total": 2, "ok": 1, "failed": 1, "skipped": 0}
        proc, outdir = self.run_collector(frame([metric()], wrong))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("summary", proc.stderr.lower())
        self.assert_no_outputs(outdir)

    def test_schema_failure_does_not_write_any_output(self):
        proc, outdir = self.run_collector(frame([metric(complete=False)]))
        self.assertEqual(proc.returncode, 3, proc.stderr)
        self.assertIn("schema validation failed", proc.stderr.lower())
        self.assert_no_outputs(outdir)

    def test_unframed_input_is_rejected(self):
        serial = "LMBENCH_JSON " + json.dumps(metric()) + "\n"
        proc, outdir = self.run_collector(serial)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("frame", proc.stderr.lower())
        self.assert_no_outputs(outdir)


if __name__ == "__main__":
    unittest.main()
