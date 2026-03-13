#!/usr/bin/env python3
"""Apply weighted consensus voting on bug buckets."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
from typing import Any


DEFAULT_WEIGHTS = {
    "Security Sentinel": 5.0,
    "Concurrency Engineer": 4.0,
    "Performance Analyst": 3.0,
    "Diverse Reviewer A": 2.0,
    "Diverse Reviewer B": 2.0,
    "Diverse Reviewer C": 2.0,
    "Diverse Reviewer D": 2.0,
    "Diverse Reviewer E": 2.0,
}


ROOT = Path(__file__).resolve().parent
DEFAULT_PERSONA_MATRIX = ROOT.parent / "references" / "persona_matrix.json"


def load_json(path: str) -> dict[str, Any]:
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def normalize_weight_map(payload: dict[str, Any]) -> dict[str, float]:
    # Preferred format from stage5 output.
    if "suggested_weights" in payload and isinstance(
        payload["suggested_weights"], dict
    ):
        payload = payload["suggested_weights"]

    # Compatible with persona matrix format: {"personas":[{"name","weight"}, ...]}.
    if "personas" in payload and isinstance(payload["personas"], list):
        mapped: dict[str, float] = {}
        for item in payload["personas"]:
            if not isinstance(item, dict):
                continue
            name = item.get("name")
            weight = item.get("weight")
            if isinstance(name, str) and isinstance(weight, (int, float)):
                mapped[name] = float(weight)
        return mapped

    mapped = {}
    for key, value in payload.items():
        if isinstance(value, (int, float)):
            mapped[str(key)] = float(value)
    return mapped


def load_default_weights(persona_matrix: str | None) -> dict[str, float]:
    path = Path(persona_matrix) if persona_matrix else DEFAULT_PERSONA_MATRIX
    if path.exists():
        data = load_json(str(path))
        weights = normalize_weight_map(data)
        if weights:
            return weights
    return DEFAULT_WEIGHTS.copy()


def main() -> int:
    parser = argparse.ArgumentParser(description="Weighted vote for semantic buckets")
    parser.add_argument("input", help="Buckets JSON from semantic_bucket.py")
    parser.add_argument(
        "-o", "--output", help="Output verdict JSON (defaults to stdout)"
    )
    parser.add_argument(
        "--threshold", type=float, default=0.60, help="Accept threshold in [0,1]"
    )
    parser.add_argument("--weights", help="Optional JSON file for persona weights")
    parser.add_argument(
        "--persona-matrix",
        default=str(DEFAULT_PERSONA_MATRIX),
        help="Persona matrix JSON used as default weights",
    )
    args = parser.parse_args()
    if not (0.0 <= args.threshold <= 1.0):
        raise ValueError("threshold must be in [0,1]")

    data = load_json(args.input)
    buckets = data.get("buckets", [])
    if not isinstance(buckets, list):
        raise ValueError("buckets must be a list")
    weights = load_default_weights(args.persona_matrix)
    if args.weights:
        weights.update(normalize_weight_map(load_json(args.weights)))
    # Never allow non-positive or NaN-like effective weight in voting.
    weights = {k: float(v) for k, v in weights.items() if isinstance(v, (int, float)) and v > 0}

    accepted: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []

    for bucket in buckets:
        if not isinstance(bucket, dict):
            continue
        num = 0.0
        den = 0.0
        for finding in bucket.get("findings", []):
            if not isinstance(finding, dict):
                continue
            agent = str(finding.get("agent", "Diverse Reviewer A"))
            conf = max(0.0, min(1.0, float(finding.get("confidence", 0.5))))
            weight = float(weights.get(agent, 1.0))
            penalty = 0.9 if not str(finding.get("fix_code", "")).strip() else 1.0
            num += weight * conf * penalty
            den += weight

        score = (num / den) if den else 0.0
        verdict = {
            "bucket_id": bucket.get("bucket_id"),
            "file": bucket.get("file"),
            "line": bucket.get("line"),
            "primary_type": bucket.get("primary_type"),
            "type_conflict": bucket.get("type_conflict", False),
            "evidence_count": bucket.get("evidence_count", 0),
            "score": round(score, 4),
            "consensus_strength": round(score * 10, 2),
            "findings": bucket.get("findings", []),
        }

        if score >= args.threshold:
            accepted.append(verdict)
        else:
            rejected.append(verdict)

    payload = {
        "schema_version": str(data.get("schema_version", "1.0")),
        "threshold": args.threshold,
        "weights": weights,
        "accepted": sorted(accepted, key=lambda x: x["score"], reverse=True),
        "rejected": sorted(rejected, key=lambda x: x["score"], reverse=True),
    }
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
