#!/usr/bin/env python3
"""Render weighted vote verdict to markdown report."""

from __future__ import annotations

import argparse
import json
import sys


SEVERITY_ORDER = {"critical": 0, "major": 1, "minor": 2}


def load_json(path: str):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def sanitize_cell(text: str) -> str:
    return text.replace("\n", " ").replace("|", "\\|").strip()


def pick_severity(findings: list[dict]) -> str:
    values = [str(f.get("severity", "minor")).lower() for f in findings]
    if not values:
        return "minor"
    return sorted(values, key=lambda s: SEVERITY_ORDER.get(s, 99))[0]


def pick_best_finding(findings: list[dict]) -> dict:
    if not findings:
        return {}
    return max(findings, key=lambda item: float(item.get("confidence", 0.0)))


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Render markdown report from verdict JSON"
    )
    parser.add_argument("input", help="Verdict JSON from weighted_vote.py")
    parser.add_argument(
        "--debate",
        help="Optional debate_candidates.json for disputed findings section",
    )
    parser.add_argument(
        "-o", "--output", help="Markdown output path (defaults to stdout)"
    )
    args = parser.parse_args()

    data = load_json(args.input)
    accepted = data.get("accepted", [])
    rejected = data.get("rejected", [])
    debate_data = load_json(args.debate) if args.debate else {"candidates": []}
    candidates = debate_data.get("candidates", [])

    accepted.sort(
        key=lambda item: (
            SEVERITY_ORDER.get(pick_severity(item.get("findings", [])), 99),
            -float(item.get("score", 0.0)),
        )
    )

    lines = []
    lines.append("## Bug Hunter Report")
    lines.append("")
    lines.append(f"- Threshold: `{data.get('threshold', 0.6)}`")
    lines.append(f"- Accepted findings: `{len(accepted)}`")
    lines.append(f"- Rejected findings: `{len(rejected)}`")
    lines.append(f"- Disputed findings: `{len(candidates)}`")
    lines.append("")
    lines.append("| 缺陷编号 | 位置 | 类型 | 严重级别 | 描述 | 建议修复 | 共识强度 |")
    lines.append("|---|---|---|---|---|---|---|")

    for item in accepted:
        findings = item.get("findings", [])
        best = pick_best_finding(findings)
        severity = pick_severity(findings)
        desc = sanitize_cell(str(best.get("description", "")))
        fix = sanitize_cell(str(best.get("fix_code", ""))) or "(需要补充修复建议)"
        position = f"{item.get('file', '')}:{item.get('line', 0)}"
        lines.append(
            "| {id} | {pos} | {typ} | {sev} | {desc} | {fix} | {score}/10 |".format(
                id=item.get("bucket_id", "-"),
                pos=sanitize_cell(position),
                typ=sanitize_cell(str(item.get("primary_type", "unknown"))),
                sev=severity,
                desc=desc,
                fix=fix,
                score=item.get("consensus_strength", 0.0),
            )
        )

    lines.append("")
    lines.append("## Developer TODO")
    lines.append("")
    if accepted:
        for item in accepted:
            findings = item.get("findings", [])
            best = pick_best_finding(findings)
            severity = pick_severity(findings)
            position = sanitize_cell(f"{item.get('file', '')}:{item.get('line', 0)}")
            desc = sanitize_cell(str(best.get("description", "")))
            fix = sanitize_cell(str(best.get("fix_code", ""))) or "补充可执行修复代码"
            owner = sanitize_cell(str(best.get("agent", "Unassigned")))
            lines.append(
                "- [ ] `{id}` `{sev}` `{pos}` owner=`{owner}`: {desc} | 修复建议: {fix}".format(
                    id=item.get("bucket_id", "-"),
                    sev=severity,
                    pos=position,
                    owner=owner,
                    desc=desc,
                    fix=fix,
                )
            )
    else:
        lines.append("- 无通过项，本轮无需修复。")

    lines.append("")
    lines.append("## Disputed Findings")
    lines.append("")
    lines.append("| 缺陷编号 | 位置 | 争议原因 | 分数 |")
    lines.append("|---|---|---|---|")
    for item in candidates:
        position = f"{item.get('file', '')}:{item.get('line', 0)}"
        lines.append(
            "| {id} | {pos} | {reason} | {score} |".format(
                id=item.get("bucket_id", "-"),
                pos=sanitize_cell(position),
                reason=sanitize_cell(str(item.get("reason", "unknown"))),
                score=item.get("score", 0.0),
            )
        )

    lines.append("")
    lines.append("## Rejected Findings")
    lines.append("")
    lines.append("| 缺陷编号 | 位置 | 类型 | 严重级别 | 分数 |")
    lines.append("|---|---|---|---|---|")
    for item in rejected:
        findings = item.get("findings", [])
        severity = pick_severity(findings)
        position = f"{item.get('file', '')}:{item.get('line', 0)}"
        lines.append(
            "| {id} | {pos} | {typ} | {sev} | {score} |".format(
                id=item.get("bucket_id", "-"),
                pos=sanitize_cell(position),
                typ=sanitize_cell(str(item.get("primary_type", "unknown"))),
                sev=severity,
                score=item.get("score", 0.0),
            )
        )

    text = "\n".join(lines) + "\n"
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
