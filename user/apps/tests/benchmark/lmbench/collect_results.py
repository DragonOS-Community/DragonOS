#!/usr/bin/env python3
"""Collect an LMbench benchmark run from the guest serial log into persisted JSON.

The guest runner (run_tests.sh) prints one ``LMBENCH_JSON`` line per metric plus
``LMBENCH_META`` / ``LMBENCH_SUMMARY`` framing. This tool parses the LAST run in the
serial log, enriches it with host/git/QEMU context, validates it against
schema/lmbench-run.schema.json, and writes:

  results/<arch>/<ts>-<commit>.json   canonical per-run snapshot
  results/history.jsonl               one line per metric (time-series friendly)
  results/github-benchmark/data.json  github-action-benchmark compatible array

Exit codes: 0 ok; 2 no results parsed; 3 schema validation failed.
"""
import argparse
import json
import os
import re
import socket
import subprocess
import sys
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
SCHEMA_VERSION = "1.0"

RUN_BEGIN = "===LMBENCH_RUN_BEGIN==="
RUN_END = "===LMBENCH_RUN_END==="
P_JSON = "LMBENCH_JSON "
P_META = "LMBENCH_META "
P_SUMMARY = "LMBENCH_SUMMARY "


def eprint(*a):
    print(*a, file=sys.stderr)


def run_git(root, *args):
    try:
        out = subprocess.run(["git", "-C", root, *args],
                             capture_output=True, text=True, timeout=15)
        if out.returncode == 0:
            return out.stdout.strip()
    except Exception:
        pass
    return None


def gather_git(root):
    commit = run_git(root, "rev-parse", "HEAD")
    if not commit:
        return None
    branch = run_git(root, "rev-parse", "--abbrev-ref", "HEAD")
    porcelain = run_git(root, "status", "--porcelain")
    return {
        "commit": commit,
        "branch": branch or "",
        "dirty": bool(porcelain),
    }


def qemu_version(arch):
    bin_ = f"qemu-system-{arch}"
    try:
        out = subprocess.run([bin_, "--version"], capture_output=True, text=True, timeout=10)
        if out.returncode == 0:
            m = re.search(r"version\s+([0-9][0-9A-Za-z.\-]*)", out.stdout)
            return m.group(1) if m else out.stdout.splitlines()[0].strip()
    except Exception:
        pass
    return None


def gather_host(arch):
    host = {"hostname": socket.gethostname()}
    qv = qemu_version(arch)
    if qv:
        host["qemu"] = qv
    if os.path.exists("/dev/kvm"):
        host["kvm"] = True
    for key, env in (("cpus", "QEMU_SMP"), ("mem_mb", "QEMU_MEMORY_MB")):
        val = os.environ.get(env)
        if val and val.isdigit():
            host[key] = int(val)
    return host


def parse_last_run(serial_path):
    """Return (meta, metrics, summary) parsed from the last run in the serial log."""
    with open(serial_path, "r", encoding="utf-8", errors="replace") as f:
        lines = f.readlines()
    # Locate the last RUN_BEGIN so a serial log spanning several boots is unambiguous.
    begin = None
    for i, ln in enumerate(lines):
        if RUN_BEGIN in ln:
            begin = i
    if begin is None:
        # No explicit frame; fall back to scanning everything (older logs).
        begin = 0
    meta, summary, metrics = None, None, []
    for ln in lines[begin:]:
        s = ln.rstrip("\n")
        idx = s.find(P_JSON)
        if idx != -1:
            metrics.append(_loads(s[idx + len(P_JSON):], "metric"))
            continue
        idx = s.find(P_META)
        if idx != -1:
            meta = _loads(s[idx + len(P_META):], "meta")
            continue
        idx = s.find(P_SUMMARY)
        if idx != -1:
            summary = _loads(s[idx + len(P_SUMMARY):], "summary")
        if RUN_END in s:
            break
    metrics = [m for m in metrics if m is not None]
    return meta, metrics, summary


def _loads(text, what):
    try:
        return json.loads(text)
    except json.JSONDecodeError as e:
        eprint(f"[collect] WARN: skipping malformed {what} JSON: {e}: {text[:120]}")
        return None


def guest_kernel_version(serial_path):
    """Best-effort: pull a DragonOS version banner out of the serial log."""
    try:
        with open(serial_path, "r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                m = re.search(r"DragonOS\s+v?[0-9][0-9A-Za-z.\-_]*", ln)
                if m:
                    return m.group(0).strip()
    except Exception:
        pass
    return None


def build_run(meta, metrics, summary, args, serial_path):
    now = datetime.now(timezone.utc)
    ts = now.strftime("%Y-%m-%dT%H:%M:%SZ")
    git = gather_git(args.root)
    short = (git["commit"][:8] if git else "nogit")
    run_id = now.strftime("%Y%m%dT%H%M%SZ") + "-" + short

    if not summary:
        ok = sum(1 for m in metrics if m.get("status") == "ok")
        failed = sum(1 for m in metrics if m.get("status") == "failed")
        skipped = sum(1 for m in metrics if m.get("status") == "skipped")
        summary = {"total": len(metrics), "ok": ok, "failed": failed, "skipped": skipped}

    cfg = {"samples": (meta or {}).get("samples", 0),
           "timeout_sec": (meta or {}).get("timeout_sec", 0)}
    if meta and "warmup" in meta:
        cfg["warmup"] = meta["warmup"]

    run = {
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "timestamp": ts,
        "arch": args.arch,
        "suite": (meta or {}).get("suite", "lmbench"),
        "config": cfg,
        "metrics": metrics,
        "summary": summary,
    }
    if meta and meta.get("suite_version"):
        run["suite_version"] = meta["suite_version"]
    if git:
        run["git"] = git
    kv = guest_kernel_version(serial_path)
    if kv:
        run["kernel_version"] = kv
    host = gather_host(args.arch)
    if host:
        run["host"] = host
    return run


def validate(run, schema_path):
    try:
        import jsonschema
    except ImportError:
        eprint("[collect] WARN: jsonschema not installed; skipping schema validation")
        return True
    with open(schema_path) as f:
        schema = json.load(f)
    try:
        jsonschema.validate(run, schema)
        return True
    except jsonschema.ValidationError as e:
        eprint("[collect] SCHEMA VALIDATION FAILED:")
        eprint("  path:", "/".join(str(p) for p in e.absolute_path) or "<root>")
        eprint("  msg :", e.message)
        return False


def write_outputs(run, outdir):
    arch = run["arch"]
    arch_dir = os.path.join(outdir, arch)
    os.makedirs(arch_dir, exist_ok=True)
    snapshot = os.path.join(arch_dir, f"{run['run_id']}.json")
    with open(snapshot, "w", encoding="utf-8") as f:
        json.dump(run, f, indent=2, ensure_ascii=False)
        f.write("\n")

    # history.jsonl: one flat record per metric
    hist = os.path.join(outdir, "history.jsonl")
    base = {"run_id": run["run_id"], "timestamp": run["timestamp"], "arch": arch,
            "commit": run.get("git", {}).get("commit")}
    with open(hist, "a", encoding="utf-8") as f:
        for m in run["metrics"]:
            rec = dict(base)
            rec.update({
                "name": m.get("name"), "category": m.get("category"),
                "metric_type": m.get("metric_type"), "unit": m.get("unit"),
                "bigger_is_better": m.get("bigger_is_better"),
                "status": m.get("status"),
            })
            st = m.get("stats") or {}
            for k in ("mean", "median", "stddev", "min", "max", "cv"):
                rec[k] = st.get(k)
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")

    # github-action-benchmark compatible array (ok metrics only)
    gh_dir = os.path.join(outdir, "github-benchmark")
    os.makedirs(gh_dir, exist_ok=True)
    gh = []
    for m in run["metrics"]:
        if m.get("status") != "ok":
            continue
        st = m["stats"]
        direction = "bigger is better" if m["bigger_is_better"] else "smaller is better"
        gh.append({
            "name": m["name"],
            "unit": m["unit"],
            "value": st["mean"],
            "extra": (f"category={m.get('category')} median={st['median']} "
                      f"stddev={st['stddev']} cv={st['cv']} n={st['count']} ({direction})"),
        })
    with open(os.path.join(gh_dir, "data.json"), "w", encoding="utf-8") as f:
        json.dump(gh, f, indent=2, ensure_ascii=False)
        f.write("\n")
    return snapshot


def print_summary(run):
    s = run["summary"]
    print(f"[collect] run_id={run['run_id']} arch={run['arch']} "
          f"commit={run.get('git', {}).get('commit', 'n/a')[:8]}")
    print(f"[collect] metrics: total={s['total']} ok={s['ok']} "
          f"failed={s['failed']} skipped={s['skipped']}")
    print(f"{'METRIC':<34} {'STATUS':<8} {'MEAN':>14} {'UNIT':<12} {'CV':>8}")
    for m in run["metrics"]:
        st = m.get("stats") or {}
        mean = f"{st['mean']:.4f}" if "mean" in st else "-"
        cv = f"{st['cv']:.4f}" if "cv" in st else "-"
        print(f"{m.get('name', '?'):<34} {m.get('status', '?'):<8} "
              f"{mean:>14} {m.get('unit', ''):<12} {cv:>8}")


def main():
    ap = argparse.ArgumentParser(description="Collect an LMbench run from serial into JSON")
    ap.add_argument("--serial", default=os.environ.get("SERIAL_FILE", "serial_opt.txt"),
                    help="guest serial log to parse")
    ap.add_argument("--schema", default=os.path.join(HERE, "schema", "lmbench-run.schema.json"))
    ap.add_argument("--outdir", default=os.path.join(HERE, "results"))
    ap.add_argument("--arch", default=os.environ.get("ARCH", "x86_64"))
    ap.add_argument("--root", default=os.environ.get("ROOT_PATH", os.getcwd()),
                    help="repo root for git metadata")
    args = ap.parse_args()

    if not os.path.isfile(args.serial):
        eprint(f"[collect] ERROR: serial log not found: {args.serial}")
        return 2

    meta, metrics, summary = parse_last_run(args.serial)
    if not metrics:
        eprint("[collect] ERROR: no LMBENCH_JSON metrics found in serial log "
               "(run did not produce results)")
        return 2

    run = build_run(meta, metrics, summary, args, args.serial)
    snapshot = write_outputs(run, args.outdir)
    print_summary(run)
    print(f"[collect] wrote {snapshot}")
    print(f"[collect] appended {os.path.join(args.outdir, 'history.jsonl')}")

    if not validate(run, args.schema):
        return 3
    print("[collect] schema validation: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
