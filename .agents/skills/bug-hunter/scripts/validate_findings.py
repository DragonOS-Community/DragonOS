#!/usr/bin/env python3
"""Validate and normalize bug-hunter raw findings."""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


VALID_TYPES = {"security", "concurrency", "performance", "logic"}
VALID_SEVERITIES = {"critical", "major", "minor"}


def _coerce_line(value: Any) -> int:
    # bool is a subclass of int in Python, reject it explicitly.
    if isinstance(value, bool):
        raise ValueError("line must be an integer")
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    raise ValueError("line must be an integer")


def _coerce_confidence(value: Any) -> float:
    # Accept JSON numbers and numeric strings for backward compatibility.
    if isinstance(value, bool):
        raise ValueError("confidence must be a number in [0,1]")
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError as exc:
            raise ValueError("confidence must be a number in [0,1]") from exc
    raise ValueError("confidence must be a number in [0,1]")


def load_findings(path: str) -> tuple[str, list[dict[str, Any]]]:
    with open(path, "r", encoding="utf-8") as f:
        payload = json.load(f)

    if isinstance(payload, list):
        return "1.0", payload
    if isinstance(payload, dict) and isinstance(payload.get("findings"), list):
        version = str(payload.get("schema_version", "1.0"))
        return version, payload["findings"]
    raise ValueError("input must be a list or an object with findings field")


def normalize_item(
    item: Any, idx: int, require_agent: bool
) -> tuple[dict[str, Any], list[str]]:
    errors: list[str] = []
    if not isinstance(item, dict):
        return {}, [f"item[{idx}] must be an object"]

    # Work on a shallow copy so we can normalize in-place without touching input.
    normalized = dict(item)

    # Keep required fields aligned with CONTRACTS.md.
    # fix_code is optional and handled as empty string when absent.
    required_fields = [
        "file",
        "line",
        "type",
        "severity",
        "description",
        "confidence",
    ]
    for field in required_fields:
        if field not in normalized:
            errors.append(f"item[{idx}] missing required field: {field}")

    if "file" in normalized:
        file_path = normalized.get("file")
        if not isinstance(file_path, str) or not file_path.strip():
            errors.append(f"item[{idx}] file must be non-empty string")
        else:
            normalized["file"] = file_path.strip()

    if "line" in normalized:
        try:
            line = _coerce_line(normalized.get("line"))
            if line < 1:
                raise ValueError("line must be >=1")
            normalized["line"] = line
        except ValueError as exc:
            errors.append(f"item[{idx}] {exc}")

    if "type" in normalized:
        issue_type = str(normalized.get("type", "")).strip().lower()
        if issue_type not in VALID_TYPES:
            errors.append(
                f"item[{idx}] type must be one of: {', '.join(sorted(VALID_TYPES))}"
            )
        else:
            normalized["type"] = issue_type

    if "severity" in normalized:
        severity = str(normalized.get("severity", "")).strip().lower()
        if severity not in VALID_SEVERITIES:
            errors.append(
                "item[{idx}] severity must be one of: {values}".format(
                    idx=idx, values=", ".join(sorted(VALID_SEVERITIES))
                )
            )
        else:
            normalized["severity"] = severity

    if "description" in normalized:
        desc = normalized.get("description")
        if not isinstance(desc, str) or not desc.strip():
            errors.append(f"item[{idx}] description must be non-empty string")
        else:
            normalized["description"] = desc.strip()

    if "fix_code" in normalized:
        fix_code = normalized.get("fix_code")
        if not isinstance(fix_code, str):
            errors.append(f"item[{idx}] fix_code must be string")
        else:
            normalized["fix_code"] = fix_code
    else:
        normalized["fix_code"] = ""

    if "confidence" in normalized:
        try:
            confidence = _coerce_confidence(normalized.get("confidence"))
            if confidence < 0 or confidence > 1:
                raise ValueError("confidence must be in [0,1]")
            normalized["confidence"] = confidence
        except ValueError as exc:
            errors.append(f"item[{idx}] {exc}")

    agent = normalized.get("agent")
    if agent is None or (isinstance(agent, str) and not agent.strip()):
        if require_agent:
            errors.append(f"item[{idx}] agent is required in strict mode")
        else:
            normalized["agent"] = "Diverse Reviewer A"
    elif not isinstance(agent, str):
        errors.append(f"item[{idx}] agent must be string")
    else:
        normalized["agent"] = agent.strip()

    return normalized, errors


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate and normalize bug-hunter raw findings"
    )
    parser.add_argument("input", help="Raw findings JSON path")
    parser.add_argument(
        "-o", "--output", help="Output normalized findings (defaults to stdout)"
    )
    parser.add_argument(
        "--require-agent",
        action="store_true",
        help="Fail if any finding misses agent",
    )
    args = parser.parse_args()

    schema_version, findings = load_findings(args.input)
    if not isinstance(findings, list):
        raise ValueError("findings must be a list")

    normalized_items: list[dict[str, Any]] = []
    errors: list[str] = []

    # Keep deterministic index-based errors so user can map back to raw payload.
    for idx, item in enumerate(findings):
        normalized, item_errors = normalize_item(item, idx, args.require_agent)
        if item_errors:
            errors.extend(item_errors)
        else:
            normalized_items.append(normalized)

    if errors:
        sys.stderr.write("validation failed:\n")
        for err in errors:
            sys.stderr.write(f"- {err}\n")
        return 2

    payload = {
        "schema_version": schema_version or "1.0",
        "findings": normalized_items,
    }
    text = json.dumps(payload, ensure_ascii=False, indent=2)

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
