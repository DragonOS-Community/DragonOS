#!/usr/bin/env python3
"""Verify one HvcConsoleLastCloseTest using a dedicated QEMU port, never hvc0/1.

Add a separate controller to QEMU (the first two ports remain console/control):
  -chardev socket,id=close,path=/tmp/hvc-close.sock,server=on,wait=off
  -device virtio-serial,id=close-bus
  -device virtconsole,bus=close-bus.0,chardev=close

Start this tool with --socket and --result (a fresh, host-visible guest result
path). After it prints 'ready', run exactly one HvcConsoleLastCloseTest with
DUNITEST_HVC_CLOSE_DEVICE=/dev/hvc2 and --gtest_output=json:PATH. The receiver
keeps backpressure until that result completes, then verifies accepted bytes.
The host and guest PATHs must refer to the same file in a shared directory.
"""
import argparse
import json
from pathlib import Path
import socket
import time


def remaining(deadline):
    value = deadline - time.monotonic()
    if value <= 0:
        raise TimeoutError('fixture deadline expired')
    return value


def count(case, key):
    value = case[key]
    if not isinstance(value, str) or not value.isascii() or not value.isdecimal() or len(value) > 8:
        raise ValueError('invalid ' + key)
    value = int(value)
    if not 0 <= value <= 32 * 1024 * 1024:
        raise ValueError('out of bounds ' + key)
    return value


def accepted(result):
    cases = [case for suite in result['testsuites'] for case in suite['testsuite']]
    if (type(result['tests']) is not int or result['tests'] != 1
            or type(result['failures']) is not int or result['failures'] != 0 or len(cases) != 1):
        raise ValueError('requires one successful test')
    case = cases[0]
    if (case['classname'] != 'HvcConsoleLastCloseTest' or case['status'] != 'RUN'
            or case['result'] != 'COMPLETED' or case.get('failures') or case.get('skipped')):
        raise ValueError('requires a completed, unskipped close test')
    if case['close_device'] in ('/dev/hvc0', '/dev/hvc1'):
        raise ValueError('console/control ports cannot be used')
    a, b = count(case, 'accepted_a'), count(case, 'accepted_b')
    if a == 0 or b > 256:
        raise ValueError('invalid accepted byte total')
    return a, b


def verify(address, result_path, timeout):
    if result_path.exists():
        raise ValueError('refusing a stale result file')
    deadline = time.monotonic() + timeout
    with socket.socket(socket.AF_UNIX) as stream:
        while True:
            stream.settimeout(remaining(deadline))
            try:
                stream.connect(address)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                time.sleep(min(.02, remaining(deadline)))
        print('ready', flush=True)
        while True:
            remaining(deadline)
            try:
                # GTest writes JSON at the end; tolerate only an incomplete write.
                with result_path.open('rb') as evidence:
                    encoded = evidence.read(65537)
                if len(encoded) > 65536:
                    raise ValueError('result JSON exceeded 64 KiB')
                result = json.loads(encoded)
                break
            except (FileNotFoundError, json.JSONDecodeError):
                time.sleep(min(.02, remaining(deadline)))
        a, b = accepted(result)
        offset, total = 0, a + b
        while offset < total:
            stream.settimeout(remaining(deadline))
            try:
                block = stream.recv(min(65536, total - offset))
            except socket.timeout as error:
                raise TimeoutError(f'received only {offset}/{total} bytes') from error
            if not block:
                raise ValueError(f'EOF after {offset}/{total} bytes')
            first = max(0, min(len(block), a - offset))
            if block != b'A' * first + b'B' * (len(block) - first):
                raise ValueError(f'payload mismatch at offset {offset}')
            offset += len(block)
        stream.settimeout(min(.2, remaining(deadline)))
        try:
            if stream.recv(1):
                raise ValueError('unexpected trailing bytes')
        except socket.timeout:
            pass
        print(f'passed: {a} A bytes + {b} B bytes', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--socket', required=True)
    parser.add_argument('--result', type=Path, required=True)
    parser.add_argument('--timeout-seconds', type=int, default=30)
    args = parser.parse_args()
    if not 1 <= args.timeout_seconds <= 300:
        parser.error('timeout must be between 1 and 300 seconds')
    try:
        verify(args.socket, args.result, args.timeout_seconds)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, type(error).__name__ + ': ' + str(error) + '\n')


if __name__ == '__main__':
    main()
