#!/usr/bin/env python3
"""Run the Docker firewall acceptance workload in disposable DragonOS guests."""

import argparse
import contextlib
import http.server
import itertools
import pathlib
import shutil
import socket
import socketserver
import struct
import subprocess
import sys
import threading

import pexpect


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


class HttpReply(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"FIREWALL_HTTP_OK\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class DnsReply(socketserver.BaseRequestHandler):
    def handle(self):
        query, sock = self.request
        # The fixture uses a single conventional question for firewall.test.
        name = b"\x08firewall\x04test\x00"
        end = 12 + len(name)
        if query[12:end].lower() != name or len(query) < end + 4:
            return
        qtype, qclass = struct.unpack_from("!HH", query, end)
        if qclass != 1 or qtype not in (1, 28):
            return
        address, answer_type = self.server.answer
        payload = address if qtype == answer_type else b""
        header = query[:2] + struct.pack("!HHHHH", 0x8180, 1, bool(payload), 0, 0)
        answer = b""
        if payload:
            answer = b"\xc0\x0c" + struct.pack("!HHIH", qtype, 1, 30, len(payload)) + payload
        sock.sendto(header + query[12:end + 4] + answer, self.client_address)
        print(f"DNS type={qtype} from {self.client_address[0]}", flush=True)


def peer_parameters(family):
    if family == 4:
        return ("10.232.70.1", "10.232.70.2", "10.232.71.20",
                24, ("10.232.71.0/24", "10.232.72.0/24"))
    return ("fd42:2324:70::1", "fd42:2324:70::2", "fd42:2324:71::20",
            64, ("fd42:2324:71::/64", "fd42:2324:72::/64"))


@contextlib.contextmanager
def peer_network(family, tap, guest, peer, prefix, routes):
    run("ip", "link", "set", "lo", "up")
    run("ip", "tuntap", "add", "dev", tap, "mode", "tap")
    try:
        run("ip", "link", "set", tap, "up")
        nodad = ("nodad",) if family == 6 else ()
        run("ip", f"-{family}", "addr", "add", f"{peer}/{prefix}", "dev", tap, *nodad)
        for route in routes:
            run("ip", f"-{family}", "route", "add", route, "via", guest, "dev", tap)

        af = socket.AF_INET if family == 4 else socket.AF_INET6

        class HttpServer(http.server.ThreadingHTTPServer):
            address_family = af

        class DnsServer(socketserver.UDPServer):
            address_family = af

        with HttpServer((peer, 18080), HttpReply) as web, DnsServer((peer, 53), DnsReply) as dns:
            dns.answer = (socket.inet_pton(af, peer), 1 if family == 4 else 28)
            services = (web, dns)
            for service in services:
                threading.Thread(target=service.serve_forever, daemon=True).start()
            try:
                yield
            finally:
                for service in services:
                    service.shutdown()
    finally:
        run("ip", "link", "delete", tap)


def probe(label, address, port, expected, source_port=None):
    if ":" in address:
        address = f"[{address}]"
    options = ["curl", "-gfsS", "--noproxy", "*", "--max-time", "2"]
    if source_port is not None:
        options.extend(("--local-port", str(source_port)))
    probe_result = subprocess.run(
        [*options, f"http://{address}:{port}/"], capture_output=True, text=True,
    )
    passed = probe_result.returncode in expected and (
        probe_result.returncode != 0 or probe_result.stdout.strip() == "FIREWALL_HTTP_OK"
    )
    print(f"{label} {'PASS' if passed else 'FAIL'} rc={probe_result.returncode} "
          f"{(probe_result.stdout + probe_result.stderr).strip()}", flush=True)
    return passed


def qemu_command(kernel, image, tap):
    image_path = str(image).replace(",", ",,")
    return [
        "-nographic", "-machine", "q35,accel=kvm,hpet=off",
        "-cpu", "host,apic,x2apic,+fpu", "-smp", "2,cores=2,threads=1,sockets=1", "-m", "2G",
        "-kernel", str(kernel),
        "-append", "console=/dev/hvc0 rw init=/bin/busybox init AUTO_TEST=none TERM=vt100",
        "-snapshot", "-drive", f"id=disk,file={image_path},if=none,format=raw",
        "-device", "virtio-blk-pci,drive=disk", "-serial", "none",
        "-chardev", "stdio,id=console,mux=on,signal=off",
        "-monitor", "chardev:console", "-device", "virtio-serial",
        "-device", "virtconsole,chardev=console", "-device", "virtio-rng-pci",
        "-device", "pci-bridge,chassis_nr=1,id=pci.1", "-device", "pcie-root-port",
        "-netdev", f"tap,id=net0,ifname={tap},script=no,downscript=no",
        "-device", "virtio-net-pci,vectors=5,netdev=net0",
    ]


def send_command(console, command):
    # Wait for the terminal echo before sending the next serial input chunk.
    for offset in range(0, len(command), 16):
        part = command[offset:offset + 16]
        console.send(part)
        console.expect_exact(part, timeout=8)
    console.sendline("")


def exercise_guest(console, args, backend, family, proxy, guest, direct, reused):
    done = r"FIREWALL DONE ([0-9]+)"
    prompt = [r"root@[^\r\n]*[#] ", r"/ # ", r"\\]\$ "]
    console.expect("Please press Enter to activate this console")
    console.sendline("")
    console.expect(prompt)
    command = f"dragon-docker-firewall-matrix {backend} {family} {proxy}"
    if args.nic:
        command += f" {args.nic}"
    send_command(console, command)

    if console.expect([r"FIREWALL READY external\r", done], timeout=180) == 1:
        return False
    checks = [probe("published-external", guest, 12580, (0,))]
    checks.extend(probe(*request) for request in direct)
    console.sendline("continue")
    if console.expect([r"FIREWALL READY reused\r", done], timeout=90) == 1:
        return False
    checks.extend(probe(*request) for request in reused)
    console.sendline("continue")
    console.expect(done)
    return all(checks) and console.match.group(1) == "0"


def run_case(args, backend, family, proxy, port_base):
    guest, peer, target, prefix, routes = peer_parameters(family)
    name = f"{backend}-ipv{family}-proxy-{proxy}"
    output = args.output / name
    output.mkdir()
    capture_path = output / "traffic.pcap"
    tap = "dragon-fw-tap"
    direct = (
        ("direct-published", target, 8080, (28,), port_base),
        ("direct-unpublished", target, 8081, (28,), port_base + 1),
    )
    reused = (
        ("old-published-port", guest, 12580, (7, 28), port_base + 2),
        ("reused-direct-access", target, 8080, (28,), port_base + 3),
    )
    with (
        (output / "peer.log").open("w") as peer_log,
        contextlib.redirect_stdout(peer_log),
        contextlib.redirect_stderr(peer_log),
        peer_network(family, tap, guest, peer, prefix, routes),
    ):
        with (output / "capture.log").open("w") as capture_log:
            capture = subprocess.Popen(
                ["tcpdump", "-U", "-i", tap, "-w", str(capture_path), f"ip{6 if family == 6 else ''}"],
                stderr=capture_log,
            )
            try:
                with (output / "serial.log").open("w") as serial:
                    console = pexpect.spawn(
                        "qemu-system-x86_64", qemu_command(args.kernel, args.image, tap),
                        timeout=75, encoding="utf-8", codec_errors="replace",
                    )
                    console.logfile_read = serial
                    try:
                        passed = exercise_guest(console, args, backend, family, proxy, guest,
                                                direct, reused)
                    finally:
                        if console.isalive():
                            console.send("\x01x")
                            try:
                                console.expect(pexpect.EOF, timeout=8)
                            except pexpect.TIMEOUT:
                                console.close(force=True)
            finally:
                if capture.poll() is None:
                    capture.terminate()
                capture.wait(timeout=8)

    # Prove each denial saw an incoming SYN and did not establish TCP.
    for label, address, port, _, source_port in direct + reused:
        observed = run(
            "tcpdump", "-nn", "-r", str(capture_path),
            f"tcp and ((src host {peer} and dst host {address} "
            f"and src port {source_port} and dst port {port}) or "
            f"(src host {address} and dst host {peer} "
            f"and src port {port} and dst port {source_port}))",
            capture_output=True,
        ).stdout
        (output / f"{label}.log").write_text(observed)
        passed &= "Flags [S]" in observed and "Flags [S.]" not in observed
    return passed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=pathlib.Path, required=True)
    parser.add_argument("--image", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True, help="new result directory")
    parser.add_argument("--backend", choices=("iptables", "nftables"))
    parser.add_argument("--family", choices=(4, 6), type=int)
    parser.add_argument("--proxy", choices=("true", "false"))
    parser.add_argument("--nic", help="guest interface name (the default is its first Ethernet device)")
    args = parser.parse_args()
    for image in (args.kernel, args.image):
        if not image.is_file():
            parser.error(f"missing image: {image}")
    if args.nic and (not args.nic.isascii() or not args.nic.replace("-", "").isalnum()):
        parser.error("invalid guest interface name")
    for tool in ("qemu-system-x86_64", "ip", "tcpdump", "curl"):
        if not shutil.which(tool):
            parser.error(f"missing tool: {tool}")
    for device in ("/dev/kvm", "/dev/net/tun"):
        if not pathlib.Path(device).exists():
            parser.error(f"missing device: {device}")
    links = run("ip", "-o", "link", "show", capture_output=True).stdout.splitlines()
    if [link.split(": ", 2)[1] for link in links] != ["lo"]:
        parser.error("run in a fresh network namespace with unshare --net")
    args.output.mkdir(parents=True)
    cases = itertools.product(
        (args.backend,) if args.backend else ("iptables", "nftables"),
        (args.family,) if args.family else (4, 6),
        (args.proxy,) if args.proxy else ("true", "false"),
    )
    all_passed = True
    with (args.output / "summary.txt").open("w") as summary:
        for index, (backend, family, proxy) in enumerate(cases):
            try:
                passed = run_case(args, backend, family, proxy, 35400 + 4 * index)
            except (OSError, subprocess.SubprocessError, pexpect.ExceptionPexpect) as error:
                print(f"{backend} IPv{family} proxy={proxy}: {error}", file=sys.stderr)
                passed = False
            result = f"{backend} IPv{family} proxy={proxy} {'PASS' if passed else 'FAIL'}"
            print(result, flush=True)
            print(result, file=summary, flush=True)
            all_passed &= passed
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
