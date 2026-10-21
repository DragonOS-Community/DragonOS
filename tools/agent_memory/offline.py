#!/usr/bin/env python3
"""Compare kernels using real OpenCode tools and session persistence without a model.

Each attempt gets a fresh VM, workspace and server configuration. This measures
local application costs, not model latency or the complete memory acceptance SLO.
Example: offline.py --config public.json --kernel before=a.elf --kernel after=b.elf
--output results --rounds 3
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import time

import run as runner


def kernel_argument(value):
    label, separator, path = value.partition('=')
    if not separator or not re.fullmatch(r'[a-zA-Z0-9_-]{1,32}', label) or not path:
        raise argparse.ArgumentTypeError('kernel must be LABEL=PATH (safe unique label)')
    return label, str(Path(path).resolve(strict=True))


def idle_delta(before, after):
    if before['errors'] or after['errors'] or not before['processes']:
        raise ValueError('unavailable idle CPU sample')
    initial = {p['pid']: p for p in before['processes']}
    if set(initial) != {p['pid'] for p in after['processes']}:
        raise ValueError('idle process set changed')
    cpu = {}
    for process in after['processes']:
        previous = initial[process['pid']]
        if previous['start_ticks'] != process['start_ticks']:
            raise ValueError('idle process identity changed')
        ticks = process['cpu_ticks'] - previous['cpu_ticks']
        if ticks < 0:
            raise ValueError('idle CPU counter regressed')
        cpu[process['role']] = ticks / os.sysconf('SC_CLK_TCK')
    return {'wall_seconds': (after['monotonic_ns'] - before['monotonic_ns']) / 1e9,
            'cpu_seconds': cpu}


def cpu_delta(helper):
    result = {}
    for key in ('usage_usec', 'user_usec', 'system_usec'):
        before = helper['baseline']['cpu_stat'][key]
        after = helper['final']['cpu_stat'][key]
        if type(before) is not int or type(after) is not int or after < before:
            raise ValueError('unavailable or regressed guest CPU counter')
        result[key] = after - before
    return result


def attempt(config, directory, ram_mib, idle_seconds, iterations, timeout):
    row = {'status': 'infrastructure_failed'}
    vm = None
    try:
        with runner.HostSampler(directory / 'host.jsonl') as sampler:
            vm = runner.GuestVM(config, directory / 'vm', sampler, ram_mib)
            try:
                vm.start()
                before = sampler.sample()
                time.sleep(idle_seconds)
                row['idle'] = idle_delta(before, sampler.sample())
                payload = vm.payload
                command = [payload + '/bin/helper', '--output',
                           '/mnt/agent-memory/results/measurement', '--deadline-seconds', str(timeout),
                           '--', payload + '/bin/node', payload + '/workloads/offline-opencode.mjs',
                           payload + '/bin/opencode', payload + '/bin/node',
                           '/root/agent-memory/offline', '/mnt/agent-memory/results/workload', str(iterations)]
                host_before = sampler.sample()
                row['helper_exit'] = vm.command(shlex.join(command), timeout + 60, allow_failure=True)
                row['host_task'] = idle_delta(host_before, sampler.sample())
                helper = json.loads((vm.share / 'results/measurement/result.json').read_text())
                row['helper'] = helper
                row['guest_cpu_usec'] = cpu_delta(helper)
                workload = json.loads((vm.share / 'results/workload/workload.json').read_text())
                row['workload'] = workload
                passed = (row['helper_exit'] == 0 and helper['status'] == 'ok'
                          and helper['exit_code'] == 0 and helper['sample_errors'] == 0
                          and helper['diagnostic_errors'] == 0 and not helper['cleanup_required']
                          and helper['descendants_empty'] and workload['success'] is True)
                row['status'] = 'passed' if passed else 'failed'
            finally:
                if vm is not None:
                    vm.close()
                    vm.overlay.unlink(missing_ok=True)
    except Exception as error:
        row['status'] = 'infrastructure_failed'
        row['error'] = type(error).__name__ + ': ' + str(error)
    runner.write_json(directory / 'attempt.json', row)
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', required=True, type=Path, help='public GuestVM configuration')
    parser.add_argument('--kernel', required=True, action='append', type=kernel_argument)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--ram-mib', type=int, default=2048)
    parser.add_argument('--idle-seconds', type=int, default=20)
    parser.add_argument('--iterations', type=int, default=4, help='warm tools per session')
    parser.add_argument('--timeout-seconds', type=int, default=240)
    args = parser.parse_args()
    if (not 1 <= args.rounds <= 12 or not 1 <= len(args.kernel) <= 4
            or len(dict(args.kernel)) != len(args.kernel) or not 256 <= args.ram_mib <= 65536
            or not 1 <= args.idle_seconds <= 60 or not 1 <= args.iterations <= 16
            or not 30 <= args.timeout_seconds <= 1800):
        parser.error('invalid bounds or duplicate kernel labels')
    source = json.loads(args.config.read_text())
    keys = ('disk', 'helper', 'opencode', 'node', 'sdk', 'qemu', 'qemu_img', 'virtiofsd')
    # Deliberately do not load or forward provider configuration or credentials.
    config = {key: str(Path(source[key]).resolve(strict=True)) for key in keys}
    config['vcpus'] = source.get('vcpus', 2)
    if type(config['vcpus']) is not int or not 1 <= config['vcpus'] <= 64:
        parser.error('invalid vCPU count')
    args.output.mkdir(mode=0o700)
    workloads = args.output.resolve() / 'workloads'
    shutil.copytree(Path(__file__).parent / 'workloads', workloads)
    config['workload_dir'] = str(workloads)
    artifacts = {key: runner.digest(config[key]) for key in keys if key != 'sdk'}
    artifacts['sdk'] = runner.tree_digest(config['sdk'])
    artifacts['workloads'] = runner.tree_digest(workloads)
    kernels = {label: runner.digest(path) for label, path in args.kernel}
    runner.write_json(args.output / 'manifest.json', {
        'artifacts_sha256': artifacts, 'kernels_sha256': kernels,
        'vcpus': config['vcpus'], 'ram_mib': args.ram_mib, 'warm_iterations': args.iterations,
        'rounds': args.rounds, 'idle_seconds': args.idle_seconds,
        'host_kernel': os.uname().release, 'host_cpu_count': os.cpu_count(),
        'host_cpu_affinity': sorted(os.sched_getaffinity(0)),
        'qemu_version': subprocess.check_output([config['qemu'], '--version'], text=True).splitlines()[0],
        'model_requests': 0, 'scope': 'local OpenCode tools and service restart, not guest power loss'})
    rows = []
    try:
        for round_index in range(args.rounds):
            order = args.kernel if round_index % 2 == 0 else list(reversed(args.kernel))
            for label, path in order:
                directory = args.output.resolve() / f'{round_index}-{label}'
                directory.mkdir(mode=0o700)
                config['kernel'] = path
                row = attempt(config, directory, args.ram_mib, args.idle_seconds,
                              args.iterations, args.timeout_seconds)
                row.update(kernel=label, round=round_index)
                rows.append(row)
                runner.write_json(args.output / 'attempts.json', rows)
                print(json.dumps({'kernel': label, 'round': round_index, 'status': row['status']}), flush=True)
    finally:
        runner.write_json(args.output / 'attempts.json', rows)
    unchanged = all(runner.digest(config[k]) == v for k, v in artifacts.items() if k not in ('sdk', 'workloads'))
    unchanged &= runner.tree_digest(config['sdk']) == artifacts['sdk']
    unchanged &= runner.tree_digest(workloads) == artifacts['workloads']
    unchanged &= all(runner.digest(path) == kernels[label] for label, path in args.kernel)
    summary = {}
    for label in kernels:
        selected = [row for row in rows if row['kernel'] == label]
        passed = [row for row in selected if row['status'] == 'passed']
        summary[label] = {'attempted': len(selected), 'passed': len(passed),
                          'failed_or_unavailable': len(selected) - len(passed),
                          'successful_system_usec_median': statistics.median(
                              row['guest_cpu_usec']['system_usec'] for row in passed) if passed else None}
    runner.write_json(args.output / 'summary.json', {'kernels': summary, 'artifacts_unchanged': unchanged})
    return 0 if unchanged and rows and all(row['status'] == 'passed' for row in rows) else 1


def cancel(*_):
    raise KeyboardInterrupt


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, cancel)
    raise SystemExit(main())
