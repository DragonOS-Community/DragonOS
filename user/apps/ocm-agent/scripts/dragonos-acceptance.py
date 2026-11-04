#!/usr/bin/env python3
"""End-to-end acceptance run for ocm-agent on DragonOS.

Boots a DragonOS image, starts ocm-agent against a protocol-compatible mock
gateway (registration + mTLS + small/large data), runs the real collector
plugins, and produces:

  bin/ocm-acceptance/latest/report.json      decoded report from the gateway
  bin/ocm-acceptance/latest/frames.ndjson    one line per received frame
  bin/ocm-acceptance/latest/agent.log        agent log copied from the guest
  bin/ocm-acceptance/latest/runtime-stats.txt guest ps/proc/load/df snapshot
  bin/ocm-acceptance/latest/qemu-transcript.log
  bin/ocm-acceptance/latest/summary.md       human-readable acceptance report

The gateway is a test double, not the production gateway service.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[4]
APP_DIR = REPO_ROOT / "user" / "apps" / "ocm-agent"
GATEWAY_DIR = APP_DIR / "scripts" / "gateway"
TEST_DIR = REPO_ROOT / "bin" / "ocm-acceptance"
RUN_DIR = TEST_DIR / "latest"
IMAGE = REPO_ROOT / "bin" / "disk-image-x86_64.img"
PART_OFFSET = 1048576
GUEST = "/opt/ocm-agent"
AGENT_ID = 1001
MACHINE_ID = "0123456789abcdef0123456789abcdef"


def log(msg: str) -> None:
    print(f"[accept] {msg}", flush=True)


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    kwargs.setdefault("check", False)
    kwargs.setdefault("text", True)
    return subprocess.run(cmd, **kwargs)


def find_tool(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise SystemExit(f"host tool not found: {name}")
    return path


def kill_stale_vms() -> None:
    """Kill QEMU instances started from this worktree (e.g. after a crash)."""
    r = run(["pgrep", "-af", f"qemu-system.*{IMAGE}"])
    for line in (r.stdout or "").splitlines():
        parts = line.split(None, 1)
        if not parts:
            continue
        pid = parts[0]
        log(f"killing stale QEMU {pid}")
        run(["sudo", "-n", "kill", "-KILL", pid])
    if r.stdout:
        time.sleep(2)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def ensure_source() -> Path:
    """Return a source tree for direct builds (DADK cache or local clone)."""
    local = APP_DIR / "upstream"
    if (local / "go.mod").exists():
        return local
    cache = REPO_ROOT / "bin" / "dadk_cache" / "source" / "ocm_agent_0_1_0"
    if (cache / "go.mod").exists():
        return cache
    log("upstream source missing; running make fetch-src")
    r = run(["make", "-C", str(APP_DIR), "fetch-src"])
    if r.returncode != 0:
        raise SystemExit("failed to fetch upstream source (run `make user` with network access)")
    return local


def build_binaries() -> tuple[Path, Path]:
    log("building agent and gateway")
    src = ensure_source()
    r = run(
        ["make", "-C", str(APP_DIR), "all", "plugins"],
        env={**os.environ, "SRC_DIR": str(src)},
    )
    if r.returncode != 0:
        raise SystemExit("agent/plugins build failed")
    agent = APP_DIR / "bin" / "ocm-agent"
    if not agent.exists():
        raise SystemExit(f"agent binary missing: {agent}")

    gateway = TEST_DIR / "ocm-gateway"
    r = run(
        ["go", "build", "-trimpath", "-ldflags", "-s -w", "-o", str(gateway), "."],
        cwd=GATEWAY_DIR,
        env={**os.environ, "CGO_ENABLED": "0"},
    )
    if r.returncode != 0:
        raise SystemExit("gateway build failed")
    return agent, gateway


def prepare_run(port: int, duration: int, agent: Path, gateway: Path) -> Path:
    if RUN_DIR.exists():
        shutil.rmtree(RUN_DIR)
    RUN_DIR.mkdir(parents=True)
    certs = RUN_DIR / "certs"

    r = run([str(gateway), "-cert-dir", str(certs), "-gen-certs", "-agent-id", str(AGENT_ID)])
    if r.returncode != 0:
        raise SystemExit("certificate generation failed")

    config = RUN_DIR / "config.yaml"
    config.write_text(
        f"""gateway_addrs:
  - "10.0.2.2:{port}"
heartbeat_interval_sec: 5
data_dir: {GUEST}/data
plugins_conf_dir: {GUEST}/plugin-conf
cgroup:
  enabled: false
log:
  level: info
  output: stdout
collector:
  enabled: false
fingerprint:
  prefer_iface: ""
taskrunner:
  workdir_ttl_sec: 3600
  workdir_cleanup_interval_sec: 600
large_data:
  req_new_wait_timeout_ms: 10000
  session_timeout_sec: 120
"""
    )

    os_monitor_conf = RUN_DIR / "os_monitor.conf.yml"
    os_monitor_conf.write_text(
        """app:
  tick_interval: 3
  push_interval: 5
  log_level: "info"
cpu:
  usage: {interval: 5}
  loadavg: {interval: 5}
mem:
  usage: {interval: 5}
net:
  flow: {interval: 5}
disk:
  usage: {interval: 10}
  util: {interval: 10}
"""
    )

    start = RUN_DIR / "acceptance-start.sh"
    start.write_text(
        "#!/bin/sh\n"
        "rm -f /opt/ocm-agent/data/client.crt /opt/ocm-agent/data/client.key\n"
        "rm -f /opt/ocm-agent/data/ongoing.db /opt/ocm-agent/data/ongoing.db-shm /opt/ocm-agent/data/ongoing.db-wal\n"
        "rm -rf /opt/ocm-agent/data/tasks\n"
        "rm -f /opt/ocm-agent/smoke.log\n"
        "/opt/ocm-agent/ocm-agent -config /opt/ocm-agent/config.yaml "
        ">/opt/ocm-agent/smoke.log 2>&1 &\n"
        "echo $! > /opt/ocm-agent/agent.pid\n"
        "echo SMOKE_STARTED=$!\n"
    )
    large = RUN_DIR / "acceptance-large.sh"
    large.write_text(
        "#!/bin/sh\n"
        "/opt/ocm-agent/ocm-gateway -push-socket /opt/ocm-agent/var/chan.sock "
        "-push-bytes 120000\n"
        "echo LARGE_DONE\n"
    )
    stats = RUN_DIR / "acceptance-stats.sh"
    stats.write_text(
        "#!/bin/sh\n"
        "{\n"
        "  echo '### uname'; uname -a\n"
        "  echo '### uptime'; cat /proc/uptime\n"
        "  echo '### loadavg'; cat /proc/loadavg\n"
        "  echo '### meminfo'; head -8 /proc/meminfo\n"
        "  echo '### ps'; ps\n"
        "  PID=$(cat /opt/ocm-agent/agent.pid)\n"
        "  echo \"### agent pid=$PID\"\n"
        "  tr -d '\\000' < /proc/$PID/status | grep -E '^(Name|State|Threads|VmRSS|VmSize|VmPeak):'\n"
        "  echo '### agent statm'; cat /proc/$PID/statm\n"
        "  echo '### agent tasks'; ls /proc/$PID/task | wc -l\n"
        "  echo '### df'; df -P -B 1 /\n"
        "  echo '### plugin dirs'; ls -l /opt/ocm-agent/plugins/*/\n"
        "  echo '### data dir'; ls -l /opt/ocm-agent/data\n"
        "  echo '### log tail'; tail -40 /opt/ocm-agent/smoke.log\n"
        "} > /opt/ocm-agent/runtime-stats.txt 2>&1\n"
        "echo STATS_DONE\n"
    )
    return config


def inject(agent: Path, gateway: Path, config: Path) -> None:
    mount = find_tool("mount")
    umount = find_tool("umount")
    install = find_tool("install")
    mountpoint = RUN_DIR / "mnt"
    mountpoint.mkdir()
    run(["sudo", "-n", mount, "-o", f"loop,rw,offset={PART_OFFSET}", str(IMAGE), str(mountpoint)], check=True)
    try:
        for d in (
            "data", "var", "plugin-conf", "plugins/os_monitor", "plugins/sysinfo_collector",
        ):
            run(["sudo", "-n", "mkdir", "-p", str(mountpoint / GUEST.strip("/") / d)], check=True)
        root = mountpoint / GUEST.strip("/")
        run(["sudo", "-n", install, "-m", "755", str(agent), str(root / "ocm-agent")], check=True)
        run(["sudo", "-n", install, "-m", "755", str(gateway), str(root / "ocm-gateway")], check=True)
        run(["sudo", "-n", install, "-m", "644", str(config), str(root / "config.yaml")], check=True)
        run(["sudo", "-n", install, "-m", "600", str(RUN_DIR / "certs" / "ca.crt"), str(root / "data" / "ca.crt")], check=True)
        run(["sudo", "-n", "rm", "-f",
             str(root / "data" / "client.crt"), str(root / "data" / "client.key"),
             str(root / "data" / "ongoing.db"), str(root / "data" / "ongoing.db-shm"),
             str(root / "data" / "ongoing.db-wal")])
        run(["sudo", "-n", "rm", "-rf", str(root / "data" / "tasks")])
        run(["sudo", "-n", "find", str(root / "plugin-conf"), "-type", "f", "-delete"])
        for stale in ("diag", "plugin-smoke.sh", "plugin-ran", "smoke.log", "runtime-stats.txt"):
            run(["sudo", "-n", "rm", "-f", str(root / stale)])

        app = APP_DIR
        run(["sudo", "-n", install, "-m", "755", str(app / "bin/plugins/os_monitor/os_monitor"),
             str(root / "plugins/os_monitor/os_monitor")], check=True)
        run(["sudo", "-n", install, "-m", "644", str(RUN_DIR / "os_monitor.conf.yml"),
             str(root / "plugins/os_monitor/app.conf.yml")], check=True)
        run(["sudo", "-n", install, "-m", "755", str(app / "bin/plugins/sysinfo_collector/sysinfo_collector"),
             str(root / "plugins/sysinfo_collector/sysinfo_collector")], check=True)
        # `make plugins` stages the collector YAMLs next to the binaries, so
        # the acceptance run does not depend on the source checkout location.
        for name in ("os_monitor", "sysinfo_collector"):
            run(["sudo", "-n", install, "-m", "644",
                 str(app / "bin" / "plugin-conf" / f"{name}.yml"),
                 str(root / "plugin-conf" / f"{name}.yml")], check=True)
        for script in ("acceptance-start.sh", "acceptance-large.sh", "acceptance-stats.sh"):
            run(["sudo", "-n", install, "-m", "755", str(RUN_DIR / script), str(root / script)], check=True)
        # Deterministic host identity for registration (the minimal image may
        # not ship /etc/machine-id).
        machine_id = mountpoint / "etc" / "machine-id"
        run(["sudo", "-n", "mkdir", "-p", str(machine_id.parent)])
        run(["sudo", "-n", "sh", "-c", f"printf '%s\\n' {MACHINE_ID} > {machine_id}"], check=True)
        run(["sync"], check=False)
    finally:
        run(["sudo", "-n", umount, str(mountpoint)], check=True)


def start_gateway(gateway: Path, port: int, duration: int) -> subprocess.Popen:
    ready = RUN_DIR / "gateway.ready"
    if ready.exists():
        ready.unlink()
    logf = open(RUN_DIR / "gateway.log", "w")
    proc = subprocess.Popen(
        [str(gateway), "-cert-dir", str(RUN_DIR / "certs"), "-listen", f"127.0.0.1:{port}",
         "-agent-id", str(AGENT_ID), "-duration", f"{duration + 120}s",
         "-out", str(RUN_DIR), "-ready-file", str(ready)],
        stdout=logf, stderr=subprocess.STDOUT,
    )
    for _ in range(100):
        if ready.exists():
            return proc
        if proc.poll() is not None:
            raise SystemExit("gateway exited during startup")
        time.sleep(0.1)
    raise SystemExit("gateway did not become ready")


def boot_and_run(duration: int, settle: int) -> tuple[str, str]:
    import pexpect

    env = dict(os.environ)
    env.setdefault("QEMU_ACCEL", "kvm")
    runner = shlex.split(os.environ.get("OCM_SMOKE_RUNNER", "make qemu-nographic"))
    child = pexpect.spawn(
        runner[0], runner[1:], cwd=str(REPO_ROOT), env=env,
        encoding="utf-8", codec_errors="replace", timeout=300,
        dimensions=(40, 200),
    )
    transcript: list[str] = []

    def record() -> None:
        transcript.append(child.before or "")
        transcript.append(child.after if isinstance(child.after, str) else "")

    try:
        child.expect([r"Please press Enter[^\r\n]*", r"root@[^\r\n]*[#$] "], timeout=300)
        record()
        if "Please press Enter" in (child.after or ""):
            child.sendline("")
            child.expect(r"root@[^\r\n]*[#$] ", timeout=120)
            record()

        child.sendline("sh /opt/ocm-agent/acceptance-start.sh")
        child.expect(r"SMOKE_STARTED", timeout=30)
        record()
        started = time.time()
        time.sleep(settle)

        child.sendline("sh /opt/ocm-agent/acceptance-large.sh")
        child.expect(r"LARGE_DONE", timeout=90)
        record()

        remaining = duration - (time.time() - started)
        if remaining > 0:
            time.sleep(remaining)

        child.sendline("sh /opt/ocm-agent/acceptance-stats.sh")
        child.expect(r"STATS_DONE", timeout=60)
        record()
        time.sleep(1)
        child.sendline("sync; echo SYNC_DONE")
        child.expect(r"SYNC_DONE", timeout=60)
        record()
        time.sleep(1)
        # The minimal image has no poweroff applet; sync already flushed the
        # log, so terminate QEMU through the mux escape.
        child.sendcontrol("a")
        child.send("x")
        try:
            child.expect(pexpect.EOF, timeout=15)
        except Exception:  # noqa: BLE001
            child.close(force=True)
        record()
    finally:
        # The pexpect child is `make`, which execs sudo/qemu in the same
        # process group; close() alone can leave the VM alive.
        try:
            os.killpg(os.getpgid(child.pid), signal.SIGKILL)
        except Exception:  # noqa: BLE001
            pass
        try:
            child.close(force=True)
        except Exception:  # noqa: BLE001
            pass
    return "".join(transcript), ""


def read_from_image(name: str) -> str:
    mount = find_tool("mount")
    umount = find_tool("umount")
    mountpoint = RUN_DIR / "mnt-ro"
    mountpoint.mkdir(exist_ok=True)
    run(["sudo", "-n", mount, "-o", f"loop,rw,offset={PART_OFFSET}", str(IMAGE), str(mountpoint)], check=True)
    try:
        path = mountpoint / GUEST.strip("/") / name
        return path.read_text(errors="replace") if path.exists() else ""
    finally:
        run(["sudo", "-n", umount, str(mountpoint)], check=True)


def parse_agent_log(text: str) -> dict:
    result = {"errors": [], "warnings": [], "plugins": [], "small_metrics": None, "large_metrics": None}
    for line in text.splitlines():
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        level = entry.get("level")
        msg = entry.get("msg", "")
        if level == "error":
            result["errors"].append(entry)
        elif level == "warn":
            result["warnings"].append(entry)
        if msg == "Loaded plugin":
            result["plugins"].append(entry.get("plugin"))
        if msg == "small-data metrics":
            result["small_metrics"] = entry
        if msg == "large-data metrics":
            result["large_metrics"] = entry
    return result


def truncate(value, n: int = 96) -> str:
    s = str(value)
    return s if len(s) <= n else s[: n - 3] + "..."


def render_summary(report: dict, agent: dict, runtime: str, transcript: str) -> str:
    lines: list[str] = []
    lines.append("# ocm-agent DragonOS 端到端验收报告\n")
    lines.append(f"- 生成时间：{time.strftime('%Y-%m-%d %H:%M:%S')}")
    lines.append(f"- 镜像：`{IMAGE}`")
    lines.append(f"- gateway：TLS 1.3 mock（协议兼容，非生产 gateway）")
    lines.append("")

    lines.append("## 1. 连接与注册\n")
    lines.append("| 项 | 值 |\n|---|---|")
    for c in report.get("connections", []):
        lines.append(f"| 连接 | {c.get('remote')} {c.get('tls_version')} CN=`{c.get('client_cn','')}` |")
    reg = report.get("register") or {}
    lines.append(f"| REGISTER | uuid=`{reg.get('uuid')}` ip=`{reg.get('ip')}` mac=`{reg.get('mac')}` |")
    lines.append(f"| 下发 agent_id | `{reg.get('agent_id')}`（客户端证书 CN=`{reg.get('cert_cn')}`）|")
    lines.append("")

    lines.append("## 2. 帧与 ACK\n")
    lines.append("| 指标 | 值 |\n|---|---|")
    for name, count in sorted(report.get("frame_counts", {}).items()):
        lines.append(f"| {name} | {count} |")
    lines.append(f"| DATA_SMALL payload bytes | {report.get('bytes_in', 0)} |")
    lines.append(f"| ACK ok / fail | {report.get('acks', 0)} / {report.get('ack_failures', 0)} |")
    if agent.get("small_metrics"):
        m = agent["small_metrics"]
        lines.append(f"| agent sent / ack | {m.get('sent_total')} / {m.get('ack_success_total')} |")
    lines.append("")

    lines.append("## 3. 采集插件\n")
    lines.append("| 插件 | 版本 | 帧 | 压缩字节 | 指标数 | 指标名数 |\n|---|---|---:|---:|---:|---:|")
    for name, p in sorted(report.get("plugins", {}).items()):
        lines.append(
            f"| {name} | {p.get('version','')} | {p.get('frames',0)} | {p.get('bytes_in',0)} "
            f"| {p.get('metrics_total',0)} | {len(p.get('metric_names',{}))} |"
        )
    lines.append("")

    for name, p in sorted(report.get("plugins", {}).items()):
        if name == "large_test":
            continue
        lines.append(f"### {name}\n")
        attrs = p.get("attr") or {}
        if attrs:
            lines.append("属性：" + ", ".join(f"`{k}={truncate(v, 64)}`" for k, v in sorted(attrs.items())) + "\n")
        groups = p.get("groups") or []
        if groups:
            lines.append("分组：" + ", ".join(f"`{g['name'] or '(unnamed)'}`×{g['metrics']}" for g in groups) + "\n")
        names = p.get("metric_names") or {}
        rows = sorted(names.items(), key=lambda kv: (-kv[1].get("count", 0), kv[0]))
        lines.append("| 指标 | 次数 | 最后值 | 标签 |\n|---|---:|---|---|")
        for metric_name, stat in rows[:120]:
            tags = stat.get("last_tags") or {}
            tag_text = ", ".join(f"{k}={truncate(v, 32)}" for k, v in sorted(tags.items()))
            lines.append(
                f"| `{metric_name}` | {stat.get('count',0)} | {truncate(stat.get('last_value'), 72)} | {truncate(tag_text, 80)} |"
            )
        if len(rows) > 120:
            lines.append(f"| …… | | 其余 {len(rows)-120} 个指标见 report.json | |")
        lines.append("")

    lines.append("## 4. 大数据拉取链路（合成 120KB 载荷）\n")
    sessions = report.get("large_sessions") or []
    if sessions:
        lines.append("| task_id | data_id | 总大小 | 分片 | 插件 | 指标数 |\n|---:|---:|---:|---:|---|---:|")
        for s in sessions:
            lines.append(
                f"| {s.get('task_id')} | {s.get('data_id')} | {s.get('total_size')} | {s.get('fragments')} "
                f"| {s.get('plugin')} | {s.get('metrics')} |"
            )
    else:
        lines.append("未触发大数据链路。")
    lines.append("")

    lines.append("## 5. DragonOS 运行效果\n")
    lines.append("```text")
    lines.append(runtime.strip()[:12000] or "(未采集到 runtime-stats)")
    lines.append("```\n")

    lines.append("## 6. Agent 日志摘要\n")
    lines.append(f"- 已加载插件：{', '.join(agent.get('plugins') or []) or '(none)'}")
    lines.append(f"- error 条数：{len(agent.get('errors', []))}")
    for e in agent.get("errors", [])[:10]:
        lines.append(f"  - `{e.get('msg')}`: {truncate(e.get('error',''), 160)}")
    lines.append(f"- warn 条数：{len(agent.get('warnings', []))}")
    for w in agent.get("warnings", [])[:10]:
        lines.append(f"  - `{w.get('msg')}`: {truncate(w.get('error',''), 160)}")
    if agent.get("large_metrics"):
        lines.append(f"- large-data 计数：`{agent['large_metrics']}`")
    lines.append("")
    lines.append("> 说明：`large_test` / `synthetic_large_metric` 是协议健壮性测试注入的合成数据，不是真实采集结果。")
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration", type=int, default=60, help="seconds the agent runs in the guest")
    parser.add_argument("--settle", type=int, default=20, help="seconds to wait before the large-payload test")
    args = parser.parse_args()

    if not IMAGE.exists():
        raise SystemExit(f"image missing: {IMAGE}\nRun: make user && SKIP_GRUB=1 make write_diskimage")

    TEST_DIR.mkdir(parents=True, exist_ok=True)
    kill_stale_vms()
    agent, gateway = build_binaries()
    port = free_port()
    config = prepare_run(port, args.duration, agent, gateway)
    log(f"injecting into {IMAGE}")
    inject(agent, gateway, config)

    log(f"starting gateway on 127.0.0.1:{port}")
    gw = start_gateway(gateway, port, args.duration)
    try:
        log("booting DragonOS")
        transcript, _ = boot_and_run(args.duration, args.settle)
        (RUN_DIR / "qemu-transcript.log").write_text(transcript)
    finally:
        gw.terminate()
        try:
            gw.wait(timeout=20)
        except subprocess.TimeoutExpired:
            gw.kill()

    report = json.loads((RUN_DIR / "report.json").read_text())
    agent_log_text = read_from_image("smoke.log")
    (RUN_DIR / "agent.log").write_text(agent_log_text)
    runtime = read_from_image("runtime-stats.txt")
    (RUN_DIR / "runtime-stats.txt").write_text(runtime)
    agent = parse_agent_log(agent_log_text)

    summary = render_summary(report, agent, runtime, transcript)
    (RUN_DIR / "summary.md").write_text(summary)
    print(summary[:20000])

    failures: list[str] = []
    if not report.get("register"):
        failures.append("no REGISTER frame")
    if not any(c.get("client_cn") == str(AGENT_ID) for c in report.get("connections", [])):
        failures.append("no mTLS connection with issued certificate")
    if report.get("frame_counts", {}).get("HEARTBEAT", 0) == 0:
        failures.append("no HEARTBEAT")
    if report.get("frame_counts", {}).get("DATA_SMALL", 0) == 0:
        failures.append("no DATA_SMALL")
    if report.get("acks", 0) == 0:
        failures.append("no DATA_SMALL ACK")
    plugins = report.get("plugins", {})
    if plugins.get("os_monitor", {}).get("metrics_total", 0) == 0:
        failures.append("os_monitor produced no metrics")
    if plugins.get("sysinfo_collector", {}).get("metrics_total", 0) == 0:
        failures.append("sysinfo_collector produced no metrics")
    if not report.get("large_sessions"):
        failures.append("large-data session was not completed")
    if any("panic" in e.get("msg", "").lower() for e in agent.get("errors", [])):
        failures.append("agent panicked")

    if failures:
        print("\nFAILURES:", file=sys.stderr)
        for f in failures:
            print("  -", f, file=sys.stderr)
        return 1
    log("PASS")
    log(f"artifacts: {RUN_DIR}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
