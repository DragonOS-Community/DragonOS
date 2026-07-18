#!/usr/bin/env python3

from __future__ import annotations

import io
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import calibrate


RUN_ID = "0123456789abcdef0123456789abcdef"


def event_fields(kind: str, case_id: str, seq: int, guest_raw_ns: int,
                 *, mode: str, affinity: str) -> dict[str, str]:
    common = {"run": RUN_ID, "seq": str(seq), "case": case_id, "status": "ok"}
    if kind in ("START", "END"):
        return {**common, "guest_raw_ns": str(guest_raw_ns),
                "guest_mono_ns": str(guest_raw_ns), "cpu": "0"}
    reads = calibrate.READS_REQUIRED if mode == "reads" else 0
    migrations = calibrate.MIGRATIONS_EXPECTED if affinity == "migrate" else 0
    return {
        **common,
        "reason": "ok",
        "work_end_raw_ns": str(guest_raw_ns),
        "work_end_mono_ns": str(guest_raw_ns),
        "checksum": "0000000000000000",
        "raw_reads": str(reads),
        "mono_reads": str(reads),
        "raw_regressions": "0",
        "mono_regressions": "0",
        "raw_max_backward_ns": "0",
        "mono_max_backward_ns": "0",
        "migrations_requested": str(migrations),
        "migrations_observed": str(migrations),
        "cpu_mask_seen": "0000000000000003" if affinity == "migrate" else "0000000000000001",
    }


def evidence_case(spec: dict, accel: str, vcpus: int, seq: int) -> dict:
    guest_start = 1_000_000_000
    elapsed = spec["target_ns"] if spec["mode"] != "reads" else 1_000_000
    guest_end = guest_start + elapsed
    start = calibrate.Event(
        "START", event_fields("START", spec["case_id"], seq, guest_start,
                              mode=spec["mode"], affinity=spec["affinity"]), ""
    )
    work_done = calibrate.Event(
        "WORK_DONE", event_fields("WORK_DONE", spec["case_id"], seq, guest_end,
                                  mode=spec["mode"], affinity=spec["affinity"]), ""
    )
    end = calibrate.Event(
        "END", event_fields("END", spec["case_id"], seq, guest_end,
                            mode=spec["mode"], affinity=spec["affinity"]), ""
    )
    bracket = {"slo_ns": 100, "shi_ns": 100, "elo_ns": 100 + elapsed,
               "ehi_ns": 100 + elapsed}
    status, reasons, metrics = calibrate.evaluate_case_evidence(
        spec, accel, vcpus, start, work_done, end, bracket
    )
    return {
        **spec, "seq": seq, "status": status, "reasons": reasons,
        "guest": {"start": start.fields, "work_done": work_done.fields, "end": end.fields},
        "host_bracket": bracket, "metrics": calibrate.public_metrics(metrics),
    }


def evidence_cases(accel: str, vcpus: int) -> tuple[list[dict], int]:
    cases = []
    seq = 1
    for spec in calibrate.expected_cases(vcpus):
        if "predefined_skip" in spec:
            cases.append({**spec, "seq": seq, "status": "skip",
                          "reasons": [spec["predefined_skip"]]})
        else:
            cases.append(evidence_case(spec, accel, vcpus, seq))
            seq += 1
    return cases, seq


class EvidenceFactory:
    def __init__(self, root: Path):
        self.root = root
        self.store = root / "store"
        self.store.mkdir(mode=0o700)
        self.paths = {
            name: root / f"artifact-{name}" for name in calibrate.ARTIFACT_NAMES
            if name != "qemu_argv"
        }
        for name, path in self.paths.items():
            path.write_bytes((name + "\n").encode())
        self.shared_artifacts = {
            name: calibrate.archive_artifact(path, self.store)
            for name, path in self.paths.items()
        }

    def qemu_argv(self, accel: str, vcpus: int, serial_socket: Path) -> list[str]:
        argv = [
            str(self.paths["qemu_binary"]),
            "-kernel", str(self.paths["kernel"]),
            "-smp", f"{vcpus},cores={vcpus},threads=1,sockets=1",
            "-machine", f"q35,accel={accel}",
            "-rtc", "clock=host,base=localtime",
            "-serial", "none", "-monitor", "none",
            "-chardev", f"socket,id=calib_console,path={serial_socket},server=on,wait=off",
            "-drive", f"id=disk,file={self.paths['disk_image']},if=none,format=raw,snapshot=on",
            "-device", "virtconsole,chardev=calib_console",
        ]
        if accel == "kvm":
            argv.append("-enable-kvm")
        return argv

    @staticmethod
    def transcript(cases: list[dict], vcpus: int, next_seq: int) -> bytes:
        cpus = "0" if vcpus == 1 else "0,1"
        lines = [f"{calibrate.PROTOCOL} READY run={RUN_ID} seq=0 cpus={cpus}"]
        for case in cases:
            if case["status"] == "skip":
                continue
            for kind, name in (("START", "start"), ("WORK_DONE", "work_done"),
                               ("END", "end")):
                fields = " ".join(f"{key}={value}" for key, value in case["guest"][name].items())
                lines.append(f"{calibrate.PROTOCOL} {kind} {fields}")
            lines.append(f"{calibrate.PROTOCOL} READY run={RUN_ID} seq={case['seq']} cpus={cpus}")
        lines.append(f"{calibrate.PROTOCOL} ACK run={RUN_ID} seq={next_seq} status=ok")
        return ("boot line\n" + "\n".join(lines) + "\n").encode()

    def write_vm(self, accel: str, vcpus: int) -> Path:
        directory = self.root / f"{accel}-{vcpus}"
        directory.mkdir()
        serial = directory / "serial.raw"
        cases, next_seq = evidence_cases(accel, vcpus)
        serial.write_bytes(self.transcript(cases, vcpus, next_seq))
        serial_socket = self.root / f"console-{accel}-{vcpus}.sock"
        argv = self.qemu_argv(accel, vcpus, serial_socket)
        qemu_argv_source = self.root / f"qemu-argv-{accel}-{vcpus}.json"
        qemu_argv_source.write_text(json.dumps(argv), encoding="utf-8")
        artifacts = dict(self.shared_artifacts)
        artifacts["qemu_argv"] = calibrate.archive_artifact(qemu_argv_source, self.store)
        status, reasons = calibrate.finalize_vm_status(cases, calibrate.Thresholds.for_accel(accel))
        encoded_argv = b"\0".join(item.encode() for item in argv) + b"\0"
        value = {
            "schema": calibrate.SCHEMA,
            "run_id": RUN_ID,
            "created_utc": "2026-01-01T00:00:00+00:00",
            "status": status,
            "accel": accel,
            "vcpus": vcpus,
            "thresholds": calibrate.Thresholds.for_accel(accel).as_json(),
            "cases": cases,
            "reasons": reasons,
            "qemu": {
                "argv": argv,
                "argv_source": str(qemu_argv_source),
                "serial_socket": str(serial_socket),
                "pid": 123,
                "requested_cpus": [2],
                "task_affinity": [{"tid": 123, "comm": "qemu", "affinity": [2]}],
                "task_affinity_after": [{"tid": 123, "comm": "qemu", "affinity": [2]}],
                "process_identity": {
                    "pid": 123, "starttime_ticks": 1,
                    "exe": str(self.paths["qemu_binary"]), "cwd": str(self.root),
                    "cmdline_sha256": hashlib.sha256(encoded_argv).hexdigest(),
                },
                "lifecycle": {"alive_after_calibration": True,
                              "termination_owner": "external_launcher",
                              "guest_helper_shutdown": "quit_ack"},
            },
            "repo": {"commit": "test", "tree": "test", "dirty": False,
                     "status_sha256": "a" * 64, "tracked_diff_sha256": "b" * 64,
                     "untracked_content_sha256": "c" * 64},
            "host": {"platform": "test", "python": "test", "affinity": [1],
                     "clock_monotonic_raw_resolution_ns": 1, "requested_cpu": 1},
            "qemu_version": "test",
            "build_artifacts": artifacts,
            "serial": calibrate.artifact_record(serial),
            "shutdown_ack": {"run": RUN_ID, "seq": str(next_seq), "status": "ok"},
        }
        result = directory / "result.json"
        result.write_text(json.dumps(value), encoding="utf-8")
        self.reindex(result)
        return result

    @staticmethod
    def reindex(result: Path) -> None:
        value = json.loads(result.read_text(encoding="utf-8"))
        (result.parent / "sha256sums.txt").write_text(
            f"{calibrate.sha256_file(result)}  result.json\n"
            f"{value['serial']['sha256']}  serial.raw\n", encoding="ascii"
        )


def rewrite_serial(result: Path, transform) -> dict:
    value = json.loads(result.read_text(encoding="utf-8"))
    serial = Path(value["serial"]["path"])
    serial.write_bytes(transform(serial.read_bytes()))
    value["serial"] = calibrate.artifact_record(serial)
    result.write_text(json.dumps(value), encoding="utf-8")
    EvidenceFactory.reindex(result)
    return value


class ProtocolTests(unittest.TestCase):
    def test_ignores_non_protocol_output(self) -> None:
        self.assertIsNone(calibrate.parse_event("kernel: booting\n"))

    def test_parses_valid_event(self) -> None:
        event = calibrate.parse_event(
            f"{calibrate.PROTOCOL} START run={RUN_ID} seq=7 case=busy-10s-r1 guest_raw_ns=123 status=ok\n"
        )
        self.assertIsNotNone(event)
        assert event is not None
        calibrate.validate_identity(event, "START", RUN_ID, 7, "busy-10s-r1")
        self.assertEqual(calibrate.uint_field(event, "guest_raw_ns"), 123)

    def test_rejects_duplicate_bad_sequence_and_bad_ack(self) -> None:
        with self.assertRaises(calibrate.ProtocolError):
            calibrate.parse_event(f"{calibrate.PROTOCOL} END run=a seq=1 seq=1\n")
        event = calibrate.parse_event(f"{calibrate.PROTOCOL} ACK run=run seq=2 status=ok extra=x\n")
        assert event is not None
        with self.assertRaises(calibrate.ProtocolError):
            calibrate.validate_ack(event, "run", 2)
        wrong_status = calibrate.parse_event(f"{calibrate.PROTOCOL} ACK run=run seq=2 status=fail\n")
        assert wrong_status is not None
        with self.assertRaises(calibrate.ProtocolError):
            calibrate.validate_ack(wrong_status, "run", 2)

    def test_rejects_u64_overflow(self) -> None:
        event = calibrate.parse_event(f"{calibrate.PROTOCOL} END run=run seq={1 << 64}\n")
        assert event is not None
        with self.assertRaises(calibrate.ProtocolError):
            calibrate.uint_field(event, "seq")

    def test_transport_has_line_and_deadline_limits(self) -> None:
        left, right = socket.socketpair()
        transport = calibrate.SerialTransport.__new__(calibrate.SerialTransport)
        transport.path = Path("socketpair")
        transport.transcript = io.BytesIO()
        transport.sock = left
        transport.sock.setblocking(False)
        transport.selector = selectors.DefaultSelector()
        transport.selector.register(left, selectors.EVENT_READ)
        transport.buffer = bytearray()
        try:
            with self.assertRaises(calibrate.CalibrationError):
                transport.send_line("x", timeout_ns=0)
            right.sendall(b"x" * (calibrate.MAX_SERIAL_LINE_BYTES + 1) + b"\n")
            with self.assertRaises(calibrate.ProtocolError):
                transport.read_line(calibrate.raw_now_ns() + 1_000_000_000)
        finally:
            transport.close()
            right.close()

    def test_transport_init_failure_closes_partial_resources(self) -> None:
        transcript = io.BytesIO()

        sock = mock.Mock()
        sock.connect.side_effect = OSError(111, "connection refused")
        with mock.patch.object(calibrate.socket, "socket", return_value=sock):
            with self.assertRaisesRegex(
                    calibrate.CalibrationError, "connection refused"):
                calibrate.SerialTransport(Path("serial.sock"), transcript)
        sock.close.assert_called_once_with()

        sock = mock.Mock()
        sock.setblocking.side_effect = RuntimeError("setblocking failed")
        with mock.patch.object(calibrate.socket, "socket", return_value=sock):
            with self.assertRaisesRegex(RuntimeError, "setblocking failed"):
                calibrate.SerialTransport(Path("serial.sock"), transcript)
        sock.close.assert_called_once_with()

        sock = mock.Mock()
        selector = mock.Mock()
        selector.register.side_effect = RuntimeError("register failed")
        with mock.patch.object(calibrate.socket, "socket", return_value=sock), \
                mock.patch.object(calibrate.selectors, "DefaultSelector", return_value=selector):
            with self.assertRaisesRegex(RuntimeError, "register failed"):
                calibrate.SerialTransport(Path("serial.sock"), transcript)
        selector.close.assert_called_once_with()
        sock.close.assert_called_once_with()

    def test_prompt_wait_retries_and_accepts_ansi_prompt_without_newline(self) -> None:
        left, right = socket.socketpair()
        transport = calibrate.SerialTransport.__new__(calibrate.SerialTransport)
        transport.path = Path("socketpair")
        transport.transcript = io.BytesIO()
        transport.sock = left
        transport.sock.setblocking(False)
        transport.selector = selectors.DefaultSelector()
        transport.selector.register(left, selectors.EVENT_READ)
        transport.buffer = bytearray()

        def delayed_console() -> None:
            received = bytearray()
            while received.count(b"\n") < 2:
                received.extend(right.recv(64))
            right.sendall(
                b"\x1b[01;32mroot@dragonos\x1b[00m:"
                b"\x1b[01;34m~\x1b[00m# "
            )

        responder = threading.Thread(target=delayed_console)
        responder.start()
        try:
            transport.wait_for_prompt(
                re.compile(r"root@dragonos:.*#\s*$"),
                calibrate.raw_now_ns() + 1_000_000_000,
                retry_ns=1_000_000,
            )
            transcript = transport.transcript.getvalue()
            self.assertGreaterEqual(transcript.count(b">>> \n"), 2)
            self.assertIn(b"root@dragonos", transcript)
        finally:
            transport.close()
            right.close()
            responder.join(timeout=1)

    def test_transcript_total_limit_is_enforced_offline_and_live(self) -> None:
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(calibrate, "MAX_TRANSCRIPT_BYTES", 32):
            transcript = Path(directory) / "serial.raw"
            transcript.write_bytes(b"ordinary console output\n" * 2)
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.parse_transcript(transcript)

            transport = calibrate.SerialTransport.__new__(calibrate.SerialTransport)
            transport.transcript = io.BytesIO()
            transport.transcript_bytes = 32
            with self.assertRaises(calibrate.CalibrationError):
                transport.write_transcript(b"x")


class MetricsTests(unittest.TestCase):
    def test_exact_ratio_and_bounds(self) -> None:
        metrics = calibrate.compute_metrics(100, 110, 1010, 1020, 5000, 5900)
        self.assertEqual(metrics["guest_delta_ns"], 900)
        self.assertEqual(metrics["host_min_ns"], 900)
        self.assertEqual(metrics["host_max_ns"], 920)
        self.assertEqual(metrics["ratio_mid"], "0.989010989")

    def test_rejects_noncausal_bracket_and_regression(self) -> None:
        with self.assertRaises(calibrate.CalibrationError):
            calibrate.compute_metrics(100, 200, 150, 300, 1, 2)
        with self.assertRaises(calibrate.CalibrationError):
            calibrate.compute_metrics(100, 110, 200, 210, 2, 1)

    def test_kvm_interval_has_pass_fail_and_incomplete_states(self) -> None:
        thresholds = calibrate.Thresholds.for_accel("kvm")
        passing = calibrate.compute_metrics(0, 0, 1_000_000, 1_000_000, 0, 1_000_000)
        self.assertEqual(calibrate.evaluate_ratio(passing, thresholds), ("pass", []))
        below = calibrate.compute_metrics(0, 0, 1_000_000, 1_000_000, 0, 994_999)
        self.assertEqual(calibrate.evaluate_ratio(below, thresholds)[0], "fail")
        overlap = calibrate.compute_metrics(0, 10_000, 1_000_000, 1_010_000, 0, 1_000_000)
        self.assertEqual(calibrate.evaluate_ratio(overlap, thresholds)[0], "incomplete")

    def test_tcg_ratio_is_informational(self) -> None:
        metrics = calibrate.compute_metrics(0, 0, 1_000_000, 1_000_000, 0, 500_000)
        self.assertEqual(calibrate.evaluate_ratio(metrics, calibrate.Thresholds.for_accel("tcg")),
                         ("pass", []))


class VerdictTests(unittest.TestCase):
    def test_read_verdict_requires_exact_counts_and_migration_evidence(self) -> None:
        fields = event_fields("WORK_DONE", "reads", 1, 100,
                              mode="reads", affinity="migrate")
        event = calibrate.Event("WORK_DONE", fields, "")
        self.assertEqual(calibrate.evaluate_read_fields(event, "migrate", 2), [])
        fields["raw_reads"] = str(calibrate.READS_REQUIRED + 1)
        self.assertIn("raw_reads_does_not_match_required_count",
                      calibrate.evaluate_read_fields(event, "migrate", 2))

    def test_expected_matrix_has_strict_vm_set_and_only_one_skip(self) -> None:
        one = calibrate.expected_cases(1)
        two = calibrate.expected_cases(2)
        self.assertEqual(len(one), 22)
        self.assertEqual(len(two), 22)
        self.assertEqual(sum("predefined_skip" in case for case in one), 1)
        self.assertEqual(sum("predefined_skip" in case for case in two), 0)
        self.assertEqual(calibrate.REQUIRED_VMS, {("kvm", 1), ("kvm", 2), ("tcg", 2)})


class ProcessAndQemuPolicyTests(unittest.TestCase):
    def test_process_identity_and_pidfd_bind_a_live_process(self) -> None:
        sleep = shutil.which("sleep")
        self.assertIsNotNone(sleep)
        assert sleep is not None
        process = subprocess.Popen([sleep, "30"])
        pidfd = None
        try:
            # Popen returns after fork and the child can still expose the
            # transient pre-exec empty cmdline.  Retry until execve has
            # published the stable identity that the production runner sees
            # for an already-started QEMU.
            identity = None
            for _ in range(100):
                try:
                    candidate = calibrate.capture_process_identity(process.pid)
                    if os.path.samefile(candidate.exe, sleep):
                        identity = candidate
                        break
                except (calibrate.CalibrationError, OSError):
                    pass
                threading.Event().wait(0.01)
            self.assertIsNotNone(identity)
            assert identity is not None
            self.assertEqual(identity.pid, process.pid)
            self.assertGreater(identity.starttime_ticks, 0)
            calibrate.validate_process_identity(identity, list(identity.argv), Path(sleep))
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.validate_process_identity(identity, [*identity.argv, "changed"], Path(sleep))
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.validate_process_identity(identity, list(identity.argv), Path(sys.executable))
            pidfd = os.pidfd_open(process.pid, 0)
            calibrate.require_pidfd_alive(pidfd)
            process.terminate()
            process.wait(timeout=5)
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.require_pidfd_alive(pidfd)
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            if pidfd is not None:
                os.close(pidfd)

    def test_qemu_policy_rejects_calibration_affecting_argv_changes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            factory = EvidenceFactory(root)
            serial = root / "serial.sock"
            argv = factory.qemu_argv("kvm", 2, serial)
            args = (str(root), "kvm", 2, factory.paths["kernel"],
                    factory.paths["disk_image"], serial)
            calibrate.validate_qemu_policy(argv, *args)

            def replace_value(option: str, value: str) -> list[str]:
                changed = list(argv)
                changed[changed.index(option) + 1] = value
                return changed

            scenarios = {
                "wrong_smp": replace_value("-smp", "1,cores=1,threads=1,sockets=1"),
                "incomplete_topology": replace_value("-smp", "2"),
                "wrong_accelerator": replace_value("-machine", "q35,accel=tcg"),
                "wrong_kernel": replace_value("-kernel", str(root / "other-kernel")),
                "mutable_disk": replace_value(
                    "-drive", f"id=disk,file={factory.paths['disk_image']},if=none,format=raw"),
                "wrong_disk": replace_value(
                    "-drive", f"id=disk,file={root / 'other-disk'},snapshot=on"),
                "monitor_enabled": replace_value("-monitor", "stdio"),
                "wrong_rtc": replace_value("-rtc", "clock=vm"),
                "wrong_socket": replace_value(
                    "-chardev", "socket,id=calib_console,path=/tmp/wrong,server=on,wait=off"),
                "debug_stop": [*argv, "-S"],
                "duplicate_smp": [*argv, "-smp", "2,cores=2,threads=1,sockets=1"],
            }
            for name, changed in scenarios.items():
                with self.subTest(name=name), self.assertRaises(calibrate.CalibrationError):
                    calibrate.validate_qemu_policy(changed, *args)


class ArtifactTests(unittest.TestCase):
    def test_untracked_source_hash_covers_names_and_contents(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            source = repo / "new-source.rs"
            source.write_text("first\n", encoding="utf-8")
            first = calibrate.hash_untracked_content(repo)
            source.write_text("second\n", encoding="utf-8")
            second = calibrate.hash_untracked_content(repo)
            source.rename(repo / "renamed-source.rs")
            renamed = calibrate.hash_untracked_content(repo)
            self.assertNotEqual(first, second)
            self.assertNotEqual(second, renamed)

    def test_cpu_list_parser_has_strict_bounds(self) -> None:
        self.assertEqual(calibrate.parse_cpu_list("1,3-5"), {1, 3, 4, 5})
        for invalid in ("5-3", "0-1000000000", str(calibrate.MAX_CPU_ID + 1)):
            with self.subTest(invalid=invalid), self.assertRaises(calibrate.CalibrationError):
                calibrate.parse_cpu_list(invalid)

    def test_output_directory_is_never_overwritten(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "run"
            calibrate.create_output_dir(output)
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.create_output_dir(output)

    def test_invalid_run_id_does_not_reserve_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "run"
            args = type("Args", (), {"output": output, "run_id": "BAD"})()
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.run_vm(args)
            self.assertFalse(output.exists())

    def test_complete_raw_matrix_aggregates(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            factory = EvidenceFactory(root)
            paths = [factory.write_vm(accel, vcpus)
                     for accel, vcpus in sorted(calibrate.REQUIRED_VMS)]
            args = type("Args", (), {"output": root / "aggregate", "vm_result": paths})()
            self.assertEqual(calibrate.aggregate(args), 0)

    def test_archived_inputs_survive_execution_tree_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            factory = EvidenceFactory(root)
            result = factory.write_vm("kvm", 1)
            value = json.loads(result.read_text(encoding="utf-8"))
            for record in value["build_artifacts"].values():
                Path(record["execution_path"]).unlink()
            calibrate.verify_vm_result(value, result)

    def test_archive_is_content_addressed_reused_and_read_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = root / "store"
            store.mkdir(mode=0o700)
            source = root / "source"
            source.write_bytes(b"immutable input\n")
            first = calibrate.archive_artifact(source, store)
            second = calibrate.archive_artifact(source, store)
            self.assertEqual(first, second)
            archive = Path(first["archive_path"])
            self.assertEqual(archive.name, first["sha256"])
            self.assertEqual(archive.stat().st_mode & 0o222, 0)
            source.unlink()
            calibrate.verify_archived_artifact(first, "test")

    def test_serial_protocol_and_json_are_strictly_bound(self) -> None:
        def remove_first(data: bytes, prefix: bytes) -> bytes:
            lines = data.splitlines(keepends=True)
            index = next(i for i, line in enumerate(lines) if line.startswith(prefix))
            del lines[index]
            return b"".join(lines)

        def duplicate_first(data: bytes, prefix: bytes) -> bytes:
            lines = data.splitlines(keepends=True)
            index = next(i for i, line in enumerate(lines) if line.startswith(prefix))
            lines.insert(index, lines[index])
            return b"".join(lines)

        def reorder_first_case(data: bytes) -> bytes:
            lines = data.splitlines(keepends=True)
            start = next(i for i, line in enumerate(lines)
                         if line.startswith(f"{calibrate.PROTOCOL} START ".encode()))
            lines[start], lines[start + 1] = lines[start + 1], lines[start]
            return b"".join(lines)

        start_prefix = f"{calibrate.PROTOCOL} START ".encode()
        ack_prefix = f"{calibrate.PROTOCOL} ACK ".encode()
        scenarios = {
            "guest_json_mismatch": lambda data: data.replace(
                b"guest_raw_ns=1000000000", b"guest_raw_ns=1000000001", 1),
            "missing_event": lambda data: remove_first(data, start_prefix),
            "duplicate_event": lambda data: duplicate_first(data, start_prefix),
            "reordered_event": reorder_first_case,
            "guest_error": lambda data: data.replace(
                start_prefix,
                f"{calibrate.PROTOCOL} ERROR run={RUN_ID} seq=1 reason=test ".encode(), 1),
            "wrong_ready_cpu_set": lambda data: data.replace(b"cpus=0\n", b"cpus=0,1\n", 1),
            "missing_ack": lambda data: remove_first(data, ack_prefix),
            "extra_event": lambda data: data +
                f"{calibrate.PROTOCOL} READY run={RUN_ID} seq=999 cpus=0\n".encode(),
        }
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            for name, transform in scenarios.items():
                with self.subTest(name=name):
                    root = parent / name
                    root.mkdir()
                    result = EvidenceFactory(root).write_vm("kvm", 1)
                    value = rewrite_serial(result, transform)
                    with self.assertRaises(calibrate.CalibrationError):
                        calibrate.verify_vm_result(value, result)

    def test_schema_v2_result_cannot_pass_verification(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = EvidenceFactory(root).write_vm("kvm", 1)
            value = json.loads(result.read_text(encoding="utf-8"))
            value["schema"] = "dragonos.timekeeping-calibration.v2"
            result.write_text(json.dumps(value), encoding="utf-8")
            EvidenceFactory.reindex(result)
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.verify_vm_result(value, result)

    def test_skeletal_pass_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = EvidenceFactory(root).write_vm("kvm", 1)
            value = json.loads(result.read_text(encoding="utf-8"))
            value["cases"][0].pop("host_bracket")
            result.write_text(json.dumps(value), encoding="utf-8")
            EvidenceFactory.reindex(result)
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.verify_vm_result(value, result)

    def test_tampered_ratio_status_and_artifact_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            factory = EvidenceFactory(root)
            for field in ("ratio", "status"):
                result = factory.write_vm("kvm", 1)
                value = json.loads(result.read_text(encoding="utf-8"))
                if field == "ratio":
                    value["cases"][0]["metrics"]["ratio_numerator"] += 1
                else:
                    value["cases"][0]["status"] = "fail"
                result.write_text(json.dumps(value), encoding="utf-8")
                EvidenceFactory.reindex(result)
                with self.subTest(field=field), self.assertRaises(calibrate.CalibrationError):
                    calibrate.verify_vm_result(value, result)
                for path in result.parent.iterdir():
                    path.unlink()
                result.parent.rmdir()
            result = factory.write_vm("kvm", 1)
            value = json.loads(result.read_text(encoding="utf-8"))
            artifact = Path(value["build_artifacts"]["kernel"]["archive_path"])
            artifact.chmod(0o644)
            artifact.write_bytes(b"tampered\n")
            with self.assertRaises(calibrate.CalibrationError):
                calibrate.verify_vm_result(value, result)

    def test_fatal_serial_marker_is_rejected_even_when_reindexed(self) -> None:
        for marker in calibrate.SERIAL_FATAL_MARKERS:
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                factory = EvidenceFactory(root)
                result = factory.write_vm("kvm", 1)
                value = json.loads(result.read_text(encoding="utf-8"))
                serial = Path(value["serial"]["path"])
                serial.write_bytes(serial.read_bytes() + marker + b"\n")
                value["serial"] = calibrate.artifact_record(serial)
                result.write_text(json.dumps(value), encoding="utf-8")
                EvidenceFactory.reindex(result)
                with self.assertRaises(calibrate.CalibrationError):
                    calibrate.verify_vm_result(value, result)

    def test_clocksource_event_during_measurement_is_rejected(self) -> None:
        for marker in calibrate.SERIAL_MEASUREMENT_FORBIDDEN_MARKERS:
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                factory = EvidenceFactory(root)
                result = factory.write_vm("kvm", 1)

                def inject_after_ready(serial: bytes) -> bytes:
                    ready_end = serial.index(b"\n", serial.index(b" READY ")) + 1
                    return serial[:ready_end] + marker + b"\n" + serial[ready_end:]

                value = rewrite_serial(result, inject_after_ready)
                with self.assertRaises(calibrate.CalibrationError):
                    calibrate.verify_vm_result(value, result)

    def test_boot_time_clocksource_events_are_outside_measurement_window(self) -> None:
        for marker in calibrate.SERIAL_MEASUREMENT_FORBIDDEN_MARKERS:
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                factory = EvidenceFactory(root)
                result = factory.write_vm("kvm", 1)
                value = rewrite_serial(result, lambda serial: marker + b"\n" + serial)
                calibrate.verify_vm_result(value, result)

    def test_missing_vm_makes_aggregate_incomplete(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            factory = EvidenceFactory(root)
            result = factory.write_vm("kvm", 1)
            args = type("Args", (), {"output": root / "aggregate", "vm_result": [result]})()
            self.assertEqual(calibrate.aggregate(args), 2)

    def test_cli_parser_builds(self) -> None:
        parser = calibrate.build_parser()
        with self.assertRaises(SystemExit) as context:
            parser.parse_args(["--help"])
        self.assertEqual(context.exception.code, 0)


if __name__ == "__main__":
    unittest.main()
