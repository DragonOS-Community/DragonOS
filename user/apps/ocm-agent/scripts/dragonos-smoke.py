#!/usr/bin/env python3
"""Boot DragonOS in QEMU and smoke-test the ported ocm-agent.

The script is intentionally self-contained and host-only:

  1. creates a throwaway CA/server/client certificate set (mTLS, TLS 1.3);
  2. injects the agent configuration and certificates into the FAT rootfs image;
  3. starts a TLS sink on the host that plays the gateway role;
  4. boots DragonOS with `nix run .#start-x86_64`, logs in and starts the agent;
  5. asserts that the agent reaches its steady state and that TLS completed.

Usage:
    python3 user/apps/ocm-agent/scripts/dragonos-smoke.py
"""

from __future__ import annotations

import argparse
import os
import re
import shlex
import shutil
import socket
import ssl
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[4]
TEST_DIR = REPO_ROOT / "bin" / "ocm-agent-smoke"
IMAGE = REPO_ROOT / "bin" / "disk-image-x86_64.img"
GUEST_DIR = "/opt/ocm-agent"
GUEST_LOG = "/tmp/ocm-agent.log"
AGENT_ID = 123
SMOKE_RUN_SECONDS = int(os.environ.get("OCM_SMOKE_SECONDS", "12"))


def log(msg: str) -> None:
    print(f"[smoke] {msg}", flush=True)


def find_tool(name: str) -> str:
    found = shutil.which(name)
    if found:
        return found
    raise SystemExit(f"host tool not found: {name}")


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    kwargs.setdefault("check", False)
    kwargs.setdefault("text", True)
    return subprocess.run(cmd, **kwargs)


def make_certs() -> None:
    data = TEST_DIR / "data"
    data.mkdir(parents=True, exist_ok=True)
    for stale in data.iterdir():
        if stale.is_file():
            stale.unlink()

    ca_key = data / "ca.key"
    ca_crt = data / "ca.crt"
    run(
        [
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(ca_key), "-out", str(ca_crt), "-days", "2",
            "-subj", "/CN=DragonOS ocm-agent smoke CA",
        ],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    san = TEST_DIR / "san.ext"
    san.write_text("subjectAltName=IP:10.0.2.2,IP:127.0.0.1\n")

    server_key = data / "server.key"
    server_csr = TEST_DIR / "server.csr"
    server_crt = data / "server.crt"
    run(
        [
            "openssl", "req", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(server_key), "-out", str(server_csr),
            "-subj", "/CN=10.0.2.2",
        ],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    run(
        [
            "openssl", "x509", "-req", "-in", str(server_csr),
            "-CA", str(ca_crt), "-CAkey", str(ca_key), "-CAcreateserial",
            "-out", str(server_crt), "-days", "2", "-extfile", str(san),
        ],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    client_key = data / "client.key"
    client_csr = TEST_DIR / "client.csr"
    client_crt = data / "client.crt"
    run(
        [
            "openssl", "req", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(client_key), "-out", str(client_csr),
            "-subj", f"/CN={AGENT_ID}",
        ],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    run(
        [
            "openssl", "x509", "-req", "-in", str(client_csr),
            "-CA", str(ca_crt), "-CAkey", str(ca_key), "-CAcreateserial",
            "-out", str(client_crt), "-days", "2",
        ],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def write_config(port: int, data_dir: str = f"{GUEST_DIR}/data") -> Path:
    config = TEST_DIR / "config.yaml"
    config.write_text(
        f"""gateway_addrs:
  - "10.0.2.2:{port}"
heartbeat_interval_sec: 5
data_dir: {data_dir}
plugins_conf_dir: {GUEST_DIR}/plugin-conf
cgroup:
  enabled: false
log:
  level: debug
  output: stdout
collector:
  enabled: false
fingerprint:
  prefer_iface: ""
taskrunner:
  workdir_ttl_sec: 3600
  workdir_cleanup_interval_sec: 600
"""
    )
    return config


def image_arg() -> str:
    return f"{IMAGE}@@{PART_OFFSET}"


def inject(config: Path, binary: Path, data_dir: str, with_client_cert: bool, plugins: bool = False) -> None:
    """Mount the ext4 rootfs image and install the test payload."""
    mountpoint = TEST_DIR / "mnt"
    mountpoint.mkdir(parents=True, exist_ok=True)
    mount = find_tool("mount")
    umount = find_tool("umount")
    subprocess.run(
        ["sudo", "-n", mount, "-o", "loop,rw,offset=1048576", str(IMAGE), str(mountpoint)],
        check=True,
    )
    try:
        data = mountpoint / data_dir.removeprefix("/")
        plugin_conf_dir = mountpoint / "opt" / "ocm-agent" / "plugin-conf"
        run(["sudo", "-n", "mkdir", "-p", str(plugin_conf_dir), str(data)], check=True)
        # Keep each smoke mode deterministic: drop plugin configs from a
        # previous run before installing this mode's payload.
        run(["sudo", "-n", "find", str(plugin_conf_dir), "-name", "*.yml", "-delete"])
        if not with_client_cert:
            for stale in ("client.crt", "client.key", "ongoing.db", "ongoing.db-shm", "ongoing.db-wal"):
                run(["sudo", "-n", "rm", "-f", str(data / stale)])
        install = find_tool("install")
        run(["sudo", "-n", install, "-m", "755", str(binary),
             str(mountpoint / "opt" / "ocm-agent" / "ocm-agent")], check=True)
        run(["sudo", "-n", install, "-m", "644", str(config),
             str(mountpoint / "opt" / "ocm-agent" / "config.yaml")], check=True)
        certs = ["ca.crt"]
        if with_client_cert:
            certs += ["client.crt", "client.key"]
        for name in certs:
            run(["sudo", "-n", install, "-m", "600", str(TEST_DIR / "data" / name),
                 str(data / name)], check=True)
        if plugins:
            plugin_root = REPO_ROOT / "user" / "apps" / "ocm-agent"
            plugin_bin = plugin_root / "bin" / "plugins" / "os_monitor" / "os_monitor"
            if not plugin_bin.exists():
                raise SystemExit(f"os_monitor not built: {plugin_bin} (run make -C user/apps/ocm-agent plugins)")
            run(["sudo", "-n", "mkdir", "-p",
                 str(mountpoint / "opt" / "ocm-agent" / "plugins" / "os_monitor")], check=True)
            run(["sudo", "-n", install, "-m", "755", str(plugin_bin),
                 str(mountpoint / "opt" / "ocm-agent" / "plugins" / "os_monitor" / "os_monitor")],
                check=True)
            app_conf = TEST_DIR / "os_monitor.conf.yml"
            app_conf.write_text(
                "app:\n"
                "  tick_interval: 2\n"
                "  push_interval: 5\n"
                "  log_level: \"info\"\n"
                "cpu:\n"
                "  usage: {interval: 5}\n"
                "  loadavg: {interval: 5}\n"
                "mem:\n"
                "  usage: {interval: 5}\n"
                "net:\n"
                "  flow: {interval: 5}\n"
                "disk:\n"
                "  usage: {interval: 5}\n"
                "  util: {interval: 5}\n"
            )
            run(["sudo", "-n", install, "-m", "644", str(app_conf),
                 str(mountpoint / "opt" / "ocm-agent" / "plugins" / "os_monitor" / "app.conf.yml")],
                check=True)
            os_conf = TEST_DIR / "os_monitor.yml"
            os_conf.write_text(
                "name: os_monitor\n"
                "cron: \"*/6 * * * *\"\n"
                "exec_path: /opt/ocm-agent/plugins/os_monitor/os_monitor\n"
                "args:\n"
                "timeout: -1\n"
                "version: v1.0.0\n"
                "enable: true\n"
                "run_on_boot: true\n"
            )
            run(["sudo", "-n", install, "-m", "644", str(os_conf),
                 str(mountpoint / "opt" / "ocm-agent" / "plugin-conf" / "os_monitor.yml")],
                check=True)

        if with_client_cert:
            plugin_script = TEST_DIR / "plugin-smoke.sh"
            plugin_script.write_text("#!/bin/sh\necho ran > /opt/ocm-agent/plugin-ran\n")
            plugin_conf = TEST_DIR / "smoke-plugin.yaml"
            plugin_conf.write_text(
                "name: smoke\n"
                "version: \"1\"\n"
                "cron: \"*/10 * * * * *\"\n"
                "exec_path: /opt/ocm-agent/plugin-smoke.sh\n"
                "args: \"\"\n"
                "timeout: -1\n"
                "enable: true\n"
                "run_on_boot: true\n"
            )
            run(["sudo", "-n", install, "-m", "755", str(plugin_script),
                 str(mountpoint / "opt" / "ocm-agent" / "plugin-smoke.sh")], check=True)
            run(["sudo", "-n", install, "-m", "644", str(plugin_conf),
                 str(mountpoint / "opt" / "ocm-agent" / "plugin-conf" / "smoke.yaml")], check=True)
            run(["sudo", "-n", "rm", "-f",
                 str(mountpoint / "opt" / "ocm-agent" / "plugin-ran")])

        script = TEST_DIR / "smoke-start.sh"
        script.write_text(
            "#!/bin/sh\n"
            "rm -f /opt/ocm-agent/smoke.log\n"
            "/opt/ocm-agent/ocm-agent -config /opt/ocm-agent/config.yaml "
            ">/opt/ocm-agent/smoke.log 2>&1 &\n"
            "echo SMOKE_STARTED=$!\n"
        )
        run(["sudo", "-n", install, "-m", "755", str(script),
             str(mountpoint / "opt" / "ocm-agent" / "smoke-start.sh")], check=True)
        run(["sync"], check=False)
    finally:
        subprocess.run(["sudo", "-n", umount, str(mountpoint)], check=True)


class TLSSink(threading.Thread):
    """Minimal gateway stand-in: complete mTLS, then keep reading frames."""

    def __init__(self, port: int) -> None:
        super().__init__(daemon=True)
        self.port = port
        self.ready = threading.Event()
        self.result: str | None = None
        self.error: str | None = None
        self.bytes_in = 0
        self.frame_counts: dict[int, int] = {}

    def run(self) -> None:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.minimum_version = ssl.TLSVersion.TLSv1_3
        ctx.load_cert_chain(TEST_DIR / "data/server.crt", TEST_DIR / "data/server.key")
        ctx.load_verify_locations(TEST_DIR / "data/ca.crt")
        ctx.verify_mode = ssl.CERT_OPTIONAL

        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", self.port))
            listener.listen(4)
            listener.settimeout(180)
            self.ready.set()
            conn, _ = listener.accept()
        try:
            with ctx.wrap_socket(conn, server_side=True) as tls:
                self.result = tls.version()
                tls.settimeout(30)
                end = time.time() + 30
                buf = b""
                while time.time() < end:
                    try:
                        chunk = tls.recv(4096)
                    except socket.timeout:
                        continue
                    if not chunk:
                        break
                    self.bytes_in += len(chunk)
                    buf += chunk
                    while len(buf) >= 12:
                        magic = int.from_bytes(buf[0:2], "big")
                        total = int.from_bytes(buf[2:6], "big")
                        if magic != 0x4F43 or total < 12 or total > 8 * 1024 * 1024:
                            self.error = f"bad frame header: magic=0x{magic:04x} len={total}"
                            return
                        if len(buf) < total:
                            break
                        ftype = buf[6]
                        self.frame_counts[ftype] = self.frame_counts.get(ftype, 0) + 1
                        buf = buf[total:]
        except Exception as exc:  # noqa: BLE001 - report to the main thread
            self.error = f"{type(exc).__name__}: {exc}"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def boot_and_run(port: int) -> tuple[str, int]:
    import pexpect

    env = dict(os.environ)
    env.setdefault("QEMU_ACCEL", "kvm")
    runner = shlex.split(os.environ.get("OCM_SMOKE_RUNNER", "nix run .#start-x86_64"))
    child = pexpect.spawn(
        runner[0], runner[1:], cwd=str(REPO_ROOT), env=env,
        encoding="utf-8", codec_errors="replace", timeout=180,
        dimensions=(40, 200),
    )
    transcript: list[str] = []

    def record() -> None:
        transcript.append(child.before or "")
        transcript.append(child.after if isinstance(child.after, str) else "")

    def expect_prompt(timeout: int = 120) -> None:
        child.expect([r"root@[^\r\n]*[#$] ", r"login: "], timeout=timeout)
        record()
        if "login:" in (child.after or ""):
            child.sendline("root")
            child.expect(r"root@[^\r\n]*[#$] ", timeout=60)
            record()

    try:
        child.expect(
            [r"Please press Enter[^\r\n]*", r"root@[^\r\n]*[#$] ", r"login: "],
            timeout=300,
        )
        record()
        if "Please press Enter" in (child.after or ""):
            child.sendline("")
            expect_prompt()
        elif "login:" in (child.after or ""):
            child.sendline("root")
            child.expect(r"root@[^\r\n]*[#$] ", timeout=60)
            record()

        # The DragonOS hvc console corrupts long input lines, so every guest
        # action is a short command that invokes a script installed by inject().
        child.sendline("sh /opt/ocm-agent/smoke-start.sh")
        child.expect(r"SMOKE_STARTED", timeout=30)
        record()
        time.sleep(SMOKE_RUN_SECONDS)
        child.sendline("sync; echo SYNC_DONE")
        child.expect(r"SYNC_DONE", timeout=60)
        record()
        time.sleep(1)
        # Clean shutdown so the ext4 journal is replayed/committed and the
        # image can be inspected afterwards.
        child.sendline("poweroff -f")
        try:
            child.expect(pexpect.EOF, timeout=60)
        except Exception:  # noqa: BLE001 - poweroff may be unsupported
            child.sendcontrol("a")
            child.send("x")
            try:
                child.expect(pexpect.EOF, timeout=10)
            except Exception:  # noqa: BLE001
                child.close(force=True)
        record()
    finally:
        if child.isalive():
            child.close(force=True)
    return "".join(transcript), child.exitstatus or 0


def read_guest_log() -> str:
    """Read the persisted agent log out of the image after QEMU exits."""
    mountpoint = TEST_DIR / "mnt-ro"
    mountpoint.mkdir(parents=True, exist_ok=True)
    mount = find_tool("mount")
    umount = find_tool("umount")
    run(["sudo", "-n", mount, "-o", "loop,rw,offset=1048576", str(IMAGE), str(mountpoint)],
        check=True)
    try:
        base = mountpoint / "opt" / "ocm-agent"
        log = base / "smoke.log"
        text = log.read_text(errors="replace") if log.exists() else ""
        if (base / "plugin-ran").exists():
            text += "\nPLUGIN_RAN\n"
        return text
    finally:
        run(["sudo", "-n", umount, str(mountpoint)], check=True)


def assert_ok(transcript: str, sink: TLSSink, mode: str) -> None:
    failures: list[str] = []
    if mode == "register":
        if sink.frame_counts.get(0x02, 0) == 0:
            failures.append("gateway received no REGISTER frame")
        required = [
            "starting ocm-agent",
            "connected to gateway",
            "fingerprint detected",
        ]
    elif mode == "plugins":
        required = [
            "starting ocm-agent",
            "connected to gateway",
            "epoch components started",
        ]
        if sink.frame_counts.get(0x06, 0) == 0:
            failures.append("gateway received no DATA_SMALL frame from the plugin pipeline")
    else:
        required = [
            "starting ocm-agent",
            "connected to gateway",
            "agent_id recovered from client cert",
            "epoch components started",
            "agent running",
            "PLUGIN_RAN",
        ]
    missing = [marker for marker in required if marker not in transcript]
    if missing:
        failures.append("missing guest log markers: " + ", ".join(missing))
    if sink.error:
        failures.append(f"TLS sink failed: {sink.error}")
    elif sink.result != "TLSv1.3":
        failures.append(f"unexpected TLS result: {sink.result!r}")
    if sink.bytes_in <= 0:
        failures.append("gateway received no frame bytes from the agent")
    if mode == "mtls" and sink.frame_counts.get(0x01, 0) == 0:
        failures.append("gateway received no HEARTBEAT frame")

    print(transcript[-8000:])
    if failures:
        print("\nFAILURES:", file=sys.stderr)
        for item in failures:
            print("  -", item, file=sys.stderr)
        raise SystemExit(1)
    log(f"TLS {sink.result}, {sink.bytes_in} frame bytes received")
    log("PASS")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path,
                        default=REPO_ROOT / "user/apps/ocm-agent/bin/ocm-agent")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--mode", choices=("mtls", "register", "plugins"), default="mtls")
    args = parser.parse_args()

    if not IMAGE.exists():
        raise SystemExit(f"rootfs image missing: {IMAGE} (run `nix run .#rootfs-x86_64`)")
    if not args.binary.exists():
        raise SystemExit(f"agent binary missing: {args.binary} (run `make -C user/apps/ocm-agent`)")

    TEST_DIR.mkdir(parents=True, exist_ok=True)
    port = args.port or free_port()
    make_certs()
    data_dir = f"{GUEST_DIR}/data" if args.mode != "register" else f"{GUEST_DIR}/data-register"
    config = write_config(port, data_dir)
    inject(config, args.binary, data_dir, args.mode != "register", plugins=args.mode == "plugins")

    sink = TLSSink(port)
    sink.start()
    if not sink.ready.wait(10):
        raise SystemExit("TLS sink failed to start")

    transcript, _ = boot_and_run(port)
    (TEST_DIR / "qemu-transcript.log").write_text(transcript)
    guest_log = read_guest_log()
    (TEST_DIR / "agent.log").write_text(guest_log)
    assert_ok(guest_log + "\n" + transcript, sink, args.mode)
    return 0


if __name__ == "__main__":
    sys.exit(main())
