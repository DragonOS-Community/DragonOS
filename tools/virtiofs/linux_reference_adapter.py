#!/usr/bin/env python3
"""Seal and replay a strict non-DAX Linux virtiofs reference case."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import stat
import sys
import tempfile
from pathlib import Path
from typing import Any

SCHEMA = "dragonos.virtiofs.linux-reference-case.v1"
RESULT_SCHEMA = "dragonos.virtiofs.non-dax-case-result.v1"
TOKEN = re.compile(r"^[A-Za-z0-9._-]+$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
TRACE_EVENT = re.compile(r": read_(sync|async): .*\bopcode=(\d+)\s+read_size=(\d+)\b")
TRACE_PID = re.compile(r"-(\d+)\s+\[[0-9]+\]")
TRACE_TIMESTAMP = re.compile(r"\s(\d+)\.(\d+):\s+read_(?:sync|async):")


class EvidenceError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise EvidenceError(message)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_regular(path: Path, label: str) -> bytes:
    try:
        info = path.lstat()
    except OSError as error:
        fail(f"cannot stat {label}: {error}")
    if not stat.S_ISREG(info.st_mode) or path.is_symlink():
        fail(f"{label} must be a regular non-symlink file")
    try:
        return path.read_bytes()
    except OSError as error:
        fail(f"cannot read {label}: {error}")


def parse_tsv(path: Path, expected: set[str], label: str) -> dict[str, str]:
    rows: dict[str, str] = {}
    for number, raw in enumerate(read_regular(path, label).decode("utf-8", "strict").splitlines(), 1):
        fields = raw.split("\t")
        if len(fields) != 2 or not fields[0] or not fields[1] or fields[0] in rows:
            fail(f"{label} has an invalid or duplicate row at line {number}")
        rows[fields[0]] = fields[1]
    if set(rows) != expected:
        fail(f"{label} fields differ: expected {sorted(expected)}, got {sorted(rows)}")
    return rows


def parse_tokens(line: str, prefix: str) -> dict[str, str]:
    if not line.startswith(prefix + " "):
        fail(f"record does not start with {prefix}")
    result: dict[str, str] = {}
    for field in line.split()[1:]:
        if "=" not in field:
            fail(f"{prefix} contains a non key=value field")
        key, value = field.split("=", 1)
        if not key or not value or key in result:
            fail(f"{prefix} contains an empty or duplicate field")
        result[key] = value
    return result


def positive_decimal(value: str, label: str, allow_zero: bool = False) -> int:
    if not re.fullmatch(r"0|[1-9][0-9]*", value):
        fail(f"{label} is not canonical decimal")
    result = int(value)
    if result == 0 and not allow_zero:
        fail(f"{label} must be positive")
    return result


def parse_transcript(path: Path, run_id: str, dataset: str, file_size: int,
                     block_size: int) -> dict[str, Any]:
    lines = read_regular(path, "helper transcript").decode("utf-8", "strict").splitlines()
    phase_lines = [line for line in lines if line.startswith("phase ")]
    result_lines = [line for line in lines if line.startswith("result ")]
    summary_lines = [line for line in lines if line.startswith("io_summary ")]
    if len(result_lines) != 1 or len(summary_lines) != 1:
        fail("transcript must contain exactly one result and one io_summary")
    expected_phases = [("open", "begin"), ("open", "end"), ("data_loop", "begin"),
                       ("data_loop", "end"), ("close", "begin"), ("close", "end"),
                       ("verify", "begin"), ("verify", "end")]
    observed: list[tuple[str, str]] = []
    data_window: list[int] = []
    for line in phase_lines:
        record = parse_tokens(line, "phase")
        for key, expected in (("workload", "sequential_read"), ("dataset", dataset),
                              ("run_id", run_id)):
            if record.get(key) != expected:
                fail(f"phase {key} differs from the case")
        observed.append((record.get("phase", ""), record.get("event", "")))
        if record.get("phase") == "data_loop":
            data_window.append(positive_decimal(record.get("monotonic_us", ""),
                                                "data-loop monotonic timestamp"))
    if observed != expected_phases:
        fail("transcript phase sequence is incomplete or out of order")
    result = parse_tokens(result_lines[0], "result")
    required = {"workload", "status", "errno", "elapsed_us", "bytes", "ops", "syscalls",
                "short_io", "eintr", "checksum", "mount", "dataset", "seed", "files",
                "file_size", "block_size", "iterations", "workers", "run_id", "cache_mode",
                "mount_options", "expect_dax", "sysname", "release"}
    if set(result) != required:
        fail("helper result fields differ from the stable schema")
    expected_values = {"workload": "sequential_read", "status": "ok", "errno": "0",
                       "bytes": str(file_size), "dataset": dataset,
                       "file_size": str(file_size), "block_size": str(block_size),
                       "run_id": run_id, "cache_mode": "linux-reference", "sysname": "Linux"}
    for key, expected in expected_values.items():
        if result[key] != expected:
            fail(f"helper result {key} differs from the case")
    if not re.fullmatch(r"[0-9a-f]{16}", result["checksum"]):
        fail("helper checksum is invalid")
    metrics = {key: positive_decimal(result[key], key, key in {"short_io", "eintr"})
               for key in ("elapsed_us", "bytes", "ops", "syscalls", "short_io", "eintr")}
    summary = parse_tokens(summary_lines[0], "io_summary")
    for key in ("workload", "run_id", "checksum", "syscalls", "short_io", "eintr"):
        expected = result[key] if key != "workload" else "sequential_read"
        if summary.get(key) != expected:
            fail(f"io_summary {key} differs from result")
    metrics["checksum"] = result["checksum"]
    if len(data_window) != 2 or data_window[0] >= data_window[1]:
        fail("helper data-loop timestamps are not strictly ordered")
    metrics["_data_begin_us"], metrics["_data_end_us"] = data_window
    return metrics


def validate_format(path: Path, expected_name: str) -> None:
    text = read_regular(path, f"trace format {expected_name}").decode("utf-8", "strict")
    if text.count(f"name: {expected_name}\n") != 1:
        fail(f"trace format name is not {expected_name}")
    for field in ("field:u32 opcode;", "field:u32 read_size;"):
        if text.count(field) != 1:
            fail(f"trace format lacks unique {field}")
    if 'opcode=%u read_size=%u' not in text:
        fail("trace print format does not expose opcode and read_size")


def parse_trace(capture: Path, run_id: str, case_id: str, helper_pid: int,
                data_begin_us: int = 0, data_end_us: int = (1 << 63) - 1) -> dict[str, Any]:
    definitions = read_regular(capture / "probe-definition", "probe definition").decode().splitlines()
    expected = [
        "p:dragonos_virtiofs_ref/read_sync fuse_simple_request args=$arg2:u64 opcode=+8($arg2):u32 read_size=+16(+32($arg2)):u32",
        "p:dragonos_virtiofs_ref/read_async fuse_simple_background args=$arg2:u64 opcode=+8($arg2):u32 read_size=+16(+32($arg2)):u32",
    ]
    if definitions != expected:
        fail("trace probe definition differs from the audited Linux fuse_args layout")
    validate_format(capture / "format-sync", "read_sync")
    validate_format(capture / "format-async", "read_async")
    lines = read_regular(capture / "trace", "raw trace").decode("utf-8", "strict").splitlines()
    count_headers = [re.search(r"entries-in-buffer/entries-written:\s*(\d+)/(\d+)", line)
                     for line in lines]
    count_headers = [match for match in count_headers if match is not None]
    if len(count_headers) != 1 or count_headers[0].group(1) != count_headers[0].group(2):
        fail("raw trace lacks one lossless entries-in-buffer/entries-written header")
    begin = f"LINUX_REF_BEGIN run_id={run_id} case_id={case_id} helper_pid={helper_pid}"
    end = f"LINUX_REF_END run_id={run_id} case_id={case_id} helper_pid={helper_pid} rc=0"
    begin_at = [index for index, line in enumerate(lines) if begin in line]
    end_at = [index for index, line in enumerate(lines) if end in line]
    if len(begin_at) != 1 or len(end_at) != 1 or begin_at[0] >= end_at[0]:
        fail("raw trace lacks one ordered run/case-bound measurement window")
    sizes: list[int] = []
    transports = {"sync": 0, "async": 0}
    for line in lines[begin_at[0] + 1:end_at[0]]:
        match = TRACE_EVENT.search(line)
        if not match:
            continue
        timestamp = TRACE_TIMESTAMP.search(line)
        if timestamp is None:
            fail("trace READ event lacks a parseable mono timestamp")
        seconds, fraction = timestamp.groups()
        event_us = int(seconds) * 1_000_000 + int((fraction + "000000")[:6])
        if not data_begin_us <= event_us <= data_end_us:
            continue
        pid_match = TRACE_PID.search(line)
        if pid_match is None or int(pid_match.group(1)) != helper_pid:
            fail("trace READ event is not owned by the frozen helper PID")
        transport, opcode, size = match.groups()
        if int(opcode) != 15:
            continue
        value = positive_decimal(size, "trace READ size")
        sizes.append(value)
        transports[transport] += 1
    if not sizes:
        fail("raw trace contains no FUSE_READ submissions in the measurement window")
    buckets = {"pages_1": 0, "pages_2_4": 0, "pages_5_16": 0, "pages_17_32": 0,
               "pages_33_64": 0, "pages_65_plus": 0}
    for size in sizes:
        pages = (size + 4095) // 4096
        key = ("pages_1" if pages == 1 else "pages_2_4" if pages <= 4 else
               "pages_5_16" if pages <= 16 else "pages_17_32" if pages <= 32 else
               "pages_33_64" if pages <= 64 else "pages_65_plus")
        buckets[key] += 1
    return {"read_requests": len(sizes), "requested_bytes": sum(sizes),
            "request_size_buckets": buckets, "submission_paths": transports}


def load_json(path: Path, label: str) -> Any:
    try:
        return json.loads(read_regular(path, label).decode("utf-8", "strict"))
    except json.JSONDecodeError as error:
        fail(f"{label} is not valid JSON: {error}")


def load_study_module() -> Any:
    path = Path(__file__).with_name("non_dax_study.py")
    spec = importlib.util.spec_from_file_location("dragonos_non_dax_study", path)
    if spec is None or spec.loader is None:
        fail("cannot load non_dax_study.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def option(argv: list[str], name: str) -> str:
    values: list[str] = []
    for index, value in enumerate(argv):
        if value == name and index + 1 < len(argv):
            values.append(argv[index + 1])
        elif value.startswith(name + "="):
            values.append(value.split("=", 1)[1])
    if len(values) != 1:
        fail(f"QEMU must provide exactly one {name}")
    return values[0]


def parse_device(argv: list[str], tag: str) -> tuple[str, str]:
    devices = []
    for index, value in enumerate(argv):
        if value == "-device" and index + 1 < len(argv):
            devices.append(argv[index + 1])
        elif value.startswith("-device="):
            devices.append(value.split("=", 1)[1])
    matches = []
    for value in devices:
        parts = value.split(",")
        if parts[0] not in {"vhost-user-fs-pci", "vhost-user-fs-device"}:
            continue
        fields = dict(part.split("=", 1) for part in parts[1:] if "=" in part)
        if fields.get("tag") == tag:
            matches.append(fields)
    if len(matches) != 1:
        fail("QEMU must expose exactly one matching vhost-user-fs device")
    fields = matches[0]
    cache_size = fields.get("cache-size", "0")
    if not re.fullmatch(r"0+[KkMmGgTt]?|0x0+", cache_size):
        fail("QEMU vhost-user-fs device has a non-zero DAX cache-size")
    if "chardev" not in fields:
        fail("vhost-user-fs device does not identify its chardev")
    return fields["chardev"], cache_size


def capture_process(pid: int, label: str) -> tuple[list[str], dict[str, Any]]:
    root = Path("/proc") / str(pid)
    raw = read_regular(root / "cmdline", f"live {label} cmdline")
    argv = [part.decode("utf-8", "strict") for part in raw.split(b"\0") if part]
    if not argv:
        fail(f"live {label} cmdline is empty")
    try:
        executable = (root / "exe").resolve(strict=True)
        cwd = (root / "cwd").resolve(strict=True)
        stat_text = (root / "stat").read_text()
    except OSError as error:
        fail(f"cannot capture live {label} identity: {error}")
    rest = stat_text[stat_text.rfind(")") + 2:].split()
    if len(rest) < 20:
        fail(f"live {label} stat is malformed")
    return argv, {"pid": pid, "start_ticks": int(rest[19]), "executable": str(executable),
                  "executable_sha256": sha256_file(executable), "cwd": str(cwd), "argv": argv}


def validate_runtime(qemu_pid: int, daemon_pid: int, worker_pids: list[int], kernel: Path,
                     initramfs: Path, tag: str, expected_qemu_sha: str,
                     expected_daemon_sha: str) -> dict[str, Any]:
    qemu_argv, qemu = capture_process(qemu_pid, "QEMU")
    daemon_argv, daemon = capture_process(daemon_pid, "virtiofsd")
    worker_captures = [capture_process(pid, f"virtiofsd worker {pid}")[1] for pid in worker_pids]
    if len({qemu_pid, daemon_pid, *worker_pids}) != 2 + len(worker_pids):
        fail("QEMU, virtiofsd, and worker PIDs must be distinct")
    if qemu["executable_sha256"] != expected_qemu_sha:
        fail("live QEMU binary differs from the pre-registered identity")
    if daemon["executable_sha256"] != expected_daemon_sha:
        fail("live virtiofsd binary differs from the pre-registered identity")
    if any(worker["executable_sha256"] != expected_daemon_sha for worker in worker_captures):
        fail("live virtiofsd worker binary differs from the pre-registered identity")
    def qemu_path(value: str) -> Path:
        path = Path(value)
        return path.resolve(strict=True) if path.is_absolute() else (Path(qemu["cwd"]) / path).resolve(strict=True)
    if qemu_path(option(qemu_argv, "-kernel")) != kernel.resolve(strict=True):
        fail("live QEMU kernel differs from the attested Linux kernel")
    if qemu_path(option(qemu_argv, "-initrd")) != initramfs.resolve(strict=True):
        fail("live QEMU initrd differs from the attested Linux initramfs")
    chardev_id, cache_size = parse_device(qemu_argv, tag)
    chardevs: list[str] = []
    for index, value in enumerate(qemu_argv):
        if value == "-chardev" and index + 1 < len(qemu_argv):
            chardevs.append(qemu_argv[index + 1])
        elif value.startswith("-chardev="):
            chardevs.append(value.split("=", 1)[1])
    socket_paths = []
    for value in chardevs:
        fields = dict(part.split("=", 1) for part in value.split(",")[1:] if "=" in part)
        if fields.get("id") == chardev_id and value.split(",", 1)[0] == "socket":
            socket_paths.append(fields.get("path", ""))
    if len(socket_paths) != 1 or not socket_paths[0].startswith("/"):
        fail("matching QEMU chardev lacks one absolute socket path")
    daemon_sockets = []
    for index, value in enumerate(daemon_argv):
        if value == "--socket-path" and index + 1 < len(daemon_argv):
            daemon_sockets.append(daemon_argv[index + 1])
        elif value.startswith("--socket-path="):
            daemon_sockets.append(value.split("=", 1)[1])
    if daemon_sockets != socket_paths:
        fail("QEMU and virtiofsd socket paths differ")
    unix_rows: dict[str, tuple[str, str]] = {}
    for line in Path("/proc/net/unix").read_text().splitlines()[1:]:
        fields = line.split()
        if len(fields) >= 7:
            unix_rows[fields[6]] = (fields[5], fields[7] if len(fields) > 7 else "")
    def held_sockets(pid: int) -> set[str]:
        result: set[str] = set()
        for descriptor in (Path("/proc") / str(pid) / "fd").iterdir():
            try:
                target = os.readlink(descriptor)
            except OSError:
                continue
            match = re.fullmatch(r"socket:\[(\d+)\]", target)
            if match:
                result.add(match.group(1))
        return result
    qemu_sockets = held_sockets(qemu_pid)
    service_socket_inodes = set().union(*(held_sockets(pid) for pid in [daemon_pid, *worker_pids]))
    named_daemon = {inode for inode in service_socket_inodes
                    if unix_rows.get(inode, ("", ""))[1] == socket_paths[0]}
    qemu_established = {inode for inode in qemu_sockets if unix_rows.get(inode, ("", ""))[0] == "03"}
    daemon_established = {inode for inode in service_socket_inodes
                          if unix_rows.get(inode, ("", ""))[0] == "03"}
    if not named_daemon or not qemu_established or not daemon_established:
        fail("live QEMU/virtiofsd Unix socket ownership is not established")
    return {"host_boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            "qemu": qemu, "virtiofsd": daemon, "virtiofsd_workers": worker_captures,
            "binding": {"tag": tag, "chardev": chardev_id,
                        "socket_path": socket_paths[0], "dax_cache_size": cache_size}}


def validate_cpu(before_path: Path, after_path: Path, delta_path: Path, run_id: str,
                 case_id: str, byte_count: int, runtime: dict[str, Any]) -> dict[str, Any]:
    before, after, delta = (load_json(before_path, "CPU before"),
                            load_json(after_path, "CPU after"), load_json(delta_path, "CPU delta"))
    study = load_study_module()
    try:
        expected = study.compute_cpu_delta(before, after, byte_count)
    except SystemExit as error:
        fail(f"CPU evidence is invalid: {error}")
    if delta != expected:
        fail("CPU delta differs from the recomputed snapshot delta")
    if before.get("run_id") != run_id or before.get("case_id") != case_id:
        fail("CPU evidence is not bound to this run/case")
    if before.get("host_boot_id") != runtime["host_boot_id"]:
        fail("CPU evidence host boot ID differs from live process capture")
    process_by_pid = {item.get("pid"): item for item in before.get("processes", [])}
    expected_processes = [("qemu", runtime["qemu"]), ("virtiofsd", runtime["virtiofsd"])]
    expected_processes.extend(("virtiofsd-worker", worker) for worker in runtime["virtiofsd_workers"])
    for label, live in expected_processes:
        process = process_by_pid.get(live["pid"])
        if process is None or process.get("pid") != live["pid"] or process.get("starttime_ticks") != live["start_ticks"]:
            fail(f"CPU {label} identity differs from live process capture")
        if process.get("label") != label:
            fail(f"CPU PID {live['pid']} has the wrong process label")
    return delta


def validate_capture(capture: Path, run_id: str, case_id: str, helper_sha: str,
                     expected_release: str, dataset: str, file_size: int,
                     block_size: int) -> tuple[dict[str, Any], dict[str, Any]]:
    identity_keys = {"schema", "run_id", "case_id", "boot_id", "sysname", "release", "version",
                     "machine", "kernel_cmdline", "helper_path", "helper_sha256", "mount_path",
                     "mount_source", "mount_fstype", "mount_options"}
    identity = parse_tsv(capture / "guest-identity.tsv", identity_keys, "guest identity")
    expected = {"schema": "dragonos.virtiofs.linux-guest-identity.v1", "run_id": run_id,
                "case_id": case_id, "sysname": "Linux", "release": expected_release,
                "helper_sha256": helper_sha}
    for key, value in expected.items():
        if identity[key] != value:
            fail(f"guest identity {key} differs from the case")
    if identity["mount_fstype"] not in {"virtiofs", "fuse.virtiofs"}:
        fail("guest identity does not describe a virtiofs mount")
    execution = parse_tsv(capture / "execution.tsv", {"helper_pid", "helper_rc"}, "execution")
    helper_pid = positive_decimal(execution["helper_pid"], "helper PID")
    if execution["helper_rc"] != "0":
        fail("guest helper did not complete successfully")
    metrics = parse_transcript(capture / "transcript", run_id, dataset, file_size, block_size)
    trace_clock = read_regular(capture / "trace-clock", "trace clock").decode("utf-8", "strict").strip()
    if len(re.findall(r"\[[^\[\]\s]+\]", trace_clock)) != 1 or "[mono]" not in trace_clock:
        fail("trace clock is not uniquely selected as mono")
    data_begin_us = metrics.pop("_data_begin_us")
    data_end_us = metrics.pop("_data_end_us")
    trace = parse_trace(capture, run_id, case_id, helper_pid, data_begin_us, data_end_us)
    if not (file_size <= trace["requested_bytes"] <= file_size * 2):
        fail("trace READ bytes violate the bounded readahead envelope")
    return {"identity": identity, "trace": trace}, metrics


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")


def pack(args: argparse.Namespace) -> None:
    for value, label in ((args.run_id, "run ID"), (args.case_id, "case ID"),
                         (args.dataset, "dataset")):
        if not TOKEN.fullmatch(value) or value in {".", ".."}:
            fail(f"{label} is not a safe token")
    output = Path(args.output_dir)
    if output.exists() or output.is_symlink():
        fail("refusing to replace output directory")
    helper, kernel, initramfs = map(Path, (args.helper, args.kernel, args.initramfs))
    for path, label in ((helper, "helper"), (kernel, "kernel"), (initramfs, "initramfs")):
        read_regular(path, label)
    helper_sha = sha256_file(helper)
    for value, label in ((args.expected_qemu_sha256, "expected QEMU SHA-256"),
                         (args.expected_virtiofsd_sha256, "expected virtiofsd SHA-256")):
        if not SHA256.fullmatch(value):
            fail(f"{label} is invalid")
    capture_data, metrics = validate_capture(Path(args.capture_dir), args.run_id, args.case_id,
                                             helper_sha, args.expected_release, args.dataset,
                                             args.file_size, args.block_size)
    runtime = validate_runtime(args.qemu_pid, args.virtiofsd_pid, args.virtiofsd_worker_pid,
                               kernel, initramfs, args.tag, args.expected_qemu_sha256,
                               args.expected_virtiofsd_sha256)
    cpu_delta = validate_cpu(Path(args.cpu_before), Path(args.cpu_after), Path(args.cpu_delta),
                             args.run_id, args.case_id, metrics["bytes"], runtime)
    case_result = {"schema": RESULT_SCHEMA, "runner_version": "4", "status": "completed",
                   "case_id": args.case_id, "workload": "sequential_read", "mode": "light",
                   "result": {**metrics, "read_requests": capture_data["trace"]["read_requests"],
                              "requested_bytes": capture_data["trace"]["requested_bytes"]},
                   "config": {"source": "linux-tracefs", "negotiated_limits": None}}
    manifest = {"schema": SCHEMA, "runner_version": "4", "status": "completed",
                "run_id": args.run_id, "case_id": args.case_id,
                "workload": {"name": "sequential_read", "dataset": args.dataset,
                             "file_size": args.file_size, "block_size": args.block_size,
                             "guest_cache": args.guest_cache, "host_cache": args.host_cache},
                "artifacts": {"helper": {"path": str(helper.resolve()), "sha256": helper_sha},
                              "kernel": {"path": str(kernel.resolve()), "sha256": sha256_file(kernel)},
                              "initramfs": {"path": str(initramfs.resolve()),
                                            "sha256": sha256_file(initramfs)}},
                "guest": capture_data["identity"], "trace_summary": capture_data["trace"],
                "runtime": runtime, "cpu_delta": cpu_delta}
    parent = output.parent.resolve()
    parent.mkdir(parents=True, exist_ok=True)
    output = parent / output.name
    claim = parent / f".{output.name}.claim"
    try:
        claim_fd = os.open(claim, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
    except FileExistsError:
        fail("another publisher already owns this output name")
    os.close(claim_fd)
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=parent))
    try:
        artifacts = {"helper-transcript": Path(args.capture_dir) / "transcript",
                     "trace": Path(args.capture_dir) / "trace",
                     "trace-format-sync": Path(args.capture_dir) / "format-sync",
                     "trace-format-async": Path(args.capture_dir) / "format-async",
                     "trace-clock": Path(args.capture_dir) / "trace-clock",
                     "trace-probe-definition": Path(args.capture_dir) / "probe-definition",
                     "guest-identity.tsv": Path(args.capture_dir) / "guest-identity.tsv",
                     "execution.tsv": Path(args.capture_dir) / "execution.tsv",
                     "cpu-before.json": Path(args.cpu_before), "cpu-after.json": Path(args.cpu_after),
                     "cpu-delta.json": Path(args.cpu_delta)}
        for name, source in artifacts.items():
            read_regular(source, name)
            shutil.copyfile(source, staging / name)
        (staging / "qemu-cmdline").write_bytes(
            b"\0".join(item.encode() for item in runtime["qemu"]["argv"]) + b"\0")
        (staging / "virtiofsd-cmdline").write_bytes(
            b"\0".join(item.encode() for item in runtime["virtiofsd"]["argv"]) + b"\0")
        for index, worker in enumerate(runtime["virtiofsd_workers"]):
            (staging / f"virtiofsd-worker-{index}-cmdline").write_bytes(
                b"\0".join(item.encode() for item in worker["argv"]) + b"\0")
        write_json(staging / "case-result.json", case_result)
        write_json(staging / "manifest.json", manifest)
        rows = []
        for path in sorted(staging.iterdir(), key=lambda item: item.name):
            rows.append(f"{path.name}\t{sha256_file(path)}\t{path.stat().st_size}")
        (staging / "artifacts.tsv").write_text("\n".join(rows) + "\n")
        for path in staging.iterdir():
            path.chmod(0o400)
        staging.chmod(0o500)
        os.rename(staging, output)
    finally:
        if staging.exists():
            shutil.rmtree(staging)
        claim.unlink(missing_ok=True)
    print(f"linux_reference_case={output}")


def verify(args: argparse.Namespace) -> None:
    root = Path(args.case_dir)
    manifest = load_json(root / "manifest.json", "manifest")
    if not isinstance(manifest, dict) or manifest.get("schema") != SCHEMA:
        fail("manifest schema is incompatible")
    raw_rows = read_regular(root / "artifacts.tsv", "artifact index").decode().splitlines()
    seen: set[str] = set()
    for row in raw_rows:
        fields = row.split("\t")
        if len(fields) != 3 or fields[0] in seen or not SHA256.fullmatch(fields[1]):
            fail("artifact index contains an invalid row")
        seen.add(fields[0])
        path = root / fields[0]
        if sha256_file(path) != fields[1] or path.stat().st_size != int(fields[2]):
            fail(f"sealed artifact changed: {fields[0]}")
    expected_files = {path.name for path in root.iterdir() if path.is_file()} - {"artifacts.tsv"}
    if seen != expected_files:
        fail("artifact index membership differs from the sealed directory")
    workload = manifest.get("workload", {})
    helper_sha = manifest.get("artifacts", {}).get("helper", {}).get("sha256")
    if not SHA256.fullmatch(helper_sha or ""):
        fail("manifest helper identity is invalid")
    identity_keys = {"schema", "run_id", "case_id", "boot_id", "sysname", "release", "version",
                     "machine", "kernel_cmdline", "helper_path", "helper_sha256", "mount_path",
                     "mount_source", "mount_fstype", "mount_options"}
    identity = parse_tsv(root / "guest-identity.tsv", identity_keys, "guest identity")
    if identity != manifest.get("guest"):
        fail("guest identity differs from the sealed manifest")
    execution = parse_tsv(root / "execution.tsv", {"helper_pid", "helper_rc"}, "execution")
    helper_pid = positive_decimal(execution["helper_pid"], "helper PID")
    if execution["helper_rc"] != "0":
        fail("sealed execution did not complete")
    metrics = parse_transcript(root / "helper-transcript", manifest["run_id"], workload["dataset"],
                               workload["file_size"], workload["block_size"])
    with tempfile.TemporaryDirectory() as temporary:
        capture = Path(temporary)
        for source, target in (("trace", "trace"), ("trace-format-sync", "format-sync"),
                               ("trace-format-async", "format-async"),
                               ("trace-probe-definition", "probe-definition")):
            shutil.copyfile(root / source, capture / target)
        trace_clock = read_regular(root / "trace-clock", "trace clock").decode("utf-8", "strict")
        if "[mono]" not in trace_clock:
            fail("sealed trace clock is not mono")
        data_begin_us = metrics.pop("_data_begin_us")
        data_end_us = metrics.pop("_data_end_us")
        trace_summary = parse_trace(capture, manifest["run_id"], manifest["case_id"], helper_pid,
                                    data_begin_us, data_end_us)
    if trace_summary != manifest.get("trace_summary"):
        fail("trace summary differs from replayed raw evidence")
    if not (workload["file_size"] <= trace_summary["requested_bytes"] <= workload["file_size"] * 2):
        fail("replayed trace READ bytes violate the bounded readahead envelope")
    case_result = load_json(root / "case-result.json", "case result")
    if (case_result.get("schema") != RESULT_SCHEMA or case_result.get("runner_version") != "4" or
            case_result.get("status") != "completed" or
            case_result.get("case_id") != manifest["case_id"] or
            case_result.get("workload") != "sequential_read" or case_result.get("mode") != "light" or
            case_result.get("config") != {"source": "linux-tracefs", "negotiated_limits": None} or
            set(case_result.get("result", {})) != {"elapsed_us", "bytes", "ops", "syscalls",
                                                   "short_io", "eintr", "checksum", "read_requests",
                                                   "requested_bytes"} or
            case_result.get("result", {}).get("read_requests") != trace_summary["read_requests"] or
            case_result.get("result", {}).get("requested_bytes") != trace_summary["requested_bytes"]):
        fail("case result is not bound to the replayed helper/trace evidence")
    for key, value in metrics.items():
        if case_result["result"].get(key) != value:
            fail(f"case result {key} differs from the replayed helper transcript")
    for name, label in (("qemu-cmdline", "qemu"), ("virtiofsd-cmdline", "virtiofsd")):
        argv = [part.decode("utf-8", "strict") for part in read_regular(root / name, name).split(b"\0") if part]
        if argv != manifest.get("runtime", {}).get(label, {}).get("argv"):
            fail(f"{label} cmdline differs from the sealed runtime identity")
    for index, worker in enumerate(manifest.get("runtime", {}).get("virtiofsd_workers", [])):
        argv = [part.decode("utf-8", "strict") for part in
                read_regular(root / f"virtiofsd-worker-{index}-cmdline", "virtiofsd worker cmdline").split(b"\0")
                if part]
        if argv != worker.get("argv"):
            fail("virtiofsd worker cmdline differs from the sealed runtime identity")
    qemu_argv = manifest["runtime"]["qemu"]["argv"]
    daemon_argv = manifest["runtime"]["virtiofsd"]["argv"]
    binding = manifest["runtime"].get("binding", {})
    chardev, cache_size = parse_device(qemu_argv, binding.get("tag", ""))
    if chardev != binding.get("chardev") or cache_size != binding.get("dax_cache_size"):
        fail("sealed QEMU argv differs from the non-DAX binding summary")
    qemu_socket = binding.get("socket_path")
    daemon_sockets = []
    for index, value in enumerate(daemon_argv):
        if value == "--socket-path" and index + 1 < len(daemon_argv):
            daemon_sockets.append(daemon_argv[index + 1])
        elif value.startswith("--socket-path="):
            daemon_sockets.append(value.split("=", 1)[1])
    if daemon_sockets != [qemu_socket]:
        fail("sealed virtiofsd argv differs from the socket binding summary")
    for label in ("qemu", "virtiofsd"):
        process = manifest["runtime"].get(label, {})
        if (not SHA256.fullmatch(process.get("executable_sha256", "")) or
                not isinstance(process.get("pid"), int) or process["pid"] <= 0 or
                not isinstance(process.get("start_ticks"), int) or process["start_ticks"] < 0):
            fail(f"sealed {label} executable/PID identity is invalid")
    for worker in manifest["runtime"].get("virtiofsd_workers", []):
        if (not SHA256.fullmatch(worker.get("executable_sha256", "")) or
                not isinstance(worker.get("pid"), int) or worker["pid"] <= 0 or
                not isinstance(worker.get("start_ticks"), int) or worker["start_ticks"] < 0):
            fail("sealed virtiofsd worker identity is invalid")
    for label in ("helper", "kernel", "initramfs"):
        artifact = manifest.get("artifacts", {}).get(label, {})
        if (not SHA256.fullmatch(artifact.get("sha256", "")) or
                not artifact.get("path", "").startswith("/")):
            fail(f"sealed {label} artifact identity is invalid")
    before = load_json(root / "cpu-before.json", "CPU before")
    after = load_json(root / "cpu-after.json", "CPU after")
    delta = load_json(root / "cpu-delta.json", "CPU delta")
    study = load_study_module()
    try:
        recomputed_delta = study.compute_cpu_delta(before, after, metrics["bytes"])
    except SystemExit as error:
        fail(f"CPU evidence is invalid: {error}")
    if delta != recomputed_delta or delta != manifest.get("cpu_delta"):
        fail("CPU delta differs from replayed CPU snapshots")
    print(f"verified_linux_reference_case={root}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    seal = commands.add_parser("pack")
    for name in ("run-id", "case-id", "dataset", "capture-dir", "helper", "kernel", "initramfs",
                 "cpu-before", "cpu-after", "cpu-delta", "output-dir", "expected-release",
                 "expected-qemu-sha256", "expected-virtiofsd-sha256"):
        seal.add_argument("--" + name, required=True)
    seal.add_argument("--file-size", type=int, required=True)
    seal.add_argument("--block-size", type=int, required=True)
    seal.add_argument("--qemu-pid", type=int, required=True)
    seal.add_argument("--virtiofsd-pid", type=int, required=True)
    seal.add_argument("--virtiofsd-worker-pid", type=int, action="append", default=[])
    seal.add_argument("--tag", default="hostshare")
    seal.add_argument("--guest-cache", choices=("cold", "warm"), required=True)
    seal.add_argument("--host-cache", choices=("warm", "unknown"), required=True)
    seal.set_defaults(handler=pack)
    replay = commands.add_parser("verify")
    replay.add_argument("--case-dir", required=True)
    replay.set_defaults(handler=verify)
    return result


def main() -> int:
    try:
        args = parser().parse_args()
        args.handler(args)
        return 0
    except (EvidenceError, OSError, UnicodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
