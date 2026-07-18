#!/usr/bin/env python3
"""Bracketed host/guest timekeeping calibration for DragonOS.

The runner never compares host and guest clock epochs.  It establishes causal
host bounds around guest CLOCK_MONOTONIC_RAW samples and performs all verdict
comparisons with integers.  A missing serial transport or incomplete protocol
is recorded as ``incomplete`` and can never become a passing result.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fractions
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import select
import secrets
import selectors
import shutil
import socket
import subprocess
import sys
import time
from typing import Any, Iterable


SCHEMA = "dragonos.timekeeping-calibration.v3"
PROTOCOL = "TKCAL/2"
TOKEN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
HEX128 = re.compile(r"^[0-9a-f]{32}$")
HEX256 = re.compile(r"^[0-9a-f]{64}$")
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
REQUIRED_VMS = {("kvm", 1), ("kvm", 2), ("tcg", 2)}
READS_REQUIRED = 10_000_000
MIGRATIONS_EXPECTED = (READS_REQUIRED - 1) // 4096
MAX_SERIAL_LINE_BYTES = 64 * 1024
MAX_SERIAL_BUFFER_BYTES = 128 * 1024
MAX_TRANSCRIPT_BYTES = 16 * 1024 * 1024
SOCKET_IO_TIMEOUT_NS = 10_000_000_000
MAX_CPU_ID = 4095
MAX_CPU_COUNT = 256
SERIAL_FATAL_MARKERS = (
    b"Kernel Panic Occurred.",
    b"Panic Counter:",
    b"fatal exception",
    b"BUG: kernel NULL pointer dereference",
    b"BUG: unable to handle page fault",
)
SERIAL_MEASUREMENT_FORBIDDEN_MARKERS = (
    b"Switching to the clocksource",
    b"clocksource :",  # prefix of the watchdog's exact "is unstable" diagnostic
    b"cs_dev_nsec =",
    b"clocksource watchdog could not initialize reference state",
    b"clocksource registration rollback could not reset watchdog state",
    b"clocksource registration rollback could not reset source",
    b"clocksource rating change could not switch source",
    b"clocksource watchdog: long readout interval, skip check",
    b"clocksource watchdog replacement could not reset state",
    b"clocksource selection failed",
    b"refusing invalid clocksource",
)


class CalibrationError(Exception):
    """Invalid evidence or a failed setup operation."""


class ProtocolError(CalibrationError):
    """Malformed, duplicated, or out-of-order guest protocol."""


@dataclasses.dataclass(frozen=True)
class Event:
    kind: str
    fields: dict[str, str]
    raw_line: str


@dataclasses.dataclass(frozen=True)
class TranscriptEvidence:
    guest_events: tuple[Event, ...]
    bytes_read: int


@dataclasses.dataclass(frozen=True)
class ProcessIdentity:
    pid: int
    starttime_ticks: int
    exe: str
    cwd: str
    argv: tuple[str, ...]

    def as_json(self) -> dict[str, Any]:
        encoded = b"\0".join(os.fsencode(item) for item in self.argv) + b"\0"
        return {
            "pid": self.pid,
            "starttime_ticks": self.starttime_ticks,
            "exe": self.exe,
            "cwd": self.cwd,
            "cmdline_sha256": hashlib.sha256(encoded).hexdigest(),
        }


@dataclasses.dataclass(frozen=True)
class Thresholds:
    ratio_min_num: int
    ratio_min_den: int
    ratio_max_num: int
    ratio_max_den: int
    dispersion_num: int
    dispersion_den: int
    ratio_is_gate: bool
    dispersion_is_gate: bool

    @staticmethod
    def for_accel(accel: str) -> "Thresholds":
        if accel == "kvm":
            return Thresholds(995, 1000, 1005, 1000, 2, 1000, True, True)
        if accel == "tcg":
            # TCG scheduling and translation overhead make rate brackets useful
            # diagnostics, but not a stable performance gate.  Correctness and
            # monotonicity remain mandatory for TCG runs.
            return Thresholds(95, 100, 105, 100, 2, 100, False, False)
        raise CalibrationError(f"unsupported accelerator: {accel}")

    def as_json(self) -> dict[str, str]:
        return {
            "ratio_min": fraction_text(fractions.Fraction(self.ratio_min_num, self.ratio_min_den)),
            "ratio_max": fraction_text(fractions.Fraction(self.ratio_max_num, self.ratio_max_den)),
            "max_midpoint_dispersion": fraction_text(
                fractions.Fraction(self.dispersion_num, self.dispersion_den)
            ),
            "ratio_is_gate": self.ratio_is_gate,
            "dispersion_is_gate": self.dispersion_is_gate,
        }


def parse_event(line: str) -> Event | None:
    """Parse one guest line; unrelated boot/shell output returns None."""
    clean = line.rstrip("\r\n")
    if not clean.startswith(PROTOCOL + " "):
        return None
    parts = clean.split(" ")
    if len(parts) < 3 or parts[0] != PROTOCOL or not TOKEN.fullmatch(parts[1]):
        raise ProtocolError(f"malformed protocol line: {clean!r}")
    fields: dict[str, str] = {}
    for token in parts[2:]:
        if token.count("=") != 1:
            raise ProtocolError(f"malformed field in protocol line: {token!r}")
        key, value = token.split("=", 1)
        if not TOKEN.fullmatch(key) or not value or len(value) > 256:
            raise ProtocolError(f"invalid protocol field: {token!r}")
        if key in fields:
            raise ProtocolError(f"duplicate protocol field: {key}")
        fields[key] = value
    return Event(parts[1], fields, clean)


def require_fields(event: Event, names: Iterable[str]) -> None:
    missing = [name for name in names if name not in event.fields]
    if missing:
        raise ProtocolError(f"{event.kind} lacks fields: {','.join(missing)}")


def uint_field(event: Event, name: str) -> int:
    require_fields(event, [name])
    value = event.fields[name]
    if not value.isascii() or not value.isdecimal():
        raise ProtocolError(f"{event.kind}.{name} is not an unsigned integer")
    parsed = int(value)
    if parsed < 0 or parsed > (1 << 64) - 1:
        raise ProtocolError(f"{event.kind}.{name} is outside u64")
    return parsed


def validate_identity(event: Event, expected_kind: str, run_id: str, seq: int, case_id: str | None) -> None:
    if event.kind != expected_kind:
        raise ProtocolError(f"expected {expected_kind}, got {event.kind}")
    require_fields(event, ["run", "seq"])
    if event.fields["run"] != run_id or uint_field(event, "seq") != seq:
        raise ProtocolError(f"{event.kind} has the wrong run or sequence")
    if case_id is not None:
        require_fields(event, ["case"])
        if event.fields["case"] != case_id:
            raise ProtocolError(f"{event.kind} has the wrong case")


def validate_ack(event: Event, run_id: str, seq: int) -> None:
    validate_identity(event, "ACK", run_id, seq, None)
    if event.fields != {"run": run_id, "seq": str(seq), "status": "ok"}:
        raise ProtocolError("ACK has invalid or extra fields")


def validate_ready(event: Event, vcpus: int) -> None:
    if set(event.fields) != {"run", "seq", "cpus"}:
        raise ProtocolError("READY has invalid or extra fields")
    cpu_items = [] if event.fields["cpus"] == "none" else event.fields["cpus"].split(",")
    try:
        cpu_set = set() if not cpu_items else parse_cpu_list(event.fields["cpus"])
    except CalibrationError as error:
        raise ProtocolError(f"READY CPU list is invalid: {error}") from error
    if len(cpu_items) != vcpus or len(cpu_set) != vcpus:
        raise ProtocolError(f"READY reports {event.fields['cpus']}, expected {vcpus} online CPUs")


def compute_metrics(slo: int, shi: int, elo: int, ehi: int, guest_start: int, guest_end: int) -> dict[str, Any]:
    values = (slo, shi, elo, ehi, guest_start, guest_end)
    if any(value < 0 or value > (1 << 64) - 1 for value in values):
        raise CalibrationError("timestamps must fit u64")
    if not (slo <= shi < elo <= ehi):
        raise CalibrationError("host brackets are not causally ordered")
    if guest_end <= guest_start:
        raise CalibrationError("guest raw clock did not advance")
    guest_delta = guest_end - guest_start
    host_min = elo - shi
    host_max = ehi - slo
    midpoint_twice = (elo + ehi) - (slo + shi)
    if host_min <= 0 or host_max < host_min or midpoint_twice <= 0:
        raise CalibrationError("invalid host interval bounds")

    ratio_low = fractions.Fraction(guest_delta, host_max)
    ratio_mid = fractions.Fraction(2 * guest_delta, midpoint_twice)
    ratio_high = fractions.Fraction(guest_delta, host_min)
    bracket_ppm = fractions.Fraction((host_max - host_min) * 2_000_000, midpoint_twice)
    error_ppm = (ratio_mid - 1) * 1_000_000
    return {
        "guest_delta_ns": guest_delta,
        "host_min_ns": host_min,
        "host_max_ns": host_max,
        "host_midpoint_twice_ns": midpoint_twice,
        "ratio_low_fraction": ratio_low,
        "ratio_mid_fraction": ratio_mid,
        "ratio_high_fraction": ratio_high,
        "ratio_low": fraction_text(ratio_low),
        "ratio_mid": fraction_text(ratio_mid),
        "ratio_high": fraction_text(ratio_high),
        "ratio_numerator": ratio_mid.numerator,
        "ratio_denominator": ratio_mid.denominator,
        "error_ppm": fraction_text(error_ppm),
        "bracket_ppm": fraction_text(bracket_ppm),
    }


def fraction_text(value: fractions.Fraction, digits: int = 9) -> str:
    sign = "-" if value < 0 else ""
    value = abs(value)
    scale = 10**digits
    rounded = (value.numerator * scale * 2 + value.denominator) // (2 * value.denominator)
    return f"{sign}{rounded // scale}.{rounded % scale:0{digits}d}"


def public_metrics(metrics: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in metrics.items() if not key.endswith("_fraction")}


def evaluate_ratio(metrics: dict[str, Any], thresholds: Thresholds) -> tuple[str, list[str]]:
    if not thresholds.ratio_is_gate:
        return "pass", []
    low: fractions.Fraction = metrics["ratio_low_fraction"]
    high: fractions.Fraction = metrics["ratio_high_fraction"]
    minimum = fractions.Fraction(thresholds.ratio_min_num, thresholds.ratio_min_den)
    maximum = fractions.Fraction(thresholds.ratio_max_num, thresholds.ratio_max_den)
    if low >= minimum and high <= maximum:
        return "pass", []
    if high < minimum:
        return "fail", ["ratio_interval_below_threshold"]
    if low > maximum:
        return "fail", ["ratio_interval_above_threshold"]
    reasons: list[str] = []
    if low < minimum:
        reasons.append("ratio_interval_overlaps_lower_threshold")
    if high > maximum:
        reasons.append("ratio_interval_overlaps_upper_threshold")
    return "incomplete", reasons


def evaluate_read_fields(work_done: Event, affinity: str, vcpus: int) -> list[str]:
    required = [
        "status", "raw_reads", "mono_reads", "raw_regressions", "mono_regressions",
        "raw_max_backward_ns", "mono_max_backward_ns", "migrations_requested",
        "migrations_observed", "cpu_mask_seen",
    ]
    require_fields(work_done, required)
    reasons: list[str] = []
    if work_done.fields["status"] != "ok":
        reasons.append("guest_workload_failed")
    for name in ("raw_reads", "mono_reads"):
        if uint_field(work_done, name) != READS_REQUIRED:
            reasons.append(f"{name}_does_not_match_required_count")
    for name in ("raw_regressions", "mono_regressions", "raw_max_backward_ns", "mono_max_backward_ns"):
        if uint_field(work_done, name) != 0:
            reasons.append(f"{name}_nonzero")
    if affinity == "migrate":
        if vcpus < 2:
            reasons.append("migration_requires_two_vcpus")
        if uint_field(work_done, "migrations_requested") != MIGRATIONS_EXPECTED:
            reasons.append("requested_migrations_do_not_match_expected_count")
        if uint_field(work_done, "migrations_observed") != MIGRATIONS_EXPECTED:
            reasons.append("observed_migrations_do_not_match_expected_count")
        try:
            cpu_mask = int(work_done.fields["cpu_mask_seen"], 16)
        except ValueError as error:
            raise ProtocolError("WORK_DONE.cpu_mask_seen is not hexadecimal") from error
        if cpu_mask.bit_count() < 2:
            reasons.append("fewer_than_two_cpus_observed")
    elif (uint_field(work_done, "migrations_requested") != 0 or
          uint_field(work_done, "migrations_observed") != 0):
        reasons.append("fixed_case_reported_migrations")
    return reasons


def evaluate_case_evidence(spec: dict[str, Any], accel: str, vcpus: int, start: Event,
                           work_done: Event, end: Event,
                           bracket: dict[str, int]) -> tuple[str, list[str], dict[str, Any]]:
    require_fields(start, ["guest_raw_ns", "status"])
    require_fields(end, ["guest_raw_ns", "status"])
    require_fields(work_done, ["work_end_raw_ns", "status", "reason"])
    metrics = compute_metrics(
        bracket["slo_ns"], bracket["shi_ns"], bracket["elo_ns"], bracket["ehi_ns"],
        uint_field(start, "guest_raw_ns"), uint_field(end, "guest_raw_ns"),
    )
    fail_reasons: list[str] = []
    incomplete_reasons: list[str] = []
    if (start.fields["status"] != "ok" or end.fields["status"] != "ok" or
            work_done.fields["status"] != "ok"):
        fail_reasons.append("guest_reported_failure")
    if spec["mode"] in ("sleep", "busy"):
        work_end = uint_field(work_done, "work_end_raw_ns")
        guest_start = uint_field(start, "guest_raw_ns")
        if work_end < guest_start:
            raise ProtocolError("WORK_DONE raw clock precedes START")
        if work_end - guest_start < spec["target_ns"]:
            fail_reasons.append("guest_workload_shorter_than_target")
        ratio_status, ratio_reasons = evaluate_ratio(metrics, Thresholds.for_accel(accel))
        if ratio_status == "fail":
            fail_reasons.extend(ratio_reasons)
        elif ratio_status == "incomplete":
            incomplete_reasons.extend(ratio_reasons)
    else:
        fail_reasons.extend(evaluate_read_fields(work_done, spec["affinity"], vcpus))
    if fail_reasons:
        return "fail", sorted(set(fail_reasons)), metrics
    if incomplete_reasons:
        return "incomplete", sorted(set(incomplete_reasons)), metrics
    return "pass", [], metrics


def evaluate_dispersion(cases: list[dict[str, Any]], thresholds: Thresholds) -> list[str]:
    if not thresholds.dispersion_is_gate:
        return []
    grouped: dict[tuple[str, int], list[fractions.Fraction]] = {}
    for case in cases:
        if case.get("mode") not in ("sleep", "busy") or case.get("status") != "pass":
            continue
        metrics = case["metrics"]
        ratio = fractions.Fraction(metrics["ratio_numerator"], metrics["ratio_denominator"])
        grouped.setdefault((case["mode"], case["target_ns"]), []).append(ratio)
    reasons: list[str] = []
    limit = fractions.Fraction(thresholds.dispersion_num, thresholds.dispersion_den)
    for (mode, target), ratios in grouped.items():
        if len(ratios) != 5:
            reasons.append(f"{mode}_{target}_does_not_have_five_passing_rounds")
        elif max(ratios) - min(ratios) > limit:
            reasons.append(f"{mode}_{target}_midpoint_dispersion_exceeded")
    for mode in ("sleep", "busy"):
        for target in (10_000_000_000, 60_000_000_000):
            if (mode, target) not in grouped:
                reasons.append(f"{mode}_{target}_group_missing")
    return sorted(set(reasons))


def expected_cases(vcpus: int) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for mode in ("sleep", "busy"):
        for target in (10_000_000_000, 60_000_000_000):
            for repetition in range(1, 6):
                cases.append({
                    "case_id": f"{mode}-{target // 1_000_000_000}s-r{repetition}",
                    "mode": mode,
                    "target_ns": target,
                    "affinity": "fixed",
                    "reads": 0,
                    "repetition": repetition,
                })
    cases.append({
        "case_id": "reads-fixed", "mode": "reads", "target_ns": 0,
        "affinity": "fixed", "reads": READS_REQUIRED, "repetition": 1,
    })
    if vcpus == 1:
        cases.append({
            "case_id": "reads-migrate", "mode": "reads", "target_ns": 0,
            "affinity": "migrate", "reads": READS_REQUIRED, "repetition": 1,
            "predefined_skip": "requires_two_vcpus",
        })
    else:
        cases.append({
            "case_id": "reads-migrate", "mode": "reads", "target_ns": 0,
            "affinity": "migrate", "reads": READS_REQUIRED, "repetition": 1,
        })
    return cases


class SerialTransport:
    def __init__(self, path: Path, transcript: Any):
        self.path = path
        self.transcript = transcript
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(SOCKET_IO_TIMEOUT_NS / 1_000_000_000)
        try:
            self.sock.connect(str(path))
        except OSError as error:
            self.sock.close()
            raise CalibrationError(
                f"cannot connect to QEMU serial socket {path}; configure the general "
                "DRAGONOS_QEMU_SERIAL_SOCKET interface before running calibration: {error}"
            ) from error
        self.sock.setblocking(False)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.sock, selectors.EVENT_READ)
        self.buffer = bytearray()
        self.transcript_bytes = 0

    def write_transcript(self, data: bytes) -> None:
        total = getattr(self, "transcript_bytes", 0) + len(data)
        if total > MAX_TRANSCRIPT_BYTES:
            raise CalibrationError("serial transcript exceeds the total byte limit")
        self.transcript.write(data)
        self.transcript.flush()
        self.transcript_bytes = total

    def close(self) -> None:
        self.selector.close()
        self.sock.close()

    def send_line(self, line: str, timeout_ns: int = SOCKET_IO_TIMEOUT_NS) -> None:
        try:
            data = (line + "\n").encode("ascii")
        except UnicodeEncodeError as error:
            raise CalibrationError("serial command must be ASCII") from error
        if len(data) > MAX_SERIAL_LINE_BYTES:
            raise CalibrationError("serial command exceeds the line limit")
        self.write_transcript(b">>> " + data)
        view = memoryview(data)
        deadline_ns = raw_now_ns() + timeout_ns
        self.selector.modify(self.sock, selectors.EVENT_READ | selectors.EVENT_WRITE)
        try:
            while view:
                remaining = deadline_ns - raw_now_ns()
                if remaining <= 0:
                    raise CalibrationError("timed out writing serial protocol")
                events = self.selector.select(remaining / 1_000_000_000)
                if not events:
                    raise CalibrationError("timed out writing serial protocol")
                if not any(mask & selectors.EVENT_WRITE for _, mask in events):
                    continue
                try:
                    count = self.sock.send(view)
                except BlockingIOError:
                    continue
                if count <= 0:
                    raise CalibrationError("serial socket closed while writing")
                view = view[count:]
        finally:
            self.selector.modify(self.sock, selectors.EVENT_READ)

    def read_line(self, deadline_ns: int) -> str:
        while True:
            newline = self.buffer.find(b"\n")
            if newline >= 0:
                if newline + 1 > MAX_SERIAL_LINE_BYTES:
                    raise ProtocolError("serial line exceeds the line limit")
                raw = bytes(self.buffer[: newline + 1])
                del self.buffer[: newline + 1]
                self.write_transcript(raw)
                return raw.decode("utf-8", errors="replace")
            remaining = deadline_ns - raw_now_ns()
            if remaining <= 0:
                raise CalibrationError("timed out waiting for serial protocol")
            events = self.selector.select(remaining / 1_000_000_000)
            if not events:
                raise CalibrationError("timed out waiting for serial protocol")
            try:
                data = self.sock.recv(65536)
            except BlockingIOError:
                continue
            if not data:
                raise CalibrationError("serial socket reached EOF")
            self.buffer.extend(data)
            if len(self.buffer) > MAX_SERIAL_BUFFER_BYTES:
                raise ProtocolError("serial input exceeds the buffer limit")
            if b"\n" not in self.buffer and len(self.buffer) > MAX_SERIAL_LINE_BYTES:
                raise ProtocolError("serial line exceeds the line limit")

    def wait_for_prompt(
        self,
        prompt: re.Pattern[str],
        deadline_ns: int,
        retry_ns: int = 2_000_000_000,
    ) -> None:
        """Wait for a possibly non-newline-terminated shell prompt.

        BusyBox prints the activation prompt before opening hvc0 for input, so
        a newline sent immediately after the socket connects can be lost.  A
        bounded, low-frequency retry closes that boot race.  Prompt matching
        strips terminal colour escapes but the transcript always retains the
        exact bytes received from the guest.
        """
        next_retry_ns = 0
        while True:
            while True:
                newline = self.buffer.find(b"\n")
                end = newline + 1 if newline >= 0 else len(self.buffer)
                if end == 0:
                    break
                raw = bytes(self.buffer[:end])
                normalized = ANSI_ESCAPE_RE.sub("", raw.decode("utf-8", errors="replace"))
                if prompt.search(normalized):
                    del self.buffer[:end]
                    self.write_transcript(raw)
                    return
                if newline < 0:
                    break
                del self.buffer[:end]
                self.write_transcript(raw)

            if len(self.buffer) > MAX_SERIAL_LINE_BYTES:
                raise ProtocolError("serial line exceeds the line limit")
            now_ns = raw_now_ns()
            if now_ns >= deadline_ns:
                raise CalibrationError("timed out waiting for guest shell prompt")
            if now_ns >= next_retry_ns:
                self.send_line("")
                next_retry_ns = raw_now_ns() + retry_ns

            wait_deadline_ns = min(deadline_ns, next_retry_ns)
            remaining_ns = wait_deadline_ns - raw_now_ns()
            if remaining_ns <= 0:
                continue
            events = self.selector.select(remaining_ns / 1_000_000_000)
            if not events:
                continue
            try:
                data = self.sock.recv(65536)
            except BlockingIOError:
                continue
            if not data:
                raise CalibrationError("serial socket reached EOF")
            self.buffer.extend(data)
            if len(self.buffer) > MAX_SERIAL_BUFFER_BYTES:
                raise ProtocolError("serial input exceeds the buffer limit")

    def read_event(self, kind: str, run_id: str, seq: int, case_id: str | None,
                   timeout_ns: int) -> Event:
        deadline = raw_now_ns() + timeout_ns
        while True:
            event = parse_event(self.read_line(deadline))
            if event is None:
                continue
            if event.kind == "ERROR":
                raise ProtocolError(f"guest protocol error: {event.raw_line}")
            validate_identity(event, kind, run_id, seq, case_id)
            return event


def raw_now_ns() -> int:
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def parse_transcript(path: Path) -> TranscriptEvidence:
    events: list[Event] = []
    total = 0
    longest_fatal_marker = max(len(marker) for marker in SERIAL_FATAL_MARKERS)
    longest_measurement_marker = max(
        len(marker) for marker in SERIAL_MEASUREMENT_FORBIDDEN_MARKERS
    )
    fatal_marker_tail = b""
    measurement_marker_tail = b""
    measurement_active = False
    try:
        with path.open("rb") as source:
            while True:
                raw = source.readline(MAX_SERIAL_LINE_BYTES + 1)
                if not raw:
                    break
                total += len(raw)
                if total > MAX_TRANSCRIPT_BYTES:
                    raise CalibrationError("serial transcript exceeds the total byte limit")
                if len(raw) > MAX_SERIAL_LINE_BYTES:
                    raise ProtocolError("serial transcript contains an overlong line")
                line = raw.decode("utf-8", errors="replace")
                event = parse_event(line)
                if event is not None and event.kind == "READY" and not measurement_active:
                    # The helper's initial READY is emitted only after boot and
                    # establishes the start of the controlled measurement
                    # session.  Boot-time source selection is expected; any
                    # source change or watchdog anomaly after this boundary
                    # invalidates the calibration run.
                    measurement_active = True
                    measurement_marker_tail = b""

                lowered = fatal_marker_tail + raw.lower()
                for marker in SERIAL_FATAL_MARKERS:
                    if marker.lower() in lowered:
                        raise CalibrationError(
                            f"guest serial contains fatal kernel marker {marker.decode('ascii')!r}"
                        )
                fatal_marker_tail = lowered[-max(0, longest_fatal_marker - 1):]

                if measurement_active:
                    measured = measurement_marker_tail + raw.lower()
                    for marker in SERIAL_MEASUREMENT_FORBIDDEN_MARKERS:
                        if marker.lower() in measured:
                            raise CalibrationError(
                                "guest serial contains clocksource event during measurement "
                                f"{marker.decode('ascii')!r}"
                            )
                    measurement_marker_tail = measured[
                        -max(0, longest_measurement_marker - 1):
                    ]

                if event is not None:
                    events.append(event)
                    if event.kind == "ACK":
                        measurement_active = False
                        measurement_marker_tail = b""
    except OSError as error:
        raise CalibrationError(f"cannot read serial transcript {path}: {error}") from error
    return TranscriptEvidence(tuple(events), total)


def verify_transcript_session(value: dict[str, Any], transcript: TranscriptEvidence) -> None:
    events = transcript.guest_events
    cursor = 0

    def take(expected_kind: str) -> Event:
        nonlocal cursor
        if cursor >= len(events):
            raise CalibrationError(f"serial transcript is missing {expected_kind}")
        event = events[cursor]
        cursor += 1
        if event.kind == "ERROR":
            raise ProtocolError(f"serial transcript contains guest error: {event.raw_line}")
        if event.kind != expected_kind:
            raise ProtocolError(
                f"serial transcript expected {expected_kind}, got {event.kind} at event {cursor}"
            )
        return event

    run_id = value["run_id"]
    vcpus = value["vcpus"]
    initial = take("READY")
    validate_identity(initial, "READY", run_id, 0, None)
    validate_ready(initial, vcpus)
    for case in value["cases"]:
        if case["status"] == "skip":
            continue
        seq = case["seq"]
        guest = case["guest"]
        for kind, name in (("START", "start"), ("WORK_DONE", "work_done"), ("END", "end")):
            event = take(kind)
            validate_identity(event, kind, run_id, seq, case["case_id"])
            if event.fields != guest[name]:
                raise CalibrationError(
                    f"serial {kind} differs from JSON evidence for {case['case_id']}"
                )
        ready = take("READY")
        validate_identity(ready, "READY", run_id, seq, None)
        validate_ready(ready, vcpus)
    ack = take("ACK")
    validate_ack(ack, run_id, int(value["shutdown_ack"]["seq"]))
    if ack.fields != value["shutdown_ack"]:
        raise CalibrationError("serial ACK differs from JSON shutdown evidence")
    if cursor != len(events):
        raise CalibrationError("serial transcript contains extra protocol events")


def create_output_dir(path: Path) -> None:
    try:
        path.mkdir(parents=True, exist_ok=False)
    except FileExistsError as error:
        raise CalibrationError(f"refusing to overwrite artifact directory: {path}") from error


def write_json_new(path: Path, value: Any) -> None:
    if path.exists():
        raise CalibrationError(f"refusing to overwrite artifact: {path}")
    temporary = path.with_name(path.name + f".tmp-{os.getpid()}-{secrets.token_hex(4)}")
    try:
        with temporary.open("x", encoding="utf-8") as output:
            json.dump(value, output, sort_keys=True, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


def artifact_record(path: Path) -> dict[str, Any]:
    path = path.resolve()
    return {"path": str(path), "sha256": sha256_file(path), "size": path.stat().st_size}


def validate_artifact_store(path: Path) -> Path:
    resolved = path.resolve()
    if (not resolved.is_dir() or path.is_symlink() or resolved.stat().st_uid != os.getuid() or
            resolved.stat().st_mode & 0o077 != 0):
        raise CalibrationError("artifact store must be an existing private directory owned by the user")
    return resolved


def archive_artifact(source: Path, store: Path) -> dict[str, Any]:
    if not source.is_file() or source.is_symlink():
        raise CalibrationError(f"cannot archive missing/non-regular artifact: {source}")
    source = source.resolve()
    digest = sha256_file(source)
    size = source.stat().st_size
    destination = store / digest

    def verify_destination() -> None:
        if (not destination.is_file() or destination.is_symlink() or
                destination.name != digest or destination.stat().st_size != size or
                destination.stat().st_mode & 0o222 != 0 or sha256_file(destination) != digest):
            raise CalibrationError(f"content-addressed artifact is invalid: {destination}")

    if destination.exists():
        verify_destination()
    else:
        temporary = store / f".{digest}.tmp-{os.getpid()}-{secrets.token_hex(4)}"
        try:
            completed = subprocess.run(
                ["cp", "--reflink=auto", "--sparse=always", "--", str(source), str(temporary)],
                check=False, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            )
            if completed.returncode != 0:
                raise CalibrationError(f"cannot archive {source}: {completed.stdout.strip()}")
            if temporary.stat().st_size != size or sha256_file(temporary) != digest:
                raise CalibrationError(f"archived copy differs from source: {source}")
            temporary.chmod(0o444)
            with temporary.open("rb") as archived:
                os.fsync(archived.fileno())
            try:
                os.link(temporary, destination)
            except FileExistsError:
                pass
            directory_fd = os.open(store, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        finally:
            if temporary.exists():
                temporary.unlink()
        verify_destination()
    return {
        "execution_path": str(source),
        "archive_path": str(destination),
        "sha256": digest,
        "size": size,
    }


def command_output(argv: list[str]) -> str:
    try:
        completed = subprocess.run(argv, check=False, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unavailable:{type(error).__name__}"
    return completed.stdout.splitlines()[0] if completed.stdout else f"exit:{completed.returncode}"


def hash_untracked_content(repo: Path) -> str:
    """Hash names, types, and contents of every non-ignored untracked path."""
    try:
        listed = subprocess.run(
            ["git", "-C", str(repo), "ls-files", "--others", "--exclude-standard", "-z"],
            check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10,
        ).stdout
    except (OSError, subprocess.SubprocessError) as error:
        raise CalibrationError(f"cannot enumerate untracked source content: {error}") from error
    digest = hashlib.sha256()
    for relative_raw in sorted(item for item in listed.split(b"\0") if item):
        relative = os.fsdecode(relative_raw)
        path = repo / relative
        digest.update(relative_raw)
        digest.update(b"\0")
        if path.is_symlink():
            digest.update(b"L\0")
            digest.update(os.fsencode(os.readlink(path)))
        elif path.is_file():
            digest.update(b"F\0")
            with path.open("rb") as source:
                for block in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(block)
        else:
            raise CalibrationError(f"unsupported untracked source path: {path}")
        digest.update(b"\0")
    return digest.hexdigest()


def provenance(qemu_argv: list[str]) -> dict[str, Any]:
    repo = Path(__file__).resolve().parents[2]
    commit = command_output(["git", "-C", str(repo), "rev-parse", "HEAD"])
    tree = command_output(["git", "-C", str(repo), "rev-parse", "HEAD^{tree}"])
    try:
        status = subprocess.run(
            ["git", "-C", str(repo), "status", "--porcelain=v1", "--untracked-files=all"],
            check=False, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10,
        ).stdout
        tracked_diff = subprocess.run(
            ["git", "-C", str(repo), "diff", "--binary", "HEAD"], check=False,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10,
        ).stdout
    except (OSError, subprocess.TimeoutExpired):
        status = b"unavailable"
        tracked_diff = b"unavailable"
    return {
        "repo": {
            "commit": commit,
            "tree": tree,
            "dirty": bool(status.strip()),
            "status_sha256": hashlib.sha256(status).hexdigest(),
            "tracked_diff_sha256": hashlib.sha256(tracked_diff).hexdigest(),
            "untracked_content_sha256": hash_untracked_content(repo),
        },
        "host": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "affinity": sorted(os.sched_getaffinity(0)),
            "clock_monotonic_raw_resolution_ns": round(
                time.clock_getres(time.CLOCK_MONOTONIC_RAW) * 1_000_000_000
            ),
        },
        "qemu_version": command_output([qemu_argv[0], "--version"]),
    }


def parse_cpu_list(text: str) -> set[int]:
    cpus: set[int] = set()
    if not text:
        raise CalibrationError("CPU list must not be empty")
    for item in text.split(","):
        if "-" in item:
            parts = item.split("-", 1)
            if len(parts) != 2 or not all(part.isdecimal() for part in parts):
                raise CalibrationError(f"invalid CPU range: {item}")
            try:
                start, end = map(int, parts)
            except ValueError as error:
                raise CalibrationError(f"invalid CPU range: {item}") from error
            if start > end:
                raise CalibrationError(f"descending CPU range: {item}")
            if end > MAX_CPU_ID or end - start + 1 > MAX_CPU_COUNT:
                raise CalibrationError(f"CPU range exceeds calibration limits: {item}")
            cpus.update(range(start, end + 1))
        elif item.isdecimal():
            try:
                cpu = int(item)
            except ValueError as error:
                raise CalibrationError(f"invalid CPU item: {item}") from error
            if cpu > MAX_CPU_ID:
                raise CalibrationError(f"CPU id exceeds calibration limit: {item}")
            cpus.add(cpu)
        else:
            raise CalibrationError(f"invalid CPU item: {item}")
        if len(cpus) > MAX_CPU_COUNT:
            raise CalibrationError("CPU list exceeds calibration count limit")
    return cpus


def capture_process_identity(pid: int) -> ProcessIdentity:
    proc = Path(f"/proc/{pid}")
    try:
        raw_cmdline = (proc / "cmdline").read_bytes()
        raw_stat = (proc / "stat").read_text(encoding="ascii")
        exe = str((proc / "exe").resolve(strict=True))
        cwd = str((proc / "cwd").resolve(strict=True))
    except (OSError, UnicodeError) as error:
        raise CalibrationError(f"cannot capture process identity for pid {pid}: {error}") from error
    if not raw_cmdline or not raw_cmdline.endswith(b"\0"):
        raise CalibrationError(f"process {pid} has an invalid /proc cmdline")
    raw_items = raw_cmdline[:-1].split(b"\0")
    if not raw_items or any(not item for item in raw_items):
        raise CalibrationError(f"process {pid} has empty argv entries")
    argv = tuple(os.fsdecode(item) for item in raw_items)
    closing = raw_stat.rfind(")")
    if closing < 0:
        raise CalibrationError(f"process {pid} has malformed /proc stat")
    fields = raw_stat[closing + 2:].split()
    if len(fields) <= 19 or not fields[19].isdecimal():
        raise CalibrationError(f"process {pid} lacks a valid starttime")
    return ProcessIdentity(pid, int(fields[19]), exe, cwd, argv)


def validate_process_identity(identity: ProcessIdentity, expected_argv: list[str],
                              qemu_binary: Path) -> None:
    if identity.argv != tuple(expected_argv):
        raise CalibrationError("live QEMU /proc cmdline differs from captured argv")
    try:
        same_binary = os.path.samefile(identity.exe, qemu_binary)
    except OSError as error:
        raise CalibrationError(f"cannot compare live QEMU executable: {error}") from error
    if not same_binary:
        raise CalibrationError("live QEMU executable differs from qemu_binary artifact")


def require_pidfd_alive(pidfd: int) -> None:
    readable, _, _ = select.select([pidfd], [], [], 0)
    if readable:
        raise CalibrationError("QEMU exited before calibration postflight")


def option_values(argv: list[str] | tuple[str, ...], option: str) -> list[str]:
    values: list[str] = []
    for index, item in enumerate(argv):
        if item == option:
            if index + 1 >= len(argv) or argv[index + 1].startswith("-"):
                raise CalibrationError(f"QEMU option {option} lacks a value")
            values.append(argv[index + 1])
    return values


def unique_option(argv: list[str] | tuple[str, ...], option: str) -> str:
    values = option_values(argv, option)
    if len(values) != 1:
        raise CalibrationError(f"QEMU calibration requires exactly one {option} option")
    return values[0]


def comma_fields(text: str, label: str) -> tuple[str, dict[str, str]]:
    parts = text.split(",")
    base = "" if "=" in parts[0] else parts[0]
    fields: dict[str, str] = {}
    for part in parts[0 if not base else 1:]:
        if "=" not in part:
            fields[part] = ""
            continue
        key, value = part.split("=", 1)
        if not key or not value or key in fields:
            raise CalibrationError(f"invalid or duplicate {label} field: {part}")
        fields[key] = value
    return base, fields


def resolve_qemu_path(cwd: str, text: str, label: str) -> Path:
    if "," in text:
        raise CalibrationError(f"{label} path must not contain commas")
    path = Path(text)
    return (Path(cwd) / path).resolve() if not path.is_absolute() else path.resolve()


def validate_qemu_policy(argv: list[str] | tuple[str, ...], cwd: str, accel: str, vcpus: int,
                         kernel: Path, disk: Path, serial_socket: Path) -> None:
    for forbidden in ("-S", "-d", "-trace", "-icount"):
        if forbidden in argv:
            raise CalibrationError(f"QEMU calibration forbids {forbidden}")
    smp, smp_fields = comma_fields(unique_option(argv, "-smp"), "SMP")
    if not smp.isdecimal() or int(smp) != vcpus:
        raise CalibrationError("QEMU -smp does not match the claimed vCPU count")
    if set(smp_fields) != {"cores", "threads", "sockets"} or any(
            not value.isdecimal() or int(value) <= 0 for value in smp_fields.values()):
        raise CalibrationError("QEMU calibration requires a complete positive SMP topology")
    if int(smp_fields["cores"]) * int(smp_fields["threads"]) * int(smp_fields["sockets"]) != vcpus:
        raise CalibrationError("QEMU SMP topology product differs from the vCPU count")
    _, machine = comma_fields(unique_option(argv, "-machine"), "machine")
    if machine.get("accel") != accel:
        raise CalibrationError("QEMU machine accelerator differs from the claimed accelerator")
    enable_kvm = sum(item == "-enable-kvm" for item in argv)
    if (accel == "kvm" and enable_kvm != 1) or (accel != "kvm" and enable_kvm != 0):
        raise CalibrationError("QEMU -enable-kvm does not match the claimed accelerator")
    kernel_path = resolve_qemu_path(cwd, unique_option(argv, "-kernel"), "kernel")
    if kernel_path != kernel.resolve():
        raise CalibrationError("QEMU -kernel differs from the kernel artifact")
    disk_matches = []
    for drive in option_values(argv, "-drive"):
        _, fields = comma_fields(drive, "drive")
        if fields.get("id") == "disk":
            disk_matches.append(fields)
    if len(disk_matches) != 1:
        raise CalibrationError("QEMU calibration requires exactly one id=disk drive")
    disk_fields = disk_matches[0]
    if disk_fields.get("snapshot") != "on" or "file" not in disk_fields:
        raise CalibrationError("QEMU calibration disk must use snapshot=on")
    disk_path = resolve_qemu_path(cwd, disk_fields["file"], "disk")
    if disk_path != disk.resolve():
        raise CalibrationError("QEMU disk differs from the disk artifact")
    if unique_option(argv, "-serial") != "none" or unique_option(argv, "-monitor") != "none":
        raise CalibrationError("QEMU calibration requires serial and monitor to be disabled")
    _, rtc = comma_fields(unique_option(argv, "-rtc"), "RTC")
    if rtc.get("clock") != "host":
        raise CalibrationError("QEMU calibration requires an RTC driven by the host clock")
    chardevs = option_values(argv, "-chardev")
    if len(chardevs) != 1:
        raise CalibrationError("QEMU calibration requires exactly one chardev")
    backend, chardev = comma_fields(chardevs[0], "chardev")
    if (backend != "socket" or chardev.get("server") != "on" or
            chardev.get("wait") != "off" or "mux" in chardev or "path" not in chardev):
        raise CalibrationError("QEMU calibration chardev policy is invalid")
    if resolve_qemu_path(cwd, chardev["path"], "serial socket") != serial_socket.resolve():
        raise CalibrationError("QEMU chardev path differs from the requested serial socket")
    chardev_id = chardev.get("id")
    devices = option_values(argv, "-device")
    if sum(device.startswith("virtconsole,") and f"chardev={chardev_id}" in device.split(",")
           for device in devices) != 1:
        raise CalibrationError("QEMU virtconsole does not reference the calibration chardev")


def qemu_affinity_snapshot(pid: int, allowed: set[int]) -> list[dict[str, Any]]:
    task_root = Path(f"/proc/{pid}/task")
    if not task_root.is_dir():
        raise CalibrationError(f"QEMU pid is not running: {pid}")
    snapshots: list[dict[str, Any]] = []
    for task in sorted(task_root.iterdir(), key=lambda path: int(path.name)):
        try:
            comm = (task / "comm").read_text(encoding="utf-8").strip()
            status = (task / "status").read_text(encoding="utf-8")
        except OSError as error:
            raise CalibrationError(f"cannot snapshot QEMU task {task.name}: {error}") from error
        match = re.search(r"^Cpus_allowed_list:\s*(\S+)\s*$", status, re.MULTILINE)
        if match is None:
            raise CalibrationError(f"QEMU task {task.name} lacks Cpus_allowed_list")
        actual = parse_cpu_list(match.group(1))
        if not actual or not actual.issubset(allowed):
            raise CalibrationError(
                f"QEMU task {task.name} affinity {sorted(actual)} is outside expected {sorted(allowed)}"
            )
        snapshots.append({"tid": int(task.name), "comm": comm, "affinity": sorted(actual)})
    if not snapshots:
        raise CalibrationError("QEMU process has no observable tasks")
    return snapshots


def case_timeout_ns(case: dict[str, Any], accel: str) -> int:
    target = int(case["target_ns"])
    if case["mode"] == "reads":
        # DragonOS currently services clock_gettime through a syscall rather
        # than a vDSO.  Two domains x 10^7 reads take several minutes and must
        # not be accidentally classified as a transport timeout.
        return (600 if accel == "kvm" else 3600) * 1_000_000_000
    if accel == "kvm":
        return (30 if target <= 10_000_000_000 else 90) * 1_000_000_000
    return max(180_000_000_000, target * 3)


def run_case(transport: SerialTransport, run_id: str, seq: int, spec: dict[str, Any],
             accel: str, vcpus: int) -> dict[str, Any]:
    case_id = spec["case_id"]
    timeout = case_timeout_ns(spec, accel)
    command = (
        f"START run={run_id} seq={seq} case={case_id} mode={spec['mode']} "
        f"target_ns={spec['target_ns']} affinity={spec['affinity']} reads={spec['reads']}"
    )
    slo = raw_now_ns()
    transport.send_line(command)
    start = transport.read_event("START", run_id, seq, case_id, 10_000_000_000)
    shi = raw_now_ns()
    work_done = transport.read_event("WORK_DONE", run_id, seq, case_id, timeout)
    elo = raw_now_ns()
    transport.send_line(f"END run={run_id} seq={seq}")
    end = transport.read_event("END", run_id, seq, case_id, 10_000_000_000)
    ehi = raw_now_ns()
    start = stored_event("START", start.fields, START_FIELDS)
    work_done = stored_event("WORK_DONE", work_done.fields, WORK_DONE_FIELDS)
    end = stored_event("END", end.fields, END_FIELDS)
    ready = transport.read_event("READY", run_id, seq, None, 10_000_000_000)
    validate_ready(ready, vcpus)

    bracket = {"slo_ns": slo, "shi_ns": shi, "elo_ns": elo, "ehi_ns": ehi}
    status, reasons, metrics = evaluate_case_evidence(
        spec, accel, vcpus, start, work_done, end, bracket
    )
    return {
        **spec,
        "seq": seq,
        "status": status,
        "reasons": reasons,
        "guest": {"start": start.fields, "work_done": work_done.fields, "end": end.fields},
        "host_bracket": bracket,
        "metrics": public_metrics(metrics),
    }


def finalize_vm_status(cases: list[dict[str, Any]], thresholds: Thresholds) -> tuple[str, list[str]]:
    incomplete = [f"case_incomplete:{case['case_id']}" for case in cases
                  if case["status"] == "incomplete"]
    failed = [f"case_failed:{case['case_id']}" for case in cases if case["status"] == "fail"]
    if incomplete:
        return "incomplete", sorted(set(incomplete + failed))
    if failed:
        return "fail", sorted(set(failed))
    dispersion = evaluate_dispersion(cases, thresholds)
    return ("pass" if not dispersion else "fail", sorted(set(dispersion)))


def load_qemu_argv(path: Path) -> list[str]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CalibrationError(f"cannot read QEMU argv JSON {path}: {error}") from error
    if not isinstance(value, list) or not value or not all(isinstance(item, str) and item for item in value):
        raise CalibrationError("QEMU argv JSON must be a non-empty string array")
    return value


def run_vm(args: argparse.Namespace) -> int:
    run_id = args.run_id or secrets.token_hex(16)
    if not HEX128.fullmatch(run_id):
        raise CalibrationError("run-id must be exactly 32 lowercase hexadecimal characters")
    if args.boot_timeout <= 0 or args.boot_timeout > 3600:
        raise CalibrationError("boot-timeout must be in the range 1..3600 seconds")
    if args.host_cpu < 0 or args.host_cpu > MAX_CPU_ID:
        raise CalibrationError("host CPU is outside calibration limits")
    if args.qemu_pid <= 0:
        raise CalibrationError("QEMU pid must be positive")
    artifact_store = validate_artifact_store(args.artifact_store)
    output = args.output.resolve()
    create_output_dir(output)
    base: dict[str, Any] = {
        "schema": SCHEMA,
        "run_id": run_id,
        "created_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "status": "incomplete",
        "accel": args.accel,
        "vcpus": args.vcpus,
        "thresholds": Thresholds.for_accel(args.accel).as_json(),
        "cases": [],
        "reasons": [],
    }
    result_path = output / "result.json"
    transcript_path = output / "serial.raw"
    pidfd: int | None = None
    try:
        qemu_cpus = parse_cpu_list(args.qemu_cpus)
        if args.host_cpu in qemu_cpus:
            raise CalibrationError("host orchestrator CPU must not overlap QEMU CPUs")
        try:
            os.sched_setaffinity(0, {args.host_cpu})
        except OSError as error:
            raise CalibrationError(f"cannot pin host orchestrator to CPU {args.host_cpu}: {error}") from error
        qemu_argv = load_qemu_argv(args.qemu_argv_json)
        qemu_binary_text = shutil.which(qemu_argv[0]) or qemu_argv[0]
        artifact_paths = {
            "kernel": args.kernel_artifact,
            "disk_image": args.disk_artifact,
            "guest_helper": args.helper_artifact,
            "host_runner": Path(__file__).resolve(),
            "qemu_argv": args.qemu_argv_json,
            "qemu_binary": Path(qemu_binary_text),
        }
        for artifact in artifact_paths.values():
            if not artifact.is_file() or artifact.is_symlink():
                raise CalibrationError(f"required regular non-symlink artifact is missing: {artifact}")
        try:
            pidfd = os.pidfd_open(args.qemu_pid, 0)
        except OSError as error:
            raise CalibrationError(f"cannot open QEMU pidfd: {error}") from error
        process_identity = capture_process_identity(args.qemu_pid)
        require_pidfd_alive(pidfd)
        validate_process_identity(process_identity, qemu_argv, artifact_paths["qemu_binary"])
        validate_qemu_policy(
            qemu_argv, process_identity.cwd, args.accel, args.vcpus,
            args.kernel_artifact, args.disk_artifact, args.serial_socket,
        )
        base["qemu"] = {
            "argv": qemu_argv,
            "argv_source": str(args.qemu_argv_json.resolve()),
            "serial_socket": str(args.serial_socket.resolve()),
        }
        base.update(provenance(qemu_argv))
        base["host"]["requested_cpu"] = args.host_cpu
        base["qemu"]["pid"] = args.qemu_pid
        base["qemu"]["requested_cpus"] = sorted(qemu_cpus)
        base["qemu"]["task_affinity"] = qemu_affinity_snapshot(args.qemu_pid, qemu_cpus)
        base["qemu"]["process_identity"] = process_identity.as_json()
        base["build_artifacts"] = {
            name: archive_artifact(path, artifact_store) for name, path in artifact_paths.items()
        }
        with transcript_path.open("xb") as transcript:
            transport = SerialTransport(args.serial_socket, transcript)
            try:
                prompt_deadline = raw_now_ns() + args.boot_timeout * 1_000_000_000
                prompt = re.compile(args.prompt)
                transport.wait_for_prompt(prompt, prompt_deadline)
                command = args.guest_command.format(run_id=run_id)
                if "\n" in command or "\r" in command:
                    raise CalibrationError("guest command must be one line")
                transport.send_line(command)
                ready = transport.read_event("READY", run_id, 0, None, 10_000_000_000)
                validate_ready(ready, args.vcpus)
                seq = 1
                cases: list[dict[str, Any]] = []
                base["cases"] = cases
                for spec in expected_cases(args.vcpus):
                    if "predefined_skip" in spec:
                        cases.append({**spec, "seq": seq, "status": "skip", "reasons": [spec["predefined_skip"]]})
                        continue
                    cases.append(run_case(transport, run_id, seq, spec, args.accel, args.vcpus))
                    seq += 1
                transport.send_line(f"QUIT run={run_id} seq={seq}")
                ack = transport.read_event("ACK", run_id, seq, None, SOCKET_IO_TIMEOUT_NS)
                validate_ack(ack, run_id, seq)
                base["shutdown_ack"] = ack.fields
                base["status"], base["reasons"] = finalize_vm_status(
                    cases, Thresholds.for_accel(args.accel)
                )
            finally:
                transport.close()
        for name, artifact in artifact_paths.items():
            current = artifact_record(artifact)
            archived = base["build_artifacts"][name]
            if (current["sha256"] != archived["sha256"] or current["size"] != archived["size"] or
                    str(artifact.resolve()) != archived["execution_path"]):
                raise CalibrationError(f"build artifact changed during calibration: {artifact}")
        require_pidfd_alive(pidfd)
        process_identity_after = capture_process_identity(args.qemu_pid)
        if process_identity_after != process_identity:
            raise CalibrationError("QEMU process identity changed during calibration")
        validate_process_identity(process_identity_after, qemu_argv, artifact_paths["qemu_binary"])
        base["qemu"]["task_affinity_after"] = qemu_affinity_snapshot(args.qemu_pid, qemu_cpus)
        base["qemu"]["lifecycle"] = {
            "alive_after_calibration": True,
            "termination_owner": "external_launcher",
            "guest_helper_shutdown": "quit_ack",
        }
        verify_transcript_session(base, parse_transcript(transcript_path))
    except (CalibrationError, OSError, ValueError) as error:
        base["status"] = "incomplete"
        base["reasons"] = [str(error)]
    finally:
        if pidfd is not None:
            os.close(pidfd)
        if transcript_path.exists():
            base["serial"] = artifact_record(transcript_path)
        write_json_new(result_path, base)
        sums = [result_path]
        if transcript_path.exists():
            sums.append(transcript_path)
        with (output / "sha256sums.txt").open("x", encoding="ascii") as index:
            for path in sums:
                index.write(f"{sha256_file(path)}  {path.name}\n")
    return 0 if base["status"] == "pass" else 1 if base["status"] == "fail" else 2


START_FIELDS = {"run", "seq", "case", "guest_raw_ns", "guest_mono_ns", "cpu", "status"}
WORK_DONE_FIELDS = {
    "run", "seq", "case", "status", "reason", "work_end_raw_ns", "work_end_mono_ns",
    "checksum", "raw_reads", "mono_reads", "raw_regressions", "mono_regressions",
    "raw_max_backward_ns", "mono_max_backward_ns", "migrations_requested",
    "migrations_observed", "cpu_mask_seen",
}
END_FIELDS = START_FIELDS
VM_TOP_LEVEL_FIELDS = {
    "schema", "run_id", "created_utc", "status", "accel", "vcpus", "thresholds", "cases",
    "reasons", "qemu", "repo", "host", "qemu_version", "build_artifacts", "serial",
    "shutdown_ack",
}


def require_exact_keys(value: dict[str, Any], expected: set[str], label: str) -> None:
    if set(value) != expected:
        missing = sorted(expected - set(value))
        extra = sorted(set(value) - expected)
        raise CalibrationError(f"{label} schema mismatch (missing={missing}, extra={extra})")


def stored_event(kind: str, value: Any, expected_fields: set[str]) -> Event:
    if not isinstance(value, dict) or not all(isinstance(key, str) and isinstance(item, str)
                                               for key, item in value.items()):
        raise CalibrationError(f"stored {kind} event must be a string map")
    require_exact_keys(value, expected_fields, f"stored {kind}")
    if value["status"] not in ("ok", "fail"):
        raise CalibrationError(f"stored {kind} has invalid status")
    event = Event(kind, value, "")
    numeric = ({"seq", "guest_raw_ns", "guest_mono_ns"} if kind in ("START", "END") else {
        "seq", "work_end_raw_ns", "work_end_mono_ns", "raw_reads", "mono_reads",
        "raw_regressions", "mono_regressions", "raw_max_backward_ns", "mono_max_backward_ns",
        "migrations_requested", "migrations_observed",
    })
    for name in numeric:
        uint_field(event, name)
    if kind in ("START", "END"):
        cpu = value["cpu"]
        if not cpu or (cpu[0] == "-" and not cpu[1:].isdecimal()) or (cpu[0] != "-" and not cpu.isdecimal()):
            raise CalibrationError(f"stored {kind} CPU is not an integer")
    else:
        if not re.fullmatch(r"[0-9a-f]{16}", value["checksum"]):
            raise CalibrationError("stored WORK_DONE checksum is not 64-bit hexadecimal")
        if not re.fullmatch(r"[0-9a-f]{16}", value["cpu_mask_seen"]):
            raise CalibrationError("stored WORK_DONE CPU mask is not 64-bit hexadecimal")
    return event


def stored_bracket(value: Any) -> dict[str, int]:
    expected = {"slo_ns", "shi_ns", "elo_ns", "ehi_ns"}
    if not isinstance(value, dict):
        raise CalibrationError("stored host bracket must be an object")
    require_exact_keys(value, expected, "stored host bracket")
    for name in expected:
        number = value[name]
        if isinstance(number, bool) or not isinstance(number, int) or number < 0 or number > (1 << 64) - 1:
            raise CalibrationError(f"stored host bracket field is not u64: {name}")
    return value


def verify_artifact_record(value: Any, label: str) -> tuple[str, str, int]:
    if not isinstance(value, dict):
        raise CalibrationError(f"{label} artifact record must be an object")
    require_exact_keys(value, {"path", "sha256", "size"}, f"{label} artifact")
    path_text, digest, size = value["path"], value["sha256"], value["size"]
    if (not isinstance(path_text, str) or not path_text or not isinstance(digest, str) or
            not HEX256.fullmatch(digest) or isinstance(size, bool) or not isinstance(size, int) or size < 0):
        raise CalibrationError(f"{label} artifact record has invalid field types")
    path = Path(path_text)
    if not path.is_file() or path.is_symlink():
        raise CalibrationError(f"{label} artifact is missing or not a regular file: {path}")
    actual_size = path.stat().st_size
    actual_digest = sha256_file(path)
    if actual_size != size or actual_digest != digest:
        raise CalibrationError(f"{label} artifact digest or size mismatch: {path}")
    return path_text, digest, size


def verify_archived_artifact(value: Any, label: str) -> tuple[str, str, str, int]:
    if not isinstance(value, dict):
        raise CalibrationError(f"{label} archived artifact must be an object")
    require_exact_keys(
        value, {"execution_path", "archive_path", "sha256", "size"},
        f"{label} archived artifact",
    )
    execution, archive_text, digest, size = (
        value["execution_path"], value["archive_path"], value["sha256"], value["size"]
    )
    if (not isinstance(execution, str) or not Path(execution).is_absolute() or
            not isinstance(archive_text, str) or not Path(archive_text).is_absolute() or
            not isinstance(digest, str) or not HEX256.fullmatch(digest) or
            isinstance(size, bool) or not isinstance(size, int) or size < 0):
        raise CalibrationError(f"{label} archived artifact has invalid fields")
    archive = Path(archive_text)
    if (archive.name != digest or not archive.is_file() or archive.is_symlink() or
            archive.stat().st_mode & 0o222 != 0 or archive.stat().st_size != size or
            sha256_file(archive) != digest):
        raise CalibrationError(f"{label} content-addressed artifact is invalid: {archive}")
    return execution, archive_text, digest, size


def verify_task_snapshots(value: Any, allowed: set[int], label: str) -> None:
    if not isinstance(value, list) or not value:
        raise CalibrationError(f"{label} must be a non-empty array")
    tids: set[int] = set()
    for snapshot in value:
        if not isinstance(snapshot, dict):
            raise CalibrationError(f"{label} entry must be an object")
        require_exact_keys(snapshot, {"tid", "comm", "affinity"}, f"{label} entry")
        tid, comm, affinity = snapshot["tid"], snapshot["comm"], snapshot["affinity"]
        if (isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0 or tid in tids or
                not isinstance(comm, str) or not comm or not isinstance(affinity, list) or
                not affinity or any(isinstance(cpu, bool) or not isinstance(cpu, int)
                                    for cpu in affinity) or not set(affinity).issubset(allowed)):
            raise CalibrationError(f"{label} entry has invalid fields")
        tids.add(tid)


def verify_provenance_schema(value: dict[str, Any]) -> None:
    qemu, repo, host = value["qemu"], value["repo"], value["host"]
    require_exact_keys(qemu, {"argv", "argv_source", "serial_socket", "pid", "requested_cpus",
                              "task_affinity", "task_affinity_after", "process_identity",
                              "lifecycle"}, "QEMU provenance")
    if (not isinstance(qemu["argv"], list) or not qemu["argv"] or
            not all(isinstance(item, str) and item for item in qemu["argv"]) or
            not isinstance(qemu["argv_source"], str) or not Path(qemu["argv_source"]).is_absolute() or
            not isinstance(qemu["serial_socket"], str) or not Path(qemu["serial_socket"]).is_absolute() or
            isinstance(qemu["pid"], bool) or not isinstance(qemu["pid"], int) or qemu["pid"] <= 0 or
            not isinstance(qemu["requested_cpus"], list) or not qemu["requested_cpus"] or
            any(isinstance(cpu, bool) or not isinstance(cpu, int) for cpu in qemu["requested_cpus"])):
        raise CalibrationError("QEMU provenance has invalid field types")
    requested = set(qemu["requested_cpus"])
    if len(requested) != len(qemu["requested_cpus"]) or len(requested) > MAX_CPU_COUNT or \
            min(requested) < 0 or max(requested) > MAX_CPU_ID:
        raise CalibrationError("QEMU requested CPU evidence is invalid")
    verify_task_snapshots(qemu["task_affinity"], requested, "QEMU pre-run task affinity")
    verify_task_snapshots(qemu["task_affinity_after"], requested, "QEMU post-run task affinity")
    process = qemu["process_identity"]
    if not isinstance(process, dict):
        raise CalibrationError("QEMU process identity must be an object")
    require_exact_keys(process, {"pid", "starttime_ticks", "exe", "cwd", "cmdline_sha256"},
                       "QEMU process identity")
    encoded_argv = b"\0".join(os.fsencode(item) for item in qemu["argv"]) + b"\0"
    if (process["pid"] != qemu["pid"] or isinstance(process["starttime_ticks"], bool) or
            not isinstance(process["starttime_ticks"], int) or process["starttime_ticks"] <= 0 or
            not isinstance(process["exe"], str) or not Path(process["exe"]).is_absolute() or
            not isinstance(process["cwd"], str) or not Path(process["cwd"]).is_absolute() or
            process["cmdline_sha256"] != hashlib.sha256(encoded_argv).hexdigest()):
        raise CalibrationError("QEMU process identity evidence is invalid")
    if qemu["lifecycle"] != {
        "alive_after_calibration": True,
        "termination_owner": "external_launcher",
        "guest_helper_shutdown": "quit_ack",
    }:
        raise CalibrationError("QEMU lifecycle evidence is invalid")

    require_exact_keys(repo, {"commit", "tree", "dirty", "status_sha256", "tracked_diff_sha256",
                              "untracked_content_sha256"},
                       "repository provenance")
    if (not isinstance(repo["commit"], str) or not isinstance(repo["tree"], str) or
            not isinstance(repo["dirty"], bool) or not isinstance(repo["status_sha256"], str) or
            not HEX256.fullmatch(repo["status_sha256"]) or
            not isinstance(repo["tracked_diff_sha256"], str) or
            not HEX256.fullmatch(repo["tracked_diff_sha256"]) or
            not isinstance(repo["untracked_content_sha256"], str) or
            not HEX256.fullmatch(repo["untracked_content_sha256"])):
        raise CalibrationError("repository provenance has invalid field types")

    require_exact_keys(host, {"platform", "python", "affinity", "clock_monotonic_raw_resolution_ns",
                              "requested_cpu"}, "host provenance")
    if (not isinstance(host["platform"], str) or not isinstance(host["python"], str) or
            not isinstance(host["affinity"], list) or not host["affinity"] or
            any(isinstance(cpu, bool) or not isinstance(cpu, int) for cpu in host["affinity"]) or
            isinstance(host["clock_monotonic_raw_resolution_ns"], bool) or
            not isinstance(host["clock_monotonic_raw_resolution_ns"], int) or
            host["clock_monotonic_raw_resolution_ns"] <= 0 or
            isinstance(host["requested_cpu"], bool) or not isinstance(host["requested_cpu"], int) or
            host["requested_cpu"] < 0 or host["requested_cpu"] > MAX_CPU_ID or
            host["requested_cpu"] in requested):
        raise CalibrationError("host provenance has invalid field types or CPU isolation")


def verify_result_index(result_path: Path, serial_record: dict[str, Any]) -> None:
    if result_path.name != "result.json":
        raise CalibrationError(f"VM result must be named result.json: {result_path}")
    serial_path = Path(serial_record["path"])
    if serial_path.name != "serial.raw":
        raise CalibrationError("serial artifact must be named serial.raw")
    index_path = result_path.parent / "sha256sums.txt"
    try:
        lines = index_path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeError) as error:
        raise CalibrationError(f"cannot read VM artifact index {index_path}: {error}") from error
    expected = [
        f"{sha256_file(result_path)}  result.json",
        f"{serial_record['sha256']}  serial.raw",
    ]
    if lines != expected:
        raise CalibrationError(f"VM artifact index does not match result and serial: {index_path}")


def verify_case(case: Any, spec: dict[str, Any], accel: str, vcpus: int,
                run_id: str, seq: int) -> dict[str, Any]:
    if not isinstance(case, dict):
        raise CalibrationError(f"case {spec['case_id']} must be an object")
    metadata = {"case_id", "mode", "target_ns", "affinity", "reads", "repetition"}
    if "predefined_skip" in spec:
        require_exact_keys(case, metadata | {"predefined_skip", "seq", "status", "reasons"},
                           f"case {spec['case_id']}")
        if any(case.get(name) != value for name, value in spec.items()):
            raise CalibrationError(f"case metadata differs from frozen matrix: {spec['case_id']}")
        if (case["seq"] != seq or case["status"] != "skip" or
                case["reasons"] != [spec["predefined_skip"]]):
            raise CalibrationError(f"predefined skip evidence is invalid: {spec['case_id']}")
        return case

    require_exact_keys(case, metadata | {"seq", "status", "reasons", "guest", "host_bracket",
                                         "metrics"}, f"case {spec['case_id']}")
    if any(case.get(name) != value for name, value in spec.items()) or case["seq"] != seq:
        raise CalibrationError(f"case metadata differs from frozen matrix: {spec['case_id']}")
    guest = case["guest"]
    if not isinstance(guest, dict):
        raise CalibrationError(f"case guest evidence must be an object: {spec['case_id']}")
    require_exact_keys(guest, {"start", "work_done", "end"}, f"case guest {spec['case_id']}")
    start = stored_event("START", guest["start"], START_FIELDS)
    work_done = stored_event("WORK_DONE", guest["work_done"], WORK_DONE_FIELDS)
    end = stored_event("END", guest["end"], END_FIELDS)
    for event in (start, work_done, end):
        validate_identity(event, event.kind, run_id, seq, spec["case_id"])
    if work_done.fields["status"] == "ok" and work_done.fields["reason"] != "ok":
        raise CalibrationError(f"successful WORK_DONE has a non-ok reason: {spec['case_id']}")
    bracket = stored_bracket(case["host_bracket"])
    status, reasons, metrics = evaluate_case_evidence(
        spec, accel, vcpus, start, work_done, end, bracket
    )
    if case["status"] != status or case["reasons"] != reasons:
        raise CalibrationError(f"stored case verdict differs from raw evidence: {spec['case_id']}")
    if case["metrics"] != public_metrics(metrics):
        raise CalibrationError(f"stored case metrics differ from raw evidence: {spec['case_id']}")
    return case


ARTIFACT_NAMES = (
    "kernel", "disk_image", "guest_helper", "host_runner", "qemu_argv", "qemu_binary",
)
SHARED_ARTIFACT_NAMES = (
    "kernel", "disk_image", "guest_helper", "host_runner", "qemu_binary",
)


def verify_vm_result(value: Any, result_path: Path) -> tuple[tuple[str, int], tuple[str, ...]]:
    if not isinstance(value, dict):
        raise CalibrationError(f"VM result must be an object: {result_path}")
    require_exact_keys(value, VM_TOP_LEVEL_FIELDS, "VM result")
    if value["schema"] != SCHEMA:
        raise CalibrationError(f"incompatible VM result schema: {result_path}")
    run_id, accel, vcpus = value["run_id"], value["accel"], value["vcpus"]
    if not isinstance(run_id, str) or not HEX128.fullmatch(run_id):
        raise CalibrationError(f"VM result has invalid run ID: {result_path}")
    if accel not in ("kvm", "tcg") or vcpus not in (1, 2):
        raise CalibrationError(f"invalid VM result identity: {result_path}")
    if (not isinstance(value["created_utc"], str) or not isinstance(value["qemu_version"], str) or
            not isinstance(value["qemu"], dict) or not isinstance(value["repo"], dict) or
            not isinstance(value["host"], dict) or not isinstance(value["reasons"], list) or
            not all(isinstance(reason, str) for reason in value["reasons"])):
        raise CalibrationError(f"VM result has invalid top-level field types: {result_path}")
    try:
        dt.datetime.fromisoformat(value["created_utc"])
    except ValueError as error:
        raise CalibrationError(f"VM result creation time is invalid: {result_path}") from error
    verify_provenance_schema(value)
    if value["thresholds"] != Thresholds.for_accel(accel).as_json():
        raise CalibrationError(f"VM result thresholds differ from frozen policy: {result_path}")
    if not isinstance(value["cases"], list):
        raise CalibrationError(f"VM result cases must be an array: {result_path}")
    expected = expected_cases(vcpus)
    if len(value["cases"]) != len(expected):
        raise CalibrationError(f"VM result has an incomplete case matrix: {result_path}")
    seq = 1
    verified_cases: list[dict[str, Any]] = []
    for case, spec in zip(value["cases"], expected):
        verified_cases.append(verify_case(case, spec, accel, vcpus, run_id, seq))
        if "predefined_skip" not in spec:
            seq += 1
    recomputed_status, recomputed_reasons = finalize_vm_status(
        verified_cases, Thresholds.for_accel(accel)
    )
    if value["status"] != recomputed_status or value["reasons"] != recomputed_reasons:
        raise CalibrationError(f"stored VM verdict differs from raw evidence: {result_path}")
    if value["shutdown_ack"] != {"run": run_id, "seq": str(seq), "status": "ok"}:
        raise CalibrationError(f"VM result lacks a valid shutdown ACK: {result_path}")
    build = value["build_artifacts"]
    if not isinstance(build, dict):
        raise CalibrationError(f"VM result build artifacts must be an object: {result_path}")
    require_exact_keys(build, set(ARTIFACT_NAMES), "build artifacts")
    verified_artifacts = {
        name: verify_archived_artifact(build[name], name) for name in ARTIFACT_NAMES
    }
    identity_hashes = tuple(verified_artifacts[name][2] for name in SHARED_ARTIFACT_NAMES)
    qemu_argv_execution = Path(verified_artifacts["qemu_argv"][0])
    qemu_argv_archive = Path(verified_artifacts["qemu_argv"][1])
    if qemu_argv_execution != Path(value["qemu"]["argv_source"]) or \
            load_qemu_argv(qemu_argv_archive) != value["qemu"]["argv"]:
        raise CalibrationError(f"QEMU argv provenance differs from its artifact: {result_path}")
    if Path(value["qemu"]["process_identity"]["exe"]) != Path(
            verified_artifacts["qemu_binary"][0]):
        raise CalibrationError(f"QEMU executable identity differs from its artifact: {result_path}")
    validate_qemu_policy(
        value["qemu"]["argv"], value["qemu"]["process_identity"]["cwd"], accel, vcpus,
        Path(verified_artifacts["kernel"][0]), Path(verified_artifacts["disk_image"][0]),
        Path(value["qemu"]["serial_socket"]),
    )
    serial = value["serial"]
    serial_path, _, _ = verify_artifact_record(serial, "serial")
    verify_transcript_session(value, parse_transcript(Path(serial_path)))
    verify_result_index(result_path, serial)
    return (accel, vcpus), identity_hashes


def aggregate(args: argparse.Namespace) -> int:
    output = args.output.resolve()
    create_output_dir(output)
    loaded: list[dict[str, Any]] = []
    reasons: list[str] = []
    saw_fail = False
    saw_incomplete = False
    seen: set[tuple[str, int]] = set()
    artifact_sets: set[tuple[str, ...]] = set()
    for path in args.vm_result:
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
            identity, identity_hashes = verify_vm_result(value, path)
            if identity in seen:
                raise CalibrationError(f"duplicate VM result identity: {identity}")
            seen.add(identity)
            loaded.append({"source": str(path), "sha256": sha256_file(path), "result": value})
            vm_status = value["status"]
            if vm_status == "fail":
                saw_fail = True
                reasons.append(f"vm_not_pass:{identity[0]}-{identity[1]}")
            elif vm_status == "incomplete":
                saw_incomplete = True
                reasons.append(f"vm_incomplete:{identity[0]}-{identity[1]}")
            elif vm_status != "pass":
                raise CalibrationError(f"invalid VM status in {path}")
            artifact_sets.add(identity_hashes)
        except (OSError, json.JSONDecodeError, CalibrationError) as error:
            saw_incomplete = True
            reasons.append(str(error))
    missing = REQUIRED_VMS - seen
    reasons.extend(f"missing_vm:{accel}-{vcpus}" for accel, vcpus in sorted(missing))
    if len(artifact_sets) > 1:
        reasons.append("vm_results_use_different_build_artifacts")
        saw_incomplete = True
    status = "pass"
    if missing or saw_incomplete:
        status = "incomplete"
    elif saw_fail or reasons:
        status = "fail"
    result = {
        "schema": SCHEMA,
        "kind": "matrix-summary",
        "created_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "status": status,
        "required_vms": [{"accel": accel, "vcpus": vcpus} for accel, vcpus in sorted(REQUIRED_VMS)],
        "reasons": sorted(set(reasons)),
        "vm_results": loaded,
    }
    result_path = output / "result.json"
    write_json_new(result_path, result)
    (output / "sha256sums.txt").write_text(f"{sha256_file(result_path)}  result.json\n", encoding="ascii")
    return 0 if status == "pass" else 1 if status == "fail" else 2


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    run = subparsers.add_parser("run-vm", help="drive one already-started VM through a Unix serial socket")
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--serial-socket", type=Path, required=True)
    run.add_argument("--qemu-argv-json", type=Path, required=True)
    run.add_argument("--kernel-artifact", type=Path, required=True)
    run.add_argument("--disk-artifact", type=Path, required=True)
    run.add_argument("--helper-artifact", type=Path, required=True)
    run.add_argument("--artifact-store", type=Path, required=True,
                     help="existing private content-addressed input archive")
    run.add_argument("--accel", choices=("kvm", "tcg"), required=True)
    run.add_argument("--vcpus", type=int, choices=(1, 2), required=True)
    run.add_argument("--host-cpu", type=int, required=True,
                     help="dedicated CPU for the host protocol/bracket process")
    run.add_argument("--qemu-pid", type=int, required=True)
    run.add_argument("--qemu-cpus", required=True,
                     help="expected non-overlapping QEMU affinity, for example 4-5")
    run.add_argument("--run-id")
    run.add_argument("--boot-timeout", type=int, default=180)
    run.add_argument("--prompt", default=r"root@dragonos:.*#\s*$")
    run.add_argument(
        "--guest-command",
        default="exec /opt/tests/timekeeping-calibration/timekeeping_calibration --run-id {run_id}",
    )
    run.set_defaults(function=run_vm)

    merge = subparsers.add_parser("aggregate", help="strictly aggregate KVM1/KVM2/TCG2 VM artifacts")
    merge.add_argument("--output", type=Path, required=True)
    merge.add_argument("--vm-result", type=Path, action="append", required=True)
    merge.set_defaults(function=aggregate)
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.function(args)
    except CalibrationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
