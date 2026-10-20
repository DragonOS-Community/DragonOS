#!/usr/bin/env python3
"""Check reviewed allocation contracts against fixed Rust boundary families.

This is lexical coverage plus a human owner ledger, not a Rust AST, call graph,
reachability proof, or per-owner allocation profiler. Only git-tracked Rust files
under kernel/ are scanned. Generic heap signals are informational candidates.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

SCHEMA = "dragonos.allocation-contracts.v1"
FAMILIES = {
    "frame_run": r"\ballocate_page_frames(?:_below)?\s*\(",
    "frame_method": r"\.\s*allocate(?:_one(?:_no_reclaim)?|_below)?\s*\(",
    "dma_buffer": r"\bDmaBuffer\s*::\s*(?:try_)?alloc_(?:bytes|pages)\s*\(",
    "dma_raw": r"\b(?:try_)?dma_alloc_pages_raw\s*\(",
    "dma_wrapper": r"\b(?:try_)?dma_alloc\s*\(",
    "coherent_area": r"\bDma(?:\s*::\s*<[^;{}]*?>)?\s*::\s*new\s*\(",
    "virtqueue": r"\bVirtQueue(?:\s*::\s*<[^;{}]*?>)?\s*::\s*new(?:_boxed)?\s*\(",
    "hal_share": r"\b(?:H|Self|HalImpl|VirtioFsQueueHal)\s*::\s*(?:share|unshare)\s*\(",
    "physical_conversion": r"\b(?:virt_2_phys|virt_to_phys)\s*\(",
    "physical_identity": r"\bPhysAddr\s*::\s*new\s*\(",
    # A prefix is completed by constructor_opening: arrays and braced const
    # generics contain semicolons/braces that a flat turbofish regex loses.
    "aligned_heap": r"\bAlignedBox\s*::",
    "raw_heap": r"\b(?:alloc_zeroed|alloc_buffer_zeroed|alloc_aligned)\s*\(",
    "provider": r"\b(?:GlobalAlloc|LocalAlloc|FrameAllocator)\s+for\b",
    "cpu_backend": r"\b(?:KernelBuffer|VmAllocation)(?:\s*::\s*<[^;{}]*?>)?\s*::\s*try_new_zeroed(?:_in(?:_with)?)?\s*\(",
    "device_output": r"\bDeviceOutputBuffer\s*::\s*new\s*\(",
}
HEAP_SIGNAL = re.compile(r"\b(?:Vec|Box|Arc)\s*::\s*(?:new\w*|with_capacity|from)\s*\(")
_CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^'\\\n])'")
_RAW = re.compile(r'(?:br|cr|r)(#{0,255})"')


class AuditError(ValueError):
    pass


def rust_code(text: str) -> str:
    """Blank comments/literals, preserving offsets and newlines; no macro expansion.

    Nested block comments, raw/byte/C strings and character escapes are handled.
    Lifetimes remain code. Malformed comments/strings fail instead of hiding code.
    """
    out = list(text)

    def blank(start: int, end: int) -> None:
        for n in range(start, end):
            if out[n] != "\n":
                out[n] = " "

    i = 0
    while i < len(text):
        start = i
        if text.startswith("//", i):
            end = text.find("\n", i)
            i = len(text) if end == -1 else end
        elif text.startswith("/*", i):
            depth = 1
            i += 2
            while i < len(text) and depth:
                if text.startswith("/*", i):
                    depth += 1
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise AuditError("unterminated Rust block comment")
        elif (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")) and (raw := _RAW.match(text, i)):
            close = '"' + raw.group(1)
            end = text.find(close, raw.end())
            if end == -1:
                raise AuditError("unterminated Rust raw string")
            i = end + len(close)
        elif text[i] == '"':
            i += 1
            while i < len(text):
                if text[i] == "\\":
                    i += 2
                elif text[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            else:
                raise AuditError("unterminated Rust string")
        elif text[i] == "'" and (char := _CHAR.match(text, i)):
            i = char.end()
        else:
            i += 1
            continue
        blank(start, i)
    return "".join(out)


def tracked_sources(root: Path) -> list[str]:
    data = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z", "--", "kernel/"], text=True)
    return sorted(p for p in data.split("\0") if p.endswith(".rs"))


def constructor_opening(code: str, position: int) -> int | None:
    """Recognize this constructor's optional balanced turbofish, not a Rust AST.

    Ignore nested delimiters when counting outer angles, so array lengths,
    const shifts, tuples and braced const arguments cannot steal the close.
    Never extend a match into the next statement or a different constructor.
    """
    position += len(code[position:]) - len(code[position:].lstrip())
    if position < len(code) and code[position] == "<":
        angles = 1
        nested = []
        position += 1
        while position < len(code) and angles:
            token = code[position]
            if token in "{[(":
                nested.append(token)
            elif token in "}])":
                if not nested or nested.pop() != {"}": "{", "]": "[", ")": "("}[token]:
                    return None
            elif not nested:
                if token == ";":
                    return None
                if token == "<":
                    angles += 1
                elif token == ">" and code[position - 1] != "-":
                    angles -= 1
            position += 1
        if angles:
            return None
        separator = re.match(r"\s*::", code[position:])
        if not separator:
            return None
        position += separator.end()
    method = re.match(r"\s*(?:new|new_zeroed|new_unchecked)\s*\(", code[position:])
    return position + method.end() - 1 if method else None


def invocation_end(code: str, start: int, family: str, opening: int | None = None) -> int:
    if family == "provider":
        end = code.find("{", start)
        if end == -1:
            raise AuditError("provider without body")
        return end
    opening = code.find("(", start) if opening is None else opening
    depth = 0
    for end in range(opening, len(code)):
        if code[end] == "(":
            depth += 1
        elif code[end] == ")":
            depth -= 1
            if depth == 0:
                return end + 1
    raise AuditError("unclosed boundary invocation")


def scan_sources(root: Path, paths: list[str]) -> tuple[list[dict], dict]:
    sites = []
    heap = Counter()
    for path in sorted(paths):
        location = root / path
        if location.is_symlink() or not location.resolve().is_relative_to(root.resolve()):
            raise AuditError(f"refuse source symlink/outside root: {path}")
        source = location.read_text()
        try:
            code = rust_code(source)
            heap[path] = len(HEAP_SIGNAL.findall(code))
            ordinals = Counter()
            for family, pattern in FAMILIES.items():
                for match in re.finditer(pattern, code):
                    # Function declarations are not call sites. Provider impls
                    # intentionally remain a separate boundary family.
                    if re.search(r"\bfn\s*$", code[max(0, match.start() - 40):match.start()]):
                        continue
                    opening = None
                    if family == "aligned_heap":
                        opening = constructor_opening(code, match.end())
                        if opening is None:
                            continue
                    end = invocation_end(code, match.start(), family, opening)
                    # Preserve exact arguments, including literal contents. Erasing
                    # whitespace or literals here can merge different Rust tokens
                    # or hide changed byte lengths. Line relocation stays stable;
                    # edits inside an invocation deliberately require review.
                    anchor = source[match.start():end].strip()
                    digest = hashlib.sha256(anchor.encode()).hexdigest()
                    ordinals[(family, digest)] += 1
                    ordinal = ordinals[(family, digest)]
                    sites.append({
                        "id": f"{path}:{family}:{digest}:{ordinal}",
                        "path": path, "family": family,
                        "sha256": digest, "ordinal": ordinal,
                        "line": source.count("\n", 0, match.start()) + 1,
                        "anchor": anchor,
                    })
        except AuditError as error:
            raise AuditError(f"{path}: {error}") from error
    return sorted(sites, key=lambda item: item["id"]), {
        "kind": "informational lexical candidates, not DMA owners or semantic proof",
        "total": sum(heap.values()),
        "files": {path: count for path, count in sorted(heap.items()) if count},
    }


def validate(ledger: dict, sites: list[dict]) -> list[str]:
    errors = []
    if not isinstance(ledger, dict):
        return ["ledger must be a JSON object"]
    if not isinstance(ledger.get("owners"), list) or not isinstance(ledger.get("sites"), list):
        return ["owners and sites must be arrays"]
    if ledger.get("schema") != SCHEMA:
        errors.append("unsupported ledger schema")
    if ledger.get("families") != FAMILIES:
        errors.append("boundary families changed; review the coverage contract")
    owners = {}
    required = ("purpose", "size", "segments", "mask", "lifetime", "allocation_context",
                "release_context", "accounting", "source_status", "dynamic_evidence", "evidence")
    for owner in ledger.get("owners", []):
        if not isinstance(owner, dict):
            errors.append("owner must be an object")
            continue
        identity = owner.get("id")
        if not isinstance(identity, str) or not identity:
            errors.append("owner missing stable id")
            continue
        if identity in owners:
            errors.append(f"duplicate owner id: {identity}")
        owners[identity] = owner
        for key in required:
            if not owner.get(key):
                errors.append(f"{identity}: missing {key}")
        if not isinstance(owner.get("evidence"), list):
            errors.append(f"{identity}: evidence must be a list")
        dynamic = owner.get("dynamic_evidence")
        if not isinstance(dynamic, dict) or dynamic.get("attribution") != "unknown_global_delta":
            errors.append(f"{identity}: global snapshots cannot identify per-owner events")
    actual = {site["id"]: site for site in sites}
    reviewed = {}
    for record in ledger.get("sites", []):
        if not isinstance(record, dict):
            errors.append("reviewed site must be an object")
            continue
        identity = record.get("id")
        if not isinstance(identity, str):
            errors.append("reviewed site missing stable id")
            continue
        if set(record) != {"id", "anchor", "owner", "disposition"}:
            errors.append(f"{identity}: reviewed site must contain only id, anchor, owner, disposition")
        if identity in reviewed:
            errors.append(f"duplicate reviewed site: {identity}")
        reviewed[identity] = record
        if record.get("owner") not in owners:
            errors.append(f"{identity}: unknown owner")
        if not record.get("disposition"):
            errors.append(f"{identity}: missing disposition")
        site = actual.get(identity)
        if site is None:
            errors.append(f"stale reviewed boundary: {identity}")
        elif record.get("anchor") != site["anchor"]:
            errors.append(f"changed reviewed boundary: {identity}")
    for identity in actual.keys() - reviewed.keys():
        site = actual[identity]
        errors.append(f"unclassified boundary: {site['path']}:{site['line']} {site['family']} {identity}")
    return sorted(errors)


def audit(root: Path, ledger: dict) -> dict:
    paths = tracked_sources(root)
    sites, heap = scan_sources(root, paths)
    errors = validate(ledger, sites)
    # Evidence anchors are exact source text and are reviewed independently
    # from lexical families (e.g. callback backing and completion ownership).
    owners = ledger.get("owners", []) if isinstance(ledger, dict) else []
    if not isinstance(owners, list):
        owners = []
    for owner in owners:
        if not isinstance(owner, dict) or not isinstance(owner.get("evidence"), list):
            continue
        for evidence in owner["evidence"]:
            if not isinstance(evidence, dict):
                errors.append(f"{owner.get('id')}: evidence must be an object")
                continue
            path, anchor = evidence.get("path"), evidence.get("anchor")
            if path not in paths or not isinstance(anchor, str) or not anchor:
                errors.append(f"{owner.get('id')}: invalid/untracked evidence {path}")
            elif anchor not in (root / path).read_text():
                errors.append(f"{owner.get('id')}: stale evidence anchor {path}: {anchor}")
    return {
        "schema": "dragonos.allocation-audit.v1", "status": "fail" if errors else "ok",
        "tree_head": subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip(),
        "scope": "git-tracked kernel/**/*.rs; fixed lexical families plus reviewed owner evidence; no AST/macro/call-graph/reachability proof",
        "source_files": len(paths), "boundary_sites": len(sites),
        "families": dict(sorted(Counter(site["family"] for site in sites).items())),
        "owners": len(owners),
        "generic_heap_candidates": heap,
        "dynamic_evidence": "not produced by static audit; global deltas have unknown per-owner attribution",
        "sites": [{"id": site["id"], "path": site["path"], "line": site["line"], "family": site["family"]} for site in sites],
        "errors": sorted(errors),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--contracts", type=Path, default=Path(__file__).with_name("allocation_contracts.json"))
    parser.add_argument("--output", type=Path, help="write the complete audit JSON (otherwise stdout)")
    args = parser.parse_args()
    try:
        result = audit(args.root.resolve(), json.loads(args.contracts.read_text()))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        result = {"status": "fail", "errors": [str(error)]}
    payload = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(payload)
    else:
        sys.stdout.write(payload)
    return int(result["status"] != "ok")


if __name__ == "__main__":
    raise SystemExit(main())
