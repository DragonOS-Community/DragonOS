#!/usr/bin/env python3
"""Cluster raw findings into semantic buckets."""

from __future__ import annotations

import argparse
import json
import re
import sys
from typing import Any, Dict, List, Tuple


def tokenize(text: str) -> set[str]:
    # Keep tokenizer intentionally simple and deterministic for reproducibility.
    return set(re.findall(r"[a-z0-9_]+", text.lower()))


def jaccard(a: set[str], b: set[str]) -> float:
    if not a and not b:
        return 1.0
    union = a | b
    if not union:
        return 0.0
    return len(a & b) / len(union)


def load_findings(path: str) -> tuple[str, List[Dict[str, Any]]]:
    with open(path, "r", encoding="utf-8") as f:
        payload = json.load(f)
    schema_version = "1.0"
    if isinstance(payload, dict):
        schema_version = str(payload.get("schema_version", "1.0"))
        payload = payload.get("findings", payload)
    if not isinstance(payload, list):
        raise ValueError("input must be a JSON list or {findings:[...]} object")
    return schema_version, payload


def get_key_fields(item: Dict[str, Any]) -> Tuple[str, int, str, str]:
    file_path = str(item.get("file", ""))
    line = int(item.get("line", 0) or 0)
    issue_type = str(item.get("type", "unknown"))
    desc = str(item.get("description", ""))
    return file_path, line, issue_type, desc


def main() -> int:
    parser = argparse.ArgumentParser(description="Semantic deduplication for findings")
    parser.add_argument("input", help="Raw findings JSON")
    parser.add_argument(
        "-o", "--output", help="Output buckets JSON (defaults to stdout)"
    )
    parser.add_argument(
        "--sim-threshold",
        type=float,
        default=0.88,
        help="Description similarity threshold",
    )
    parser.add_argument(
        "--line-window", type=int, default=3, help="Allowed line distance for merge"
    )
    parser.add_argument(
        "--same-type-threshold",
        type=float,
        default=0.35,
        help="Minimum similarity required when merging only by same issue type",
    )
    args = parser.parse_args()
    if not (0.0 <= args.sim_threshold <= 1.0):
        raise ValueError("sim-threshold must be in [0,1]")
    if not (0.0 <= args.same_type_threshold <= 1.0):
        raise ValueError("same-type-threshold must be in [0,1]")
    if args.line_window < 0:
        raise ValueError("line-window must be >= 0")

    schema_version, findings = load_findings(args.input)
    buckets: List[Dict[str, Any]] = []

    for item in findings:
        if not isinstance(item, dict):
            continue
        f, line, issue_type, desc = get_key_fields(item)
        tokens = tokenize(desc)
        merged = False

        for b in buckets:
            same_file = b["file"] == f
            line_close = abs(b["line"] - line) <= args.line_window
            sim = jaccard(tokens, b["desc_tokens"])
            same_type = b["primary_type"] == issue_type
            can_merge = sim >= args.sim_threshold or (
                same_type and sim >= args.same_type_threshold
            )

            if same_file and line_close and can_merge:
                b["findings"].append(item)
                b["desc_tokens"] |= tokens
                b["types"].add(issue_type)
                b["severities"].add(str(item.get("severity", "minor")))
                merged = True
                break

        if not merged:
            # A bucket's anchor (file/line/primary_type) follows the first finding.
            # This keeps output deterministic and traceable.
            buckets.append(
                {
                    "file": f,
                    "line": line,
                    "primary_type": issue_type,
                    "types": {issue_type},
                    "severities": {str(item.get("severity", "minor"))},
                    "desc_tokens": tokens,
                    "findings": [item],
                }
            )

    output_buckets: list[dict[str, Any]] = []
    for idx, b in enumerate(buckets, start=1):
        output_buckets.append(
            {
                "bucket_id": f"BUG-{idx:03d}",
                "file": b["file"],
                "line": b["line"],
                "primary_type": b["primary_type"],
                "type_conflict": len(b["types"]) > 1,
                "types": sorted(b["types"]),
                "severities": sorted(b["severities"]),
                "evidence_count": len(b["findings"]),
                "findings": b["findings"],
            }
        )

    payload = {"schema_version": schema_version or "1.0", "buckets": output_buckets}
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
