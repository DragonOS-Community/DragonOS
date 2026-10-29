#!/usr/bin/env python3
"""Host-only destructive tests for cube_non_dax_evidence.py."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("cube_non_dax_evidence.py")
SPEC = importlib.util.spec_from_file_location("cube_non_dax_evidence", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
cube = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cube)

SANDBOX = "0123456789abcdef0123456789abcdef"
CASE = "read-f1048576-b4096"
RUN = "cube-run"


def sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


class CubeEvidenceTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.make_fixture()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write(self, name: str, value: str | bytes) -> None:
        path = self.source / name
        if isinstance(value, bytes):
            path.write_bytes(value)
        else:
            path.write_text(value, encoding="utf-8")

    def make_fixture(self) -> None:
        request = b'{"containers":[{"image":{"image":"busybox@sha256:fixture"}}]}\n'
        self.write("request.json", request)
        capture = {
            "schema": cube.CAPTURE_SCHEMA, "adapter_version": cube.ADAPTER_VERSION,
            "case_id": CASE, "run_id": RUN, "status": "completed",
            "sandbox_id": SANDBOX, "phase": "read", "mode": "performance",
            "workload": "sequential_read", "dataset": "cube-dataset",
            "file_size": 1048576, "block_size": 4096, "guest_cache": "cold",
            "host_cache": "unknown", "kernel_path": "/cube/kernel/vmlinux",
            "image_path": "/cube/image/root.img", "helper_guest_path": "/bin/virtiofs_bench",
            "request_path": "/root/cubecli-busybox.json", "shim_pid": 123,
            "shim_start_ticks": 456,
        }
        self.write("capture.json", json.dumps(capture) + "\n")
        self.write("request.sha256", f"{sha(request)}  /root/cubecli-busybox.json\n")
        self.write("kernel.sha256", f"{'1'*64}  /cube/kernel/vmlinux\n")
        self.write("image.sha256", f"{'2'*64}  /cube/image/root.img\n")
        self.write("helper.sha256", f"{'3'*64}  /bin/virtiofs_bench\n")
        self.write("shim.exe.sha256", f"{'4'*64}  /cube/bin/containerd-shim-cube-rs\n")
        self.write("shim.cmdline", b"/cube/bin/containerd-shim-cube-rs\0-id\0" + SANDBOX.encode() + b"\0-debug\0")
        self.write("multirun.log", f"RunContainer RequestId:r,sandBoxId:{SANDBOX},code:200\n"
                                      "totalRunSuccCnt:1\ntotalRunErr:0\n")
        header = "NS CONTAINER CUBEBOX TYPE STATUS IMAGE CREATED RAW\n"
        self.write("sandboxes-before.txt", header)
        self.write("sandboxes-active.txt", header +
                   f"default {SANDBOX} {SANDBOX} sandbox Up image now {{}}\n")
        self.write("sandboxes-after.txt", header)
        self.write("backend.log", f"{SANDBOX} --- INFO --- Creating virtio-fs device: FsConfig "
                                  "{{ tag: cubeShared, backendfs_config: Some(BackendFsConfig "
                                  "{ shared_dir: /data/cubelet, cache: 2 }) }}\n")
        self.write("workload.log", f"P0_CASE_BEGIN:{CASE}:run_id={RUN}\n"
                                   "result workload=sequential_read status=ok errno=0 elapsed_us=1000 "
                                   "bytes=1048576 ops=256 syscalls=256 short_io=0 eintr=0 "
                                   "checksum=0123456789abcdef dataset=cube-dataset file_size=1048576 "
                                   f"block_size=4096 run_id={RUN}\n"
                                   f"P0_CASE_END:{CASE}:run_id={RUN}:rc=0\n")
        self.write("config.txt", f"P0_CONFIG_RUN:{RUN}:{CASE}\n[fuse]\ninit_epoch 1\n"
                                "negotiated_max_read_bytes 1048576\nnegotiated_max_pages 256\n"
                                "negotiated_max_readahead_bytes 1048576\nnegotiated_async_read 1\n"
                                "effective_read_payload_limit_bytes 131072\n[virtiofs]\n"
                                "sg_limit_pages_configured 32\n")
        before = {"schema": "dragonos.virtiofs.cube-cpu-snapshot.v1", "host_boot_id": "boot",
                  "sandbox_id": SANDBOX, "pid": 123, "start_ticks": 456,
                  "clock_ticks_per_second": 100, "user_ticks": 10, "system_ticks": 20,
                  "voluntary_ctxt_switches": 30, "nonvoluntary_ctxt_switches": 40}
        after = {**before, "user_ticks": 12, "system_ticks": 23,
                 "voluntary_ctxt_switches": 35, "nonvoluntary_ctxt_switches": 41}
        self.write("cpu-before.json", json.dumps(before) + "\n")
        self.write("cpu-after.json", json.dumps(after) + "\n")
        for name in ("interrupts-before.txt", "interrupts-after.txt",
                     "softirqs-before.txt", "softirqs-after.txt"):
            self.write(name, "CPU0 CPU1\nvirtiofs: 1 2\n")
        self.write("cubeshim.log", f'{{"InstanceId":"{SANDBOX}"}}\n')
        self.write("cubevmm.log", f"{SANDBOX} --- runtime evidence\n")
        self.write("cubelet.log", f'{{"InstanceId":"{SANDBOX}"}}\n')
        self.write("destroy.log", f"destroy sandbox: {SANDBOX}\ndestroy rsp: ret_code:Success\n")
        cleanup = {"schema": "dragonos.virtiofs.cube-cleanup.v1", "sandbox_id": SANDBOX,
                   "destroy_rc": 0, "target_absent": True, "shim_pid_absent": True}
        self.write("cleanup.json", json.dumps(cleanup) + "\n")

    def run_cli(self, *args: str, ok: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run([str(SCRIPT), *args], text=True, capture_output=True)
        if ok and result.returncode != 0:
            self.fail(result.stderr)
        if not ok and result.returncode == 0:
            self.fail("command unexpectedly succeeded")
        return result

    def test_seal_and_replay(self) -> None:
        case = self.root / "case"
        self.run_cli("seal", "--input", str(self.source), "--output", str(case))
        self.run_cli("verify", "--case-dir", str(case))
        result = json.loads((case / "case-result.json").read_text())
        self.assertEqual(result["schema"], cube.CASE_SCHEMA)
        self.assertEqual(result["runner_version"], "cube-1")
        self.assertEqual(result["result"]["bytes"], 1048576)
        self.assertTrue(json.loads((case / "collector_context.json").read_text())["non_dax"])

    def test_rejects_tampered_sealed_artifact(self) -> None:
        case = self.root / "case"
        self.run_cli("seal", "--input", str(self.source), "--output", str(case))
        (case / "workload.log").chmod(0o600)
        with (case / "workload.log").open("a") as stream:
            stream.write("tampered\n")
        self.run_cli("verify", "--case-dir", str(case), ok=False)

    def test_rejects_other_running_sandbox(self) -> None:
        with (self.source / "sandboxes-active.txt").open("a") as stream:
            stream.write(f"default {'a'*32} {'a'*32} sandbox Up image now {{}}\n")
        self.run_cli("seal", "--input", str(self.source),
                     "--output", str(self.root / "case"), ok=False)

    def test_rejects_dax_backend_claim(self) -> None:
        path = self.source / "backend.log"
        path.write_text(path.read_text().rstrip() + " dax: true\n")
        self.run_cli("seal", "--input", str(self.source),
                     "--output", str(self.root / "case"), ok=False)

    def test_rejects_failed_or_unconfirmed_cleanup(self) -> None:
        cleanup = json.loads((self.source / "cleanup.json").read_text())
        cleanup["target_absent"] = False
        self.write("cleanup.json", json.dumps(cleanup) + "\n")
        self.run_cli("seal", "--input", str(self.source),
                     "--output", str(self.root / "case"), ok=False)

    def test_rejects_helper_identity_for_another_path(self) -> None:
        self.write("helper.sha256", f"{'3'*64}  /tmp/other-helper\n")
        self.run_cli("seal", "--input", str(self.source),
                     "--output", str(self.root / "case"), ok=False)

    def test_rejects_foreign_log_rows(self) -> None:
        with (self.source / "cubelet.log").open("a") as stream:
            stream.write('{"InstanceId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}\n')
        self.run_cli("seal", "--input", str(self.source),
                     "--output", str(self.root / "case"), ok=False)

    def test_cpu_snapshot_is_bound_to_one_process_lifetime(self) -> None:
        output = self.root / "cpu.json"
        self.run_cli("cpu-snapshot", "--pid", str(os.getpid()),
                     "--sandbox-id", SANDBOX, "--output", str(output))
        value = json.loads(output.read_text())
        self.assertEqual(value["pid"], os.getpid())
        self.assertEqual(value["sandbox_id"], SANDBOX)
        self.assertGreater(value["start_ticks"], 0)

    def test_rejects_output_inside_mutable_capture(self) -> None:
        self.run_cli("seal", "--input", str(self.source),
                     "--output", str(self.source / "case"), ok=False)


if __name__ == "__main__":
    unittest.main()
