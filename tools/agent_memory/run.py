#!/usr/bin/env python3
"""Run the fixed OpenCode memory profile in disposable DragonOS VMs.

No model credentials are read by this host process. All mutable guest state is
private to one qcow2 overlay; only an explicit restart reuses that overlay.
"""
import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import subprocess
import threading
import time
import uuid

CANCELLED = threading.Event()


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def tree_digest(root):
    manifest = []
    for path in sorted(Path(root).rglob('*')):
        if path.is_symlink():
            raise ValueError('SDK payload must contain regular files, not symbolic links')
        if path.is_file():
            manifest.append((path.relative_to(root).as_posix(), digest(path)))
    if not manifest:
        raise ValueError('empty SDK payload')
    return hashlib.sha256(json.dumps(manifest, separators=(',', ':')).encode()).hexdigest()


def percentile(values, fraction):
    """Nearest rank; callers publish sample count and failed-attempt denominator."""
    return sorted(values)[math.ceil(len(values) * fraction) - 1] if values else None


def proc_sample(pid, proc=Path('/proc')):
    # comm may contain spaces and ')' characters. Process stat already sums threads.
    fields = (proc / str(pid) / 'stat').read_text().rsplit(')', 1)[1].split()
    identity = int(fields[19])  # field 22, starttime since boot
    ticks = int(fields[11]) + int(fields[12])
    rollup = {}
    for line in (proc / str(pid) / 'smaps_rollup').read_text().splitlines():
        parts = line.split()
        if parts and parts[0] in ('Pss:', 'Rss:'):
            if len(parts) != 3 or parts[2] != 'kB':
                raise ValueError('invalid smaps unit')
            rollup[parts[0][:-1].lower() + '_bytes'] = int(parts[1]) * 1024
    if set(rollup) != {'pss_bytes', 'rss_bytes'}:
        raise ValueError('missing PSS/RSS')
    status = {}
    for line in (proc / str(pid) / 'status').read_text().splitlines():
        parts = line.split()
        if parts and parts[0] in ('VmPTE:', 'VmHWM:'):
            if len(parts) != 3 or parts[2] != 'kB':
                raise ValueError('invalid process status unit')
            status[parts[0][:-1].lower() + '_bytes'] = int(parts[1]) * 1024
    if set(status) != {'vmpte_bytes', 'vmhwm_bytes'}:
        raise ValueError('missing process page-table/resident high-water metrics')
    # Reject PID reuse during the two-file read, rather than attributing a neighbor.
    again = (proc / str(pid) / 'stat').read_text().rsplit(')', 1)[1].split()
    if int(again[19]) != identity:
        raise ValueError('process identity changed')
    return dict(pid=pid, start_ticks=identity, cpu_ticks=ticks, **rollup, **status)


class HostSampler:
    def __init__(self, output, interval=1):
        self.output = output
        self.interval = interval
        self.lock = threading.Lock()
        self.owned = {}
        self.rows = []
        self.stopped = threading.Event()
        self.failure = None
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def register(self, process, vm, role):
        row = proc_sample(process.pid)
        with self.lock:
            self.owned[process.pid] = (process, row['start_ticks'], vm, role)

    def unregister(self, process):
        with self.lock:
            self.owned.pop(process.pid, None)

    def sample(self):
        with self.lock:
            return self._sample_locked()

    def _sample_locked(self):
        # One sampler supplies a common host time axis. A missing live PID is an
        # unavailable sample, never zero. Never sum independent per-VM peaks.
        owners = list(self.owned.items())
        row = {'monotonic_ns': time.monotonic_ns(), 'processes': [], 'errors': []}
        for pid, (process, identity, vm, role) in owners:
            try:
                part = proc_sample(pid)
                if part['start_ticks'] != identity:
                    raise ValueError('PID reused')
                part.update(vm=vm, role=role)
                row['processes'].append(part)
            except (OSError, ValueError, IndexError) as error:
                row['errors'].append({'vm': vm, 'role': role, 'unavailable': type(error).__name__,
                                      'exited': process.poll() is not None})
        row['fleet_pss_bytes'] = (sum(p['pss_bytes'] for p in row['processes'])
                                  if not row['errors'] else None)
        self.rows.append(row)
        return row

    def loop(self):
        try:
            with open(self.output, 'x') as stream:
                while not self.stopped.is_set():
                    stream.write(json.dumps(self.sample()) + '\n')
                    stream.flush()
                    if self.stopped.wait(self.interval):
                        stream.write(json.dumps(self.sample()) + '\n')
                        stream.flush()
                        break
        except Exception as error:
            self.failure = type(error).__name__ + ': ' + str(error)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *args):
        self.stopped.set()
        self.thread.join()


class Budget:
    def __init__(self, count):
        self.remaining = count
        self.lock = threading.Lock()

    def admit(self):
        with self.lock:
            if not self.remaining:
                return False
            self.remaining -= 1
            return True


class GuestVM:
    """Own precisely two child processes, their console, share and disk overlay."""
    def __init__(self, config, directory, sampler, ram_mib, overlay=None):
        self.config, self.directory, self.sampler = config, directory, sampler
        directory.mkdir(mode=0o700)
        self.share = directory / 'share'
        self.share.mkdir(mode=0o700)
        (self.share / 'results').mkdir()
        self.overlay = overlay or directory / 'disk.qcow2'
        self.ram_mib = ram_mib
        self.processes = []
        self.console = None
        self.buffer = b''
        self.serial = open(directory / 'serial.log', 'xb')
        self.identity = uuid.uuid4().hex
        self.socket_root = directory / 'sockets'
        # AF_UNIX paths have a 108 byte limit. Fail before launching anything.
        if len(os.fsencode(self.socket_root / 'console')) >= 104:
            raise ValueError('output path too long for AF_UNIX sockets')
        self.socket_root.mkdir()
        self.payload = '/mnt/agent-memory/payload'
        self.payload_mounted = False

    def launch(self, argv, role):
        log = open(self.directory / (role + '.log'), 'xb')
        try:
            process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
        finally:
            log.close()
        self.processes.append(process)
        self.sampler.register(process, self.identity, role)
        return process

    def wait_socket(self, path, process, timeout=15):
        deadline = time.monotonic() + timeout
        while not path.exists():
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError('backend failed to create socket')
            time.sleep(.05)

    def start(self):
        c = self.config
        if not self.overlay.exists():
            subprocess.run([c['qemu_img'], 'create', '-q', '-f', 'qcow2', '-F', 'raw',
                            '-b', c['disk'], str(self.overlay)], check=True)
        # Binaries and public dependencies are immutable payload. Workspaces and
        # results are private per VM; symlinks outside the share are not used.
        payload = self.share / 'payload'
        (payload / 'bin').mkdir(parents=True)
        for key in ('opencode', 'node', 'helper'):
            os.link(c[key], payload / 'bin' / key)
        shutil.copytree(c.get('workload_dir', Path(__file__).parent / 'workloads'), payload / 'workloads')
        shutil.copytree(c['sdk'], payload / 'sdk', copy_function=os.link)
        # A bind mount enforces read-only at the host VFS boundary, including
        # guest root writes. Keep the results directory outside this mount.
        subprocess.run(['mount', '--bind', str(payload), str(payload)], check=True)
        self.payload_mounted = True
        subprocess.run(['mount', '-o', 'remount,bind,ro', str(payload)], check=True)
        fs_socket = self.socket_root / 'fs'
        backend = self.launch([c['virtiofsd'], '--socket-path', str(fs_socket),
                               '--shared-dir', str(self.share), '--sandbox', 'none'], 'virtiofsd')
        self.wait_socket(fs_socket, backend)
        serial_socket = self.socket_root / 'console'
        argv = [c['qemu'], '-display', 'none', '-monitor', 'none', '-serial', 'none',
                '-kernel', c['kernel'], '-append',
                'console=/dev/hvc0 rw init=/bin/busybox init AUTO_TEST=none',
                '-machine', 'q35,accel=kvm,hpet=off', '-cpu', 'host,apic,x2apic,+fpu,check,+vmx',
                '-smp', str(c.get('vcpus', 2)), '-m', str(self.ram_mib),
                '-rtc', 'clock=host,base=localtime',
                '-object', f'memory-backend-memfd,id=mem,size={self.ram_mib}M,share=on',
                '-numa', 'node,memdev=mem',
                '-chardev', f'socket,id=console,path={serial_socket},server=on,wait=on',
                '-device', 'virtio-serial', '-device', 'virtconsole,chardev=console',
                '-drive', f'id=disk,file={self.overlay},if=none,format=qcow2',
                '-device', 'virtio-blk-pci,drive=disk', '-device', 'virtio-rng-pci',
                '-netdev', 'user,id=net', '-device', 'virtio-net-pci,vectors=5,netdev=net',
                '-chardev', f'socket,id=fs,path={fs_socket}',
                '-device', 'vhost-user-fs-pci,chardev=fs,tag=hostshare,queue-size=512']
        write_json(self.directory / 'vm.json', {'identity': self.identity, 'ram_mib': self.ram_mib,
                   'vcpus': c.get('vcpus', 2), 'argv': argv})
        qemu = self.launch(argv, 'qemu')
        self.wait_socket(serial_socket, qemu)
        self.console = socket.socket(socket.AF_UNIX)
        self.console.connect(str(serial_socket))
        self.console.settimeout(1)
        self.expect(b'Please press Enter to activate this console.', 180)
        self.console.sendall(b'\n')
        self.expect(b'# ', 30)
        self.command('true', 30)
        self.command('mkdir -p /mnt/agent-memory /root/agent-memory && '
                     'mount -t virtiofs hostshare /mnt/agent-memory && '
                     "echo '+memory +cpu' > /sys/fs/cgroup/cgroup.subtree_control", 60)
        return self

    def receive(self, timeout):
        if CANCELLED.is_set():
            raise InterruptedError('run cancelled')
        if self.sampler.failure:
            raise RuntimeError('host sampling failed: ' + self.sampler.failure)
        if self.overlay.stat().st_blocks * 512 > self.config.get('overlay_budget_mib', 256) * 1024**2:
            raise RuntimeError('private overlay exceeded disk budget')
        self.console.settimeout(max(.01, min(timeout, 1)))
        try:
            block = self.console.recv(65536)
        except socket.timeout:
            return
        if not block:
            raise RuntimeError('console disconnected')
        if b'Kernel Panic Occurred' in block:
            self.serial.write(block)
            self.serial.flush()
            # Capture the bounded trailing backtrace before destroying this VM.
            # The panic marker often precedes the call trace in a later packet.
            until, captured = time.monotonic() + 2, 0
            while time.monotonic() < until and captured < 1024 * 1024:
                self.console.settimeout(.2)
                try:
                    tail = self.console.recv(65536)
                except (socket.timeout, ConnectionError):
                    break
                if not tail:
                    break
                self.serial.write(tail)
                captured += len(tail)
            self.serial.flush()
            raise RuntimeError('guest kernel panic; private serial evidence retained')
        self.serial.write(block)
        self.serial.flush()
        self.buffer = (self.buffer + block).replace(b'\r', b'')
        if len(self.buffer) > 4 * 1024 * 1024:
            raise RuntimeError('console output exceeded bounded buffer')

    def expect(self, needle, timeout):
        deadline = time.monotonic() + timeout
        while needle not in self.buffer:
            if time.monotonic() >= deadline:
                raise TimeoutError('guest console timeout')
            self.receive(deadline - time.monotonic())
        end = self.buffer.index(needle) + len(needle)
        result, self.buffer = self.buffer[:end], self.buffer[end:]
        return result

    def command(self, command, timeout=300, allow_failure=False):
        token = ('AM_' + uuid.uuid4().hex).encode()
        wire = (command + "; printf '\\n" + token.decode() + ":%s\\n' \"$?\"\n").encode()
        for start in range(0, len(wire), 8):
            self.console.sendall(wire[start:start + 8])
            time.sleep(.025)
        pattern = re.compile(rb'(?:^|\n)' + token + rb':([0-9]+)\n')
        deadline = time.monotonic() + timeout
        while True:
            match = pattern.search(self.buffer)
            if match:
                self.buffer = self.buffer[match.end():]
                code = int(match.group(1))
                if code and not allow_failure:
                    raise RuntimeError('guest command failed, exit=' + str(code))
                return code
            if time.monotonic() >= deadline:
                raise TimeoutError('guest command timeout')
            self.receive(deadline - time.monotonic())

    def prepare(self, name, mode='coding'):
        workspace = '/root/agent-memory/' + name
        output = '/mnt/agent-memory/results/' + name
        args = ['/bin/bash', self.payload + '/workloads/opencode.sh', workspace,
                self.payload, output, 'prepare-cold' if mode == 'cold' else 'prepare']
        self.command(shlex.join(args), 180)

    def trial_command(self, name, budget, mode='coding', limit=None, workspace=None,
                      diagnostic=False, prepared=False):
        if not re.fullmatch(r'[a-zA-Z0-9_-]+', name):
            raise ValueError('invalid trial name')
        context = dict(name=name, vm=self.identity, ram_mib=self.ram_mib, mode=mode,
                       limit_bytes=limit, admitted=False)
        if shutil.disk_usage(self.directory).free < 256 * 1024**2:
            return None, dict(context, status='disk_budget_exhausted')
        workspace = workspace or '/root/agent-memory/' + name
        output = '/mnt/agent-memory/results/' + name
        if mode != 'resume' and not prepared:
            self.prepare(name, mode)
        if not budget.admit():
            return None, dict(context, status='budget_exhausted')
        context.update(admitted=True, host_start_ns=time.monotonic_ns())
        args = [self.payload + '/bin/helper', '--output', output, '--deadline-seconds',
                str(self.config.get('task_timeout_seconds', 240))]
        if limit is not None:
            args += ['--limit-bytes', str(limit)]
        if diagnostic:
            args.append('--diagnostic')
        args += ['--', '/bin/bash', self.payload + '/workloads/opencode.sh',
                 workspace, self.payload, output, mode]
        env = 'AGENT_ENV_FILE=' + shlex.quote(self.config.get('environment_file', '/opt/env.sh'))
        env += ' AGENT_MODEL=' + shlex.quote(self.config.get('model', 'deepseek/deepseek-flash'))
        return env + ' ' + shlex.join(args), context

    def collect(self, result, helper_exit):
        root = self.share / 'results' / result['name']
        helper = json.loads((root / 'result.json').read_text())
        workload = json.loads((root / 'workload.json').read_text()) if (root / 'workload.json').exists() else None
        if not isinstance(helper, dict) or helper.get('schema') != 1:
            raise ValueError('unsupported helper result schema')
        if workload is not None and (not isinstance(workload, dict) or workload.get('schema') != 1):
            raise ValueError('unsupported workload result schema')
        result.update(helper=helper, helper_exit=helper_exit, workload=workload)
        for key in ('task_elapsed_ns', 'memory_peak_bytes'):
            value = helper.get(key)
            if value is not None and (type(value) is not int or value < 0):
                raise ValueError('invalid helper measurement: ' + key)
        result['status'] = ('passed' if helper_exit == 0 and helper['status'] == 'ok' and
                            helper['descendants_empty'] is True and helper['cleanup_required'] is False and
                            helper['sample_errors'] == 0 and helper['diagnostic_errors'] == 0 and
                            helper.get('task_elapsed_ns') is not None and
                            helper.get('memory_peak_bytes') is not None and
                            workload and workload.get('success') is True else 'failed')
        samples = [json.loads(line) for line in (root / 'metrics.jsonl').read_text().splitlines()]
        if not samples or not {'baseline', 'final', 'settled'} <= {sample.get('phase') for sample in samples}:
            raise ValueError('missing mandatory sampling phases')
        times = [sample['monotonic_ns'] for sample in samples]
        if any(type(value) is not int or value < 0 for value in times) or times != sorted(times):
            raise ValueError('invalid guest sample time axis')
        result['sampled_memory_stat_peaks'] = {key: max((sample['memory_stat'][key]
            for sample in samples if sample['memory_stat'].get(key) is not None), default=None)
            for key in ('anon', 'file', 'shmem', 'kernel_stack', 'pagetables', 'vmalloc', 'kernel')}
        used = [(sample['guest_meminfo']['MemTotal'] - sample['guest_meminfo']['MemFree']) * 1024
                for sample in samples if sample['guest_meminfo'].get('MemTotal') is not None and
                sample['guest_meminfo'].get('MemFree') is not None]
        result['sampled_guest_used_peak_bytes'] = max(used, default=None)
        result['memory_peak_bytes'] = helper['memory_peak_bytes']
        result['elapsed_ns'] = helper['task_elapsed_ns']
        result['quarantine_vm'] = not helper['descendants_empty'] or helper['status'] == 'cleanup_failed'
        return result

    def trial(self, name, budget, **kwargs):
        result = dict(name=name, admitted=False)
        try:
            command, result = self.trial_command(name, budget, **kwargs)
            if command is not None:
                code = self.command(command, self.config.get('task_timeout_seconds', 240) + 90,
                                    allow_failure=True)
                self.collect(result, code)
        except (OSError, ValueError, KeyError, TypeError, IndexError, AttributeError, RuntimeError, TimeoutError) as error:
            result.update(status='infrastructure_failed', error=type(error).__name__ + ': ' + str(error))
        result['host_end_ns'] = time.monotonic_ns()
        write_json(self.directory / (name + '.json'), result)
        return result

    def pair(self, budget, limits):
        # Two real Agents in one guest, with independently owned writable state
        # and leaves. Only the victim receives the low quota.
        commands, rows = [], []
        for name, limit in zip(('victim', 'neighbor'), limits):
            command, row = self.trial_command(name, budget, limit=limit, prepared=True)
            rows.append(row)
            if command is not None:
                exit_path = '/mnt/agent-memory/results/' + name + '.exit'
                commands.append('(' + command + "; printf '%s' \"$?\" > " + shlex.quote(exit_path) + ') &')
        transport_error = None
        if commands:
            try:
                self.command(' '.join(commands) + ' wait', self.config.get('task_timeout_seconds', 240) + 90)
            except (OSError, RuntimeError, TimeoutError) as error:
                transport_error = type(error).__name__ + ': ' + str(error)
        for row in rows:
            if row['admitted']:
                try:
                    if transport_error:
                        raise RuntimeError(transport_error)
                    code = int((self.share / 'results' / (row['name'] + '.exit')).read_text())
                    self.collect(row, code)
                except (OSError, ValueError, KeyError, TypeError, IndexError, AttributeError, RuntimeError) as error:
                    row.update(status='infrastructure_failed', quarantine_vm=True,
                               error=type(error).__name__ + ': ' + str(error))
        victim, neighbor = rows
        if isinstance(victim.get('helper'), dict) and isinstance(neighbor.get('helper'), dict):
            v, n = victim['helper'], neighbor['helper']
            # Guest CLOCK_MONOTONIC is shared by both leaves, unlike clocks in
            # different VMs. Require genuine execution overlap and an OOM event.
            ends = [v.get('leader_exit_ns'), n.get('leader_exit_ns')]
            starts = [v.get('workload_start_ns'), n.get('workload_start_ns')]
            overlap = (min(ends) - max(starts)) if all(type(x) is int and x >= 0 for x in ends + starts) else None
            victim['overlap_ns'] = neighbor['overlap_ns'] = overlap
            final = v.get('final')
            baseline = v.get('baseline')
            events = final.get('memory_events', {}) if isinstance(final, dict) else {}
            initial_events = baseline.get('memory_events', {}) if isinstance(baseline, dict) else {}
            if not isinstance(events, dict) or not isinstance(initial_events, dict):
                events, initial_events = {}, {}
            if (limits[0] is not None and victim['status'] == 'failed' and
                    not victim.get('quarantine_vm') and victim.get('helper_exit') == 1 and
                    v['status'] == 'workload_failed' and
                    v['sample_errors'] == 0 and v['diagnostic_errors'] == 0 and
                    all(type(events.get(key)) is int and events[key] > 0 and
                        initial_events.get(key) == 0
                        for key in ('max', 'oom', 'oom_kill')) and v['descendants_empty'] and
                    not v['cleanup_required'] and
                    v.get('task_elapsed_ns') is not None and v.get('memory_peak_bytes') is not None and
                    neighbor['status'] == 'passed' and overlap and overlap > 0):
                victim['status'] = 'expected_quota_oom'
        for row in rows:
            row['host_end_ns'] = time.monotonic_ns()
            write_json(self.directory / (row['name'] + '.json'), row)
        return rows

    def shutdown(self):
        self.command('sync', 60)
        for process in self.processes:
            self.sampler.unregister(process)
        # A clean ACPI poweroff is required for the restart acceptance case.
        self.console.sendall(b'/bin/busybox poweroff -f\n')
        self.processes[-1].wait(timeout=60)

    def close(self):
        failures = []
        if self.console:
            self.console.close()
            self.console = None
        # QEMU shutdown also disconnects (and can terminate) virtiofsd. Retire
        # the whole owned set before stopping either member of the pair.
        for process in self.processes:
            self.sampler.unregister(process)
        for process in reversed(self.processes):
            try:
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
                    except ProcessLookupError:
                        process.wait(timeout=5)
            except (OSError, subprocess.SubprocessError) as error:
                failures.append(type(error).__name__)
        self.processes.clear()
        if self.payload_mounted:
            try:
                subprocess.run(['umount', str(self.share / 'payload')], check=True,
                               timeout=10)
                self.payload_mounted = False
            except (OSError, subprocess.SubprocessError) as error:
                failures.append(type(error).__name__)
        self.serial.close()
        if failures:
            raise RuntimeError('VM cleanup failed: ' + ','.join(failures))


def summarize(results, rows):
    groups = {}
    for row in results:
        key = str(row.get("case", "unknown")) + "/" + str(row.get("mode", "unknown"))
        group = groups.setdefault(key, {'attempted': 0, 'passed': 0, 'latencies_ns': [], 'peaks_bytes': []})
        group['attempted'] += int(row.get('admitted', False))
        if row['status'] == 'passed' and row.get('elapsed_ns') is not None:
            group['passed'] += 1
            group['latencies_ns'].append(row['elapsed_ns'])
            group['peaks_bytes'].append(row['memory_peak_bytes'])
    for group in groups.values():
        latencies = group.pop('latencies_ns')
        group.update(p95_success_ns=percentile(latencies, .95), p99_success_ns=percentile(latencies, .99),
                     p99_descriptive_only=len(latencies) < 100)
    host_by_case = {}
    vm_cases = {r['vm']: str(r['case']) for r in results if 'vm' in r and 'case' in r}
    process_windows = {}
    for sample in rows:
        case_pss = {}
        for process in sample['processes']:
            case = vm_cases.get(process['vm'])
            if case is None:
                continue
            case_pss[case] = case_pss.get(case, 0) + process['pss_bytes']
            identity = (process['pid'], process['start_ticks'])
            window = process_windows.setdefault(identity, {'case': case, 'role': process['role'],
                    'first_ticks': process['cpu_ticks'], 'first_ns': sample['monotonic_ns']})
            window.update(last_ticks=process['cpu_ticks'], last_ns=sample['monotonic_ns'])
        if not sample['errors']:
            for case, pss in case_pss.items():
                metrics = host_by_case.setdefault(case, {'fleet_pss_peak_bytes': 0, 'observed_cpu_seconds': 0})
                metrics['fleet_pss_peak_bytes'] = max(metrics['fleet_pss_peak_bytes'], pss)
    ticks_per_second = os.sysconf('SC_CLK_TCK')
    for window in process_windows.values():
        case = window['case']
        if case in host_by_case:
            host_by_case[case]['observed_cpu_seconds'] += (window['last_ticks'] - window['first_ticks']) / ticks_per_second
    peaks = [r['fleet_pss_bytes'] for r in rows if r['fleet_pss_bytes'] is not None]
    return {'schema': 1, 'groups': groups, 'host_by_case': host_by_case,
            'host_cpu_scope': 'between first and last periodic samples per process identity; startup/retirement tails censored', 'fleet_pss_peak_bytes': max(peaks, default=None),
            'host_unavailable_samples': sum(bool(r['errors']) for r in rows),
            'statuses': {status: sum(r['status'] == status for r in results)
                         for status in sorted({r['status'] for r in results})},
            'final_managed_process_count': len(rows[-1]['processes']) if rows else None,
            'production_slo_claimed': False}


def validate_config(config):
    def integer(value, low, high, name):
        if type(value) is not int or not low <= value <= high:
            raise ValueError('invalid ' + name)
    integer(config.get('max_attempts', 100), 1, 100, 'max_attempts')
    integer(config.get('task_timeout_seconds', 240), 1, 1800, 'task_timeout_seconds')
    for flag in ('keep_disks',):
        if flag in config and type(config[flag]) is not bool:
            raise ValueError('invalid boolean ' + flag)
    integer(config.get('vcpus', 2), 1, 64, 'vcpus')
    integer(config.get('disk_reserve_mib', 1024), 512, 1048576, 'disk_reserve_mib')
    integer(config.get('overlay_budget_mib', 256), 128, 8192, 'overlay_budget_mib')
    if not isinstance(config.get('cases'), list) or not config['cases']:
        raise ValueError('cases must be a nonempty list')
    maximum_ram = 0
    for case in config['cases']:
        for flag in ('restart_resume', 'reuse_session', 'diagnostic'):
            if flag in case and type(case[flag]) is not bool:
                raise ValueError('invalid boolean ' + flag)
        integer(case.get('vms'), 1, 16, 'vms')
        integer(case.get('ram_mib'), 128, 8192, 'ram_mib')
        integer(case.get('duration_seconds', 0), 0, 86400, 'duration_seconds')
        integer(case.get('period_seconds', 50), 1, 86400, 'period_seconds')
        if case.get('duration_seconds', 0) and case.get('period_seconds', 50) < 50:
            raise ValueError('steady period must be at least 50 seconds')
        if case.get('mode', 'coding') not in ('coding', 'cold'):
            raise ValueError('initial mode must be coding or cold')
        if 'agent_limits_bytes' in case:
            if not isinstance(case['agent_limits_bytes'], list) or len(case['agent_limits_bytes']) != 2:
                raise ValueError('pair requires exactly two explicit limits (null means unlimited)')
            for limit in case['agent_limits_bytes']:
                if limit is not None:
                    integer(limit, 0, 2**63-1, 'agent limit')
            if case.get('duration_seconds') or case.get('restart_resume'):
                raise ValueError('pair is a separate single-run profile')
        if 'limit_bytes' in case:
            integer(case['limit_bytes'], 0, 2**63-1, 'limit_bytes')
        maximum_ram = max(maximum_ram, case['ram_mib'] * case['vms'])
    meminfo = dict((line.split(':')[0], int(line.split()[1]))
                   for line in Path('/proc/meminfo').read_text().splitlines())
    # Reserve both fixed host headroom and per-VM QEMU/backend overhead. This
    # prevents swapping/host OOM from being misreported as guest density evidence.
    need_kib = maximum_ram * 1024 + max(c['vms'] for c in config['cases']) * 128 * 1024 + 1024**2
    if meminfo['MemAvailable'] < need_kib:
        raise ValueError('insufficient available host memory for configured fleet')


def run(config, output):
    os.umask(0o077)
    validate_config(config)
    output = output.resolve()
    output.mkdir(mode=0o700)
    disk_need = (config.get('disk_reserve_mib', 1024) + max(c['vms'] for c in config['cases']) *
                 config.get('overlay_budget_mib', 256)) * 1024**2
    if shutil.disk_usage(output).free < disk_need:
        raise ValueError('insufficient free disk for configured reserve')
    required = ('kernel', 'disk', 'opencode', 'node', 'helper', 'sdk', 'qemu', 'qemu_img', 'virtiofsd')
    for key in required:
        config[key] = str(Path(config[key]).resolve(strict=True))
    budget = Budget(config.get('max_attempts', 100))
    frozen_workloads = output / 'workloads-source'
    shutil.copytree(Path(__file__).parent / 'workloads', frozen_workloads)
    config['workload_dir'] = str(frozen_workloads)
    hashes = {key: digest(config[key]) for key in ('kernel', 'disk', 'opencode', 'node', 'helper')}
    sdk_hash = tree_digest(config['sdk'])
    write_json(output / 'manifest.json', {'schema': 1, 'config': config,
               'artifacts_sha256': hashes,
               'sdk_tree_sha256': sdk_hash,
               'runner_sha256': digest(__file__),
               'qemu_version': subprocess.check_output([config['qemu'], '--version'], text=True).splitlines()[0],
               'virtiofsd_sha256': digest(config['virtiofsd']),
               'workload_sha256': {p.name: digest(p) for p in frozen_workloads.iterdir() if p.is_file()},
               'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
               'host_kernel': os.uname().release, 'host_machine': os.uname().machine,
               'host_logical_cpus': os.cpu_count(),
               'host_cache_policy': 'artifact hashes pre-read; inherited page cache retained; no drop_caches',
               'disk_budget_policy': 'per-overlay physical-block polling abort threshold, not a filesystem hard quota',
               'percentile': 'nearest rank, successful attempts only; failures reported separately',
               'latency_scope': 'guest execution gate to naturally empty leaf, includes checker; excludes boot, preparation and settle'})
    results = []
    results_lock = threading.Lock()
    def record(row, index, number, case):
        row.update(case=index, worker=number, fleet_vms=case['vms'], ram_mib=case['ram_mib'])
        with results_lock:
            results.append(row)
            write_json(output / 'attempts.json', results)
    with HostSampler(output / 'host.jsonl') as sampler:
        for index, case in enumerate(config['cases']):
            ready = threading.Barrier(case['vms'], timeout=240)
            def worker(number):
                vm = None
                try:
                    if shutil.disk_usage(output).free < (case['vms'] * config.get('overlay_budget_mib', 256) + 256) * 1024**2:
                        raise ValueError('insufficient disk reserve for this fleet')
                    vm = GuestVM(config, output / f'c{index}v{number}', sampler, case['ram_mib'])
                    vm.start()
                    if 'agent_limits_bytes' in case:
                        vm.prepare('victim')
                        vm.prepare('neighbor')
                    else:
                        vm.prepare('t0', case.get('mode', 'coding'))
                    ready.wait()
                    if 'agent_limits_bytes' in case:
                        for paired in vm.pair(budget, case['agent_limits_bytes']):
                            record(paired, index, number, case)
                        return
                    start = time.monotonic()
                    duration = case.get('duration_seconds', 0)
                    period = case.get('period_seconds', 50)
                    attempt = 0
                    while True:
                        arrival = time.monotonic()
                        task_mode = 'resume' if attempt and case.get('reuse_session') else case.get('mode', 'coding')
                        row = vm.trial(f't{attempt}', budget, mode=task_mode,
                                       workspace='/root/agent-memory/t0' if case.get('reuse_session') else None,
                                       limit=case.get('limit_bytes'), diagnostic=case.get('diagnostic', False),
                                       prepared=attempt == 0)
                        record(row, index, number, case)
                        attempt += 1
                        if row['status'] in ('infrastructure_failed', 'budget_exhausted', 'disk_budget_exhausted') or row.get('quarantine_vm'):
                            break
                        if not duration:
                            break
                        # Paced, finite offered load: slow work skips arrivals,
                        # never queues a catch-up burst. Do not idle-pad a budget failure.
                        next_arrival = arrival + period
                        completed = time.monotonic()
                        if completed > next_arrival:
                            row['missed_arrivals'] = math.floor((completed - next_arrival) / period) + 1
                            next_arrival += row['missed_arrivals'] * period
                        if next_arrival >= start + duration:
                            CANCELLED.wait(max(0, start + duration - time.monotonic()))
                            break
                        CANCELLED.wait(max(0, next_arrival - time.monotonic()))
                        if CANCELLED.is_set() or time.monotonic() >= start + duration:
                            break
                    if case.get('restart_resume') and row['status'] == 'passed':
                        vm.shutdown()
                        overlay = vm.overlay
                        vm.close()
                        vm = GuestVM(config, output / f'c{index}v{number}r', sampler, case['ram_mib'], overlay)
                        vm.start()
                        record(vm.trial('resume', budget, mode='resume', workspace='/root/agent-memory/t0'), index, number, case)
                    write_json(output / f'c{index}v{number}-window.json',
                               {'elapsed_seconds': time.monotonic() - start,
                                'requested_seconds': duration, 'last_status': row['status']})
                except Exception as error:
                    ready.abort()
                    record({'status': 'infrastructure_failed', 'admitted': False,
                            'vm': vm.identity if vm else None, 'error': type(error).__name__ + ': ' + str(error)}, index, number, case)
                finally:
                    if vm:
                        try:
                            vm.close()
                            if not config.get('keep_disks', False) and vm.overlay.exists():
                                vm.overlay.unlink()
                        except Exception as error:
                            record({'status': 'cleanup_failed', 'admitted': False,
                                    'vm': vm.identity, 'error': type(error).__name__ + ': ' + str(error)}, index, number, case)
            with concurrent.futures.ThreadPoolExecutor(max_workers=case['vms']) as pool:
                futures = [pool.submit(worker, n) for n in range(case['vms'])]
                for future in futures:
                    future.result()
    write_json(output / 'attempts.json', results)
    report = summarize(results, sampler.rows)
    windows = [json.loads(p.read_text()) for p in output.glob('c*-window.json')]
    report['steady_windows'] = windows
    report['steady_workers_expected'] = sum(c['vms'] for c in config['cases'] if c.get('duration_seconds'))
    report['steady_complete'] = (sum(bool(w['requested_seconds']) for w in windows) == report['steady_workers_expected'] and all(w['elapsed_seconds'] >= w['requested_seconds'] and
        w['last_status'] == 'passed' for w in windows if w['requested_seconds']))
    report['host_sampler_failure'] = sampler.failure
    report['artifacts_unchanged'] = (all(digest(config[key]) == value for key, value in hashes.items()) and
                                     tree_digest(config['sdk']) == sdk_hash)
    write_json(output / 'summary.json', report)
    print(json.dumps(report))
    return 0 if (results and all(r['status'] in ('passed', 'expected_quota_oom') for r in results) and
                 sampler.rows and not sampler.failure and report['artifacts_unchanged'] and report['steady_complete'] and
                 report['host_unavailable_samples'] == 0) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: CANCELLED.set())
    return run(json.loads(args.config.read_text()), args.output)


if __name__ == '__main__':
    raise SystemExit(main())
