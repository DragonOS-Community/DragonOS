#!/usr/bin/env python3
"""MM-266: same-guest OpenCode followed by bounded allocation/compaction regression.

Requires an x86_64 mm_selftest kernel and built dunitest binaries. No model
provider configuration is consumed. A cold baseline cannot substitute for the
post-Agent run; insufficient DMA32 fragmentation is explicitly not a pass.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import stat
import time
import xml.etree.ElementTree as ET

import run as runner

MIB = 1024 * 1024
RESERVE = 512 * MIB
OUTPUT_LIMIT = 128 * MIB
FILE_LIMIT = 16 * MIB
FIXTURE_LIMIT = 64 * MIB
GUEST_TEST_ROOT = '/opt/mm266-tests'
GUEST_TESTS = GUEST_TEST_ROOT + '/dunitest/bin/normal'
TESTS = (
    'kernel_buffer_selftest', 'mobility_placement', 'page_compaction',
    'dma_allocator_selftest', 'page_migration', 'shared_page_migration',
    'page_migration_prerequisites', 'page_retirement', 'page_cache_accounting',
    'slab_memory_accounting', 'allocation_diagnostics', 'exec_string_layout',
    'exec_abi', 'exec_write_access', 'udp_ipv6_send_semantics',
    'ipv6_routed_output', 'tcp_namespace_progress', 'rtnetlink_bridge_veth_semantics',
    'forward_mtu', 'bridge_forwarding', 'raw_icmp_filter_semantics',
)
BASELINE = TESTS[:4]
REPORTS = ('kernel_buffer_selftest', 'placement_selftest', 'page_compaction_selftest',
           'dma_allocator_selftest', 'page_migration_selftest', 'page_access_selftest',
           'page_retirement_selftest')
PATH_KEYS = ('kernel', 'disk', 'helper', 'opencode', 'node', 'sdk', 'qemu',
             'qemu_img', 'virtiofsd')


def read_small(path):
    with path.open('rb') as stream:
        data = stream.read(FILE_LIMIT + 1)
    if len(data) > FILE_LIMIT:
        raise ValueError('result file exceeds budget')
    return data


def check_xml(data):
    """An exit status of zero can hide GTEST_SKIP or zero selected tests."""
    root = ET.fromstring(data)
    for suite in root.iter():
        if suite.tag in ('testsuite', 'testsuites'):
            if any(int(suite.get(key, '0')) != 0 for key in ('failures', 'errors', 'skipped', 'disabled')):
                raise ValueError('dunitest suite did not complete all cases')
    cases = root.findall('.//testcase')
    if not cases:
        raise ValueError('dunitest executed no cases')
    for case in cases:
        if (case.find('skipped') is not None or case.find('failure') is not None
                or case.find('error') is not None or case.get('status') != 'run'
                or case.get('result', 'completed') != 'completed'):
            raise ValueError('dunitest failed or skipped a case')
    return len(cases)


def allocation_snapshot(data):
    text = data.decode('utf-8')
    if not text.startswith('version=1\n'):
        raise ValueError('unsupported allocation statistics')
    blocks, free_pages = {}, {}
    for line in text.splitlines():
        free = re.fullmatch(r'free_pages (DMA32|Normal) (\d+)', line)
        if free:
            zone, count = free.groups()
            if zone in free_pages:
                raise ValueError('duplicate zone statistics')
            free_pages[zone] = int(count)
        match = re.fullmatch(r'zone (DMA32|Normal) order (\d+) free_blocks (\d+) attempts (\d+) misses (\d+)', line)
        if match:
            zone, order, count, _, _ = match.groups()
            if int(order) > 63:
                raise ValueError('invalid allocation order')
            key = (zone, int(order))
            if key in blocks:
                raise ValueError('duplicate allocation order')
            blocks[key] = int(count)
    if not blocks or not any(zone == 'DMA32' for zone, _ in blocks):
        raise ValueError('missing DMA32 statistics')
    orders = {order for zone, order in blocks if zone == 'DMA32'}
    if orders != set(range(max(orders) + 1)):
        raise ValueError('missing DMA32 order')
    for zone in ('DMA32', 'Normal'):
        if ({order for z, order in blocks if z == zone} != orders
                or zone not in free_pages
                or sum(count << order for (z, order), count in blocks.items() if z == zone) != free_pages[zone]):
            raise ValueError('incomplete or inconsistent zone statistics')
    return {'target_zone': 'DMA32', 'free_pages': free_pages,
            'target_free_blocks': sum(value for (zone, _), value in blocks.items() if zone == 'DMA32'),
            'orders': {zone: {str(order): value for (z, order), value in blocks.items() if z == zone}
                       for zone in ('DMA32', 'Normal')}}


def bounded_output(vm):
    total = 0
    files = list(vm.directory.glob('*.log')) + [vm.directory.parent / 'host.jsonl']
    files.extend((vm.share / 'results').rglob('*'))
    for path in files:
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            raise ValueError('symlink in private result output')
        if stat.S_ISREG(info.st_mode):
            total += max(info.st_size, info.st_blocks * 512)
            if total > OUTPUT_LIMIT:
                raise ValueError('private output exceeded disk budget')
    if shutil.disk_usage(vm.directory).free < RESERVE:
        raise ValueError('host disk reserve exhausted')


class CompactionVM(runner.GuestVM):
    def launch(self, argv, role):
        if role == 'qemu':
            argv = list(argv)
            index = argv.index('-append') + 1
            argv[index] += ' dragonos.net_test_fixtures'
        return super().launch(argv, role)

    def _check_health(self):
        super()._check_health()
        now = time.monotonic()
        if now >= getattr(self, '_next_disk_check', 0):
            bounded_output(self)
            self._next_disk_check = now + 1


def load_config(path, kernel):
    source = json.loads(path.read_text())
    config = {key: str(Path(kernel if key == 'kernel' else source[key]).resolve(strict=True))
              for key in PATH_KEYS}
    runner.tree_digest(config['sdk'])  # Reject symlinks/nonregular SDK payloads before copying.
    config['vcpus'] = source.get('vcpus', 2)
    if type(config['vcpus']) is not int or not 1 <= config['vcpus'] <= 64:
        raise ValueError('invalid vCPU count')
    config['overlay_budget_mib'] = 512
    return config


def command(vm, rows, name, wire, timeout):
    start = time.monotonic()
    row = {'stage': name, 'status': 'failed'}
    rows.append(row)
    try:
        code = vm.command(wire, timeout, allow_failure=True)
        row.update(exit_code=code, status='passed' if code == 0 else 'failed')
    except (Exception, KeyboardInterrupt) as error:
        row['error'] = type(error).__name__ + ': ' + str(error)
        raise
    finally:
        row['wall_seconds'] = time.monotonic() - start
    return row


def run_suite(vm, rows, phase, name, timeout):
    stem = phase + '-' + name
    guest = '/mnt/agent-memory/results/' + stem
    row = command(vm, rows, stem,
                  'DUNITEST_MEMORY_FILE_DIR=/root ' + GUEST_TESTS + '/' + name
                  + '_test --gtest_output=xml:' + guest + '.xml > ' + guest + '.log 2>&1', timeout)
    try:
        row['cases'] = check_xml(read_small(vm.share / 'results' / (stem + '.xml')))
    except (OSError, ValueError, ET.ParseError) as error:
        row.update(status='failed', error=type(error).__name__ + ': ' + str(error))
    return row['status'] == 'passed'



def snapshot_wire(report, destination):
    # Virtual snapshot metadata is a hint, not its stream length. Explicit read
    # until EOF avoids file-copy optimizations which use the stat size as EOF.
    # No count: a short read is not EOF and reports may grow beyond one page.
    return shlex.join(['dd', 'if=/sys/kernel/debug/mm/' + report,
                       'of=/mnt/agent-memory/results/' + destination, 'bs=256'])


def check_report(data):
    lines = data.decode('utf-8').splitlines()
    if 'status=ok' not in lines or 'summary_fail=0' not in lines:
        raise ValueError('selftest report failed or incomplete')


def snapshot(vm, rows, name, timeout):
    row = command(vm, rows, name,
                  snapshot_wire('allocation_stats', name + '.txt'), timeout)
    if row['exit_code'] != 0:
        raise ValueError('allocation statistics command failed')
    value = allocation_snapshot(read_small(vm.share / 'results' / (name + '.txt')))
    row.update(value)
    return value


def agent_round(vm, rows, index, timeout):
    name = 'opencode-' + str(index)
    guest = '/mnt/agent-memory/results/' + name
    argv = [vm.payload + '/bin/helper', '--output', guest + '-helper',
            '--deadline-seconds', str(timeout), '--', vm.payload + '/bin/node',
            vm.payload + '/workloads/offline-opencode.mjs', vm.payload + '/bin/opencode',
            vm.payload + '/bin/node', '/root/agent-memory/' + name, guest, '4']
    row = command(vm, rows, name, shlex.join(argv), timeout + 60)
    row['status'] = 'failed'
    helper = json.loads(read_small(vm.share / 'results' / (name + '-helper') / 'result.json'))
    workload = json.loads(read_small(vm.share / 'results' / name / 'workload.json'))
    good = (row['exit_code'] == 0 and helper['status'] == 'ok' and helper['exit_code'] == 0
            and helper['sample_errors'] == 0 and helper['diagnostic_errors'] == 0
            and helper['cleanup_required'] is False and helper['descendants_empty'] is True
            and workload['success'] is True)
    # Full helper/workload evidence already lives in the bounded results tree.
    # Do not duplicate arbitrary JSON strings into the stage ledger.
    row.update(status='passed' if good else 'failed', results_directory=name,
               descendants_empty=helper['descendants_empty'] is True,
               workload_success=workload['success'] is True)
    return good



def fixture_input(path):
    path = path.resolve(strict=True)
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= FIXTURE_LIMIT:
        raise ValueError('requires a regular ext4 fixture of at most 64 MiB')
    return path


def install_tests(vm, rows, timeout):
    # Only public test inputs leave the private share. A new guest-only tree
    # makes setuid self-exec possible without opening host results or payload.
    root = GUEST_TEST_ROOT
    directories = [root, root + '/dunitest', root + '/dunitest/bin',
                   GUEST_TESTS, root + '/dunitest/fixtures']
    wire = ('test ! -e ' + root + ' && mkdir -p ' + GUEST_TESTS + ' '
            + root + '/dunitest/fixtures && chmod 0755 ' + ' '.join(directories)
            + ' && cp /mnt/agent-memory/tests/*_test ' + GUEST_TESTS
            + ' && chmod 0755 ' + GUEST_TESTS + '/*_test'
            + ' && cp /mnt/agent-memory/fixtures/ext4_inode_identity.img '
            + root + '/dunitest/fixtures/ext4_inode_identity.img'
            + ' && chmod 0644 ' + root + '/dunitest/fixtures/ext4_inode_identity.img')
    if command(vm, rows, 'guest-test-install', wire, timeout)['exit_code'] != 0:
        raise ValueError('guest dunitest installation failed')


def exercise(vm, rows, timeout):
    install_tests(vm, rows, timeout)
    required = ' && '.join('test -r /sys/kernel/debug/mm/' + name for name in REPORTS)
    if command(vm, rows, 'feature-availability', required, timeout)['exit_code'] != 0:
        raise ValueError('requires x86_64 mm_selftest reports')
    if command(vm, rows, 'warm-cache',
               'dd if=/dev/zero of=/root/mm266-cache bs=4096 count=8192 && sync', timeout)['exit_code'] != 0:
        raise ValueError('cache warmup failed')
    snapshot(vm, rows, 'before-stats', timeout)
    # Baseline failures are retained, but do not reorder or suppress the Agent workload.
    for name in BASELINE:
        run_suite(vm, rows, 'before', name, timeout)
    for index in range(3):
        if not agent_round(vm, rows, index, timeout):
            raise ValueError('OpenCode round failed')
    value = snapshot(vm, rows, 'after-agent-stats', timeout)
    if value['target_free_blocks'] <= 16384:
        rows.append({'stage': 'fragmentation-coverage', 'status': 'coverage_not_reached', **value})
        return
    rows.append({'stage': 'fragmentation-coverage', 'status': 'passed', **value})
    # Keep bounded raw cost/ownership reports even when gtest emits only PASS.
    for name in ('page_compaction_selftest', 'kernel_buffer_selftest', 'placement_selftest'):
        row = command(vm, rows, 'after-report-' + name,
                      snapshot_wire(name, name + '.txt'), timeout)
        try:
            check_report(read_small(vm.share / 'results' / (name + '.txt')))
        except (OSError, ValueError) as error:
            row.update(status='failed', error=type(error).__name__ + ': ' + str(error))
    for name in TESTS:
        run_suite(vm, rows, 'after', name, timeout)
    command(vm, rows, 'block-io',
            'dd if=/dev/zero of=/root/mm266-io bs=4096 count=64 && sync && '
            'test "$(wc -c < /root/mm266-io)" = 262144 && rm /root/mm266-io && sync', timeout)
    snapshot(vm, rows, 'final-stats', timeout)


def cleanup(vm):
    # close() stops only processes launched by this GuestVM and unmounts payload.
    # Never remove disks or mount contents if close failed.
    vm.close()
    if vm.payload_mounted:
        raise RuntimeError('private payload remains mounted')
    vm.overlay.unlink(missing_ok=True)
    for path in (vm.share / 'payload', vm.share / 'tests', vm.share / 'fixtures'):
        if path.exists():
            shutil.rmtree(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tests', type=Path, default=Path('user/apps/tests/dunitest/bin/normal'))
    parser.add_argument('--ext4-fixture', type=Path,
                        default=Path('user/apps/tests/dunitest/build/fixtures/ext4_inode_identity.img'))
    parser.add_argument('--ram-mib', type=int, default=2048, choices=(1024, 2048, 4096))
    parser.add_argument('--timeout-seconds', type=int, default=300)
    args = parser.parse_args()
    if not 30 <= args.timeout_seconds <= 1800:
        parser.error('timeout must be between 30 and 1800 seconds')
    os.umask(0o077)
    config = load_config(args.config, args.kernel)
    fixture = fixture_input(args.ext4_fixture)
    tests = {name: (args.tests / (name + '_test')).resolve(strict=True) for name in TESTS}
    output = args.output.resolve()
    sources = [Path(config['sdk']), Path(__file__).parent / 'workloads', args.tests.resolve()]
    if any(output.is_relative_to(path.resolve()) or path.resolve().is_relative_to(output) for path in sources):
        raise ValueError('output overlaps immutable inputs')
    payload_bytes = sum(Path(config[key]).stat().st_size for key in ('helper', 'node', 'opencode'))
    payload_bytes += sum(path.stat().st_size for path in Path(config['sdk']).rglob('*') if path.is_file())
    payload_bytes += sum(path.stat().st_size for path in tests.values()) + fixture.stat().st_size
    output.mkdir(mode=0o700)
    if shutil.disk_usage(output).free < RESERVE + OUTPUT_LIMIT + 512 * MIB + payload_bytes:
        raise ValueError('insufficient disk reserve for private payload/overlay')
    config['cases'] = [{'vms': 1, 'ram_mib': args.ram_mib}]
    runner.validate_config(config)
    config.pop('cases')
    hashes = {key: runner.digest(config[key]) for key in PATH_KEYS if key != 'sdk'}
    hashes['ext4_fixture'] = runner.digest(fixture)
    hashes['sdk'] = runner.tree_digest(config['sdk'])
    hashes.update({'test/' + name: runner.digest(path) for name, path in tests.items()})
    hashes['workloads'] = runner.tree_digest(Path(__file__).parent / 'workloads')
    hashes['runner'] = runner.digest(__file__)
    hashes['guest_runner'] = runner.digest(runner.__file__)
    runner.write_json(output / 'manifest.json', {
        'schema': 1, 'artifacts_sha256': hashes, 'ram_mib': args.ram_mib,
        'vcpus': config['vcpus'], 'agent_rounds': 3, 'warm_iterations': 4,
        'target_zone': 'DMA32', 'required_free_blocks_exclusive': 16384,
        'timeout_seconds': args.timeout_seconds, 'overlay_budget_mib': 512,
        'output_budget_mib': 128, 'disk_reserve_mib': 512,
        'model_requests': 0, 'production_slo_claimed': False,
        'scope': 'feature-kernel integration; pin/unmap/IO are layered coverage'})
    rows, vm = [], None
    try:
        with runner.HostSampler(output / 'host.jsonl') as sampler:
            try:
                vm = CompactionVM(config, output / 'vm', sampler, args.ram_mib)
                private_tests = vm.share / 'tests'
                private_tests.mkdir()
                private_fixtures = vm.share / 'fixtures'
                private_fixtures.mkdir(mode=0o700)
                shutil.copy2(fixture, private_fixtures / 'ext4_inode_identity.img')
                for name, source in tests.items():
                    shutil.copy2(source, private_tests / (name + '_test'))
                vm.start()
                exercise(vm, rows, args.timeout_seconds)
            finally:
                if vm is not None:
                    try:
                        cleanup(vm)
                        rows.append({'stage': 'cleanup', 'status': 'passed'})
                    except Exception as error:
                        rows.append({'stage': 'cleanup', 'status': 'failed',
                                     'error': type(error).__name__ + ': ' + str(error)})
    except (Exception, KeyboardInterrupt) as error:
        rows.append({'stage': 'execution', 'status': 'failed',
                     'error': type(error).__name__ + ': ' + str(error)})
    finally:
        runner.write_json(output / 'results.json', rows)
    passed = bool(rows) and all(row['status'] == 'passed' for row in rows)
    runner.write_json(output / 'summary.json', {'status': 'passed' if passed else 'failed',
                                               'stages': len(rows)})
    return 0 if passed else 1


def cancel(*_):
    raise KeyboardInterrupt


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, cancel)
    raise SystemExit(main())
