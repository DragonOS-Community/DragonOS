#!/usr/bin/env python3
"""Strict A1/B/A2 study and CPU accounting for virtiofs non-DAX evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Iterable, Sequence


PLAN_SCHEMA = "dragonos.virtiofs.non-dax-study-plan.v3"
INDEX_SCHEMA = "dragonos.virtiofs.non-dax-study-results.v1"
CASE_SCHEMA = "dragonos.virtiofs.non-dax-study-case.v1"
ACCEPTANCE_SCHEMA = "dragonos.virtiofs.non-dax-acceptance.v1"
CPU_SCHEMA = "dragonos.virtiofs.non-dax-cpu-snapshot.v2"
CPU_DELTA_SCHEMA = "dragonos.virtiofs.non-dax-cpu-delta.v2"
STRATA = ("A1", "B", "A2")
STUDY_KINDS = ("candidate-effect", "mode-overhead")
MAX_SAMPLES_PER_STRATUM = 10_000
MAX_TOTAL_SAMPLES = 3 * MAX_SAMPLES_PER_STRATUM
MAX_BOOTSTRAP_ITERATIONS = 1_000_000
WORKLOAD_PATTERN = re.compile(r"read-f[1-9][0-9]*-b[1-9][0-9]*\Z")
CACHE_TUPLES = ("guest-cold-host-warm", "guest-cold-host-unknown")


class StudyError(ValueError):
    pass


def fail(message: str) -> None:
    raise StudyError(message)


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def canonical_sha256(value: Any) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def read_json(path: Path) -> Any:
    try:
        with path.open("r", encoding="utf-8") as stream:
            value = json.load(stream)
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read JSON {path}: {error}")
    return value


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    data = json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2,
                      allow_nan=False) + "\n"
    try:
        with path.open("x", encoding="utf-8") as stream:
            stream.write(data)
    except FileExistsError:
        fail(f"refusing to replace existing output: {path}")
    except OSError as error:
        fail(f"cannot write {path}: {error}")


def require_exact_keys(value: dict[str, Any], keys: set[str], where: str) -> None:
    actual = set(value)
    if actual != keys:
        fail(f"{where} keys differ: missing={sorted(keys - actual)}, "
             f"unexpected={sorted(actual - keys)}")


def require_int(value: Any, where: str, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{where} must be an integer >= {minimum}")
    return value


def require_number(value: Any, where: str, *, positive: bool = False) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        fail(f"{where} must be numeric")
    result = float(value)
    if not math.isfinite(result) or (positive and result <= 0) or (not positive and result < 0):
        fail(f"{where} is outside its valid range")
    return result


def require_token(value: Any, where: str) -> str:
    if (not isinstance(value, str) or not value or len(value) > 128 or
            any(character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for character in value)):
        fail(f"{where} is not a safe non-empty token")
    return value


def require_workload(value: Any, where: str) -> str:
    workload = require_token(value, where)
    if WORKLOAD_PATTERN.fullmatch(workload) is None:
        fail(f"{where} must match read-f<positive integer>-b<positive integer>")
    return workload


def require_cache_tuple(value: Any, where: str) -> str:
    cache = require_token(value, where)
    if cache not in CACHE_TUPLES:
        fail(f"{where} must be guest-cold-host-warm or guest-cold-host-unknown")
    return cache


def require_bounded_int(value: Any, where: str, minimum: int, maximum: int) -> int:
    result = require_int(value, where, minimum)
    if result > maximum:
        fail(f"{where} must be <= {maximum}")
    return result


def require_revision(value: Any, where: str) -> str:
    revision = require_token(value, where)
    if len(revision) not in (40, 64) or any(c not in "0123456789abcdef" for c in revision):
        fail(f"{where} must be a full 40- or 64-character lowercase hexadecimal revision")
    return revision


def require_sha256(value: Any, where: str) -> str:
    if (not isinstance(value, str) or len(value) != 64 or
            any(character not in "0123456789abcdef" for character in value)):
        fail(f"{where} must be a lowercase SHA256")
    return value


def percentile(values: Sequence[float], fraction: float) -> float:
    if not values:
        fail("cannot calculate a percentile of no samples")
    ordered = sorted(values)
    position = fraction * (len(ordered) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def median(values: Iterable[float]) -> float:
    materialized = list(values)
    if not materialized:
        fail("cannot calculate a median of no samples")
    return float(statistics.median(materialized))


def relative_improvement(baseline: float, candidate: float) -> float:
    if baseline <= 0:
        fail("baseline metric must be positive")
    return 1.0 - candidate / baseline


def plan_command(args: argparse.Namespace) -> None:
    baseline = require_revision(args.baseline_revision, "baseline revision")
    candidate = require_revision(args.candidate_revision, "candidate revision")
    kind = args.study_kind
    if kind not in STUDY_KINDS:
        fail("study kind is invalid")
    if kind == "candidate-effect" and baseline == candidate:
        fail("candidate-effect revisions must differ")
    if kind == "mode-overhead" and baseline != candidate:
        fail("mode-overhead must use one revision")
    baseline_build = require_sha256(args.baseline_build_manifest_sha256,
                                    "baseline build manifest SHA256")
    candidate_build = require_sha256(args.candidate_build_manifest_sha256,
                                     "candidate build manifest SHA256")
    if kind == "mode-overhead" and baseline_build != candidate_build:
        fail("mode-overhead must use one build artifact set")
    shared_runtime = {
        "qemu_sha256": require_sha256(args.qemu_sha256, "QEMU SHA256"),
        "virtiofsd_sha256": require_sha256(args.virtiofsd_sha256, "virtiofsd SHA256"),
        "helper_sha256": require_sha256(args.helper_sha256, "guest helper SHA256"),
    }
    count = require_bounded_int(args.samples_per_stratum, "samples per stratum", 9,
                                MAX_SAMPLES_PER_STRATUM)
    seed = require_int(args.seed, "study seed", 0)
    measured_mode = args.mode
    if measured_mode not in ("performance", "light", "diagnostic"):
        fail("measured mode is invalid")
    if kind == "mode-overhead" and measured_mode == "performance":
        fail("mode-overhead B must enable light or diagnostic observation")
    if not args.workload or not args.cache:
        fail("study plan requires at least one exact workload and cache tuple")
    workload_inputs = args.workload
    cache_inputs = args.cache
    workloads = [require_workload(item, "workload") for item in workload_inputs]
    caches = [require_cache_tuple(item, "cache") for item in cache_inputs]
    if len(set(workloads)) != len(workloads) or len(set(caches)) != len(caches):
        fail("workload/cache values must be unique")
    cells = [(workload, cache) for workload in workloads for cache in caches]
    if count < len(cells):
        fail("samples per stratum must cover every workload/cache cell")

    rng = random.Random(seed)
    samples: list[dict[str, Any]] = []
    order = 0
    for stratum in STRATA:
        assignments = [cells[index % len(cells)] for index in range(count)]
        rng.shuffle(assignments)
        use_candidate = kind == "candidate-effect" and stratum == "B"
        revision = candidate if use_candidate else baseline
        build_manifest = candidate_build if use_candidate else baseline_build
        mode = measured_mode if kind == "candidate-effect" or stratum == "B" else "performance"
        for index, (workload, cache) in enumerate(assignments, start=1):
            order += 1
            samples.append({
                "revision": revision,
                "build_manifest_sha256": build_manifest,
                "mode": mode,
                "stratum": stratum,
                "sample_id": f"{stratum}-{index:03d}",
                "seed": rng.getrandbits(63),
                "order": order,
                "workload": workload,
                "cache": cache,
            })
    plan = {
        "schema": PLAN_SCHEMA,
        "study_kind": kind,
        "artifact_sets": {
            "baseline": {"revision": baseline, "build_manifest_sha256": baseline_build},
            "candidate": {"revision": candidate, "build_manifest_sha256": candidate_build},
        },
        "shared_runtime": shared_runtime,
        "measured_mode": measured_mode,
        "cpu_max_window_seconds": require_int(args.cpu_max_window_seconds,
                                               "CPU maximum window seconds", 1),
        "study_seed": seed,
        "bootstrap_iterations": require_bounded_int(
            args.bootstrap_iterations, "bootstrap iterations", 1000,
            MAX_BOOTSTRAP_ITERATIONS),
        "samples_per_stratum": count,
        "samples": samples,
    }
    write_json(Path(args.output), plan)


def validate_plan(plan: Any) -> list[dict[str, Any]]:
    if not isinstance(plan, dict):
        fail("study plan must be an object")
    require_exact_keys(plan, {"schema", "study_kind", "artifact_sets", "shared_runtime",
                              "measured_mode", "cpu_max_window_seconds", "study_seed",
                              "bootstrap_iterations", "samples_per_stratum", "samples"}, "study plan")
    if plan["schema"] != PLAN_SCHEMA:
        fail("study plan schema is incompatible")
    kind = plan["study_kind"]
    if kind not in STUDY_KINDS:
        fail("study kind is invalid")
    if not isinstance(plan["artifact_sets"], dict):
        fail("artifact sets must be an object")
    require_exact_keys(plan["artifact_sets"], {"baseline", "candidate"}, "artifact sets")
    artifact_sets: dict[str, dict[str, str]] = {}
    for name in ("baseline", "candidate"):
        value = plan["artifact_sets"][name]
        if not isinstance(value, dict):
            fail(f"{name} artifact set must be an object")
        require_exact_keys(value, {"revision", "build_manifest_sha256"}, f"{name} artifact set")
        artifact_sets[name] = {
            "revision": require_revision(value["revision"], f"{name} revision"),
            "build_manifest_sha256": require_sha256(value["build_manifest_sha256"],
                                                     f"{name} build manifest SHA256"),
        }
    baseline, candidate = artifact_sets["baseline"], artifact_sets["candidate"]
    if kind == "candidate-effect" and baseline["revision"] == candidate["revision"]:
        fail("candidate-effect revisions must differ")
    if kind == "mode-overhead" and baseline != candidate:
        fail("mode-overhead must use one revision and build artifact set")
    if not isinstance(plan["shared_runtime"], dict):
        fail("shared runtime must be an object")
    require_exact_keys(plan["shared_runtime"], {"qemu_sha256", "virtiofsd_sha256", "helper_sha256"},
                       "shared runtime")
    for key, value in plan["shared_runtime"].items():
        require_sha256(value, f"shared runtime {key}")
    if plan["measured_mode"] not in ("performance", "light", "diagnostic"):
        fail("measured mode is invalid")
    if kind == "mode-overhead" and plan["measured_mode"] == "performance":
        fail("mode-overhead B must enable observation")
    require_int(plan["cpu_max_window_seconds"], "CPU maximum window seconds", 1)
    require_int(plan["study_seed"], "study seed", 0)
    require_bounded_int(plan["bootstrap_iterations"], "bootstrap iterations", 1000,
                        MAX_BOOTSTRAP_ITERATIONS)
    count = require_bounded_int(plan["samples_per_stratum"], "samples per stratum", 9,
                                MAX_SAMPLES_PER_STRATUM)
    if not isinstance(plan["samples"], list):
        fail("plan samples must be an array")
    if len(plan["samples"]) > MAX_TOTAL_SAMPLES:
        fail(f"plan must contain no more than {MAX_TOTAL_SAMPLES} total samples")
    expected_keys = {"revision", "build_manifest_sha256", "mode", "stratum", "sample_id",
                     "seed", "order", "workload", "cache"}
    samples: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    seen_orders: set[int] = set()
    counts = {stratum: 0 for stratum in STRATA}
    cells = {stratum: set() for stratum in STRATA}
    for index, raw in enumerate(plan["samples"]):
        if not isinstance(raw, dict):
            fail(f"plan sample {index} is not an object")
        require_exact_keys(raw, expected_keys, f"plan sample {index}")
        stratum = raw["stratum"]
        if stratum not in STRATA:
            fail(f"plan sample {index} has invalid stratum")
        sample_id = require_token(raw["sample_id"], f"plan sample {index} id")
        if sample_id in seen_ids:
            fail(f"duplicate sample id: {sample_id}")
        seen_ids.add(sample_id)
        order = require_int(raw["order"], f"sample {sample_id} order", 1)
        if order in seen_orders:
            fail(f"duplicate sample order: {order}")
        seen_orders.add(order)
        require_int(raw["seed"], f"sample {sample_id} seed", 0)
        workload = require_workload(raw["workload"], f"sample {sample_id} workload")
        cache = require_cache_tuple(raw["cache"], f"sample {sample_id} cache")
        use_candidate = kind == "candidate-effect" and stratum == "B"
        expected_artifact = candidate if use_candidate else baseline
        if (raw["revision"] != expected_artifact["revision"] or
                raw["build_manifest_sha256"] != expected_artifact["build_manifest_sha256"]):
            fail(f"sample {sample_id} has the wrong artifact set for {stratum}")
        expected_mode = plan["measured_mode"] if kind == "candidate-effect" or stratum == "B" else "performance"
        if raw["mode"] != expected_mode:
            fail(f"sample {sample_id} has the wrong mode for {stratum}")
        counts[stratum] += 1
        cells[stratum].add((workload, cache))
        samples.append(raw)
    if any(value != count for value in counts.values()):
        fail(f"each A1/B/A2 stratum must contain exactly {count} samples; got {counts}")
    if seen_orders != set(range(1, len(samples) + 1)):
        fail("sample order must be a complete 1..N sequence")
    if not (cells["A1"] == cells["B"] == cells["A2"]):
        fail("workload/cache cells differ across A1/B/A2")
    return sorted(samples, key=lambda item: item["order"])


def safe_reference(base: Path, reference: Any) -> Path:
    if not isinstance(reference, str) or not reference or os.path.isabs(reference):
        fail("case_result references must be non-empty relative paths")
    base_real = base.resolve()
    unresolved = base / reference
    current = unresolved
    while current != base:
        if current.is_symlink():
            fail(f"case_result reference must not traverse a symlink: {reference}")
        current = current.parent
    target = unresolved.resolve()
    try:
        common = os.path.commonpath((str(base_real), str(target)))
    except ValueError:
        fail(f"case_result reference escapes its index directory: {reference}")
    if common != str(base_real) or not target.is_file():
        fail(f"case_result reference is missing or escapes its index directory: {reference}")
    return target


def load_results(plan: dict[str, Any], samples: list[dict[str, Any]], index_path: Path) -> list[dict[str, Any]]:
    index = read_json(index_path)
    if not isinstance(index, dict):
        fail("results index must be an object")
    require_exact_keys(index, {"schema", "plan_sha256", "results"}, "results index")
    if index["schema"] != INDEX_SCHEMA:
        fail("results index schema is incompatible")
    plan_hash = canonical_sha256(plan)
    if index["plan_sha256"] != plan_hash:
        fail("results index refers to a different study plan")
    if not isinstance(index["results"], list):
        fail("results index results must be an array")
    indexed: dict[str, dict[str, Any]] = {}
    for row_number, row in enumerate(index["results"]):
        if not isinstance(row, dict):
            fail(f"result index row {row_number} is not an object")
        require_exact_keys(row, {"sample_id", "case_result", "summary_sha256"},
                           f"result index row {row_number}")
        sample_id = require_token(row["sample_id"], f"result index row {row_number} sample id")
        if sample_id in indexed:
            fail(f"duplicate indexed result for {sample_id}")
        if (not isinstance(row["summary_sha256"], str) or len(row["summary_sha256"]) != 64 or
                any(c not in "0123456789abcdef" for c in row["summary_sha256"])):
            fail(f"result {sample_id} has an invalid summary hash")
        indexed[sample_id] = row
    planned_ids = {sample["sample_id"] for sample in samples}
    if set(indexed) != planned_ids:
        fail(f"result set differs from plan: missing={sorted(planned_ids - set(indexed))}, "
             f"unexpected={sorted(set(indexed) - planned_ids)}")

    result_keys = {"schema", "status", "revision", "build_manifest_sha256", "mode", "stratum",
                   "sample_id", "seed", "order", "workload", "cache", "summary", "summary_sha256"}
    summary_keys = {"data_loop_elapsed_us", "bytes", "read_requests", "cpu_seconds_per_mib"}
    observations: list[dict[str, Any]] = []
    for sample in samples:
        sample_id = sample["sample_id"]
        row = indexed[sample_id]
        result = read_json(safe_reference(index_path.parent, row["case_result"]))
        if not isinstance(result, dict):
            fail(f"case result {sample_id} is not an object")
        require_exact_keys(result, result_keys, f"case result {sample_id}")
        if result["schema"] != CASE_SCHEMA or result["status"] != "completed":
            fail(f"case result {sample_id} is incompatible or non-completed")
        for key in ("revision", "build_manifest_sha256", "mode", "stratum", "sample_id",
                    "seed", "order", "workload", "cache"):
            if result[key] != sample[key]:
                fail(f"case result {sample_id} has wrong pre-registered field: {key}")
        if not isinstance(result["summary"], dict):
            fail(f"case result {sample_id} summary is not an object")
        require_exact_keys(result["summary"], summary_keys, f"case result {sample_id} summary")
        summary = result["summary"]
        require_number(summary["data_loop_elapsed_us"], f"{sample_id} elapsed_us", positive=True)
        require_int(summary["bytes"], f"{sample_id} bytes", 1)
        if sample["mode"] == "performance":
            if summary["read_requests"] is not None:
                fail(f"{sample_id} performance sample must not claim READ evidence")
        else:
            require_int(summary["read_requests"], f"{sample_id} read_requests", 1)
        require_number(summary["cpu_seconds_per_mib"], f"{sample_id} CPU seconds/MiB", positive=True)
        actual_hash = canonical_sha256(summary)
        if result["summary_sha256"] != actual_hash or row["summary_sha256"] != actual_hash:
            fail(f"case result {sample_id} summary hash does not match its canonical summary")
        observations.append({**sample, **summary, "summary_sha256": actual_hash})
    return observations


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            while block := stream.read(1024 * 1024):
                digest.update(block)
    except OSError as error:
        fail(f"cannot hash {path}: {error}")
    return digest.hexdigest()


def verify_sealed_runner_sources(run_dir: Path) -> None:
    """Reject indirection and compare both executable shell sources byte-for-byte."""
    trusted_dir = Path(__file__).parent
    for sealed_name, trusted_name in (("runner.sh", "non_dax_bench_runner.sh"),
                                      ("common.sh", "common.sh")):
        sealed = run_dir / sealed_name
        trusted = trusted_dir / trusted_name
        if (sealed.is_symlink() or not sealed.is_file() or trusted.is_symlink() or
                not trusted.is_file()):
            fail(f"sealed or trusted {sealed_name} is missing, non-regular, or a symlink")
        try:
            if sealed.read_bytes() != trusted.read_bytes():
                fail(f"sealed {sealed_name} is not byte-identical to its trusted implementation")
        except OSError as error:
            fail(f"cannot compare sealed {sealed_name} with its trusted implementation: {error}")


def sealed_artifacts(case_dir: Path) -> dict[str, Path]:
    index_path = case_dir / "artifacts.tsv"
    try:
        rows = index_path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        fail(f"cannot read sealed artifact index {index_path}: {error}")
    artifacts: dict[str, Path] = {}
    for row_number, row in enumerate(rows, start=1):
        fields = row.split("\t")
        if len(fields) != 3:
            fail(f"invalid artifacts.tsv row {row_number}")
        name, digest, size_text = fields
        require_token(name, f"artifact row {row_number} name")
        if name in artifacts:
            fail(f"duplicate sealed artifact: {name}")
        if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
            fail(f"invalid digest for sealed artifact {name}")
        size = require_int(int(size_text) if size_text.isdigit() else None,
                           f"sealed artifact {name} size", 0)
        path = case_dir / name
        if path.is_symlink() or not path.is_file():
            fail(f"sealed artifact is missing or a symlink: {name}")
        if path.stat().st_size != size or sha256_file(path) != digest:
            fail(f"sealed artifact bytes differ from artifacts.tsv: {name}")
        artifacts[name] = path
    return artifacts


def stats_read_requests(serial_path: Path, workload: str) -> int:
    wanted = "virtiofs.read_requested_requests_total"
    values: list[int] = []
    try:
        lines = serial_path.read_text(encoding="utf-8", errors="strict").splitlines()
    except (OSError, UnicodeError) as error:
        fail(f"cannot read sealed serial stats deltas: {error}")
    for line in lines:
        if not line.startswith("stats_delta "):
            continue
        tokens: dict[str, str] = {}
        invalid = False
        for field in line.split()[1:]:
            if "=" not in field:
                invalid = True
                break
            key, value = field.split("=", 1)
            if not key or key in tokens:
                invalid = True
                break
            tokens[key] = value
        if invalid:
            continue
        if tokens.get("workload") == workload and tokens.get("key") == wanted:
            delta = tokens.get("delta", "")
            if not delta.isdigit():
                fail("sealed READ request delta is not a non-negative integer")
            values.append(int(delta))
    if len(values) != 1 or values[0] <= 0:
        fail("light case must contain exactly one positive sealed READ request delta")
    return values[0]


def validate_packed_cpu_delta(value: Any, before: Any, after: Any, run_id: str, case_id: str,
                              completed_bytes: int, max_window_seconds: int,
                              collector_context: dict[str, Any]) -> float:
    if not isinstance(value, dict):
        fail("sealed cpu-delta.json must be an object")
    require_exact_keys(value, {"schema", "run_id", "case_id", "host_boot_id",
                               "before_sha256", "after_sha256", "window_boottime_ns",
                               "window_monotonic_ns", "bytes", "delta_ticks",
                               "clock_ticks_per_second", "cpu_seconds",
                               "cpu_seconds_per_mib", "processes"}, "sealed CPU delta")
    if value["schema"] != CPU_DELTA_SCHEMA or value["run_id"] != run_id or value["case_id"] != case_id:
        fail("sealed CPU delta schema or run/case binding is incompatible")
    expected_delta = compute_cpu_delta(before, after, completed_bytes, max_window_seconds)
    if value != expected_delta:
        fail("sealed CPU delta is not the canonical result of its raw snapshots")
    if value["host_boot_id"] != collector_context.get("host_boot_id"):
        fail("CPU snapshots and collector context have different host boot IDs")
    ticks = require_int(value["delta_ticks"], "sealed CPU delta ticks", 1)
    hz = require_int(value["clock_ticks_per_second"], "sealed CPU clock ticks", 1)
    seconds = require_number(value["cpu_seconds"], "sealed CPU seconds", positive=True)
    per_mib = require_number(value["cpu_seconds_per_mib"], "sealed CPU seconds/MiB", positive=True)
    expected_seconds = ticks / hz
    expected_per_mib = expected_seconds / (completed_bytes / (1024 * 1024))
    if not math.isclose(seconds, expected_seconds, rel_tol=1e-12, abs_tol=1e-15):
        fail("sealed CPU seconds do not match ticks/clock rate")
    if not math.isclose(per_mib, expected_per_mib, rel_tol=1e-12, abs_tol=1e-15):
        fail("sealed CPU seconds/MiB do not match ticks and completed bytes")
    if not isinstance(value["processes"], list) or not value["processes"]:
        fail("sealed CPU delta has no process contributions")
    process_ticks = 0
    pids: set[int] = set()
    labels: list[str] = []
    for item in value["processes"]:
        if not isinstance(item, dict):
            fail("sealed CPU process contribution is not an object")
        require_exact_keys(item, {"label", "pid", "starttime_ticks", "delta_ticks"},
                           "sealed CPU process contribution")
        label = item["label"]
        if label not in ("qemu", "virtiofsd", "virtiofsd-worker"):
            fail("sealed CPU process contribution has an invalid label")
        labels.append(label)
        pid = require_int(item["pid"], "sealed CPU process PID", 1)
        if pid in pids:
            fail(f"sealed CPU delta repeats PID {pid}")
        pids.add(pid)
        require_int(item["starttime_ticks"], f"sealed CPU PID {pid} starttime", 0)
        process_ticks += require_int(item["delta_ticks"], "sealed CPU process delta ticks", 0)
    if labels.count("qemu") != 1 or labels.count("virtiofsd") != 1:
        fail("sealed CPU delta must contain exactly one QEMU and one primary virtiofsd process")
    if process_ticks != ticks:
        fail("sealed CPU process contributions do not conserve total ticks")
    try:
        expected = {
            ("qemu", require_int(collector_context["qemu"]["pid"],
                                 "collector QEMU PID", 1),
             require_int(collector_context["qemu"]["start_ticks"],
                         "collector QEMU start time", 0)),
            ("virtiofsd", require_int(collector_context["virtiofsd"]["pid"],
                                      "collector virtiofsd PID", 1),
             require_int(collector_context["virtiofsd"]["start_ticks"],
                         "collector virtiofsd start time", 0)),
        }
        worker_pid = require_int(collector_context["binding"]["worker_pid"],
                                 "collector virtiofsd worker PID", 1)
        worker_ticks = require_int(collector_context["binding"]["worker_start_ticks"],
                                   "collector virtiofsd worker start time", 0)
    except (KeyError, TypeError):
        fail("collector context lacks CPU identity binding fields")
    daemon_pid = collector_context["virtiofsd"]["pid"]
    if worker_pid != daemon_pid:
        expected.add(("virtiofsd-worker", worker_pid, worker_ticks))
    actual = {(item["label"], item["pid"], item["starttime_ticks"])
              for item in value["processes"]}
    raw_identities = []
    for raw in (before, after):
        raw_identities.append({(item["label"], item["pid"], item["starttime_ticks"])
                               for item in raw["processes"]})
    if actual != expected or any(identities != expected for identities in raw_identities):
        fail("sealed CPU process identities differ from collector QEMU/virtiofsd binding")
    return per_mib


def pack_case_command(args: argparse.Namespace) -> None:
    plan = read_json(Path(args.plan))
    samples = validate_plan(plan)
    matching = [sample for sample in samples if sample["sample_id"] == args.sample_id]
    if len(matching) != 1:
        fail(f"sample id is not uniquely pre-registered: {args.sample_id}")
    sample = matching[0]
    case_id = require_token(args.runner_case_id, "runner case id")
    cpu_artifact = require_token(args.cpu_delta_artifact, "CPU delta artifact name")
    cpu_before_artifact = require_token(args.cpu_before_artifact, "CPU before artifact name")
    cpu_after_artifact = require_token(args.cpu_after_artifact, "CPU after artifact name")
    verify_timeout = require_int(args.verify_timeout, "runner verification timeout", 1)
    run_dir_input = Path(args.verified_run_dir)
    if run_dir_input.is_symlink() or not run_dir_input.is_dir():
        fail("verified run directory must be a non-symlink directory")
    run_dir = run_dir_input.resolve()
    sealed_runner = run_dir / "runner.sh"
    verify_sealed_runner_sources(run_dir)
    replay: subprocess.CompletedProcess[str] | None = None
    replay_error: OSError | subprocess.TimeoutExpired | None = None
    try:
        replay = subprocess.run(["bash", str(sealed_runner), "verify", "--run-dir", str(run_dir)],
                                cwd=run_dir, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, check=False, timeout=verify_timeout)
    except (OSError, subprocess.TimeoutExpired) as error:
        replay_error = error
    finally:
        verify_sealed_runner_sources(run_dir)
    if replay_error is not None:
        fail(f"cannot replay sealed runner verification: {replay_error}")
    assert replay is not None
    if replay.returncode != 0:
        fail(f"sealed runner verification failed: {replay.stderr.strip() or replay.stdout.strip()}")

    manifest = read_json(run_dir / "manifest.json")
    if not isinstance(manifest, dict) or manifest.get("schema") != "dragonos.virtiofs.non-dax-run.v2":
        fail("verified runner manifest schema is incompatible")
    try:
        run_id = require_token(manifest["run_id"], "runner run id")
        revision = manifest["repo"]["commit"]
        run_mode = manifest["mode"]
        guest_cache = manifest["cache"]["guest"]
        host_cache = manifest["cache"]["host"]
        build_manifest_sha = manifest["repo"]["build_manifest_sha256"]
        kernel_sha = require_sha256(manifest["artifacts"]["kernel_sha256"],
                                    "runner kernel SHA256")
        disk_sha = require_sha256(manifest["artifacts"]["disk_image_sha256"],
                                  "runner disk image SHA256")
        helper_run_sha = require_sha256(manifest["artifacts"]["guest_helper_sha256"],
                                        "runner guest helper SHA256")
        qemu_sha = manifest["artifacts"]["qemu_sha256"]
        virtiofsd_sha = manifest["artifacts"]["virtiofsd_sha256"]
    except (KeyError, TypeError):
        fail("verified runner manifest lacks study binding fields")
    build_manifest = read_json(run_dir / "build-manifest.json")
    try:
        kernel_build_sha = require_sha256(build_manifest["artifacts"]["kernel"]["sha256"],
                                          "build kernel SHA256")
        disk_build_sha = require_sha256(build_manifest["artifacts"]["disk_image"]["sha256"],
                                        "build disk image SHA256")
        helper_sha = require_sha256(build_manifest["artifacts"]["guest_helper"]["sha256"],
                                    "build guest helper SHA256")
    except (KeyError, TypeError):
        fail("verified build manifest lacks the guest helper identity")
    if revision != sample["revision"]:
        fail("verified runner revision differs from the pre-registered sample")
    if build_manifest_sha != sample["build_manifest_sha256"]:
        fail("verified runner build artifact set differs from the pre-registered sample")
    if ((kernel_sha, disk_sha, helper_run_sha) !=
            (kernel_build_sha, disk_build_sha, helper_sha)):
        fail("verified runner kernel/disk/helper identities differ from its build manifest")
    runtime = plan["shared_runtime"]
    if (qemu_sha != runtime["qemu_sha256"] or virtiofsd_sha != runtime["virtiofsd_sha256"] or
            helper_sha != runtime["helper_sha256"]):
        fail("verified runner runtime/helper identity differs from the study plan")
    expected_cache = f"guest-{guest_cache}-host-{host_cache}"
    if expected_cache != sample["cache"]:
        fail("verified runner cache tuple differs from the pre-registered sample")
    if run_mode != sample["mode"]:
        fail("verified runner mode differs from the pre-registered sample mode")

    case_dir = run_dir / "cases" / case_id
    if case_dir.is_symlink() or not case_dir.is_dir():
        fail(f"verified run has no non-symlink case directory for {case_id}")
    status = read_json(case_dir / "status.json")
    if (not isinstance(status, dict) or status.get("schema") != "dragonos.virtiofs.non-dax-case.v4" or
            status.get("runner_version") != "4" or status.get("case_id") != case_id or
            status.get("status") != "completed"):
        fail("runner case status is not a verified v4 completed case")
    matrix_rows = (run_dir / "case-matrix.tsv").read_text(encoding="utf-8").splitlines()
    selected = [row.split("\t") for row in matrix_rows[1:] if row.split("\t", 1)[0] == case_id]
    if len(selected) != 1 or len(selected[0]) != 7:
        fail("runner case is not uniquely represented in case-matrix.tsv")
    _, matrix_mode, phase, _, _, guest_cache, _ = selected[0]
    if (case_id != sample["workload"] or matrix_mode != run_mode or phase != "read" or
            f"guest-{guest_cache}-host-{selected[0][6]}" != sample["cache"]):
        fail("runner case matrix does not match the study read/cache/mode binding")

    artifacts = sealed_artifacts(case_dir)
    for required in ("case-result.json", "serial", "collector_context", cpu_before_artifact,
                     cpu_after_artifact, cpu_artifact):
        if required not in artifacts:
            fail(f"verified case lacks required sealed artifact: {required}")
    runner_result = read_json(artifacts["case-result.json"])
    if not isinstance(runner_result, dict):
        fail("runner case-result.json is not an object")
    require_exact_keys(runner_result, {"schema", "runner_version", "status", "case_id",
                                       "workload", "mode", "result", "config"},
                       "runner case result")
    if (runner_result["schema"] != "dragonos.virtiofs.non-dax-case-result.v1" or
            runner_result["runner_version"] != "4" or runner_result["case_id"] != case_id or
            runner_result["status"] != "completed" or runner_result["mode"] != run_mode or
            runner_result["workload"] != "sequential_read"):
        fail("runner case result does not match runner v4 or the pre-registered workload")
    if not isinstance(runner_result["result"], dict):
        fail("runner result metrics are not an object")
    require_exact_keys(runner_result["result"], {"elapsed_us", "bytes", "ops", "syscalls",
                                                     "short_io", "eintr", "checksum",
                                                     "read_requests", "requested_bytes"},
                       "runner metrics")
    elapsed_us = require_int(runner_result["result"]["elapsed_us"], "runner elapsed_us", 1)
    completed_bytes = require_int(runner_result["result"]["bytes"], "runner bytes", 1)
    require_int(runner_result["result"]["ops"], "runner ops", 1)
    require_int(runner_result["result"]["syscalls"], "runner syscalls", 1)
    require_int(runner_result["result"]["short_io"], "runner short I/O count", 0)
    require_int(runner_result["result"]["eintr"], "runner EINTR count", 0)
    checksum = runner_result["result"]["checksum"]
    if (not isinstance(checksum, str) or len(checksum) != 16 or
            any(character not in "0123456789abcdef" for character in checksum)):
        fail("runner checksum is invalid")
    if run_mode == "performance":
        if (runner_result["result"]["read_requests"] is not None or
                runner_result["result"]["requested_bytes"] is not None):
            fail("performance runner result must not claim READ evidence")
        read_requests = None
    else:
        read_requests = require_int(runner_result["result"]["read_requests"],
                                    "runner READ request count", 1)
        requested_bytes = require_int(runner_result["result"]["requested_bytes"],
                                      "runner requested bytes", completed_bytes)
        if requested_bytes > (completed_bytes * 5 + 3) // 4:
            fail("runner requested bytes exceed the sealed read-amplification limit")
        if read_requests != stats_read_requests(artifacts["serial"], "sequential_read"):
            fail("runner READ request count differs from its sealed serial delta")
    config = runner_result["config"]
    if not isinstance(config, dict):
        fail("runner negotiated config is not an object")
    require_exact_keys(config, {"init_epoch", "negotiated_max_read_bytes",
                                "negotiated_max_pages", "negotiated_max_readahead_bytes",
                                "negotiated_async_read", "sg_limit_pages_configured",
                                "effective_read_payload_limit_bytes"}, "runner negotiated config")
    if require_int(config["init_epoch"], "runner init epoch", 1) != 1:
        fail("runner config is not the first completed FUSE initialization epoch")
    max_read = require_int(config["negotiated_max_read_bytes"], "runner max_read", 1)
    max_pages = require_int(config["negotiated_max_pages"], "runner max_pages", 1)
    sg_pages = require_int(config["sg_limit_pages_configured"], "runner SG pages", 1)
    require_int(config["negotiated_max_readahead_bytes"], "runner max_readahead", 0)
    if config["negotiated_async_read"] not in (0, 1):
        fail("runner ASYNC_READ flag is invalid")
    effective = require_int(config["effective_read_payload_limit_bytes"],
                            "runner effective payload", 1)
    if effective != min(max_read, max_pages * 4096, sg_pages * 4096):
        fail("runner effective payload differs from negotiated/SG limits")
    collector_context = read_json(artifacts["collector_context"])
    if (not isinstance(collector_context, dict) or
            collector_context.get("schema") != "dragonos.virtiofs.collector-process-context.v3"):
        fail("sealed collector process context is incompatible")
    cpu_per_mib = validate_packed_cpu_delta(
        read_json(artifacts[cpu_artifact]), read_json(artifacts[cpu_before_artifact]),
        read_json(artifacts[cpu_after_artifact]), run_id, case_id, completed_bytes,
        plan["cpu_max_window_seconds"], collector_context)
    summary = {"data_loop_elapsed_us": elapsed_us, "bytes": completed_bytes,
               "read_requests": read_requests, "cpu_seconds_per_mib": cpu_per_mib}
    digest = canonical_sha256(summary)
    packed = {"schema": CASE_SCHEMA, "status": "completed", **sample,
              "summary": summary, "summary_sha256": digest}
    output = Path(args.output)
    write_json(output, packed)
    index_entry = {"sample_id": sample["sample_id"], "case_result": output.name,
                   "summary_sha256": digest}
    if args.index_entry_output:
        entry_output = Path(args.index_entry_output)
        relative = os.path.relpath(output.resolve(), entry_output.parent.resolve())
        if relative == ".." or relative.startswith(f"..{os.sep}"):
            fail("packed case must be inside the index-entry output directory")
        index_entry["case_result"] = Path(relative).as_posix()
        write_json(entry_output, index_entry)
    print(json.dumps(index_entry, sort_keys=True, separators=(",", ":")))


def bootstrap_effects(observations: list[dict[str, Any]], metric: str, iterations: int,
                      seed: int) -> tuple[float, float, float]:
    groups: dict[tuple[str, str, str], list[float]] = {}
    cells: set[tuple[str, str]] = set()
    for observation in observations:
        cell = (observation["workload"], observation["cache"])
        cells.add(cell)
        groups.setdefault((observation["stratum"], *cell), []).append(float(observation[metric]))
    rng = random.Random(seed)
    effects: list[float] = []
    ordered_cells = sorted(cells)
    for _ in range(iterations):
        sampled_a: list[float] = []
        sampled_b: list[float] = []
        for cell in ordered_cells:
            for stratum in ("A1", "A2"):
                values = groups[(stratum, *cell)]
                sampled_a.extend(rng.choice(values) for _ in values)
            values = groups[("B", *cell)]
            # Equal cell weighting avoids an accidental imbalance becoming a weight.
            target = len(groups[("A1", *cell)]) + len(groups[("A2", *cell)])
            sampled_b.extend(rng.choice(values) for _ in range(target))
        effects.append(relative_improvement(median(sampled_a), median(sampled_b)))
    point_a = median(float(item[metric]) for item in observations if item["stratum"] != "B")
    point_b = median(float(item[metric]) for item in observations if item["stratum"] == "B")
    return relative_improvement(point_a, point_b), percentile(effects, 0.025), percentile(effects, 0.975)


def aggregate_command(args: argparse.Namespace) -> None:
    for output in (Path(args.acceptance), Path(args.report)):
        if output.exists():
            fail(f"refusing to replace existing output: {output}")
    plan_path = Path(args.plan)
    plan = read_json(plan_path)
    samples = validate_plan(plan)
    observations = load_results(plan, samples, Path(args.results_index))
    a1_latency = median(float(item["data_loop_elapsed_us"]) for item in observations if item["stratum"] == "A1")
    a2_latency = median(float(item["data_loop_elapsed_us"]) for item in observations if item["stratum"] == "A2")
    drift = abs(a2_latency / a1_latency - 1.0)
    drift_valid = drift <= args.max_drift
    baseline_latency = [float(item["data_loop_elapsed_us"]) for item in observations if item["stratum"] != "B"]
    candidate_latency = [float(item["data_loop_elapsed_us"]) for item in observations if item["stratum"] == "B"]
    p90_regression = percentile(candidate_latency, .90) / percentile(baseline_latency, .90) - 1.0
    baseline_cpu = median(float(item["cpu_seconds_per_mib"]) for item in observations if item["stratum"] != "B")
    candidate_cpu = median(float(item["cpu_seconds_per_mib"]) for item in observations if item["stratum"] == "B")
    cpu_regression = candidate_cpu / baseline_cpu - 1.0
    effects: dict[str, Any]
    if plan["study_kind"] == "candidate-effect":
        iterations = plan["bootstrap_iterations"]
        base_seed = plan["study_seed"]
        latency = bootstrap_effects(observations, "data_loop_elapsed_us", iterations,
                                    base_seed ^ 0x4C4154)
        checks = {
            "a1_a2_drift": drift_valid,
            "latency_improvement_ci_lower": latency[1] >= args.min_improvement,
            "p90_latency_regression": p90_regression <= args.max_p90_regression,
            "cpu_seconds_per_mib_regression": cpu_regression <= args.max_cpu_regression,
        }
        effects = {
            "latency_improvement": {"point": latency[0], "ci95": [latency[1], latency[2]]},
            "p90_latency_regression": p90_regression,
            "cpu_seconds_per_mib_regression": cpu_regression,
        }
        if plan["measured_mode"] != "performance":
            reads = bootstrap_effects(observations, "read_requests", iterations,
                                      base_seed ^ 0x52454144)
            checks["read_request_improvement_ci_lower"] = reads[1] >= args.min_improvement
            effects["read_request_improvement"] = {
                "point": reads[0], "ci95": [reads[1], reads[2]]}
        thresholds = {
            "max_a1_a2_median_drift": args.max_drift,
            "min_improvement_ci_lower": args.min_improvement,
            "max_p90_latency_regression": args.max_p90_regression,
            "max_cpu_seconds_per_mib_regression": args.max_cpu_regression,
        }
    else:
        median_overhead = median(candidate_latency) / median(baseline_latency) - 1.0
        checks = {
            "a1_a2_drift": drift_valid,
            "median_elapsed_overhead": median_overhead <= args.max_mode_overhead,
            "p90_elapsed_overhead": p90_regression <= args.max_mode_p90_overhead,
            "cpu_seconds_per_mib_overhead": cpu_regression <= args.max_mode_cpu_overhead,
        }
        effects = {
            "median_elapsed_overhead": median_overhead,
            "p90_elapsed_overhead": p90_regression,
            "cpu_seconds_per_mib_overhead": cpu_regression,
        }
        thresholds = {
            "max_a1_a2_median_drift": args.max_drift,
            "max_median_elapsed_overhead": args.max_mode_overhead,
            "max_p90_elapsed_overhead": args.max_mode_p90_overhead,
            "max_cpu_seconds_per_mib_overhead": args.max_mode_cpu_overhead,
        }
    accepted = all(checks.values())
    acceptance = {
        "schema": ACCEPTANCE_SCHEMA,
        "study_kind": plan["study_kind"],
        "accepted": accepted,
        "plan_sha256": canonical_sha256(plan),
        "thresholds": thresholds,
        "sample_counts": {stratum: sum(item["stratum"] == stratum for item in observations)
                          for stratum in STRATA},
        "a1_a2": {"a1_median_elapsed_us": a1_latency, "a2_median_elapsed_us": a2_latency,
                  "absolute_relative_drift": drift, "valid": drift_valid},
        "effects": effects,
        "checks": checks,
        "observations": observations,
    }
    write_json(Path(args.acceptance), acceptance)
    report_path = Path(args.report)
    report_path.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        f"# virtiofs non-DAX {plan['study_kind']} A1/B/A2 report", "",
        f"- Result: **{'ACCEPTED' if accepted else 'REJECTED'}**",
        f"- Samples: A1={acceptance['sample_counts']['A1']}, B={acceptance['sample_counts']['B']}, A2={acceptance['sample_counts']['A2']}",
        f"- A1/A2 median drift: {drift:.2%} (limit {args.max_drift:.2%})",
        "## Checks", "",
    ]
    lines.extend(f"- [{'x' if passed else ' '}] `{name}`" for name, passed in checks.items())
    try:
        with report_path.open("x", encoding="utf-8") as stream:
            stream.write("\n".join(lines) + "\n")
    except FileExistsError:
        fail(f"refusing to replace existing output: {report_path}")
    except OSError as error:
        fail(f"cannot write {report_path}: {error}")
    if not accepted:
        raise SystemExit(1)


def parse_proc_stat(text: str, where: str) -> tuple[int, int, int, str]:
    # comm is parenthesized and may contain spaces or ')' characters; the final
    # ')' before the state field is the only safe split point.
    left = text.find("(")
    right = text.rfind(")")
    if left <= 0 or right <= left or right + 2 >= len(text):
        fail(f"invalid proc stat at {where}")
    try:
        pid = int(text[:left].strip())
        comm = text[left + 1:right]
        fields = text[right + 2:].split()
        # fields[0] is kernel field 3 (state).
        utime = int(fields[11])
        stime = int(fields[12])
        starttime = int(fields[19])
    except (ValueError, IndexError):
        fail(f"invalid proc stat numeric fields at {where}")
    if min(pid, utime, stime, starttime) < 0:
        fail(f"negative proc stat field at {where}")
    return pid, utime + stime, starttime, comm


def process_snapshot(proc_root: Path, pid: int, label: str) -> dict[str, Any]:
    process_stat_path = proc_root / str(pid) / "stat"
    try:
        process_pid, _, starttime, comm = parse_proc_stat(process_stat_path.read_text(encoding="utf-8"), str(process_stat_path))
    except OSError as error:
        fail(f"cannot read {process_stat_path}: {error}")
    if process_pid != pid:
        fail(f"PID mismatch in {process_stat_path}")
    task_root = proc_root / str(pid) / "task"
    threads: list[dict[str, Any]] = []
    try:
        entries = sorted(task_root.iterdir(), key=lambda item: int(item.name))
    except (OSError, ValueError) as error:
        fail(f"cannot enumerate tasks for PID {pid}: {error}")
    for entry in entries:
        if not entry.name.isdigit() or not entry.is_dir():
            continue
        stat_path = entry / "stat"
        try:
            tid, ticks, thread_starttime, thread_comm = parse_proc_stat(stat_path.read_text(encoding="utf-8"), str(stat_path))
        except OSError as error:
            fail(f"cannot read {stat_path}: {error}")
        if tid != int(entry.name):
            fail(f"thread id mismatch in {stat_path}")
        threads.append({"tid": tid, "comm": thread_comm, "starttime_ticks": thread_starttime,
                        "user_system_ticks": ticks})
    if not threads:
        fail(f"PID {pid} has no readable tasks")
    try:
        final_pid, process_ticks, final_starttime, final_comm = parse_proc_stat(
            process_stat_path.read_text(encoding="utf-8"), str(process_stat_path))
    except OSError as error:
        fail(f"cannot re-read {process_stat_path}: {error}")
    if (final_pid, final_starttime, final_comm) != (process_pid, starttime, comm):
        fail(f"PID {pid} identity changed while taking its CPU snapshot")
    live_thread_ticks = sum(thread["user_system_ticks"] for thread in threads)
    if process_ticks < live_thread_ticks:
        fail(f"PID {pid} process ticks are older than its live-thread accounting; retry snapshot")
    return {"label": label, "pid": pid, "comm": comm, "starttime_ticks": starttime,
            "thread_count": len(threads),
            "user_system_ticks": process_ticks,
            "live_thread_user_system_ticks": live_thread_ticks,
            "threads": threads}


def cpu_snapshot_command(args: argparse.Namespace) -> None:
    clock_ticks = args.clock_ticks
    if clock_ticks is None:
        try:
            clock_ticks = os.sysconf("SC_CLK_TCK")
        except (ValueError, OSError):
            fail("cannot determine SC_CLK_TCK; pass --clock-ticks")
    clock_ticks = require_int(clock_ticks, "clock ticks", 1)
    processes = [process_snapshot(Path(args.proc_root), args.qemu_pid, "qemu"),
                 process_snapshot(Path(args.proc_root), args.virtiofsd_pid, "virtiofsd")]
    for worker_pid in args.virtiofsd_worker_pid:
        processes.append(process_snapshot(Path(args.proc_root), worker_pid, "virtiofsd-worker"))
    pids = [process["pid"] for process in processes]
    if len(set(pids)) != len(pids):
        fail("QEMU, virtiofsd, and worker PIDs must be distinct")
    boot_id = args.host_boot_id
    if boot_id is None:
        try:
            boot_id = (Path(args.proc_root) / "sys/kernel/random/boot_id").read_text(
                encoding="utf-8").strip()
        except OSError as error:
            fail(f"cannot read host boot ID: {error}")
    boot_id = require_token(boot_id, "host boot ID")
    snapshot = {"schema": CPU_SCHEMA, "run_id": require_token(args.run_id, "CPU run id"),
                "case_id": require_token(args.case_id, "CPU case id"),
                "phase": args.phase, "host_boot_id": boot_id,
                "captured_boottime_ns": time.clock_gettime_ns(time.CLOCK_BOOTTIME),
                "captured_monotonic_ns": time.monotonic_ns(),
                "clock_ticks_per_second": clock_ticks, "processes": processes}
    write_json(Path(args.output), snapshot)


def compute_cpu_delta(before: Any, after: Any, byte_count: int,
                      max_window_seconds: int | None = None) -> dict[str, Any]:
    for name, snapshot, phase in (("before", before, "before"), ("after", after, "after")):
        if not isinstance(snapshot, dict):
            fail(f"{name} CPU snapshot is not an object")
        require_exact_keys(snapshot, {"schema", "run_id", "case_id", "phase",
                                      "host_boot_id", "captured_boottime_ns",
                                      "captured_monotonic_ns", "clock_ticks_per_second",
                                      "processes"}, f"{name} snapshot")
        if snapshot["schema"] != CPU_SCHEMA or snapshot["phase"] != phase:
            fail(f"{name} CPU snapshot schema/phase is incompatible")
        require_token(snapshot["host_boot_id"], f"{name} host boot ID")
        require_int(snapshot["captured_boottime_ns"], f"{name} boottime", 1)
        require_int(snapshot["captured_monotonic_ns"], f"{name} monotonic time", 1)
        require_int(snapshot["clock_ticks_per_second"], f"{name} clock ticks", 1)
        if not isinstance(snapshot["processes"], list) or not snapshot["processes"]:
            fail(f"{name} snapshot has no processes")
    if before["clock_ticks_per_second"] != after["clock_ticks_per_second"]:
        fail("before/after clock tick rates differ")
    if before["run_id"] != after["run_id"] or before["case_id"] != after["case_id"]:
        fail("before/after run or case bindings differ")
    if before["host_boot_id"] != after["host_boot_id"]:
        fail("before/after host boot IDs differ")
    boot_window = after["captured_boottime_ns"] - before["captured_boottime_ns"]
    monotonic_window = after["captured_monotonic_ns"] - before["captured_monotonic_ns"]
    if boot_window <= 0 or monotonic_window <= 0:
        fail("CPU snapshot capture times are not strictly ordered")
    if max_window_seconds is not None:
        limit = require_int(max_window_seconds, "CPU maximum window seconds", 1) * 1_000_000_000
        if boot_window > limit or monotonic_window > limit:
            fail("CPU measurement window exceeds its pre-registered limit")
    run_id = require_token(before["run_id"], "CPU run id")
    case_id = require_token(before["case_id"], "CPU case id")
    process_keys = {"label", "pid", "comm", "starttime_ticks", "thread_count", "user_system_ticks",
                    "live_thread_user_system_ticks", "threads"}
    thread_keys = {"tid", "comm", "starttime_ticks", "user_system_ticks"}
    before_by_pid: dict[int, dict[str, Any]] = {}
    after_by_pid: dict[int, dict[str, Any]] = {}
    for name, snapshot, destination in (("before", before, before_by_pid), ("after", after, after_by_pid)):
        for process in snapshot["processes"]:
            if not isinstance(process, dict):
                fail(f"{name} process is not an object")
            require_exact_keys(process, process_keys, f"{name} process")
            pid = require_int(process["pid"], f"{name} PID", 1)
            if pid in destination:
                fail(f"{name} snapshot repeats PID {pid}")
            require_int(process["starttime_ticks"], f"{name} PID {pid} starttime", 0)
            if (process["starttime_ticks"] * 1_000_000_000 >
                    snapshot["captured_boottime_ns"] * snapshot["clock_ticks_per_second"]):
                fail(f"{name} PID {pid} starts after its CPU snapshot")
            total_ticks = require_int(process["user_system_ticks"], f"{name} PID {pid} ticks", 0)
            live_thread_ticks = require_int(process["live_thread_user_system_ticks"],
                                            f"{name} PID {pid} live-thread ticks", 0)
            if not isinstance(process["threads"], list) or not process["threads"]:
                fail(f"{name} PID {pid} has no thread accounting")
            thread_ids: set[int] = set()
            recomputed_ticks = 0
            for thread in process["threads"]:
                if not isinstance(thread, dict):
                    fail(f"{name} PID {pid} thread is not an object")
                require_exact_keys(thread, thread_keys, f"{name} PID {pid} thread")
                tid = require_int(thread["tid"], f"{name} PID {pid} TID", 1)
                if tid in thread_ids:
                    fail(f"{name} PID {pid} repeats TID {tid}")
                thread_ids.add(tid)
                if (not isinstance(thread["comm"], str) or not thread["comm"] or
                        len(thread["comm"]) > 256):
                    fail(f"{name} PID {pid} TID {tid} comm is invalid")
                require_int(thread["starttime_ticks"], f"{name} PID {pid} TID {tid} starttime", 0)
                recomputed_ticks += require_int(thread["user_system_ticks"],
                                                f"{name} PID {pid} TID {tid} ticks", 0)
            if process["thread_count"] != len(process["threads"]):
                fail(f"{name} PID {pid} thread_count does not match its thread array")
            if live_thread_ticks != recomputed_ticks:
                fail(f"{name} PID {pid} live-thread ticks do not match its thread array")
            if total_ticks < live_thread_ticks:
                fail(f"{name} PID {pid} process ticks are below its live-thread ticks")
            destination[pid] = process
        labels = [process["label"] for process in snapshot["processes"]]
        if labels.count("qemu") != 1 or labels.count("virtiofsd") != 1 or any(
                label not in ("qemu", "virtiofsd", "virtiofsd-worker") for label in labels):
            fail(f"{name} snapshot has an invalid process label set")
    if set(before_by_pid) != set(after_by_pid):
        fail("before/after process sets differ")
    total_delta = 0
    deltas: list[dict[str, Any]] = []
    for pid in sorted(before_by_pid):
        first, second = before_by_pid[pid], after_by_pid[pid]
        if first["starttime_ticks"] != second["starttime_ticks"]:
            fail(f"PID {pid} starttime changed (PID reuse or process restart)")
        if first["label"] != second["label"]:
            fail(f"PID {pid} label changed")
        delta = second["user_system_ticks"] - first["user_system_ticks"]
        if delta < 0:
            fail(f"PID {pid} CPU ticks decreased")
        total_delta += delta
        deltas.append({"label": first["label"], "pid": pid,
                       "starttime_ticks": first["starttime_ticks"], "delta_ticks": delta})
    byte_count = require_int(byte_count, "completed bytes", 1)
    hz = before["clock_ticks_per_second"]
    seconds = total_delta / hz
    mib = byte_count / (1024 * 1024)
    return {"schema": CPU_DELTA_SCHEMA, "run_id": run_id, "case_id": case_id,
              "host_boot_id": before["host_boot_id"],
              "before_sha256": canonical_sha256(before), "after_sha256": canonical_sha256(after),
              "window_boottime_ns": boot_window, "window_monotonic_ns": monotonic_window,
              "bytes": byte_count, "delta_ticks": total_delta,
              "clock_ticks_per_second": hz, "cpu_seconds": seconds,
              "cpu_seconds_per_mib": seconds / mib, "processes": deltas}


def cpu_delta_command(args: argparse.Namespace) -> None:
    before = read_json(Path(args.before))
    after = read_json(Path(args.after))
    result = compute_cpu_delta(before, after, args.bytes, args.max_window_seconds)
    write_json(Path(args.output), result)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("study-plan", help="pre-register an A1/B/A2 study")
    plan.add_argument("--study-kind", choices=STUDY_KINDS, default="candidate-effect")
    plan.add_argument("--baseline-revision", required=True)
    plan.add_argument("--candidate-revision", required=True)
    plan.add_argument("--baseline-build-manifest-sha256", required=True)
    plan.add_argument("--candidate-build-manifest-sha256", required=True)
    plan.add_argument("--qemu-sha256", required=True)
    plan.add_argument("--virtiofsd-sha256", required=True)
    plan.add_argument("--helper-sha256", required=True)
    plan.add_argument("--cpu-max-window-seconds", type=int, default=300)
    plan.add_argument("--samples-per-stratum", type=int, default=9)
    plan.add_argument("--seed", type=int, required=True)
    plan.add_argument("--bootstrap-iterations", type=int, default=10000)
    plan.add_argument("--mode", choices=("performance", "light", "diagnostic"), default="light")
    plan.add_argument("--workload", action="append", default=[])
    plan.add_argument("--cache", action="append", default=[])
    plan.add_argument("--output", required=True)
    plan.set_defaults(handler=plan_command)

    aggregate = commands.add_parser("aggregate", help="verify and aggregate sealed case summaries")
    aggregate.add_argument("--plan", required=True)
    aggregate.add_argument("--results-index", required=True)
    aggregate.add_argument("--acceptance", required=True)
    aggregate.add_argument("--report", required=True)
    aggregate.add_argument("--max-drift", type=float, default=.10)
    aggregate.add_argument("--min-improvement", type=float, default=.25)
    aggregate.add_argument("--max-p90-regression", type=float, default=.10)
    aggregate.add_argument("--max-cpu-regression", type=float, default=.20)
    aggregate.add_argument("--max-mode-overhead", type=float, default=.02)
    aggregate.add_argument("--max-mode-p90-overhead", type=float, default=.05)
    aggregate.add_argument("--max-mode-cpu-overhead", type=float, default=.05)
    aggregate.set_defaults(handler=aggregate_command)

    pack = commands.add_parser("pack-case", help="derive a study case from a verified runner v4 case")
    pack.add_argument("--plan", required=True)
    pack.add_argument("--sample-id", required=True)
    pack.add_argument("--verified-run-dir", required=True)
    pack.add_argument("--runner-case-id", required=True)
    pack.add_argument("--cpu-delta-artifact", default="cpu-delta.json")
    pack.add_argument("--cpu-before-artifact", default="cpu-before.json")
    pack.add_argument("--cpu-after-artifact", default="cpu-after.json")
    pack.add_argument("--verify-timeout", type=int, default=300)
    pack.add_argument("--output", required=True)
    pack.add_argument("--index-entry-output")
    pack.set_defaults(handler=pack_case_command)

    snapshot = commands.add_parser("cpu-snapshot", help="capture process/thread CPU ticks")
    snapshot.add_argument("--phase", choices=("before", "after"), required=True)
    snapshot.add_argument("--run-id", required=True)
    snapshot.add_argument("--case-id", required=True)
    snapshot.add_argument("--qemu-pid", type=int, required=True)
    snapshot.add_argument("--virtiofsd-pid", type=int, required=True)
    snapshot.add_argument("--virtiofsd-worker-pid", type=int, action="append", default=[])
    snapshot.add_argument("--proc-root", default="/proc")
    snapshot.add_argument("--clock-ticks", type=int)
    snapshot.add_argument("--host-boot-id")
    snapshot.add_argument("--output", required=True)
    snapshot.set_defaults(handler=cpu_snapshot_command)

    delta = commands.add_parser("cpu-delta", help="validate snapshots and compute CPU seconds/MiB")
    delta.add_argument("--before", required=True)
    delta.add_argument("--after", required=True)
    delta.add_argument("--bytes", type=int, required=True)
    delta.add_argument("--max-window-seconds", type=int)
    delta.add_argument("--output", required=True)
    delta.set_defaults(handler=cpu_delta_command)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if args.command == "study-plan":
        if not args.workload:
            args.workload = ["sequential_read"]
        if not args.cache:
            args.cache = ["cold"]
    for threshold in ("max_drift", "min_improvement", "max_p90_regression", "max_cpu_regression",
                      "max_mode_overhead", "max_mode_p90_overhead", "max_mode_cpu_overhead"):
        if hasattr(args, threshold):
            require_number(getattr(args, threshold), threshold)
    args.handler(args)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except StudyError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(2)
