#!/usr/bin/env python3
"""Pure host tests: no QEMU, backend, model call or real process signalling."""
import concurrent.futures
import errno
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("agent_memory_run", Path(__file__).with_name("run.py"))
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


def stat_record(start=100, user=7, system=3):
    fields = ["0"] * 50
    fields[0], fields[11], fields[12], fields[19] = "S", str(user), str(system), str(start)
    return "123 (name with ) spaces)) " + " ".join(fields) + "\n"


def status_record():
    return "Name:\tfixture\nVmPTE:\t8 kB\nVmHWM:\t16 kB\n"


def write_metrics(output):
    """A valid helper stream for collector unit tests, not guest execution evidence."""
    samples = []
    for index, (phase, anon, file, free) in enumerate((
            ('baseline', 0, 1, 900), ('running', 50, 4, 600),
            ('final', 30, 8, 700), ('settled', 0, 6, 800))):
        samples.append(dict(phase=phase, monotonic_ns=index,
            memory_current_bytes=anon + file + 16, memory_peak_bytes=70,
            memory_stat=dict(anon=anon, file=file, shmem=1, kernel_stack=4,
                             pagetables=5, vmalloc=6, kernel=16),
            memory_events=dict(low=0, high=0, max=0, oom=0, oom_kill=0, oom_group_kill=0),
            cpu_stat=dict(usage_usec=index, user_usec=index, system_usec=0),
            guest_meminfo=dict(MemTotal=1024, MemFree=free)))
    (output / 'metrics.jsonl').write_text(''.join(json.dumps(s) + '\n' for s in samples))
    return samples


class PayloadTests(unittest.TestCase):
    def test_link_copy_preserves_content_on_cross_device_or_unsupported_link(self):
        with tempfile.TemporaryDirectory(prefix='am-payload-') as root:
            root = Path(root)
            source = root / 'source'
            source.write_bytes(b'fixed public payload')
            source.chmod(0o755)
            for code in (errno.EXDEV, errno.EOPNOTSUPP, errno.EPERM):
                target = root / str(code)
                with mock.patch.object(RUN.os, 'link', side_effect=OSError(code, 'fixture')):
                    RUN.link_or_copy(source, target)
                self.assertEqual(target.read_bytes(), source.read_bytes())
                self.assertEqual(target.stat().st_mode & 0o777, 0o755)
            for code in (errno.ENOSPC, errno.EIO):
                with mock.patch.object(RUN.os, 'link', side_effect=OSError(code, 'fixture')):
                    with self.assertRaises(OSError) as error:
                        RUN.link_or_copy(source, root / ('bad-' + str(code)))
                    self.assertEqual(error.exception.errno, code)


class ProcTests(unittest.TestCase):
    def test_comm_parentheses_and_cpu_unit(self):
        with mock.patch.object(Path, "read_text", side_effect=[
                stat_record(), "Rss: 12 kB\nPss: 9 kB\n", status_record(), stat_record()]):
            value = RUN.proc_sample(123)
        self.assertEqual(value["start_ticks"], 100)
        self.assertEqual(value["cpu_ticks"], 10)
        self.assertEqual(value["rss_bytes"], 12 * 1024)
        self.assertEqual(value["pss_bytes"], 9 * 1024)
        self.assertEqual(value["vmpte_bytes"], 8 * 1024)
        self.assertEqual(value["vmhwm_bytes"], 16 * 1024)

    def test_pid_reuse_rejected(self):
        with mock.patch.object(Path, "read_text", side_effect=[
                stat_record(), "Rss: 12 kB\nPss: 9 kB\n", status_record(), stat_record(start=101)]):
            with self.assertRaises(ValueError):
                RUN.proc_sample(123)

    def test_missing_or_bad_process_status_metrics_rejected(self):
        for status in ("VmPTE: 8 kB\n", "VmHWM: 16 kB\n", "", "VmPTE: 8 pages\nVmHWM: 16 kB\n"):
            with self.subTest(status=status), mock.patch.object(Path, "read_text", side_effect=[
                    stat_record(), "Rss: 12 kB\nPss: 9 kB\n", status]):
                with self.assertRaises(ValueError):
                    RUN.proc_sample(123)

    def test_missing_rollup_is_not_zero(self):
        with mock.patch.object(Path, "read_text", side_effect=[stat_record(), "Rss: 12 kB\n"]):
            with self.assertRaises(ValueError):
                RUN.proc_sample(123)
        with mock.patch.object(Path, "read_text", side_effect=[
                stat_record(), "Rss: 12 pages\nPss: 9 kB\n"]):
            with self.assertRaises(ValueError):
                RUN.proc_sample(123)

    def test_sampler_preserves_unavailable_identity(self):
        sampler = RUN.HostSampler(Path("unused"))
        process = mock.Mock(pid=123)
        process.poll.return_value = None
        with mock.patch.object(RUN, "proc_sample", return_value={"pid": 123, "start_ticks": 100}):
            sampler.register(process, "vm-a", "qemu")
        with mock.patch.object(RUN, "proc_sample", side_effect=FileNotFoundError):
            row = sampler.sample()
        self.assertIsNone(row["fleet_pss_bytes"])
        self.assertTrue(row["errors"])
        self.assertFalse(row["errors"][0]["exited"])
        sampler.unregister(process)

    def test_sampler_thread_failure_is_preserved(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "already-existing"
            output.touch()
            sampler = RUN.HostSampler(output)
            with sampler:
                sampler.thread.join(timeout=2)
            self.assertIsNotNone(sampler.failure)
            self.assertFalse(sampler.thread.is_alive())


class StatisticsTests(unittest.TestCase):
    def test_nearest_rank_and_empty(self):
        self.assertIsNone(RUN.percentile([], .99))
        self.assertEqual(RUN.percentile([4], .99), 4)
        self.assertEqual(RUN.percentile(list(range(1, 101)), .95), 95)
        self.assertEqual(RUN.percentile(list(range(1, 101)), .99), 99)

    def test_success_latency_excludes_failed_and_settle(self):
        rows = [
            dict(ram_mib=512, mode="coding", status="passed", admitted=True,
                 elapsed_ns=20, task_elapsed_ns=20, memory_peak_bytes=100),
            dict(ram_mib=512, mode="coding", status="failed", admitted=True,
                 elapsed_ns=9_000_000_000, task_elapsed_ns=None, memory_peak_bytes=None),
        ]
        report = RUN.summarize(rows, [])
        group = next(iter(report["groups"].values()))
        self.assertEqual(group["attempted"], 2)
        self.assertEqual(group["passed"], 1)
        self.assertEqual(group["p95_success_ns"], 20)
        self.assertEqual(group["p99_success_ns"], 20)
        self.assertTrue(group["p99_descriptive_only"])
        self.assertIsNone(report["fleet_pss_peak_bytes"])
        self.assertEqual(report["statuses"]["failed"], 1)

    def test_case_density_groups_are_not_merged(self):
        rows = [dict(case=case, ram_mib=512, mode="coding", status="passed",
                     admitted=True, elapsed_ns=20, memory_peak_bytes=100)
                for case in (0, 1)]
        samples = [{"fleet_pss_bytes": 123, "errors": [], "processes": [], "monotonic_ns": 1},
                   {"fleet_pss_bytes": None, "errors": [{"unavailable": "OSError"}], "processes": [], "monotonic_ns": 2},
                   {"fleet_pss_bytes": 345, "errors": [], "processes": [], "monotonic_ns": 3}]
        report = RUN.summarize(rows, samples)
        self.assertEqual(set(report["groups"]), {"0/coding", "1/coding"})
        self.assertEqual(report["fleet_pss_peak_bytes"], 345)
        self.assertEqual(report["host_unavailable_samples"], 1)

    def test_budget_concurrent_hard_limit(self):
        budget = RUN.Budget(7)
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            outcomes = list(pool.map(lambda _: budget.admit(), range(128)))
        self.assertEqual(sum(outcomes), 7)
        self.assertEqual(budget.remaining, 0)

    def test_host_costs_use_same_samples_and_observed_cpu_deltas(self):
        attempts = [dict(case=0, vm='a', mode=mode, status='passed', admitted=True,
                         elapsed_ns=20, memory_peak_bytes=100) for mode in ('coding', 'resume')]
        def process(pid, ticks, pss):
            return dict(pid=pid, start_ticks=100, cpu_ticks=ticks, pss_bytes=pss,
                        vm='a', role='qemu' if pid == 1 else 'virtiofsd')
        samples = [dict(monotonic_ns=1, processes=[process(1, 100, 90), process(2, 30, 20)],
                        errors=[], fleet_pss_bytes=110),
                   dict(monotonic_ns=1_000_000_001, processes=[process(1, 110, 80), process(2, 35, 40)],
                        errors=[], fleet_pss_bytes=120)]
        report = RUN.summarize(attempts, samples)
        self.assertEqual(set(report['groups']), {'0/coding', '0/resume'})
        self.assertEqual(report['host_by_case']['0']['fleet_pss_peak_bytes'], 120)
        self.assertAlmostEqual(report['host_by_case']['0']['observed_cpu_seconds'], 15 / RUN.os.sysconf('SC_CLK_TCK'))
        self.assertIn('censored', report['host_cpu_scope'])

    def test_invalid_configuration_rejected_before_host_launch(self):
        valid = dict(max_attempts=100, cases=[dict(vms=1, ram_mib=512)])
        for key, value in (("max_attempts", -1), ("max_attempts", 101),
                           ("vcpus", 0), ("task_timeout_seconds", 0)):
            config = {**valid, key: value}
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                RUN.validate_config(config)
        for update in (dict(vms=0), dict(ram_mib=0), dict(period_seconds=0),
                       dict(duration_seconds=-1)):
            config = {**valid, "cases": [{**valid["cases"][0], **update}]}
            with self.subTest(update=update), self.assertRaises(ValueError):
                RUN.validate_config(config)

    def test_pair_unsupported_combinations_are_rejected(self):
        pair = dict(vms=1, ram_mib=512, agent_limits_bytes=[128, None])
        for field, value in (('reuse_session', True), ('diagnostic', True),
                             ('limit_bytes', 0), ('duration_seconds', 1800),
                             ('restart_resume', True)):
            with self.subTest(field=field), self.assertRaises(ValueError):
                RUN.validate_config(dict(cases=[{**pair, field: value}]))


class VMTests(unittest.TestCase):
    def config(self):
        return dict(qemu="/qemu", qemu_img="/qemu-img", virtiofsd="/virtiofsd",
                    kernel="/kernel", disk="/base.raw", opencode="/opencode",
                    node="/node", helper="/helper", sdk="/sdk", vcpus=2)

    def test_default_quota_omitted_and_explicit_zero_preserved(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                for name, kwargs in (("ordinary", {}), ("zero", {"limit": 0})):
                    out = vm.share / "results" / name
                    out.mkdir()
                    helper = dict(schema=1, status="ok", descendants_empty=True,
                                  cleanup_required=False, sample_errors=0, diagnostic_errors=0,
                                  memory_peak_bytes=100, elapsed_ns=5_000_000_020,
                                  task_elapsed_ns=20)
                    (out / "result.json").write_text(json.dumps(helper))
                    (out / "workload.json").write_text('{"schema":1,"success":true}')
                    write_metrics(out)
                    vm.command = mock.Mock(return_value=0)
                    result = vm.trial(name, RUN.Budget(1), **kwargs)
                    self.assertEqual(result["elapsed_ns"], 20)
                    command = vm.command.call_args[0][0]
                    if kwargs:
                        self.assertIn("--limit-bytes 0", command)
                    else:
                        self.assertNotIn("--limit-bytes", command)
            finally:
                vm.close()

    def test_start_constructs_private_qemu_resources(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            vm.overlay.touch()
            launches = []
            vm.launch = lambda argv, role: launches.append((role, argv)) or mock.Mock()
            vm.wait_socket = mock.Mock()
            vm.expect = mock.Mock()
            vm.command = mock.Mock()
            try:
                with mock.patch.object(RUN.os, "link"), \
                     mock.patch.object(RUN.shutil, "copytree"), \
                     mock.patch.object(RUN.socket, "socket"), \
                     mock.patch.object(RUN.subprocess, "run") as mounts:
                    vm.start()
                    self.assertTrue(any(call.args[0][:3] == ["mount", "-o", "remount,bind,ro"]
                                        for call in mounts.call_args_list))
                argv = dict(launches)["qemu"]
                self.assertEqual(argv[argv.index("-m") + 1], "512")
                self.assertEqual(argv[argv.index("-smp") + 1], "2")
                self.assertTrue(any(str(vm.overlay) in arg for arg in argv))
                self.assertFalse(any("file=/base.raw" in arg for arg in argv))
                self.assertTrue(any(str(vm.socket_root / "console") in arg for arg in argv))
                self.assertTrue(any("queue-size=512" in arg for arg in argv))
            finally:
                with mock.patch.object(RUN.subprocess, "run"):
                    vm.close()

    def test_helper_exit_and_residual_descendants_cannot_report_success(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                out = vm.share / 'results' / 'task'
                out.mkdir()
                helper = dict(schema=1, status='ok', descendants_empty=True, cleanup_required=False,
                              sample_errors=0, diagnostic_errors=0, memory_peak_bytes=100,
                              task_elapsed_ns=20)
                (out / 'result.json').write_text(json.dumps(helper))
                (out / 'workload.json').write_text('{"schema":1,"success":true}')
                write_metrics(out)
                result = vm.collect(dict(name='task'), 0)
                self.assertEqual(result['status'], 'passed')
                self.assertEqual(result['sampled_memory_stat_peaks']['anon'], 50)
                self.assertEqual(result['sampled_memory_stat_peaks']['file'], 8)
                self.assertEqual(result['sampled_guest_used_peak_bytes'], (1024 - 600) * 1024)
                for key in ('task_elapsed_ns', 'memory_peak_bytes'):
                    original = helper[key]
                    for invalid in ('bad-measurement', -1, True, []):
                        with self.subTest(key=key, invalid=invalid):
                            helper[key] = invalid
                            (out / 'result.json').write_text(json.dumps(helper))
                            with self.assertRaises(ValueError):
                                vm.collect(dict(name='task'), 0)
                    helper[key] = None
                    (out / 'result.json').write_text(json.dumps(helper))
                    self.assertEqual(vm.collect(dict(name='task'), 0)['status'], 'failed')
                    helper[key] = original
                (out / 'result.json').write_text(json.dumps(helper))
                (out / 'metrics.jsonl').write_text('')
                with self.assertRaises(ValueError):
                    vm.collect(dict(name='task'), 0)
                (out / 'metrics.jsonl').unlink()
                with self.assertRaises(FileNotFoundError):
                    vm.collect(dict(name='task'), 0)
                samples = write_metrics(out)
                for invalid in ([s for s in samples if s['phase'] != 'settled'], list(reversed(samples))):
                    (out / 'metrics.jsonl').write_text(''.join(json.dumps(s) + '\n' for s in invalid))
                    with self.assertRaises(ValueError):
                        vm.collect(dict(name='task'), 0)
                write_metrics(out)
                self.assertEqual(vm.collect(dict(name='task'), 1)['status'], 'failed')
                helper['descendants_empty'] = False
                (out / 'result.json').write_text(json.dumps(helper))
                result = vm.collect(dict(name='task'), 0)
                self.assertEqual(result['status'], 'failed')
                self.assertTrue(result['quarantine_vm'])
            finally:
                vm.close()

    def test_quota_oom_does_not_hide_measurement_errors(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                for name, code in (('victim', 1), ('neighbor', 0)):
                    out = vm.share / 'results' / name
                    out.mkdir()
                    helper = dict(schema=1, status='workload_failed' if code else 'ok',
                                  descendants_empty=True, cleanup_required=False,
                                  sample_errors=0, diagnostic_errors=0, memory_peak_bytes=100,
                                  task_elapsed_ns=20, workload_start_ns=10, leader_exit_ns=30,
                                  baseline={'memory_events': {'max': 0, 'oom': 0, 'oom_kill': 0}},
                                  final={'memory_events': {key: int(bool(code)) for key in ('max', 'oom', 'oom_kill')}})
                    (out / 'result.json').write_text(json.dumps(helper))
                    (out / 'workload.json').write_text(json.dumps({'schema': 1, 'success': not bool(code)}))
                    write_metrics(out)
                    (vm.share / 'results' / (name + '.exit')).write_text(str(code))
                victim_file = vm.share / 'results/victim/result.json'
                for field in ('sample_errors', 'diagnostic_errors'):
                    with self.subTest(field=field):
                        victim = json.loads(victim_file.read_text())
                        victim.update(sample_errors=0, diagnostic_errors=0)
                        victim[field] = 1
                        victim_file.write_text(json.dumps(victim))
                        vm.trial_command = mock.Mock(side_effect=[
                            ('victim-command', dict(name='victim', admitted=True)),
                            ('neighbor-command', dict(name='neighbor', admitted=True))])
                        vm.command = mock.Mock(return_value=0)
                        rows = vm.pair(RUN.Budget(2), [128 * 1024**2, None])
                        self.assertEqual(rows[0]['status'], 'failed')
                        self.assertEqual(rows[1]['status'], 'passed')
            finally:
                vm.close()

    def test_quota_requires_complete_metrics_and_limit_events(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                for name in ('victim', 'neighbor'):
                    (vm.share / 'results' / name).mkdir()
                for failure in ('none', 'missing_metrics', 'missing_phase', 'max_not_hit', 'oom_not_hit',
                                'helper_list', 'unknown_schema', 'workload_list', 'null_events',
                                'missing_final', 'bad_time', 'metrics_list', 'over_limit'):
                    with self.subTest(failure=failure):
                        for name, code in (('victim', 1), ('neighbor', 0)):
                            out = vm.share / 'results' / name
                            events = {key: int(bool(code)) for key in ('max', 'oom', 'oom_kill')}
                            if code and failure == 'max_not_hit':
                                events['max'] = 0
                            if code and failure == 'oom_not_hit':
                                events['oom'] = 0
                            helper = dict(schema=1, status='workload_failed' if code else 'ok',
                                descendants_empty=True, cleanup_required=False, sample_errors=0,
                                diagnostic_errors=0, memory_peak_bytes=100, task_elapsed_ns=20,
                                workload_start_ns=10, leader_exit_ns=30,
                                baseline={'memory_events': {'max': 0, 'oom': 0, 'oom_kill': 0}},
                                final={'memory_events': events})
                            if code:
                                if failure == 'over_limit':
                                    helper['memory_peak_bytes'] = 128 * 1024**2 + 1
                                elif failure == 'helper_list':
                                    helper = []
                                elif failure == 'unknown_schema':
                                    helper['schema'] = 2
                                elif failure == 'null_events':
                                    helper['final']['memory_events'] = None
                                elif failure == 'missing_final':
                                    del helper['final']
                                elif failure == 'bad_time':
                                    helper['workload_start_ns'] = 'invalid-time'
                            (out / 'result.json').write_text(json.dumps(helper))
                            workload = [] if code and failure == 'workload_list' else {'schema': 1, 'success': not bool(code)}
                            (out / 'workload.json').write_text(json.dumps(workload))
                            (vm.share / 'results' / (name + '.exit')).write_text(str(code))
                            samples = write_metrics(out)
                            if code:
                                for sample in samples:
                                    if sample['phase'] in ('final', 'settled'):
                                        sample['memory_events'].update(events)
                                if failure == 'missing_phase':
                                    samples = [s for s in samples if s['phase'] != 'settled']
                                (out / 'metrics.jsonl').write_text(''.join(json.dumps(s) + '\n' for s in samples))
                                if failure == 'missing_metrics':
                                    (out / 'metrics.jsonl').unlink()
                                elif failure == 'metrics_list':
                                    (out / 'metrics.jsonl').write_text('[[]]\n')
                        vm.trial_command = mock.Mock(side_effect=[
                            ('victim-command', dict(name='victim', admitted=True, limit_bytes=128 * 1024**2)),
                            ('neighbor-command', dict(name='neighbor', admitted=True))])
                        vm.command = mock.Mock(return_value=0)
                        rows = vm.pair(RUN.Budget(2), [128 * 1024**2, None])
                        expected = ('expected_quota_oom' if failure == 'none' else
                                    'infrastructure_failed' if failure in ('missing_metrics', 'missing_phase',
                                      'helper_list', 'unknown_schema', 'workload_list', 'metrics_list') else 'failed')
                        self.assertEqual(rows[0]['status'], expected)
                        self.assertEqual(rows[1]['status'], 'passed')
                        self.assertEqual(sum(r['admitted'] for r in rows), 2)
            finally:
                vm.close()

    def test_finite_limit_rejects_successful_overcharge(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                out = vm.share / 'results/task'
                out.mkdir()
                helper = dict(schema=1, status='ok', descendants_empty=True, cleanup_required=False,
                              sample_errors=0, diagnostic_errors=0, memory_peak_bytes=4096,
                              task_elapsed_ns=20)
                (out / 'result.json').write_text(json.dumps(helper))
                (out / 'workload.json').write_text('{"schema":1,"success":true}')
                write_metrics(out)
                for limit, expected in ((None, 'passed'), (4097, 'passed'), (4096, 'passed'), (4095, 'failed'), (0, 'failed')):
                    with self.subTest(limit=limit):
                        row = vm.collect(dict(name='task', limit_bytes=limit), 0)
                        self.assertEqual(row['status'], expected)
            finally:
                vm.close()

    def test_cold_pair_cannot_pass_over_limit(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                for name in ('victim', 'neighbor'):
                    out = vm.share / 'results' / name
                    out.mkdir()
                    helper = dict(schema=1, status='ok', descendants_empty=True, cleanup_required=False,
                                  sample_errors=0, diagnostic_errors=0, memory_peak_bytes=100,
                                  task_elapsed_ns=20, workload_start_ns=10, leader_exit_ns=30)
                    (out / 'result.json').write_text(json.dumps(helper))
                    (out / 'workload.json').write_text('{"schema":1,"success":true}')
                    (vm.share / 'results' / (name + '.exit')).write_text('0')
                    write_metrics(out)
                vm.command = mock.Mock(return_value=0)
                rows = vm.pair(RUN.Budget(2), [99, None], mode='cold')
                self.assertEqual([r['status'] for r in rows], ['failed', 'passed'])
                self.assertEqual([r['mode'] for r in rows], ['cold', 'cold'])
                self.assertEqual(vm.command.call_args.args[0].count(' cold'), 2)
            finally:
                vm.close()

    def test_successful_pair_requires_real_execution_overlap(self):
        with tempfile.TemporaryDirectory(prefix='am-pair-time-') as root:
            vm = RUN.GuestVM(self.config(), Path(root) / 'vm', mock.Mock(), 512)
            try:
                for name in ('victim', 'neighbor'):
                    (vm.share / 'results' / name).mkdir()
                for start, end, neighbor_start, expected in (
                        (10, 30, 20, 'passed'), (10, 30, 30, 'failed'),
                        (10, 30, 31, 'failed'), (None, 30, 20, 'failed'),
                        ('bad', 30, 20, 'failed'), (30, 10, 0, 'failed')):
                    with self.subTest(start=start, end=end, neighbor_start=neighbor_start):
                        for name in ('victim', 'neighbor'):
                            out = vm.share / 'results' / name
                            helper = dict(schema=1, status='ok', descendants_empty=True,
                                cleanup_required=False, sample_errors=0, diagnostic_errors=0,
                                memory_peak_bytes=4096, task_elapsed_ns=20,
                                workload_start_ns=start if name == 'victim' else neighbor_start,
                                leader_exit_ns=end if name == 'victim' else 50)
                            (out / 'result.json').write_text(json.dumps(helper))
                            (out / 'workload.json').write_text('{"schema":1,"success":true}')
                            (vm.share / 'results' / (name + '.exit')).write_text('0')
                            write_metrics(out)
                        vm.trial_command = mock.Mock(side_effect=[
                            ('victim-command', dict(name='victim', admitted=True)),
                            ('neighbor-command', dict(name='neighbor', admitted=True))])
                        vm.command = mock.Mock(return_value=0)
                        rows = vm.pair(RUN.Budget(2), [None, None])
                        self.assertEqual([row['status'] for row in rows], [expected, expected])
                        self.assertEqual(sum(row['admitted'] for row in rows), 2)
                        if expected == 'failed':
                            self.assertTrue(all(row['error'] == 'pair_not_concurrent' for row in rows))
            finally:
                vm.close()

    def test_pair_retains_both_admitted_attempts_on_transport_or_result_failure(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", mock.Mock(), 512)
            try:
                for transport_error in (True, False):
                    with self.subTest(transport_error=transport_error):
                        vm.trial_command = mock.Mock(side_effect=[
                            ('victim-command', dict(name='victim', admitted=True)),
                            ('neighbor-command', dict(name='neighbor', admitted=True))])
                        vm.command = mock.Mock(side_effect=TimeoutError('fixture timeout') if transport_error else None,
                                               return_value=0)
                        # Missing result/exit files model a completed transport with
                        # failed collection; neither failure may erase the attempts.
                        rows = vm.pair(RUN.Budget(2), [128 * 1024**2, None])
                        self.assertEqual({r['name'] for r in rows}, {'victim', 'neighbor'})
                        self.assertEqual(sum(r['admitted'] for r in rows), 2)
                        self.assertTrue(all(r['status'] == 'infrastructure_failed' for r in rows))
                        self.assertTrue(all(r['quarantine_vm'] for r in rows))
            finally:
                vm.close()

    def test_close_attempts_all_resources_when_first_wait_fails(self):
        with tempfile.TemporaryDirectory(prefix="am-") as root:
            sampler = mock.Mock()
            vm = RUN.GuestVM(self.config(), Path(root) / "vm", sampler, 512)
            backend, qemu = mock.Mock(pid=801), mock.Mock(pid=802)
            backend.poll.return_value = qemu.poll.return_value = None
            qemu.wait.side_effect = subprocess.TimeoutExpired("qemu", 5)
            vm.processes = [backend, qemu]
            with mock.patch.object(RUN.os, "killpg"):
                try:
                    vm.close()
                except (subprocess.TimeoutExpired, RuntimeError):
                    pass
            self.assertTrue(backend.wait.called, "QEMU cleanup failure must not orphan backend")
            self.assertTrue(vm.serial.closed)
            self.assertEqual(sampler.unregister.call_count, 2)


class DiskAdmissionTests(unittest.TestCase):
    def fixture(self, root, cases):
        artifact = root / 'artifact'
        artifact.write_bytes(b'x' * 4096)
        sdk = root / 'sdk'
        sdk.mkdir()
        (sdk / 'package.json').write_bytes(b'y' * 4096)
        config = {key: str(artifact) for key in ('kernel', 'disk', 'opencode', 'node',
                  'helper', 'qemu', 'qemu_img', 'virtiofsd')}
        config.update(sdk=str(sdk), cases=cases)
        return config, 4 * 4096

    def vm(self, root, name):
        vm = mock.Mock(overlay=root / 'absent-overlay', identity=name)
        vm.trial.return_value = dict(name=name, status='passed', admitted=True)
        return vm

    def invoke(self, root, config, free, constructor):
        sampler = mock.Mock(rows=[], failure=None)
        context = mock.MagicMock()
        context.__enter__.return_value = sampler
        old_umask = RUN.os.umask(0o077)
        try:
            with mock.patch.object(RUN, 'validate_config'), \
                 mock.patch.object(RUN.shutil, 'disk_usage', side_effect=lambda _: mock.Mock(free=free())), \
                 mock.patch.object(RUN, 'HostSampler', return_value=context), \
                 mock.patch.object(RUN, 'GuestVM', constructor), \
                 mock.patch.object(RUN.subprocess, 'check_output', return_value='fixture QEMU\n'), \
                 mock.patch('builtins.print'):
                return RUN.run(config, root / 'output')
        finally:
            RUN.os.umask(old_umask)

    def test_restart_reserves_both_retained_payloads_before_launch(self):
        with tempfile.TemporaryDirectory(prefix='am-disk-restart-') as directory:
            root = Path(directory)
            config, payload = self.fixture(root, [dict(vms=1, ram_mib=512, restart_resume=True)])
            free = (1024 + 256) * 1024**2 + 2 * payload - 1
            constructor = mock.Mock(side_effect=[self.vm(root, 'first'), self.vm(root, 'restored')])
            with self.assertRaisesRegex(ValueError, 'payload'):
                self.invoke(root, config, lambda: free, constructor)
            constructor.assert_not_called()

    def test_fleet_is_admitted_once_before_workers_consume_space(self):
        with tempfile.TemporaryDirectory(prefix='am-disk-fleet-') as directory:
            root = Path(directory)
            config, payload = self.fixture(root, [dict(vms=2, ram_mib=512)])
            free = [(1024 + 2 * 256) * 1024**2 + 2 * payload]
            guests = [self.vm(root, 'first'), self.vm(root, 'second')]
            for guest in guests:
                guest.start.side_effect = lambda: free.__setitem__(0, free[0] - payload)
            constructor = mock.Mock(side_effect=guests)
            # Model the valid schedule where the first worker copies its payload
            # before the next worker enters. No real thread timing or barrier.
            pool = mock.MagicMock()
            pool.__enter__.return_value = pool
            def submit(function, *args):
                future = concurrent.futures.Future()
                future.set_result(function(*args))
                return future
            pool.submit.side_effect = submit
            with mock.patch.object(RUN.concurrent.futures, 'ThreadPoolExecutor', return_value=pool), \
                 mock.patch.object(RUN.threading, 'Barrier'):
                self.invoke(root, config, lambda: free[0], constructor)
            attempts = json.loads((root / 'output/attempts.json').read_text())
            self.assertEqual([row['status'] for row in attempts], ['passed', 'passed'])
            self.assertEqual(constructor.call_count, 2)

    def test_later_restart_capacity_failure_preserves_prior_results_without_workers(self):
        with tempfile.TemporaryDirectory(prefix='am-disk-later-') as directory:
            root = Path(directory)
            config, payload = self.fixture(root, [dict(vms=1, ram_mib=512),
                                                   dict(vms=1, ram_mib=512, restart_resume=True)])
            base = (1024 + 256) * 1024**2
            free = [1 << 40]
            first = self.vm(root, 'first')
            first.start.side_effect = lambda: free.__setitem__(0, base + 2 * payload - 1)
            constructor = mock.Mock(side_effect=[first, self.vm(root, 'second'), self.vm(root, 'restored')])
            real_pool = concurrent.futures.ThreadPoolExecutor
            with mock.patch.object(RUN.concurrent.futures, 'ThreadPoolExecutor', wraps=real_pool) as pools:
                self.invoke(root, config, lambda: free[0], constructor)
            attempts = json.loads((root / 'output/attempts.json').read_text())
            self.assertEqual([row['status'] for row in attempts], ['passed', 'infrastructure_failed'])
            self.assertFalse(attempts[1]['admitted'])
            self.assertIsNone(attempts[1]['vm'])
            self.assertEqual(constructor.call_count, 1)
            self.assertEqual(pools.call_count, 1)
            self.assertTrue((root / 'output/summary.json').is_file())

    def test_later_disk_stat_error_keeps_prior_results(self):
        with tempfile.TemporaryDirectory(prefix='am-disk-error-') as directory:
            root = Path(directory)
            config, _ = self.fixture(root, [dict(vms=1, ram_mib=512), dict(vms=1, ram_mib=512)])
            failed = [False]
            first = self.vm(root, 'first')
            first.start.side_effect = lambda: failed.__setitem__(0, True)
            constructor = mock.Mock(return_value=first)
            def free():
                if failed[0]:
                    raise OSError(errno.EIO, 'fixture disk stat failure')
                return 1 << 40
            self.invoke(root, config, free, constructor)
            attempts = json.loads((root / 'output/attempts.json').read_text())
            self.assertEqual([row['status'] for row in attempts], ['passed', 'infrastructure_failed'])
            self.assertIn('OSError', attempts[1]['error'])
            self.assertFalse(attempts[1]['admitted'])
            self.assertEqual(constructor.call_count, 1)
            self.assertTrue((root / 'output/summary.json').is_file())


class RunCoverageTests(unittest.TestCase):
    def test_restart_resume_inherits_case_policy(self):
        with tempfile.TemporaryDirectory(prefix='am-restart-') as root:
            root = Path(root)
            artifact = root / 'artifact'
            artifact.write_text('public fixture')
            sdk = root / 'sdk'
            sdk.mkdir()
            (sdk / 'package.json').write_text('{}')
            config = {key: str(artifact) for key in ('kernel', 'disk', 'opencode', 'node',
                      'helper', 'qemu', 'qemu_img', 'virtiofsd')}
            config.update(sdk=str(sdk), cases=[dict(vms=1, ram_mib=512,
                          restart_resume=True, limit_bytes=64 * 1024**2, diagnostic=True)])
            sampler = mock.Mock(rows=[], failure=None)
            context = mock.MagicMock()
            context.__enter__.return_value = sampler
            original = mock.Mock(overlay=root / 'overlay', identity='first-vm')
            restored = mock.Mock(overlay=root / 'overlay', identity='restored-vm')
            for vm, name in ((original, 't0'), (restored, 'resume')):
                vm.trial.return_value = dict(name=name, status='passed', admitted=True)
            old_umask = RUN.os.umask(0o077)
            try:
                with mock.patch.object(RUN, 'validate_config'), \
                     mock.patch.object(RUN.shutil, 'disk_usage', return_value=mock.Mock(free=1 << 40)), \
                     mock.patch.object(RUN, 'HostSampler', return_value=context), \
                     mock.patch.object(RUN, 'GuestVM', side_effect=[original, restored]), \
                     mock.patch.object(RUN.subprocess, 'check_output', return_value='fixture QEMU\n'), \
                     mock.patch('builtins.print'):
                    RUN.run(config, root / 'output')
            finally:
                RUN.os.umask(old_umask)
            for vm in (original, restored):
                self.assertEqual(vm.trial.call_args.kwargs['limit'], 64 * 1024**2)
                self.assertTrue(vm.trial.call_args.kwargs['diagnostic'])
            self.assertEqual(restored.trial.call_args.kwargs['mode'], 'resume')
            self.assertEqual(restored.trial.call_args.kwargs['workspace'], '/root/agent-memory/t0')
            self.assertEqual(len(json.loads((root / 'output/attempts.json').read_text())), 2)

    def test_cold_pair_preparation_and_execution_use_requested_mode(self):
        with tempfile.TemporaryDirectory(prefix='am-cold-') as root:
            root = Path(root)
            artifact = root / 'artifact'
            artifact.write_text('public fixture')
            sdk = root / 'sdk'
            sdk.mkdir()
            (sdk / 'package.json').write_text('{}')
            config = {key: str(artifact) for key in ('kernel', 'disk', 'opencode', 'node',
                      'helper', 'qemu', 'qemu_img', 'virtiofsd')}
            config.update(sdk=str(sdk), cases=[dict(vms=1, ram_mib=512,
                          agent_limits_bytes=[128, None], mode='cold')])
            sampler = mock.Mock(rows=[], failure=None)
            context = mock.MagicMock()
            context.__enter__.return_value = sampler
            vm = mock.Mock(overlay=root / 'no-overlay', identity='fixture-vm')
            vm.pair.return_value = [dict(name=name, status='passed', mode='cold', admitted=True)
                                    for name in ('victim', 'neighbor')]
            old_umask = RUN.os.umask(0o077)
            try:
                with mock.patch.object(RUN, 'validate_config'), \
                     mock.patch.object(RUN.shutil, 'disk_usage', return_value=mock.Mock(free=1 << 40)), \
                     mock.patch.object(RUN, 'HostSampler', return_value=context), \
                     mock.patch.object(RUN, 'GuestVM', return_value=vm), \
                     mock.patch.object(RUN.subprocess, 'check_output', return_value='fixture QEMU\n'), \
                     mock.patch('builtins.print'):
                    RUN.run(config, root / 'output')
            finally:
                RUN.os.umask(old_umask)
            self.assertEqual(vm.prepare.call_args_list,
                             [mock.call('victim', 'cold'), mock.call('neighbor', 'cold')])
            self.assertEqual(vm.pair.call_args.kwargs, dict(mode='cold'))
            self.assertFalse(vm.trial.called)
            self.assertEqual(len(json.loads((root / 'output/attempts.json').read_text())), 2)

    def test_payload_copy_fallback_is_reserved_before_launch(self):
        with tempfile.TemporaryDirectory(prefix='am-copy-budget-') as root:
            root = Path(root)
            artifact = root / 'artifact'
            artifact.write_bytes(b'x' * 4096)
            sdk = root / 'sdk'
            sdk.mkdir()
            package = sdk / 'package.json'
            package.write_bytes(b'y' * 4096)
            config = {key: str(artifact) for key in ('kernel', 'disk', 'opencode', 'node',
                      'helper', 'qemu', 'qemu_img', 'virtiofsd')}
            config.update(sdk=str(sdk), cases=[dict(vms=2, ram_mib=512)])
            base_bytes = (1024 + 2 * 256) * 1024**2
            free = base_bytes + 2 * 4 * 4096 - 1
            old_umask = RUN.os.umask(0o077)
            try:
                with mock.patch.object(RUN, 'validate_config'), \
                     mock.patch.object(RUN.shutil, 'disk_usage', return_value=mock.Mock(free=free)), \
                     mock.patch.object(RUN, 'HostSampler') as sampler, \
                     mock.patch.object(RUN, 'GuestVM') as guest:
                    with self.assertRaisesRegex(ValueError, 'payload'):
                        RUN.run(config, root / 'output')
                    sampler.assert_not_called()
                    guest.assert_not_called()
            finally:
                RUN.os.umask(old_umask)

    def test_missing_steady_worker_window_is_incomplete(self):
        with tempfile.TemporaryDirectory(prefix='am-coverage-') as root:
            root = Path(root)
            artifact = root / 'artifact'
            artifact.write_text('fixed fixture')
            sdk = root / 'sdk'
            sdk.mkdir()
            (sdk / 'package.json').write_text('{"fixture":true}')
            config = {key: str(artifact) for key in ('kernel', 'disk', 'opencode', 'node',
                      'helper', 'qemu', 'qemu_img', 'virtiofsd')}
            config.update(sdk=str(sdk), cases=[dict(vms=1, ram_mib=512, duration_seconds=1800)])
            sampler = mock.Mock(rows=[], failure=None)
            vm = mock.Mock(overlay=root / 'nonexistent-overlay', identity='fixture-vm')
            vm.start.side_effect = RuntimeError('fixture boot failure')
            context = mock.MagicMock()
            context.__enter__.return_value = sampler
            old_umask = RUN.os.umask(0o077)
            try:
                with mock.patch.object(RUN, 'validate_config'), \
                     mock.patch.object(RUN.shutil, 'disk_usage', return_value=mock.Mock(free=1 << 40)), \
                     mock.patch.object(RUN, 'HostSampler', return_value=context), \
                     mock.patch.object(RUN, 'GuestVM', return_value=vm), \
                     mock.patch.object(RUN.subprocess, 'check_output', return_value='fixture QEMU\n'), \
                     mock.patch('builtins.print'):
                    code = RUN.run(config, root / 'output')
            finally:
                RUN.os.umask(old_umask)
            report = json.loads((root / 'output/summary.json').read_text())
            self.assertEqual(code, 1)
            self.assertEqual(report['steady_workers_expected'], 1)
            self.assertEqual(report['steady_windows'], [])
            self.assertFalse(report['steady_complete'])


class ArtifactBoundaryTests(unittest.TestCase):
    def fixture(self, root):
        artifact = root / 'artifact'
        artifact.write_text('public fixture')
        sdk = root / 'sdk'
        sdk.mkdir()
        (sdk / 'package.json').write_text('{}')
        runner = root / 'source' / 'run.py'
        runner.parent.mkdir()
        runner.write_text('fixture runner')
        workloads = runner.parent / 'workloads'
        workloads.mkdir()
        (workloads / 'input.json').write_text('{}')
        config = {key: str(artifact) for key in ('kernel', 'disk', 'opencode', 'node',
                  'helper', 'qemu', 'qemu_img', 'virtiofsd')}
        config.update(sdk=str(sdk), cases=[dict(vms=1, ram_mib=512)])
        return config, runner

    def test_output_rejects_recursive_source_overlap_before_creation(self):
        for variant in ('sdk-child', 'workloads-child', 'sdk-same', 'source-parent', 'symlink-child'):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory(prefix='am-path-') as tmp:
                root = Path(tmp)
                config, runner = self.fixture(root)
                sdk, workloads = root / 'sdk', runner.parent / 'workloads'
                link = root / 'sdk-link'
                link.symlink_to(sdk, target_is_directory=True)
                output = {'sdk-child': sdk / 'output', 'workloads-child': workloads / 'output',
                          'sdk-same': sdk, 'source-parent': root,
                          'symlink-child': link / 'output'}[variant]
                existed = output.exists()
                old_umask = RUN.os.umask(0o077)
                try:
                    with mock.patch.object(RUN, '__file__', str(runner)), \
                         mock.patch.object(RUN, 'validate_config'), \
                         mock.patch.object(RUN.shutil, 'disk_usage', return_value=mock.Mock(free=1 << 40)), \
                         mock.patch.object(RUN.shutil, 'copytree', side_effect=RuntimeError('recursive copy reached')) as copy, \
                         mock.patch.object(RUN, 'GuestVM') as guest:
                        with self.assertRaisesRegex(ValueError, 'overlap'):
                            RUN.run(config, output)
                        copy.assert_not_called()
                        guest.assert_not_called()
                finally:
                    RUN.os.umask(old_umask)
                self.assertEqual(output.exists(), existed)

    def test_virtualization_tools_are_in_initial_and_final_hash_checks(self):
        for changed in (None, 'qemu', 'qemu_img', 'virtiofsd'):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory(prefix='am-tools-') as tmp:
                root = Path(tmp)
                config, runner = self.fixture(root)
                for key in ('qemu', 'qemu_img', 'virtiofsd'):
                    tool = root / key
                    tool.write_text('original ' + key)
                    config[key] = str(tool)
                # Match execution: resolve a launcher symlink once, then check
                # the actual configured executable rather than its link text.
                launcher = root / 'qemu-link'
                launcher.symlink_to(root / 'qemu')
                config['qemu'] = str(launcher)
                initial = {key: RUN.digest(config[key]) for key in ('qemu', 'qemu_img', 'virtiofsd')}
                vm = mock.Mock(overlay=root / 'no-overlay', identity='fixture-vm')
                def trial(*args, **kwargs):
                    if changed:
                        Path(config[changed]).write_text('replacement ' + changed)
                    return dict(name='t0', status='passed', admitted=True)
                vm.trial.side_effect = trial
                context = mock.MagicMock()
                context.__enter__.return_value = mock.Mock(
                    rows=[dict(processes=[], errors=[], fleet_pss_bytes=0)], failure=None)
                old_umask = RUN.os.umask(0o077)
                try:
                    with mock.patch.object(RUN, '__file__', str(runner)), \
                         mock.patch.object(RUN, 'validate_config'), \
                         mock.patch.object(RUN.shutil, 'disk_usage', return_value=mock.Mock(free=1 << 40)), \
                         mock.patch.object(RUN, 'HostSampler', return_value=context), \
                         mock.patch.object(RUN, 'GuestVM', return_value=vm), \
                         mock.patch.object(RUN.subprocess, 'check_output', return_value='fixture QEMU\n'), \
                         mock.patch('builtins.print'):
                        code = RUN.run(config, root / 'output')
                finally:
                    RUN.os.umask(old_umask)
                report = json.loads((root / 'output/summary.json').read_text())
                manifest = json.loads((root / 'output/manifest.json').read_text())
                self.assertEqual(report['artifacts_unchanged'], changed is None)
                self.assertEqual(code, 0 if changed is None else 1)
                for key, value in initial.items():
                    self.assertEqual(manifest['artifacts_sha256'][key], value)


if __name__ == "__main__":
    unittest.main()
