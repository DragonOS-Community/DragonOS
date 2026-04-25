#!/usr/bin/env python3
"""Redact common secrets from diff content."""

from __future__ import annotations

import argparse
import re
import sys
from typing import Pattern


# Ordered from high-confidence credential signatures to generic key/value forms.
# Keep this order stable to avoid generic patterns masking more specific ones.
PATTERNS: list[tuple[Pattern[str], str]] = [
    (
        re.compile(r"(?i)(api[_-]?key\s*[=:]\s*)([\"']?[A-Za-z0-9_\-]{16,}[\"']?)"),
        r"\1<REDACTED_API_KEY>",
    ),
    (
        re.compile(r"(?i)(secret\s*[=:]\s*)([\"']?[^\s\"']{8,}[\"']?)"),
        r"\1<REDACTED_SECRET>",
    ),
    (
        re.compile(r"(?i)(token\s*[=:]\s*)([\"']?[A-Za-z0-9_\-\.]{12,}[\"']?)"),
        r"\1<REDACTED_TOKEN>",
    ),
    (re.compile(r"AKIA[0-9A-Z]{16}"), "<REDACTED_AWS_ACCESS_KEY>"),
    (
        re.compile(
            r"(?i)-----BEGIN (?:RSA|EC|OPENSSH|PRIVATE) KEY-----[\s\S]*?-----END [A-Z ]+-----"
        ),
        "<REDACTED_PRIVATE_KEY>",
    ),
    (
        re.compile(r"(?i)(password\s*[=:]\s*)([\"']?[^\s\"']{6,}[\"']?)"),
        r"\1<REDACTED_PASSWORD>",
    ),
]


def redact(text: str) -> str:
    """Return text with known secret-like substrings replaced."""
    out = text
    for pattern, replacement in PATTERNS:
        out = pattern.sub(replacement, out)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Redact sensitive strings from diff text"
    )
    parser.add_argument("input", nargs="?", help="Input diff file (defaults to stdin)")
    parser.add_argument("-o", "--output", help="Output file (defaults to stdout)")
    args = parser.parse_args()

    # Stage1 may run in pipelines; support both file and stdin streaming modes.
    if args.input:
        with open(args.input, "r", encoding="utf-8") as f:
            src = f.read()
    else:
        src = sys.stdin.read()

    dst = redact(src)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(dst)
    else:
        sys.stdout.write(dst)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
