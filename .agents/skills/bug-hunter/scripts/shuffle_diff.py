#!/usr/bin/env python3
"""Generate deterministic shuffled diff passes for Stage1 review."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import sys
from dataclasses import dataclass


@dataclass(frozen=True)
class FileBlock:
    """A unified diff file block with optional per-file hunks."""

    header: str
    hunks: tuple[str, ...]
    path: str

    def render(self) -> str:
        if not self.hunks:
            return self.header
        return self.header + "".join(self.hunks)


def read_input(path: str | None) -> str:
    if path:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    return sys.stdin.read()


def split_file_blocks(text: str) -> list[str]:
    lines = text.splitlines(keepends=True)
    if not lines:
        return []

    blocks: list[list[str]] = []
    current: list[str] = []
    saw_git_block = False

    for line in lines:
        if line.startswith("diff --git "):
            saw_git_block = True
            if current:
                blocks.append(current)
            current = [line]
            continue
        current.append(line)

    if current:
        blocks.append(current)

    if saw_git_block:
        return ["".join(block) for block in blocks]

    # Fallback for plain patches without git headers: keep original diff as one block.
    return [text]


def parse_path(block_text: str) -> str:
    for line in block_text.splitlines():
        if line.startswith("+++ b/"):
            return line[6:].strip()
        if line.startswith("diff --git "):
            parts = line.split()
            if len(parts) >= 4 and parts[3].startswith("b/"):
                return parts[3][2:]
    return "<unknown>"


def split_hunks(block_text: str) -> FileBlock:
    lines = block_text.splitlines(keepends=True)
    header: list[str] = []
    hunks: list[list[str]] = []
    current_hunk: list[str] | None = None

    for line in lines:
        if line.startswith("@@ "):
            if current_hunk is not None:
                hunks.append(current_hunk)
            current_hunk = [line]
            continue
        if current_hunk is None:
            header.append(line)
        else:
            current_hunk.append(line)

    if current_hunk is not None:
        hunks.append(current_hunk)

    return FileBlock(
        header="".join(header),
        hunks=tuple("".join(hunk) for hunk in hunks),
        path=parse_path(block_text),
    )


def rotate_items(items: list[str], rng: random.Random) -> list[str]:
    if len(items) <= 1:
        return items[:]
    offset = rng.randrange(len(items))
    return items[offset:] + items[:offset]


def shuffle_block(block: FileBlock, rng: random.Random) -> FileBlock:
    hunks = list(block.hunks)
    if len(hunks) > 1:
        rng.shuffle(hunks)
        hunks = rotate_items(hunks, rng)
    return FileBlock(header=block.header, hunks=tuple(hunks), path=block.path)


def shuffle_passes(text: str, passes: int, seed: int) -> dict[str, object]:
    file_blocks = [split_hunks(block) for block in split_file_blocks(text)]
    rendered_original = [block.render() for block in file_blocks]

    payload_passes: list[dict[str, object]] = []
    for pass_id in range(1, passes + 1):
        rng = random.Random(seed + pass_id * 1009)
        block_order = list(file_blocks)
        if len(block_order) > 1:
            rng.shuffle(block_order)
            block_order = rotate_items(block_order, rng)
        shuffled = [shuffle_block(block, rng) for block in block_order]
        diff_text = "".join(block.render() for block in shuffled)
        payload_passes.append(
            {
                "pass_id": pass_id,
                "seed": seed + pass_id * 1009,
                "file_order": [block.path for block in shuffled],
                "block_count": len(shuffled),
                "diff": diff_text,
            }
        )

    return {
        "schema_version": "1.0",
        "strategy": "deterministic_file_and_hunk_shuffle",
        "original_block_count": len(rendered_original),
        "passes": payload_passes,
    }


def derive_seed(text: str) -> int:
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    return int(digest[:16], 16)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate deterministic shuffled diff passes"
    )
    parser.add_argument("input", nargs="?", help="Input diff file (defaults to stdin)")
    parser.add_argument(
        "--passes", type=int, default=8, help="Number of shuffled passes to emit"
    )
    parser.add_argument(
        "--seed",
        type=int,
        help="Base seed; defaults to sha256-derived stable seed from input diff",
    )
    parser.add_argument(
        "-o", "--output", help="Output JSON path (defaults to stdout)"
    )
    args = parser.parse_args()

    if args.passes < 1:
        raise ValueError("passes must be >= 1")

    text = read_input(args.input)
    seed = args.seed if args.seed is not None else derive_seed(text)
    payload = shuffle_passes(text, args.passes, seed)
    output = json.dumps(payload, ensure_ascii=False, indent=2)

    if args.output:
        os.makedirs(os.path.dirname(args.output) or ".", exist_ok=True)
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(output)
    else:
        sys.stdout.write(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
