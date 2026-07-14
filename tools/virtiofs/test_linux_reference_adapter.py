#!/usr/bin/env python3

import importlib.util
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("linux_reference_adapter.py")
SPEC = importlib.util.spec_from_file_location("linux_reference_adapter", MODULE_PATH)
assert SPEC and SPEC.loader
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)


class LinuxReferenceAdapterTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def transcript(self) -> Path:
        phases = []
        for phase, event in (("open", "begin"), ("open", "end"), ("data_loop", "begin"),
                             ("data_loop", "end"), ("close", "begin"), ("close", "end"),
                             ("verify", "begin"), ("verify", "end")):
            phases.append("phase workload=sequential_read dataset=data phase=" + phase +
                          " event=" + event + " pid=10 monotonic_us=" +
                          ("2000000" if phase == "data_loop" and event == "end" else "1000000") +
                          " elapsed_us=1 offset=0 "
                          "requested=4 returned=4 errno=0 run_id=run")
        result = ("result workload=sequential_read status=ok errno=0 elapsed_us=123 bytes=4 "
                  "ops=1 syscalls=1 short_io=0 eintr=0 checksum=0123456789abcdef mount=/mnt "
                  "dataset=data seed=1 files=1 file_size=4 block_size=4 iterations=1 workers=1 "
                  "run_id=run cache_mode=linux-reference mount_options=none expect_dax=unspecified "
                  "sysname=Linux release=6.6.139")
        summary = ("io_summary workload=sequential_read syscalls=1 short_io=0 eintr=0 "
                   "checksum=0123456789abcdef run_id=run verify_us=1")
        path = self.root / "transcript"
        path.write_text("\n".join(phases + [result, summary]) + "\n")
        return path

    def capture(self) -> Path:
        capture = self.root / "capture"
        capture.mkdir()
        (capture / "probe-definition").write_text(
            "p:dragonos_virtiofs_ref/read_sync fuse_simple_request args=$arg2:u64 opcode=+8($arg2):u32 read_size=+16(+32($arg2)):u32\n"
            "p:dragonos_virtiofs_ref/read_async fuse_simple_background args=$arg2:u64 opcode=+8($arg2):u32 read_size=+16(+32($arg2)):u32\n")
        for name, event in (("format-sync", "read_sync"), ("format-async", "read_async")):
            (capture / name).write_text(
                f"name: {event}\nfield:u32 opcode;\nfield:u32 read_size;\n"
                'print fmt: "opcode=%u read_size=%u"\n')
        (capture / "trace").write_text(
            "# entries-in-buffer/entries-written: 5/5   #P:1\n"
            "sh-7 [000] .... 0.500000: tracing_mark_write: LINUX_REF_BEGIN run_id=run case_id=case helper_pid=10\n"
            "bench-10 [000] .... 0.800000: read_sync: (0) args=3 opcode=15 read_size=4096\n"
            "bench-10 [000] .... 1.200000: read_async: (0) args=1 opcode=15 read_size=4096\n"
            "bench-10 [000] .... 1.300000: read_sync: (0) args=2 opcode=15 read_size=8192\n"
            "sh-7 [000] .... 2.500000: tracing_mark_write: LINUX_REF_END run_id=run case_id=case helper_pid=10 rc=0\n")
        return capture

    def test_transcript_and_trace_derive_metrics(self) -> None:
        metrics = adapter.parse_transcript(self.transcript(), "run", "data", 4, 4)
        self.assertEqual(metrics["elapsed_us"], 123)
        summary = adapter.parse_trace(self.capture(), "run", "case", 10, 1_000_000, 2_000_000)
        self.assertEqual(summary["read_requests"], 2)
        self.assertEqual(summary["requested_bytes"], 12288)
        self.assertEqual(summary["request_size_buckets"]["pages_2_4"], 1)

    def test_rejects_duplicate_result(self) -> None:
        path = self.transcript()
        lines = path.read_text().splitlines()
        result = next(line for line in lines if line.startswith("result "))
        path.write_text(path.read_text() + result + "\n")
        with self.assertRaises(adapter.EvidenceError):
            adapter.parse_transcript(path, "run", "data", 4, 4)

    def test_rejects_unbound_or_empty_trace(self) -> None:
        capture = self.capture()
        (capture / "trace").write_text((capture / "trace").read_text().replace("case_id=case", "case_id=old"))
        with self.assertRaises(adapter.EvidenceError):
            adapter.parse_trace(capture, "run", "case", 10)

    def test_rejects_probe_layout_drift(self) -> None:
        capture = self.capture()
        definition = capture / "probe-definition"
        definition.write_text(definition.read_text().replace("+32", "+24"))
        with self.assertRaises(adapter.EvidenceError):
            adapter.parse_trace(capture, "run", "case", 10)

    def test_qemu_device_requires_non_dax(self) -> None:
        chardev, cache = adapter.parse_device(
            ["qemu", "-device", "vhost-user-fs-pci,chardev=fs0,tag=hostshare"], "hostshare")
        self.assertEqual((chardev, cache), ("fs0", "0"))
        with self.assertRaises(adapter.EvidenceError):
            adapter.parse_device(
                ["qemu", "-device", "vhost-user-fs-pci,chardev=fs0,tag=hostshare,cache-size=1G"],
                "hostshare")

    def test_tsv_rejects_duplicate_identity_field(self) -> None:
        path = self.root / "identity"
        path.write_text("schema\tv1\nschema\tv1\n")
        with self.assertRaises(adapter.EvidenceError):
            adapter.parse_tsv(path, {"schema"}, "identity")


if __name__ == "__main__":
    unittest.main()
