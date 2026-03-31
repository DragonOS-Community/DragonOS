#!/usr/bin/env python3
"""Select contentious buckets for adversarial debate."""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


def load_json(path: str) -> dict[str, Any]:
    with open(path, "r", encoding="utf-8") as f:
        payload = json.load(f)
    if not isinstance(payload, dict):
        raise ValueError("input must be an object containing buckets")
    return payload


def bucket_score(bucket: dict[str, Any]) -> float:
    # Average confidence across supporting findings represents consensus confidence.
    findings = bucket.get("findings", [])
    if not findings:
        return 0.0
    return sum(float(f.get("confidence", 0.5)) for f in findings) / len(findings)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Pick debate candidates from bucket list"
    )
    parser.add_argument("input", help="Buckets JSON from semantic_bucket.py")
    parser.add_argument("-o", "--output", help="Output debate candidates JSON")
    parser.add_argument(
        "--low", type=float, default=0.50, help="Debate lower score bound"
    )
    parser.add_argument(
        "--high", type=float, default=0.60, help="Debate upper score bound"
    )
    args = parser.parse_args()
    if not (0.0 <= args.low <= 1.0 and 0.0 <= args.high <= 1.0):
        raise ValueError("low/high must be in [0,1]")
    if args.low >= args.high:
        raise ValueError("low must be smaller than high")

    data = load_json(args.input)
    buckets = data.get("buckets", [])
    if not isinstance(buckets, list):
        raise ValueError("buckets must be a list")
    candidates: list[dict[str, Any]] = []

    for b in buckets:
        if not isinstance(b, dict):
            continue
        score = bucket_score(b)
        score = max(0.0, min(1.0, score))
        conflict = bool(b.get("type_conflict", False))
        if conflict or (args.low <= score < args.high):
            candidates.append(
                {
                    "bucket_id": b.get("bucket_id"),
                    "file": b.get("file"),
                    "line": b.get("line"),
                    "score": round(score, 4),
                    "type_conflict": conflict,
                    "reason": "type_conflict" if conflict else "borderline_score",
                    "findings": b.get("findings", []),
                }
            )

    payload = {"schema_version": str(data.get("schema_version", "1.0")), "candidates": candidates}
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
