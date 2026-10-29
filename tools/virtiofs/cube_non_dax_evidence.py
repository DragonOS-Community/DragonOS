#!/usr/bin/env python3
"""Seal and replay a CubeSandbox virtiofs non-DAX case evidence bundle.

The collector deliberately consumes raw artifacts captured on the CubeSandbox
host.  It never invents a missing identity, counter, result, or cleanup fact.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Any


ADAPTER_VERSION = "1"
CAPTURE_SCHEMA = "dragonos.virtiofs.cube-capture.v1"
CASE_SCHEMA = "dragonos.virtiofs.non-dax-case-result.v1"
CONTEXT_SCHEMA = "dragonos.virtiofs.cube-context.v1"
INDEX_SCHEMA = "dragonos.virtiofs.cube-artifacts.v1"
SAFE_TOKEN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
SANDBOX_ID = re.compile(r"[0-9a-f]{32}\Z")

RAW_ARTIFACTS = (
    "capture.json",
    "request.json",
    "multirun.log",
    "sandboxes-before.txt",
    "sandboxes-active.txt",
    "sandboxes-after.txt",
    "kernel.sha256",
    "image.sha256",
    "helper.sha256",
    "request.sha256",
    "shim.cmdline",
    "shim.exe.sha256",
    "backend.log",
    "workload.log",
    "config.txt",
    "cpu-before.json",
    "cpu-after.json",
    "interrupts-before.txt",
    "interrupts-after.txt",
    "softirqs-before.txt",
    "softirqs-after.txt",
    "cubeshim.log",
    "cubevmm.log",
    "cubelet.log",
    "destroy.log",
    "cleanup.json",
)


class EvidenceError(ValueError):
    pass


def fail(message: str) -> None:
    raise EvidenceError(message)


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read JSON {path}: {error}")


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode()


def write_new_json(path: Path, value: Any) -> None:
    with path.open("xb") as stream:
        stream.write(json.dumps(value, sort_keys=True, indent=2,
                                ensure_ascii=False, allow_nan=False).encode())
        stream.write(b"\n")


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def exact_keys(value: Any, keys: set[str], where: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        actual = set(value) if isinstance(value, dict) else set()
        fail(f"{where} keys differ: missing={sorted(keys-actual)}, unexpected={sorted(actual-keys)}")
    return value


def token(value: Any, where: str) -> str:
    if not isinstance(value, str) or SAFE_TOKEN.fullmatch(value) is None:
        fail(f"{where} is not a safe token")
    return value


def positive_int(value: Any, where: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        fail(f"{where} must be a positive integer")
    return value


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        fail(f"cannot read text artifact {path}: {error}")


def parse_sha_file(path: Path, expected_path: str) -> str:
    lines = read_text(path).splitlines()
    if len(lines) != 1:
        fail(f"{path.name} must contain exactly one sha256sum row")
    match = re.fullmatch(r"([0-9a-f]{64})  (/.+)", lines[0])
    if match is None or match.group(2) != expected_path:
        fail(f"{path.name} is not bound to {expected_path}")
    return match.group(1)


def running_ids(path: Path) -> list[str]:
    ids: list[str] = []
    for line in read_text(path).splitlines():
        if not line.strip() or line.startswith("NS "):
            continue
        fields = line.split()
        if len(fields) >= 5 and fields[3] == "sandbox" and fields[4] == "Up":
            if SANDBOX_ID.fullmatch(fields[1]) is None:
                fail(f"invalid running sandbox row in {path.name}")
            ids.append(fields[1])
    return ids


def parse_result(path: Path, capture: dict[str, Any]) -> dict[str, Any]:
    text = read_text(path)
    case_id = capture["case_id"]
    run_id = capture["run_id"]
    begin = f"P0_CASE_BEGIN:{case_id}:run_id={run_id}"
    end = f"P0_CASE_END:{case_id}:run_id={run_id}:rc=0"
    lines = [line.rstrip("\r") for line in text.splitlines()]
    if lines.count(begin) != 1 or lines.count(end) != 1 or lines.index(begin) >= lines.index(end):
        fail("workload transcript lacks one ordered run-bound case marker pair")
    inside = lines[lines.index(begin) + 1:lines.index(end)]
    results = [line for line in inside if line.startswith("result ")]
    if len(results) != 1:
        fail("workload transcript must contain exactly one result inside its markers")
    fields: dict[str, str] = {}
    for item in results[0].split()[1:]:
        if "=" not in item:
            fail("malformed result field")
        key, value = item.split("=", 1)
        if not key or key in fields:
            fail("duplicate or empty result field")
        fields[key] = value
    required = {"workload", "status", "errno", "elapsed_us", "bytes", "ops",
                "syscalls", "short_io", "eintr", "checksum", "dataset", "file_size",
                "block_size", "run_id"}
    if not required.issubset(fields):
        fail(f"result lacks fields: {sorted(required-set(fields))}")
    if (fields["workload"] != capture["workload"] or fields["status"] != "ok" or
            fields["errno"] != "0" or fields["dataset"] != capture["dataset"] or
            fields["run_id"] != run_id or
            fields["file_size"] != str(capture["file_size"]) or
            fields["block_size"] != str(capture["block_size"])):
        fail("result does not match the pre-registered Cube case")
    numeric: dict[str, int] = {}
    for key in ("elapsed_us", "bytes", "ops", "syscalls", "short_io", "eintr"):
        if re.fullmatch(r"[0-9]+", fields[key]) is None:
            fail(f"result {key} is not numeric")
        numeric[key] = int(fields[key])
    if (numeric["elapsed_us"] <= 0 or numeric["bytes"] != capture["file_size"] or
            numeric["ops"] <= 0 or numeric["syscalls"] <= 0):
        fail("result elapsed time/byte count is invalid")
    if re.fullmatch(r"[0-9a-f]{16}", fields["checksum"]) is None:
        fail("result checksum is invalid")
    return {**numeric, "checksum": fields["checksum"],
            "read_requests": None, "requested_bytes": None}


def parse_config(path: Path, capture: dict[str, Any]) -> dict[str, int]:
    lines = [line.rstrip("\r") for line in read_text(path).splitlines()]
    expected = f"P0_CONFIG_RUN:{capture['run_id']}:{capture['case_id']}"
    if not lines or lines[0] != expected:
        fail("config snapshot is not bound to this run/case")
    section = ""
    values: dict[tuple[str, str], int] = {}
    for line in lines[1:]:
        if re.fullmatch(r"\[[^]]+\]", line):
            section = line[1:-1]
            continue
        fields = line.split()
        if len(fields) == 2 and fields[1].isdigit():
            key = (section, fields[0])
            if key in values:
                fail("duplicate config counter")
            values[key] = int(fields[1])
    wanted = {
        "init_epoch": ("fuse", "init_epoch"),
        "negotiated_max_read_bytes": ("fuse", "negotiated_max_read_bytes"),
        "negotiated_max_pages": ("fuse", "negotiated_max_pages"),
        "negotiated_max_readahead_bytes": ("fuse", "negotiated_max_readahead_bytes"),
        "negotiated_async_read": ("fuse", "negotiated_async_read"),
        "sg_limit_pages_configured": ("virtiofs", "sg_limit_pages_configured"),
        "effective_read_payload_limit_bytes": ("fuse", "effective_read_payload_limit_bytes"),
    }
    if any(source not in values for source in wanted.values()):
        fail("config snapshot lacks a negotiated non-DAX limit")
    result = {name: values[source] for name, source in wanted.items()}
    if (result["init_epoch"] != 1 or result["negotiated_max_read_bytes"] <= 0 or
            result["negotiated_max_pages"] <= 0 or result["sg_limit_pages_configured"] <= 0 or
            result["negotiated_async_read"] not in (0, 1)):
        fail("negotiated config values are invalid")
    expected_limit = min(result["negotiated_max_read_bytes"],
                         result["negotiated_max_pages"] * 4096,
                         result["sg_limit_pages_configured"] * 4096)
    if result["effective_read_payload_limit_bytes"] != expected_limit:
        fail("effective payload differs from negotiated and SG limits")
    return result


def validate_cpu(path: Path, capture: dict[str, Any]) -> dict[str, Any]:
    value = exact_keys(read_json(path), {"schema", "host_boot_id", "sandbox_id", "pid",
                                         "start_ticks", "clock_ticks_per_second",
                                         "user_ticks", "system_ticks", "voluntary_ctxt_switches",
                                         "nonvoluntary_ctxt_switches"}, path.name)
    if (value["schema"] != "dragonos.virtiofs.cube-cpu-snapshot.v1" or
            value["sandbox_id"] != capture["sandbox_id"] or
            not isinstance(value["host_boot_id"], str) or not value["host_boot_id"]):
        fail(f"{path.name} identity is invalid")
    for key in ("pid", "start_ticks", "clock_ticks_per_second", "user_ticks", "system_ticks",
                "voluntary_ctxt_switches", "nonvoluntary_ctxt_switches"):
        positive_int(value[key], f"{path.name}.{key}") if key in ("pid", "start_ticks", "clock_ticks_per_second") else None
        if isinstance(value[key], bool) or not isinstance(value[key], int) or value[key] < 0:
            fail(f"{path.name}.{key} is invalid")
    return value


def parse_proc_counter_table(path: Path) -> tuple[list[str], dict[str, tuple[int, ...]]]:
    lines = [line for line in read_text(path).splitlines() if line.strip()]
    if not lines:
        fail(f"{path.name} is empty")
    cpus = lines[0].split()
    if not cpus or any(re.fullmatch(r"CPU[0-9]+", item) is None for item in cpus):
        fail(f"{path.name} lacks a /proc counter header")
    rows: dict[str, tuple[int, ...]] = {}
    for line in lines[1:]:
        fields = line.split()
        if len(fields) < len(cpus) + 1 or not fields[0].endswith(":"):
            continue
        counters = fields[1:len(cpus) + 1]
        if any(not item.isdigit() for item in counters):
            continue
        key = fields[0][:-1]
        if not key or key in rows:
            fail(f"{path.name} has a duplicate counter row")
        rows[key] = tuple(int(item) for item in counters)
    if not rows:
        fail(f"{path.name} has no parseable counters")
    return cpus, rows


def validate_counter_pair(before_path: Path, after_path: Path) -> None:
    before_cpus, before = parse_proc_counter_table(before_path)
    after_cpus, after = parse_proc_counter_table(after_path)
    if before_cpus != after_cpus or set(before) != set(after):
        fail(f"{before_path.name}/{after_path.name} topology changed")
    for key in before:
        if any(new < old for old, new in zip(before[key], after[key])):
            fail(f"counter moved backwards in {after_path.name}: {key}")


def validate_capture(root: Path) -> tuple[dict[str, Any], dict[str, Any], dict[str, int], dict[str, Any]]:
    for name in RAW_ARTIFACTS:
        path = root / name
        if not path.is_file() or path.is_symlink():
            fail(f"missing regular raw artifact: {name}")
    capture = exact_keys(read_json(root / "capture.json"), {
        "schema", "adapter_version", "case_id", "run_id", "status", "sandbox_id",
        "phase", "mode", "workload", "dataset", "file_size", "block_size",
        "guest_cache", "host_cache", "kernel_path", "image_path", "helper_guest_path",
        "request_path", "shim_pid", "shim_start_ticks"}, "capture")
    if capture["schema"] != CAPTURE_SCHEMA or capture["adapter_version"] != ADAPTER_VERSION:
        fail("capture schema/version is incompatible")
    for key in ("case_id", "run_id", "dataset"):
        token(capture[key], key)
    if (capture["status"] != "completed" or capture["phase"] != "read" or
            capture["workload"] != "sequential_read" or capture["mode"] != "performance" or
            capture["guest_cache"] not in ("cold", "warm") or
            capture["host_cache"] not in ("warm", "unknown") or
            SANDBOX_ID.fullmatch(capture["sandbox_id"]) is None):
        fail("capture case classification is invalid")
    positive_int(capture["file_size"], "file_size")
    positive_int(capture["block_size"], "block_size")
    positive_int(capture["shim_pid"], "shim_pid")
    positive_int(capture["shim_start_ticks"], "shim_start_ticks")
    for key in ("kernel_path", "image_path", "helper_guest_path", "request_path"):
        if not isinstance(capture[key], str) or not capture[key].startswith("/"):
            fail(f"{key} must be an absolute remote path")

    request_sha = parse_sha_file(root / "request.sha256", capture["request_path"])
    if digest(root / "request.json") != request_sha:
        fail("request JSON differs from its remote identity hash")
    request = read_json(root / "request.json")
    if not isinstance(request, dict):
        fail("request JSON is not an object")
    kernel_sha = parse_sha_file(root / "kernel.sha256", capture["kernel_path"])
    image_sha = parse_sha_file(root / "image.sha256", capture["image_path"])
    helper_sha = parse_sha_file(root / "helper.sha256", capture["helper_guest_path"])

    sandbox = capture["sandbox_id"]
    if running_ids(root / "sandboxes-before.txt") != []:
        fail("Cube host was not single-instance isolated before launch")
    if running_ids(root / "sandboxes-active.txt") != [sandbox]:
        fail("active sandbox snapshot is not exactly the target")
    if running_ids(root / "sandboxes-after.txt") != []:
        fail("Cube host still has a running sandbox after cleanup")
    multirun = read_text(root / "multirun.log")
    if len(re.findall(r"sandBoxId:([0-9a-f]{32})", multirun)) != 1 or f"sandBoxId:{sandbox}" not in multirun:
        fail("multirun output does not uniquely identify the target")
    if "totalRunSuccCnt:1" not in multirun or "totalRunErr:0" not in multirun:
        fail("multirun did not report one successful isolated launch")

    cmdline = (root / "shim.cmdline").read_bytes()
    if not cmdline.endswith(b"\0") or b"\0-id\0" + sandbox.encode() + b"\0" not in cmdline:
        fail("shim argv is not NUL-terminated or bound to the sandbox")
    shim_sha_lines = read_text(root / "shim.exe.sha256").splitlines()
    shim_sha_match = re.fullmatch(r"([0-9a-f]{64})  (/.+)", shim_sha_lines[0]) if len(shim_sha_lines) == 1 else None
    if shim_sha_match is None:
        fail("shim executable identity is invalid")
    backend = read_text(root / "backend.log").splitlines()
    backend_rows = [line for line in backend if "Creating virtio-fs device: FsConfig" in line]
    if len(backend_rows) != 1 or not backend_rows[0].startswith(sandbox + " "):
        fail("backend log lacks one sandbox-bound FsConfig")
    lowered = backend_rows[0].lower()
    if "backendfs_config: some(" not in lowered or "cache:" not in lowered or "dax" in lowered or "window_size" in lowered:
        fail("backend FsConfig does not prove the integrated non-DAX backend")

    result = parse_result(root / "workload.log", capture)
    config = parse_config(root / "config.txt", capture)
    before = validate_cpu(root / "cpu-before.json", capture)
    after = validate_cpu(root / "cpu-after.json", capture)
    for key in ("host_boot_id", "sandbox_id", "pid", "start_ticks", "clock_ticks_per_second"):
        if before[key] != after[key]:
            fail("CPU snapshots do not describe one process lifetime")
    if before["pid"] != capture["shim_pid"] or before["start_ticks"] != capture["shim_start_ticks"]:
        fail("CPU snapshots differ from the capture process identity")
    for key in ("user_ticks", "system_ticks", "voluntary_ctxt_switches", "nonvoluntary_ctxt_switches"):
        if after[key] < before[key]:
            fail("CPU/context-switch counters moved backwards")
    validate_counter_pair(root / "interrupts-before.txt", root / "interrupts-after.txt")
    validate_counter_pair(root / "softirqs-before.txt", root / "softirqs-after.txt")
    for name in ("cubeshim.log", "cubevmm.log", "cubelet.log"):
        rows = read_text(root / name).splitlines()
        if not rows or any(sandbox not in row for row in rows):
            fail(f"{name} is empty or contains another instance")
    destroy = read_text(root / "destroy.log")
    if f"destroy sandbox: {sandbox}" not in destroy or "ret_code:Success" not in destroy:
        fail("destroy log lacks the target and successful response")
    cleanup = exact_keys(read_json(root / "cleanup.json"),
                         {"schema", "sandbox_id", "destroy_rc", "target_absent",
                          "shim_pid_absent"}, "cleanup")
    if (cleanup["schema"] != "dragonos.virtiofs.cube-cleanup.v1" or
            cleanup["sandbox_id"] != sandbox or cleanup["destroy_rc"] != 0 or
            cleanup["target_absent"] is not True or cleanup["shim_pid_absent"] is not True):
        fail("cleanup artifact does not prove successful destroy and disappearance")

    context = {"schema": CONTEXT_SCHEMA, "adapter_version": ADAPTER_VERSION,
               "sandbox_id": sandbox, "shim_pid": capture["shim_pid"],
               "shim_start_ticks": capture["shim_start_ticks"],
               "kernel_sha256": kernel_sha, "image_sha256": image_sha,
               "helper_sha256": helper_sha, "request_sha256": request_sha,
               "shim_executable_sha256": shim_sha_match.group(1),
               "shim_cmdline_sha256": digest(root / "shim.cmdline"),
               "backend_kind": "cube-integrated-virtiofs", "non_dax": True}
    return capture, result, config, context


def artifact_index(root: Path, names: tuple[str, ...]) -> dict[str, Any]:
    return {"schema": INDEX_SCHEMA, "adapter_version": ADAPTER_VERSION,
            "artifacts": [{"name": name, "sha256": digest(root / name),
                           "size": (root / name).stat().st_size} for name in names]}


def proc_stat_fields(path: Path) -> list[str]:
    try:
        line = path.read_text(encoding="ascii")
    except OSError as error:
        fail(f"cannot read {path}: {error}")
    close = line.rfind(")")
    if close < 0:
        fail(f"malformed proc stat: {path}")
    fields = line[close + 2:].split()
    if len(fields) < 20:
        fail(f"short proc stat: {path}")
    return fields


def cpu_snapshot(args: argparse.Namespace) -> None:
    if SANDBOX_ID.fullmatch(args.sandbox_id) is None:
        fail("sandbox ID must be 32 lowercase hex characters")
    pid = positive_int(args.pid, "pid")
    process = Path("/proc") / str(pid)
    process_fields = proc_stat_fields(process / "stat")
    start_ticks = int(process_fields[19])
    user_ticks = system_ticks = voluntary = nonvoluntary = 0
    tasks = list((process / "task").iterdir())
    if not tasks:
        fail("target process has no visible threads")
    for task in tasks:
        fields = proc_stat_fields(task / "stat")
        user_ticks += int(fields[11])
        system_ticks += int(fields[12])
        status = (task / "status").read_text(encoding="ascii")
        values: dict[str, int] = {}
        for line in status.splitlines():
            if line.startswith("voluntary_ctxt_switches:"):
                values["voluntary"] = int(line.split(":", 1)[1])
            elif line.startswith("nonvoluntary_ctxt_switches:"):
                values["nonvoluntary"] = int(line.split(":", 1)[1])
        if set(values) != {"voluntary", "nonvoluntary"}:
            fail(f"thread {task.name} lacks context-switch counters")
        voluntary += values["voluntary"]
        nonvoluntary += values["nonvoluntary"]
    boot_id = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").strip()
    value = {"schema": "dragonos.virtiofs.cube-cpu-snapshot.v1", "host_boot_id": boot_id,
             "sandbox_id": args.sandbox_id, "pid": pid, "start_ticks": start_ticks,
             "clock_ticks_per_second": os.sysconf("SC_CLK_TCK"), "user_ticks": user_ticks,
             "system_ticks": system_ticks, "voluntary_ctxt_switches": voluntary,
             "nonvoluntary_ctxt_switches": nonvoluntary}
    write_new_json(Path(args.output), value)


def seal(args: argparse.Namespace) -> None:
    source = Path(args.input).resolve()
    output = Path(args.output).resolve()
    if output.exists():
        fail(f"refusing to replace existing output: {output}")
    if source == output or source in output.parents:
        fail("output must not be inside the raw capture directory")
    fingerprints = {name: ((source / name).stat().st_dev, (source / name).stat().st_ino,
                           (source / name).stat().st_size, (source / name).stat().st_mtime_ns,
                           digest(source / name)) for name in RAW_ARTIFACTS}
    validate_capture(source)
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        for name in RAW_ARTIFACTS:
            shutil.copyfile(source / name, staging / name)
        for name in RAW_ARTIFACTS:
            stat = (source / name).stat()
            current = (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns,
                       digest(source / name))
            if current != fingerprints[name] or digest(staging / name) != fingerprints[name][4]:
                fail(f"raw artifact changed while sealing: {name}")
        capture, result, config, context = validate_capture(staging)
        case_result = {"schema": CASE_SCHEMA, "runner_version": f"cube-{ADAPTER_VERSION}",
                       "status": "completed", "case_id": capture["case_id"],
                       "workload": capture["workload"], "mode": capture["mode"],
                       "result": result, "config": config}
        write_new_json(staging / "case-result.json", case_result)
        write_new_json(staging / "collector_context.json", context)
        indexed_names = RAW_ARTIFACTS + ("case-result.json", "collector_context.json")
        write_new_json(staging / "artifacts.json", artifact_index(staging, indexed_names))
        os.chmod(staging, 0o500)
        for member in staging.iterdir():
            os.chmod(member, 0o400)
        staging.rename(output)
    except Exception:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    print(f"sealed={output}")


def verify(args: argparse.Namespace) -> None:
    root = Path(args.case_dir).resolve()
    if not root.is_dir() or root.is_symlink():
        fail("case directory is missing or is a symlink")
    expected_members = set(RAW_ARTIFACTS) | {"case-result.json", "collector_context.json", "artifacts.json"}
    actual_members = {item.name for item in root.iterdir()}
    if actual_members != expected_members or any(not item.is_file() or item.is_symlink() for item in root.iterdir()):
        fail("sealed case members differ from the fixed schema")
    capture, result, config, context = validate_capture(root)
    index = exact_keys(read_json(root / "artifacts.json"),
                       {"schema", "adapter_version", "artifacts"}, "artifact index")
    if index["schema"] != INDEX_SCHEMA or index["adapter_version"] != ADAPTER_VERSION:
        fail("artifact index schema/version is incompatible")
    names = RAW_ARTIFACTS + ("case-result.json", "collector_context.json")
    if not isinstance(index["artifacts"], list) or len(index["artifacts"]) != len(names):
        fail("artifact index cardinality differs")
    for row, name in zip(index["artifacts"], names):
        exact_keys(row, {"name", "sha256", "size"}, "artifact index row")
        if (row["name"] != name or SHA256.fullmatch(row["sha256"]) is None or
                row["sha256"] != digest(root / name) or row["size"] != (root / name).stat().st_size):
            fail(f"sealed artifact changed: {name}")
    expected_result = {"schema": CASE_SCHEMA, "runner_version": f"cube-{ADAPTER_VERSION}",
                       "status": "completed", "case_id": capture["case_id"],
                       "workload": capture["workload"], "mode": capture["mode"],
                       "result": result, "config": config}
    if canonical_bytes(read_json(root / "case-result.json")) != canonical_bytes(expected_result):
        fail("case-result.json differs from replayed raw evidence")
    if canonical_bytes(read_json(root / "collector_context.json")) != canonical_bytes(context):
        fail("collector context differs from replayed identities")
    print(f"verified={root}")


def main() -> int:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    seal_parser = commands.add_parser("seal")
    seal_parser.add_argument("--input", required=True)
    seal_parser.add_argument("--output", required=True)
    seal_parser.set_defaults(handler=seal)
    verify_parser = commands.add_parser("verify")
    verify_parser.add_argument("--case-dir", required=True)
    verify_parser.set_defaults(handler=verify)
    cpu_parser = commands.add_parser("cpu-snapshot")
    cpu_parser.add_argument("--pid", required=True, type=int)
    cpu_parser.add_argument("--sandbox-id", required=True)
    cpu_parser.add_argument("--output", required=True)
    cpu_parser.set_defaults(handler=cpu_snapshot)
    args = parser.parse_args()
    try:
        args.handler(args)
    except EvidenceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
