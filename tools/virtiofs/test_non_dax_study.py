#!/usr/bin/env python3
"""Host-only tests for non_dax_study.py."""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import non_dax_study as study


SCRIPT = Path(__file__).with_name("non_dax_study.py")
BASELINE = "1" * 40
CANDIDATE = "2" * 40
BASE_BUILD = "a" * 64
CANDIDATE_BUILD = "b" * 64
QEMU_SHA = "c" * 64
VIRTIOFSD_SHA = "d" * 64
HELPER_SHA = "e" * 64
KERNEL_SHA = "8" * 64
DISK_SHA = "9" * 64


def invoke(*arguments: object, expected: int = 0) -> subprocess.CompletedProcess[str]:
    process = subprocess.run([str(SCRIPT), *(str(value) for value in arguments)],
                             text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if process.returncode != expected:
        raise AssertionError(f"exit={process.returncode}, expected={expected}\nstdout={process.stdout}\nstderr={process.stderr}")
    return process


def proc_stat(pid: int, comm: str, utime: int, stime: int, starttime: int) -> str:
    fields = ["S"] + ["0"] * 49
    fields[11] = str(utime)
    fields[12] = str(stime)
    fields[19] = str(starttime)
    return f"{pid} ({comm}) " + " ".join(fields) + "\n"


class StudyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def make_plan(self, kind: str = "candidate-effect", mode: str = "light") -> tuple[Path, dict]:
        path = self.root / "plan.json"
        candidate = CANDIDATE if kind == "candidate-effect" else BASELINE
        candidate_build = CANDIDATE_BUILD if kind == "candidate-effect" else BASE_BUILD
        invoke("study-plan", "--study-kind", kind,
               "--baseline-revision", BASELINE, "--candidate-revision", candidate,
               "--baseline-build-manifest-sha256", BASE_BUILD,
               "--candidate-build-manifest-sha256", candidate_build,
               "--qemu-sha256", QEMU_SHA, "--virtiofsd-sha256", VIRTIOFSD_SHA,
               "--helper-sha256", HELPER_SHA,
               "--mode", mode,
               "--seed", 7, "--bootstrap-iterations", 1000,
               "--workload", "read-f1048576-b4096",
               "--cache", "guest-cold-host-unknown", "--output", path)
        return path, json.loads(path.read_text())

    def make_results(self, plan: dict, *, drift: float = 0.02, tamper: str | None = None) -> Path:
        rows = []
        results = self.root / "results"
        results.mkdir()
        for sample in plan["samples"]:
            stratum = sample["stratum"]
            latency = 1000.0 * (1.0 if stratum == "A1" else 1.0 + drift if stratum == "A2" else .60)
            if plan["study_kind"] == "mode-overhead":
                latency = 1000.0 * (1.0 if stratum == "A1" else 1.0 + drift if stratum == "A2" else 1.01)
                cpu = 0.10 if stratum != "B" else .101
            else:
                cpu = 0.10 if stratum != "B" else .09
            summary = {"data_loop_elapsed_us": latency, "bytes": 16 * 1024 * 1024,
                       "read_requests": None if sample["mode"] == "performance" else
                       (100 if stratum != "B" else 60),
                       "cpu_seconds_per_mib": cpu}
            digest = study.canonical_sha256(summary)
            result = {"schema": study.CASE_SCHEMA, "status": "completed", **sample,
                      "summary": summary, "summary_sha256": digest}
            path = results / f"{sample['sample_id']}.json"
            path.write_text(json.dumps(result))
            rows.append({"sample_id": sample["sample_id"],
                         "case_result": f"results/{path.name}", "summary_sha256": digest})
        if tamper == "hash":
            first = json.loads((results / "A1-001.json").read_text())
            first["summary"]["data_loop_elapsed_us"] += 1
            (results / "A1-001.json").write_text(json.dumps(first))
        elif tamper == "revision":
            first = json.loads((results / "B-001.json").read_text())
            first["revision"] = BASELINE
            (results / "B-001.json").write_text(json.dumps(first))
        elif tamper == "status":
            first = json.loads((results / "B-001.json").read_text())
            first["status"] = "failed"
            (results / "B-001.json").write_text(json.dumps(first))
        elif tamper == "missing":
            rows.pop()
        elif tamper == "duplicate":
            rows.append(rows[0])
        index = {"schema": study.INDEX_SCHEMA, "plan_sha256": study.canonical_sha256(plan), "results": rows}
        path = self.root / "index.json"
        path.write_text(json.dumps(index))
        return path

    def aggregate(self, plan_path: Path, index: Path, expected: int = 0) -> subprocess.CompletedProcess[str]:
        return invoke("aggregate", "--plan", plan_path, "--results-index", index,
                      "--acceptance", self.root / "acceptance.json", "--report", self.root / "report.md",
                      expected=expected)

    def test_plan_is_deterministic_and_has_three_nines(self) -> None:
        first, value = self.make_plan()
        second = self.root / "plan2.json"
        invoke("study-plan", "--baseline-revision", BASELINE, "--candidate-revision", CANDIDATE,
               "--baseline-build-manifest-sha256", BASE_BUILD,
               "--candidate-build-manifest-sha256", CANDIDATE_BUILD,
               "--qemu-sha256", QEMU_SHA, "--virtiofsd-sha256", VIRTIOFSD_SHA,
               "--helper-sha256", HELPER_SHA,
               "--seed", 7, "--bootstrap-iterations", 1000,
               "--workload", "read-f1048576-b4096",
               "--cache", "guest-cold-host-unknown", "--output", second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual({stratum: sum(row["stratum"] == stratum for row in value["samples"])
                          for stratum in study.STRATA}, {"A1": 9, "B": 9, "A2": 9})

    def test_mode_overhead_plan_and_acceptance(self) -> None:
        plan_path, plan = self.make_plan("mode-overhead")
        self.assertEqual({sample["mode"] for sample in plan["samples"] if sample["stratum"] != "B"},
                         {"performance"})
        self.assertEqual({sample["mode"] for sample in plan["samples"] if sample["stratum"] == "B"},
                         {"light"})
        self.aggregate(plan_path, self.make_results(plan))
        acceptance = json.loads((self.root / "acceptance.json").read_text())
        self.assertTrue(acceptance["accepted"])
        self.assertEqual(acceptance["study_kind"], "mode-overhead")

    def test_mode_overhead_rejects_different_artifact_set(self) -> None:
        invoke("study-plan", "--study-kind", "mode-overhead",
               "--baseline-revision", BASELINE, "--candidate-revision", BASELINE,
               "--baseline-build-manifest-sha256", BASE_BUILD,
               "--candidate-build-manifest-sha256", CANDIDATE_BUILD,
               "--qemu-sha256", QEMU_SHA, "--virtiofsd-sha256", VIRTIOFSD_SHA,
               "--helper-sha256", HELPER_SHA, "--seed", 1,
               "--workload", "read-f1048576-b4096", "--cache", "guest-cold-host-unknown",
               "--output", self.root / "bad-plan.json", expected=2)

    def test_plan_and_validator_reject_malformed_workload_or_cache(self) -> None:
        bad_workloads = ("read-f0-b4096", "read-f1-b0", "read-f1-b2-extra",
                         "xread-f1-b2", "read-f-b2")
        for workload in bad_workloads:
            with self.subTest(workload=workload):
                invoke("study-plan", "--baseline-revision", BASELINE,
                       "--candidate-revision", CANDIDATE,
                       "--baseline-build-manifest-sha256", BASE_BUILD,
                       "--candidate-build-manifest-sha256", CANDIDATE_BUILD,
                       "--qemu-sha256", QEMU_SHA, "--virtiofsd-sha256", VIRTIOFSD_SHA,
                       "--helper-sha256", HELPER_SHA, "--seed", 1,
                       "--workload", workload, "--cache", "guest-cold-host-unknown",
                       "--output", self.root / f"bad-{len(workload)}.json", expected=2)
        plan_path, plan = self.make_plan()
        del plan_path
        for field, value in (("workload", "read-f0-b4096"),
                             ("cache", "guest-cold-host-cold"),
                             ("cache", "guest-warm-host-warm")):
            with self.subTest(field=field, value=value):
                tampered = json.loads(json.dumps(plan))
                tampered["samples"][0][field] = value
                with self.assertRaises(study.StudyError):
                    study.validate_plan(tampered)

    def test_plan_and_validator_enforce_engineering_limits(self) -> None:
        common = ("study-plan", "--baseline-revision", BASELINE,
                  "--candidate-revision", CANDIDATE,
                  "--baseline-build-manifest-sha256", BASE_BUILD,
                  "--candidate-build-manifest-sha256", CANDIDATE_BUILD,
                  "--qemu-sha256", QEMU_SHA, "--virtiofsd-sha256", VIRTIOFSD_SHA,
                  "--helper-sha256", HELPER_SHA, "--seed", 1,
                  "--workload", "read-f1-b1", "--cache", "guest-cold-host-warm")
        invoke(*common, "--samples-per-stratum", study.MAX_SAMPLES_PER_STRATUM + 1,
               "--output", self.root / "too-many-samples.json", expected=2)
        invoke(*common, "--bootstrap-iterations", study.MAX_BOOTSTRAP_ITERATIONS + 1,
               "--output", self.root / "too-many-bootstrap.json", expected=2)
        _, plan = self.make_plan()
        for mutate in ("count", "bootstrap", "total"):
            with self.subTest(mutate=mutate):
                tampered = json.loads(json.dumps(plan))
                if mutate == "count":
                    tampered["samples_per_stratum"] = study.MAX_SAMPLES_PER_STRATUM + 1
                elif mutate == "bootstrap":
                    tampered["bootstrap_iterations"] = study.MAX_BOOTSTRAP_ITERATIONS + 1
                else:
                    tampered["samples"] = tampered["samples"] * \
                        ((study.MAX_TOTAL_SAMPLES // len(tampered["samples"])) + 2)
                with self.assertRaises(study.StudyError):
                    study.validate_plan(tampered)

    def test_candidate_effect_performance_omits_read_acceptance(self) -> None:
        plan_path, plan = self.make_plan(mode="performance")
        self.aggregate(plan_path, self.make_results(plan))
        acceptance = json.loads((self.root / "acceptance.json").read_text())
        self.assertTrue(acceptance["accepted"])
        self.assertNotIn("read_request_improvement", acceptance["effects"])

    def test_aggregate_accepts_and_is_deterministic(self) -> None:
        plan_path, plan = self.make_plan()
        index = self.make_results(plan)
        self.aggregate(plan_path, index)
        first = json.loads((self.root / "acceptance.json").read_text())
        self.assertTrue(first["accepted"])
        self.assertGreaterEqual(first["effects"]["latency_improvement"]["ci95"][0], .25)

        # Re-run in another output location and compare all statistical fields.
        invoke("aggregate", "--plan", plan_path, "--results-index", index,
               "--acceptance", self.root / "acceptance2.json", "--report", self.root / "report2.md")
        second = json.loads((self.root / "acceptance2.json").read_text())
        self.assertEqual(first, second)

    def test_rejects_drift_over_ten_percent(self) -> None:
        plan_path, plan = self.make_plan()
        self.aggregate(plan_path, self.make_results(plan, drift=.11), expected=1)
        self.assertFalse(json.loads((self.root / "acceptance.json").read_text())["accepted"])

    def test_rejects_tampered_or_incomplete_evidence(self) -> None:
        for tamper in ("hash", "revision", "status", "missing", "duplicate"):
            with self.subTest(tamper=tamper):
                case_root = self.root / tamper
                case_root.mkdir()
                old_root, self.root = self.root, case_root
                try:
                    plan_path, plan = self.make_plan()
                    self.aggregate(plan_path, self.make_results(plan, tamper=tamper), expected=2)
                finally:
                    self.root = old_root

    def make_verified_run_fixture(self, plan: dict, sample_id: str = "A1-001") -> tuple[Path, str]:
        sample = next(item for item in plan["samples"] if item["sample_id"] == sample_id)
        run_dir = self.root / "verified-run"
        case_id = "read-f1048576-b4096"
        case_dir = run_dir / "cases" / case_id
        case_dir.mkdir(parents=True)
        shutil.copy(SCRIPT.with_name("non_dax_bench_runner.sh"), run_dir / "runner.sh")
        shutil.copy(SCRIPT.with_name("common.sh"), run_dir / "common.sh")
        manifest = {"schema": "dragonos.virtiofs.non-dax-run.v2", "run_id": "run-1",
                    "repo": {"commit": sample["revision"],
                             "build_manifest_sha256": sample["build_manifest_sha256"]},
                    "mode": sample["mode"],
                    "cache": {"guest": "cold", "host": "unknown"},
                    "artifacts": {"kernel_sha256": KERNEL_SHA,
                                  "disk_image_sha256": DISK_SHA,
                                  "guest_helper_sha256": HELPER_SHA,
                                  "qemu_sha256": QEMU_SHA, "virtiofsd_sha256": VIRTIOFSD_SHA},
                    "guest": {}}
        (run_dir / "manifest.json").write_text(json.dumps(manifest))
        (run_dir / "build-manifest.json").write_text(json.dumps({
            "artifacts": {"kernel": {"sha256": KERNEL_SHA},
                          "disk_image": {"sha256": DISK_SHA},
                          "guest_helper": {"sha256": HELPER_SHA}}
        }))
        (run_dir / "case-matrix.tsv").write_text(
            "case_id\tmode\tphase\tfile_size\tblock_size\tguest_cache\thost_cache\n"
            f"{case_id}\t{sample['mode']}\tread\t1048576\t4096\tcold\tunknown\n")
        status = {"schema": "dragonos.virtiofs.non-dax-case.v4", "runner_version": "4",
                  "case_id": case_id, "status": "completed"}
        (case_dir / "status.json").write_text(json.dumps(status))
        runner_result = {"schema": "dragonos.virtiofs.non-dax-case-result.v1", "runner_version": "4",
                         "status": "completed", "case_id": case_id,
                         "workload": "sequential_read", "mode": sample["mode"],
                         "result": {"elapsed_us": 1000, "bytes": 1048576, "ops": 256,
                                    "syscalls": 256, "short_io": 0, "eintr": 0,
                                    "checksum": "0123456789abcdef",
                                    "read_requests": None if sample["mode"] == "performance" else 64,
                                    "requested_bytes": None if sample["mode"] == "performance" else 1048576},
                         "config": {"init_epoch": 1, "negotiated_max_read_bytes": 262144,
                                    "negotiated_max_pages": 64,
                                    "negotiated_max_readahead_bytes": 524288,
                                    "negotiated_async_read": 1,
                                    "sg_limit_pages_configured": 64,
                                    "effective_read_payload_limit_bytes": 262144}}
        (case_dir / "case-result.json").write_text(json.dumps(runner_result))
        (case_dir / "serial").write_text(
            "stats_delta workload=sequential_read key=virtiofs.read_requested_requests_total delta=64\n")
        def process(label: str, pid: int, start: int, ticks: int) -> dict:
            return {"label": label, "pid": pid, "comm": label, "starttime_ticks": start,
                    "thread_count": 1, "user_system_ticks": ticks,
                    "live_thread_user_system_ticks": ticks,
                    "threads": [{"tid": pid, "comm": label, "starttime_ticks": start,
                                 "user_system_ticks": ticks}]}
        cpu_before = {"schema": study.CPU_SCHEMA, "run_id": "run-1", "case_id": case_id,
                      "phase": "before", "host_boot_id": "boot-1",
                      "captured_boottime_ns": 30_000_000_000,
                      "captured_monotonic_ns": 30_000_000_000,
                      "clock_ticks_per_second": 100,
                      "processes": [process("qemu", 100, 1000, 10),
                                    process("virtiofsd", 200, 2000, 20)]}
        cpu_after = {"schema": study.CPU_SCHEMA, "run_id": "run-1", "case_id": case_id,
                     "phase": "after", "host_boot_id": "boot-1",
                     "captured_boottime_ns": 31_000_000_000,
                     "captured_monotonic_ns": 31_000_000_000,
                     "clock_ticks_per_second": 100,
                     "processes": [process("qemu", 100, 1000, 16),
                                   process("virtiofsd", 200, 2000, 24)]}
        (case_dir / "cpu-before.json").write_text(json.dumps(cpu_before))
        (case_dir / "cpu-after.json").write_text(json.dumps(cpu_after))
        cpu_delta = study.compute_cpu_delta(cpu_before, cpu_after, 1048576, 300)
        (case_dir / "cpu-delta.json").write_text(json.dumps(cpu_delta))
        collector_context = {
            "schema": "dragonos.virtiofs.collector-process-context.v3",
            "host_boot_id": "boot-1",
            "qemu": {"pid": 100, "start_ticks": 1000},
            "virtiofsd": {"pid": 200, "start_ticks": 2000},
            "binding": {"worker_pid": 200, "worker_start_ticks": 2000},
        }
        (case_dir / "collector_context").write_text(json.dumps(collector_context))
        rows = []
        for name in ("case-result.json", "serial", "cpu-before.json", "cpu-after.json",
                     "cpu-delta.json", "collector_context"):
            path = case_dir / name
            rows.append(f"{name}\t{study.sha256_file(path)}\t{path.stat().st_size}")
        (case_dir / "artifacts.tsv").write_text("\n".join(rows) + "\n")
        return run_dir, case_id

    def test_pack_case_derives_only_from_verified_sealed_artifacts(self) -> None:
        plan_path, plan = self.make_plan()
        run_dir, case_id = self.make_verified_run_fixture(plan)
        output = self.root / "packed.json"
        entry = self.root / "entry.json"
        arguments = type("Arguments", (), {
            "plan": str(plan_path), "sample_id": "A1-001", "verified_run_dir": str(run_dir),
            "runner_case_id": case_id, "cpu_delta_artifact": "cpu-delta.json",
            "cpu_before_artifact": "cpu-before.json", "cpu_after_artifact": "cpu-after.json",
            "verify_timeout": 30, "output": str(output), "index_entry_output": str(entry),
        })()
        replay = subprocess.CompletedProcess([], 0, f"verified={run_dir}\n", "")
        with mock.patch.object(study.subprocess, "run", return_value=replay) as runner_verify:
            study.pack_case_command(arguments)
        runner_verify.assert_called_once()
        packed = json.loads(output.read_text())
        self.assertEqual(packed["schema"], study.CASE_SCHEMA)
        self.assertEqual(packed["summary"], {"data_loop_elapsed_us": 1000, "bytes": 1048576,
                                             "read_requests": 64, "cpu_seconds_per_mib": .1})
        self.assertEqual(packed["summary_sha256"], study.canonical_sha256(packed["summary"]))
        self.assertEqual(json.loads(entry.read_text())["summary_sha256"], packed["summary_sha256"])

    def test_pack_case_accepts_performance_without_read_evidence(self) -> None:
        plan_path, plan = self.make_plan("mode-overhead")
        run_dir, case_id = self.make_verified_run_fixture(plan, "A1-001")
        output = self.root / "packed-performance.json"
        arguments = type("Arguments", (), {
            "plan": str(plan_path), "sample_id": "A1-001", "verified_run_dir": str(run_dir),
            "runner_case_id": case_id, "cpu_delta_artifact": "cpu-delta.json",
            "cpu_before_artifact": "cpu-before.json", "cpu_after_artifact": "cpu-after.json",
            "verify_timeout": 30, "output": str(output), "index_entry_output": None,
        })()
        replay = subprocess.CompletedProcess([], 0, f"verified={run_dir}\n", "")
        with mock.patch.object(study.subprocess, "run", return_value=replay):
            study.pack_case_command(arguments)
        self.assertIsNone(json.loads(output.read_text())["summary"]["read_requests"])

    def test_pack_case_rejects_cpu_delta_from_another_run(self) -> None:
        plan_path, plan = self.make_plan()
        run_dir, case_id = self.make_verified_run_fixture(plan)
        cpu_path = run_dir / "cases" / case_id / "cpu-delta.json"
        cpu = json.loads(cpu_path.read_text())
        cpu["run_id"] = "another-run"
        cpu_path.write_text(json.dumps(cpu))
        case_dir = cpu_path.parent
        rows = []
        for name in ("case-result.json", "serial", "cpu-before.json", "cpu-after.json",
                     "cpu-delta.json", "collector_context"):
            path = case_dir / name
            rows.append(f"{name}\t{study.sha256_file(path)}\t{path.stat().st_size}")
        (case_dir / "artifacts.tsv").write_text("\n".join(rows) + "\n")
        arguments = type("Arguments", (), {
            "plan": str(plan_path), "sample_id": "A1-001", "verified_run_dir": str(run_dir),
            "runner_case_id": case_id, "cpu_delta_artifact": "cpu-delta.json",
            "cpu_before_artifact": "cpu-before.json", "cpu_after_artifact": "cpu-after.json",
            "verify_timeout": 30, "output": str(self.root / "packed.json"),
            "index_entry_output": None,
        })()
        replay = subprocess.CompletedProcess([], 0, f"verified={run_dir}\n", "")
        with mock.patch.object(study.subprocess, "run", return_value=replay):
            with self.assertRaises(study.StudyError):
                study.pack_case_command(arguments)

    def test_pack_case_rejects_build_or_runtime_identity_tamper(self) -> None:
        for target in ("build", "qemu", "helper"):
            with self.subTest(target=target):
                case_root = self.root / target
                case_root.mkdir()
                old_root, self.root = self.root, case_root
                try:
                    plan_path, plan = self.make_plan()
                    run_dir, case_id = self.make_verified_run_fixture(plan)
                    manifest_path = run_dir / "manifest.json"
                    manifest = json.loads(manifest_path.read_text())
                    if target == "build":
                        manifest["repo"]["build_manifest_sha256"] = "f" * 64
                    elif target == "qemu":
                        manifest["artifacts"]["qemu_sha256"] = "f" * 64
                    else:
                        build_path = run_dir / "build-manifest.json"
                        build = json.loads(build_path.read_text())
                        build["artifacts"]["guest_helper"]["sha256"] = "f" * 64
                        build_path.write_text(json.dumps(build))
                    manifest_path.write_text(json.dumps(manifest))
                    arguments = type("Arguments", (), {
                        "plan": str(plan_path), "sample_id": "A1-001",
                        "verified_run_dir": str(run_dir), "runner_case_id": case_id,
                        "cpu_delta_artifact": "cpu-delta.json",
                        "cpu_before_artifact": "cpu-before.json",
                        "cpu_after_artifact": "cpu-after.json", "verify_timeout": 30,
                        "output": str(self.root / "packed.json"), "index_entry_output": None,
                    })()
                    replay = subprocess.CompletedProcess([], 0, f"verified={run_dir}\n", "")
                    with mock.patch.object(study.subprocess, "run", return_value=replay):
                        with self.assertRaises(study.StudyError):
                            study.pack_case_command(arguments)
                finally:
                    self.root = old_root

    def test_pack_case_rejects_tampered_or_symlinked_common(self) -> None:
        for tamper in ("bytes", "symlink"):
            with self.subTest(tamper=tamper):
                case_root = self.root / f"common-{tamper}"
                case_root.mkdir()
                old_root, self.root = self.root, case_root
                try:
                    plan_path, plan = self.make_plan()
                    run_dir, case_id = self.make_verified_run_fixture(plan)
                    common = run_dir / "common.sh"
                    if tamper == "bytes":
                        common.write_text(common.read_text() + "\n# tampered\n")
                    else:
                        common.unlink()
                        common.symlink_to(SCRIPT.with_name("common.sh"))
                    arguments = type("Arguments", (), {
                        "plan": str(plan_path), "sample_id": "A1-001",
                        "verified_run_dir": str(run_dir), "runner_case_id": case_id,
                        "cpu_delta_artifact": "cpu-delta.json",
                        "cpu_before_artifact": "cpu-before.json",
                        "cpu_after_artifact": "cpu-after.json", "verify_timeout": 30,
                        "output": str(self.root / "packed.json"), "index_entry_output": None,
                    })()
                    with mock.patch.object(study.subprocess, "run") as runner_verify:
                        with self.assertRaises(study.StudyError):
                            study.pack_case_command(arguments)
                    runner_verify.assert_not_called()
                finally:
                    self.root = old_root

    def test_pack_case_rejects_run_artifact_identity_replacement(self) -> None:
        for target in ("kernel_sha256", "disk_image_sha256", "guest_helper_sha256"):
            with self.subTest(target=target):
                case_root = self.root / target
                case_root.mkdir()
                old_root, self.root = self.root, case_root
                try:
                    plan_path, plan = self.make_plan()
                    run_dir, case_id = self.make_verified_run_fixture(plan)
                    manifest_path = run_dir / "manifest.json"
                    manifest = json.loads(manifest_path.read_text())
                    manifest["artifacts"][target] = "f" * 64
                    manifest_path.write_text(json.dumps(manifest))
                    arguments = type("Arguments", (), {
                        "plan": str(plan_path), "sample_id": "A1-001",
                        "verified_run_dir": str(run_dir), "runner_case_id": case_id,
                        "cpu_delta_artifact": "cpu-delta.json",
                        "cpu_before_artifact": "cpu-before.json",
                        "cpu_after_artifact": "cpu-after.json", "verify_timeout": 30,
                        "output": str(self.root / "packed.json"), "index_entry_output": None,
                    })()
                    replay = subprocess.CompletedProcess([], 0, f"verified={run_dir}\n", "")
                    with mock.patch.object(study.subprocess, "run", return_value=replay):
                        with self.assertRaises(study.StudyError):
                            study.pack_case_command(arguments)
                finally:
                    self.root = old_root

    def test_pack_case_recomputes_delta_from_sealed_raw_snapshots(self) -> None:
        plan_path, plan = self.make_plan()
        run_dir, case_id = self.make_verified_run_fixture(plan)
        before_path = run_dir / "cases" / case_id / "cpu-before.json"
        before = json.loads(before_path.read_text())
        before["processes"][0]["user_system_ticks"] += 1
        before["processes"][0]["live_thread_user_system_ticks"] += 1
        before["processes"][0]["threads"][0]["user_system_ticks"] += 1
        before_path.write_text(json.dumps(before))
        case_dir = before_path.parent
        rows = []
        for name in ("case-result.json", "serial", "cpu-before.json", "cpu-after.json",
                     "cpu-delta.json", "collector_context"):
            path = case_dir / name
            rows.append(f"{name}\t{study.sha256_file(path)}\t{path.stat().st_size}")
        (case_dir / "artifacts.tsv").write_text("\n".join(rows) + "\n")
        arguments = type("Arguments", (), {
            "plan": str(plan_path), "sample_id": "A1-001", "verified_run_dir": str(run_dir),
            "runner_case_id": case_id, "cpu_delta_artifact": "cpu-delta.json",
            "cpu_before_artifact": "cpu-before.json", "cpu_after_artifact": "cpu-after.json",
            "verify_timeout": 30, "output": str(self.root / "packed.json"),
            "index_entry_output": None,
        })()
        replay = subprocess.CompletedProcess([], 0, f"verified={run_dir}\n", "")
        with mock.patch.object(study.subprocess, "run", return_value=replay):
            with self.assertRaises(study.StudyError):
                study.pack_case_command(arguments)

    def write_process(self, proc_root: Path, pid: int, starttime: int, thread_ticks: list[tuple[int, int]]) -> None:
        process = proc_root / str(pid)
        (process / "task").mkdir(parents=True)
        (process / "stat").write_text(proc_stat(pid, "main worker", sum(ticks for _, ticks in thread_ticks), 0,
                                                 starttime))
        for tid, ticks in thread_ticks:
            task = process / "task" / str(tid)
            task.mkdir()
            (task / "stat").write_text(proc_stat(tid, "thread ) name", ticks, 0, starttime + tid))

    def test_cpu_fixture_sums_threads_and_calculates_seconds_per_mib(self) -> None:
        before_proc = self.root / "before-proc"
        after_proc = self.root / "after-proc"
        self.write_process(before_proc, 100, 1000, [(100, 10), (101, 20)])
        self.write_process(before_proc, 200, 2000, [(200, 30), (201, 40)])
        self.write_process(after_proc, 100, 1000, [(100, 30), (101, 40)])
        self.write_process(after_proc, 200, 2000, [(200, 60), (201, 60)])
        before = self.root / "before.json"
        after = self.root / "after.json"
        invoke("cpu-snapshot", "--phase", "before", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1",
               "--proc-root", before_proc, "--clock-ticks", 100, "--host-boot-id", "boot-1",
               "--output", before)
        invoke("cpu-snapshot", "--phase", "after", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1",
               "--proc-root", after_proc, "--clock-ticks", 100, "--host-boot-id", "boot-1",
               "--output", after)
        output = self.root / "delta.json"
        invoke("cpu-delta", "--before", before, "--after", after,
               "--bytes", 2 * 1024 * 1024, "--output", output)
        value = json.loads(output.read_text())
        self.assertEqual(value["delta_ticks"], 90)
        self.assertAlmostEqual(value["cpu_seconds"], .9)
        self.assertAlmostEqual(value["cpu_seconds_per_mib"], .45)

    def test_cpu_rejects_pid_reuse(self) -> None:
        before_proc = self.root / "before-proc"
        after_proc = self.root / "after-proc"
        for proc_root, starttime in ((before_proc, 1000), (after_proc, 1001)):
            self.write_process(proc_root, 100, starttime, [(100, 10)])
            self.write_process(proc_root, 200, 2000, [(200, 10)])
        before, after = self.root / "before.json", self.root / "after.json"
        invoke("cpu-snapshot", "--phase", "before", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1",
               "--proc-root", before_proc, "--clock-ticks", 100, "--host-boot-id", "boot-1",
               "--output", before)
        invoke("cpu-snapshot", "--phase", "after", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1",
               "--proc-root", after_proc, "--clock-ticks", 100, "--host-boot-id", "boot-1",
               "--output", after)
        invoke("cpu-delta", "--before", before, "--after", after, "--bytes", 1048576,
               "--output", self.root / "delta.json", expected=2)

    def test_cpu_rejects_tampered_thread_total(self) -> None:
        proc_root = self.root / "proc"
        self.write_process(proc_root, 100, 1000, [(100, 10)])
        self.write_process(proc_root, 200, 2000, [(200, 10)])
        before, after = self.root / "before.json", self.root / "after.json"
        invoke("cpu-snapshot", "--phase", "before", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1",
               "--proc-root", proc_root, "--clock-ticks", 100, "--host-boot-id", "boot-1",
               "--output", before)
        value = json.loads(before.read_text())
        value["processes"][0]["live_thread_user_system_ticks"] += 1
        before.write_text(json.dumps(value))
        invoke("cpu-snapshot", "--phase", "after", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1",
               "--proc-root", proc_root, "--clock-ticks", 100, "--host-boot-id", "boot-1",
               "--output", after)
        invoke("cpu-delta", "--before", before, "--after", after, "--bytes", 1048576,
               "--output", self.root / "delta.json", expected=2)

    def test_cpu_rejects_cross_boot_and_overlong_window(self) -> None:
        proc_root = self.root / "proc"
        self.write_process(proc_root, 100, 1000, [(100, 10)])
        self.write_process(proc_root, 200, 2000, [(200, 10)])
        before, after = self.root / "before.json", self.root / "after.json"
        invoke("cpu-snapshot", "--phase", "before", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1", "--proc-root", proc_root,
               "--clock-ticks", 100, "--host-boot-id", "boot-1", "--output", before)
        invoke("cpu-snapshot", "--phase", "after", "--qemu-pid", 100, "--virtiofsd-pid", 200,
               "--run-id", "run-1", "--case-id", "case-1", "--proc-root", proc_root,
               "--clock-ticks", 100, "--host-boot-id", "boot-1", "--output", after)
        value = json.loads(after.read_text())
        value["host_boot_id"] = "boot-2"
        after.write_text(json.dumps(value))
        invoke("cpu-delta", "--before", before, "--after", after, "--bytes", 1048576,
               "--max-window-seconds", 1, "--output", self.root / "cross-boot.json", expected=2)

        value["host_boot_id"] = "boot-1"
        first = json.loads(before.read_text())
        value["captured_boottime_ns"] = first["captured_boottime_ns"] + 2_000_000_000
        value["captured_monotonic_ns"] = first["captured_monotonic_ns"] + 2_000_000_000
        after.write_text(json.dumps(value))
        invoke("cpu-delta", "--before", before, "--after", after, "--bytes", 1048576,
               "--max-window-seconds", 1, "--output", self.root / "overlong.json", expected=2)


if __name__ == "__main__":
    unittest.main()
