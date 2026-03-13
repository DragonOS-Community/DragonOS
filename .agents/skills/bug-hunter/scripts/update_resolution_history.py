#!/usr/bin/env python3
"""Update resolution history and suggest persona weights."""

from __future__ import annotations

import argparse
import json
import os
import sys
from datetime import datetime, timezone


def load_json(path: str, default):
    if not os.path.exists(path):
        return default
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def safe_rate(numerator: int, denominator: int) -> float:
    return (numerator / denominator) if denominator else 0.0


def main() -> int:
    parser = argparse.ArgumentParser(description="Track suggestion resolution rate")
    parser.add_argument(
        "decisions",
        help="JSON list of {agent, status} where status in accepted/rejected",
    )
    parser.add_argument(
        "-o",
        "--output",
        default="artifacts/review_history.json",
        help="History output file",
    )
    parser.add_argument(
        "--weights-output",
        default="artifacts/weight_suggestion.json",
        help="Weight suggestion file",
    )
    args = parser.parse_args()

    with open(args.decisions, "r", encoding="utf-8") as f:
        decisions = json.load(f)
    if not isinstance(decisions, list):
        raise ValueError("decisions file must be a JSON list")

    history = load_json(
        args.output, {"schema_version": "1.0", "updated_at": None, "persona": {}}
    )
    persona = history.setdefault("persona", {})
    history.setdefault("schema_version", "1.0")

    for d in decisions:
        agent = str(d.get("agent", "Unknown"))
        status = str(d.get("status", "rejected")).lower()
        row = persona.setdefault(agent, {"accepted": 0, "rejected": 0, "total": 0})
        if status not in {"accepted", "rejected"}:
            continue
        row[status] += 1
        row["total"] += 1

    history["updated_at"] = datetime.now(timezone.utc).isoformat()

    resolution_total = sum(v.get("accepted", 0) for v in persona.values())
    suggestion_total = sum(v.get("total", 0) for v in persona.values())
    history["resolution_rate"] = round(safe_rate(resolution_total, suggestion_total), 4)

    os.makedirs(os.path.dirname(args.output) or ".", exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as f:
        json.dump(history, f, ensure_ascii=False, indent=2)

    suggestions = {}
    for name, stat in persona.items():
        rate = safe_rate(stat.get("accepted", 0), stat.get("total", 0))
        weight = 1.0 + 4.0 * rate
        suggestions[name] = round(max(1.0, min(5.0, weight)), 2)

    os.makedirs(os.path.dirname(args.weights_output) or ".", exist_ok=True)
    with open(args.weights_output, "w", encoding="utf-8") as f:
        json.dump(
            {"schema_version": "1.0", "suggested_weights": suggestions},
            f,
            ensure_ascii=False,
            indent=2,
        )

    sys.stdout.write(
        json.dumps(
            {
                "resolution_rate": history["resolution_rate"],
                "history": args.output,
                "weights": args.weights_output,
            },
            ensure_ascii=False,
        )
        + "\n"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
