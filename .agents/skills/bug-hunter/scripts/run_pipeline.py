#!/usr/bin/env python3
"""Run Bug Hunter Stage1/3/4 pipeline with filesystem artifacts."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Sequence


ROOT = Path(__file__).resolve().parent
SEVERITY_ORDER = {"critical": 0, "major": 1, "minor": 2}


def pick_bucket_severity(bucket: dict) -> str:
    """Pick the highest severity from bucket findings."""
    values = [str(f.get("severity", "minor")).lower() for f in bucket.get("findings", [])]
    if not values:
        return "minor"
    return sorted(values, key=lambda s: SEVERITY_ORDER.get(s, 99))[0]


def should_fail_gate(accepted: list[dict], gate: str) -> bool:
    """Return True when accepted findings reach the gate severity."""
    if gate == "none":
        return False
    gate_rank = SEVERITY_ORDER[gate]
    for item in accepted:
        severity = pick_bucket_severity(item)
        if SEVERITY_ORDER.get(severity, 99) <= gate_rank:
            return True
    return False


def run(cmd: Sequence[str]) -> None:
    """Run a stage command and surface stderr/stdout on failure."""
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        if proc.stdout:
            sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise SystemExit(proc.returncode)


def ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description="Run bug-hunter pipeline stages")
    parser.add_argument("--diff-file", help="Optional unified diff file for Stage1")
    parser.add_argument(
        "--raw-findings", required=True, help="Stage2 raw findings JSON file"
    )
    parser.add_argument(
        "--out-dir", default="artifacts", help="Artifact output directory"
    )
    parser.add_argument(
        "--passes", type=int, default=8, help="Shuffle pass count for Stage1"
    )
    parser.add_argument(
        "--threshold", type=float, default=0.6, help="Consensus threshold"
    )
    parser.add_argument(
        "--weights",
        help="Optional persona weights JSON file for weighted voting",
    )
    parser.add_argument(
        "--strict-validation",
        action="store_true",
        help="Fail when agent field is missing in raw findings",
    )
    parser.add_argument(
        "--fail-on-severity",
        choices=["none", "critical", "major", "minor"],
        default="none",
        help="Exit non-zero if accepted findings include this severity or higher",
    )
    parser.add_argument(
        "--ci-mode",
        action="store_true",
        help="Enable CI-oriented defaults: strict validation + fail on critical",
    )
    args = parser.parse_args()
    if args.passes < 1:
        raise ValueError("passes must be >= 1")
    if not (0.0 <= args.threshold <= 1.0):
        raise ValueError("threshold must be in [0,1]")
    if args.ci_mode:
        args.strict_validation = True
        if args.fail_on_severity == "none":
            args.fail_on_severity = "critical"

    out_dir = Path(args.out_dir)
    ensure_dir(out_dir)

    stage_files = {
        "redacted_diff": out_dir / "redacted.diff",
        "shuffled": out_dir / "shuffled_passes.json",
        "validated_findings": out_dir / "raw_findings.validated.json",
        "buckets": out_dir / "buckets.json",
        "debate": out_dir / "debate_candidates.json",
        "verdict": out_dir / "verdict.json",
        "report": out_dir / "bug_hunter_report.md",
    }

    if args.diff_file:
        # Stage1: redact + shuffle only runs when diff is explicitly provided.
        run(
            [
                sys.executable,
                str(ROOT / "redact_sensitive.py"),
                args.diff_file,
                "-o",
                str(stage_files["redacted_diff"]),
            ]
        )
        run(
            [
                sys.executable,
                str(ROOT / "shuffle_diff.py"),
                str(stage_files["redacted_diff"]),
                "--passes",
                str(args.passes),
                "-o",
                str(stage_files["shuffled"]),
            ]
        )

    if not os.path.exists(args.raw_findings):
        raise SystemExit(f"raw findings not found: {args.raw_findings}")

    # Stage2 output validation protects later stages from malformed agent payloads.
    validate_cmd = [
        sys.executable,
        str(ROOT / "validate_findings.py"),
        args.raw_findings,
        "-o",
        str(stage_files["validated_findings"]),
    ]
    if args.strict_validation:
        validate_cmd.append("--require-agent")
    run(validate_cmd)

    # Stage3 semantic grouping.
    run(
        [
            sys.executable,
            str(ROOT / "semantic_bucket.py"),
            str(stage_files["validated_findings"]),
            "-o",
            str(stage_files["buckets"]),
        ]
    )
    # Debate pre-selection for borderline/conflicting buckets.
    run(
        [
            sys.executable,
            str(ROOT / "debate_picker.py"),
            str(stage_files["buckets"]),
            "-o",
            str(stage_files["debate"]),
        ]
    )
    # Stage4 weighted consensus verdict.
    run(
        [
            sys.executable,
            str(ROOT / "weighted_vote.py"),
            str(stage_files["buckets"]),
            "--threshold",
            str(args.threshold),
            *(["--weights", args.weights] if args.weights else []),
            "-o",
            str(stage_files["verdict"]),
        ]
    )
    # Final markdown rendering.
    run(
        [
            sys.executable,
            str(ROOT / "render_report.py"),
            str(stage_files["verdict"]),
            "--debate",
            str(stage_files["debate"]),
            "-o",
            str(stage_files["report"]),
        ]
    )

    with open(stage_files["verdict"], "r", encoding="utf-8") as f:
        verdict = json.load(f)
    accepted = verdict.get("accepted", [])
    gate_fail = should_fail_gate(accepted, args.fail_on_severity)

    summary = {
        "out_dir": str(out_dir),
        "artifacts": {k: str(v) for k, v in stage_files.items() if v.exists()},
        "gate": {
            "fail_on_severity": args.fail_on_severity,
            "failed": gate_fail,
        },
    }
    sys.stdout.write(json.dumps(summary, ensure_ascii=False) + "\n")
    return 3 if gate_fail else 0


if __name__ == "__main__":
    raise SystemExit(main())
