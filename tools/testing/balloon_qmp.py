#!/usr/bin/env python3
"""Exercise an already booted guest's balloon using its dedicated QMP socket.

Use a disposable QEMU snapshot of the existing disk, and add these arguments:
  -snapshot -qmp unix:/tmp/balloon-qmp.sock,server=on,wait=off
  -device virtio-balloon-pci,id=balloon0,disable-legacy=on,free-page-reporting=on,deflate-on-oom=off

After the guest boots, start its memory integrity test before this tool:
  DUNITEST_BALLOON_SECONDS=90 /opt/tests/dunitest/bin/normal/balloon_memory_integrity_test
Then on the host:
  python3 tools/testing/balloon_qmp.py --socket /tmp/balloon-qmp.sock --reporting on --deflate-on-oom off
Repeat with reporting=off and deflate-on-oom=on in BOTH QEMU and this tool.
Use --baseline-unsupported on the pre-driver kernel with the SAME device.
This tool checks actual memory and negotiated features; actual host RSS release
must be measured separately and is not inferred from successful negotiation.
The guest, disk, VM lifecycle and console belong to the caller.
"""
import argparse
import json
import socket
import sys
import time


class QmpError(RuntimeError):
    def __init__(self, error):
        self.kind = error['class']
        super().__init__(f"{self.kind}: {error['desc']}")


class Qmp:
    def __init__(self, path, timeout):
        self.stream = socket.socket(socket.AF_UNIX)
        self.stream.settimeout(timeout)
        self.stream.connect(path)
        self.file = self.stream.makefile('rwb')
        self.sequence = 0
        if 'QMP' not in self.read():
            raise ValueError('missing QMP greeting')
        self.command('qmp_capabilities')

    def close(self):
        self.file.close()
        self.stream.close()

    def read(self):
        line = self.file.readline(1024 * 1024)
        if not line or not line.endswith(b'\n'):
            raise ValueError('QMP EOF or oversized response')
        return json.loads(line)

    def command(self, name, **arguments):
        self.sequence += 1
        self.file.write(json.dumps({'execute': name, 'arguments': arguments,
                                    'id': self.sequence}).encode() + b'\n')
        self.file.flush()
        # Events may arrive before this command's response.
        while True:
            message = self.read()
            if 'event' in message:
                continue
            if message.get('id') != self.sequence:
                raise ValueError('unexpected QMP response ID')
            if 'error' in message:
                raise QmpError(message['error'])
            return message['return']


def has_feature(status, name):
    return any(item.split(':', 1)[0] == name
               for item in status['guest-features']['dev-features'])


def wait_actual(qmp, target, timeout):
    deadline = time.monotonic() + timeout
    while True:
        actual = qmp.command('query-balloon')['actual']
        if actual == target:
            print(f'PASS actual={actual}', flush=True)
            return
        if time.monotonic() >= deadline:
            raise TimeoutError(f'actual={actual}, target={target} did not converge')
        time.sleep(.1)


def verify(qmp, args):
    if args.baseline_unsupported:
        try:
            qmp.command('query-balloon')
        except QmpError as error:
            if error.kind != 'DeviceNotActive':
                raise
            print('PASS baseline: balloon driver not active', flush=True)
            return
        status = qmp.command('x-query-virtio-status', path=args.device)
        if any(item.startswith('VIRTIO_CONFIG_S_DRIVER_OK:')
               for item in status['status']['statuses']):
            raise ValueError('baseline unexpectedly negotiated DRIVER_OK')
        initial = qmp.command('query-balloon')['actual']
        target = initial - args.delta_mib * 1024 * 1024
        if target < 128 * 1024 * 1024:
            raise ValueError('baseline target would leave less than 128 MiB')
        try:
            qmp.command('balloon', value=target)
            deadline = time.monotonic() + min(args.timeout_seconds, 3)
            while time.monotonic() < deadline:
                if qmp.command('query-balloon')['actual'] != initial:
                    raise ValueError('baseline actual changed without DRIVER_OK')
                time.sleep(.1)
        finally:
            qmp.command('balloon', value=initial)
        print('PASS baseline: DRIVER_OK absent; actual unchanged after request', flush=True)
        return

    status = qmp.command('x-query-virtio-status', path=args.device)
    for name, expected in [('VIRTIO_BALLOON_F_REPORTING', args.reporting),
                           ('VIRTIO_BALLOON_F_DEFLATE_ON_OOM', args.deflate_on_oom)]:
        enabled = has_feature(status, name)
        if enabled != (expected == 'on'):
            raise ValueError(f'{name}: negotiated={enabled}, expected={expected}')
        print(f'PASS {name}={expected}', flush=True)
    if status['broken'] or status['disabled'] or not status['started']:
        raise ValueError('balloon device is not running')
    initial = qmp.command('query-balloon')['actual']
    delta = args.delta_mib * 1024 * 1024
    if initial - 2 * delta < 128 * 1024 * 1024:
        raise ValueError('test would leave less than 128 MiB; use smaller --delta-mib')
    try:
        for target in [initial - delta, initial]:
            qmp.command('balloon', value=target)
            wait_actual(qmp, target, args.timeout_seconds)
        # Reverse the requested direction without waiting for the first target.
        # The worker must reread configuration and converge to the final target.
        qmp.command('balloon', value=initial - 2 * delta)
        qmp.command('balloon', value=initial - delta)
        wait_actual(qmp, initial - delta, args.timeout_seconds)
        qmp.command('balloon', value=initial)
        wait_actual(qmp, initial, args.timeout_seconds)
    finally:
        # Best effort restoration is independent of whether a test failed.
        qmp.command('balloon', value=initial)
    print('PASS inflate/deflate/reversal; confirm the guest integrity test passed', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--socket', required=True)
    parser.add_argument('--device', default='/machine/peripheral/balloon0/virtio-backend')
    parser.add_argument('--reporting', choices=['on', 'off'], default='on')
    parser.add_argument('--deflate-on-oom', choices=['on', 'off'], default='off')
    parser.add_argument('--baseline-unsupported', action='store_true')
    parser.add_argument('--delta-mib', type=int, default=64)
    parser.add_argument('--timeout-seconds', type=int, default=30)
    args = parser.parse_args()
    if not 1 <= args.delta_mib <= 1024 or not 1 <= args.timeout_seconds <= 300:
        parser.error('delta must be 1..1024 MiB; timeout must be 1..300 seconds')
    qmp = None
    try:
        qmp = Qmp(args.socket, args.timeout_seconds)
        verify(qmp, args)
    except (OSError, ValueError, RuntimeError, KeyError, TypeError) as error:
        print(f'FAIL {error}', file=sys.stderr)
        return 1
    finally:
        if qmp is not None:
            qmp.close()
    return 0


if __name__ == '__main__':
    sys.exit(main())
