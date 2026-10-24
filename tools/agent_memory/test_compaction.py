"""Pure host validation of MM-266 admission, coverage, result and cleanup contracts."""
import json
import shlex
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import compaction as c


def stats(dma=16385, normal=0):
    return ('version=1\nfree_pages DMA32 ' + str(dma) + '\n'
            + f'zone DMA32 order 0 free_blocks {dma} attempts 0 misses 0\n'
            + f'free_pages Normal {normal}\n'
            + f'zone Normal order 0 free_blocks {normal} attempts 0 misses 0\n').encode()


class ResultContracts(unittest.TestCase):
    def test_no_skips_or_empty_or_failed_tests(self):
        valid = b'<testsuites><testsuite><testcase status="run" result="completed"/></testsuite></testsuites>'
        self.assertEqual(c.check_xml(valid), 1)
        for case in ('', '<testcase status="notrun"/>', '<testcase status="run"><skipped/></testcase>',
                     '<testcase status="run"><failure/></testcase>',
                     '<testcase status="run" result="skipped"/>',
                     '<testcase status="run"><error/></testcase>'):
            with self.subTest(case=case), self.assertRaises(ValueError):
                c.check_xml(('<testsuites>' + case + '</testsuites>').encode())

    def test_threshold_cannot_be_met_by_another_zone(self):
        value = c.allocation_snapshot(stats(1, 20000))
        self.assertEqual(value['target_free_blocks'], 1)
        self.assertEqual(value['orders']['Normal']['0'], 20000)
        self.assertEqual(c.allocation_snapshot(stats())['target_free_blocks'], 16385)

    def test_malformed_or_incoherent_snapshot_rejected(self):
        base = stats()
        for bad in (b'', base.replace(b'version=1', b'version=2'),
                    base + b'zone DMA32 order 0 free_blocks 1 attempts 0 misses 0\n',
                    base.replace(b'free_pages DMA32 16385', b'free_pages DMA32 2'),
                    base.replace(b'order 0', b'order 64')):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                c.allocation_snapshot(bad)

    def test_failed_exit_cannot_be_hidden_by_passing_xml(self):
        with tempfile.TemporaryDirectory() as root:
            vm = mock.Mock(share=Path(root))
            (Path(root) / 'results').mkdir()
            (Path(root) / 'results/after-example.xml').write_text(
                '<testsuites><testcase status="run"/></testsuites>')
            vm.command.return_value = 1
            rows = []
            self.assertFalse(c.run_suite(vm, rows, 'after', 'example', 10))
            self.assertEqual(rows[0]['cases'], 1)
            self.assertEqual(rows[0]['status'], 'failed')
            wire = vm.command.call_args.args[0]
            self.assertIn(c.GUEST_TESTS + '/example_test', wire)
            self.assertNotIn('/mnt/agent-memory/tests/example_test', wire)


class SnapshotStreamContracts(unittest.TestCase):
    def test_capture_explicitly_reads_to_eof_without_a_stat_count(self):
        argv = shlex.split(c.snapshot_wire('page_compaction_selftest', 'report.txt'))
        self.assertEqual(argv[0], 'dd')
        self.assertIn('bs=256', argv)
        self.assertFalse(any(arg.startswith(('count=', 'skip=', 'seek=')) for arg in argv))
        # proc streams have zero metadata size but nonempty read() contents.
        # Execute the same capture operation on that real virtual-file boundary.
        source = Path('/proc/version')
        self.assertEqual(source.stat().st_size, 0)
        with tempfile.TemporaryDirectory() as root:
            destination = Path(root) / 'report.txt'
            argv = ['dd', 'if=' + str(source), 'of=' + str(destination), 'bs=256']
            subprocess.run(argv, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.assertEqual(destination.read_bytes(), source.read_bytes())
            self.assertTrue(destination.read_bytes())

    def test_summary_beyond_one_page_is_required_and_preserved(self):
        report = b'status=ok\n' + b'detail=value\n' * 400 + b'summary_fail=0\n'
        self.assertGreater(report.index(b'summary_fail'), 4096)
        c.check_report(report)
        with self.assertRaises(ValueError):
            c.check_report(report[:4096])
        with self.assertRaises(ValueError):
            c.check_report(report.replace(b'summary_fail=0', b'summary_fail=1'))


class PhaseContracts(unittest.TestCase):
    def test_three_agent_rounds_precede_coverage_and_post_tests(self):
        rows, events = [], []
        vm = mock.Mock(share=Path('/private/share'))
        def command(*args):
            events.append(args[2])
            row = {'stage': args[2], 'exit_code': 0, 'status': 'passed'}
            rows.append(row)
            return row
        def snapshot(*args):
            events.append(args[2])
            return {'target_free_blocks': 16385, 'target_zone': 'DMA32'}
        def agent(*args):
            events.append('agent-' + str(args[2]))
            return True
        def suite(*args):
            events.append(args[2] + '-' + args[3])
            return True
        with mock.patch.object(c, 'command', side_effect=command), \
             mock.patch.object(c, 'snapshot', side_effect=snapshot), \
             mock.patch.object(c, 'agent_round', side_effect=agent), \
             mock.patch.object(c, 'run_suite', side_effect=suite), \
             mock.patch.object(c, 'read_small', return_value=b'status=ok\nsummary_fail=0\n'):
            c.exercise(vm, rows, 30)
        self.assertLess(events.index('guest-test-install'), events.index('before-stats'))
        self.assertLess(events.index('agent-2'), events.index('after-agent-stats'))
        self.assertLess(events.index('after-agent-stats'), events.index('after-page_compaction'))
        self.assertEqual([e for e in events if e.startswith('agent-')], ['agent-0', 'agent-1', 'agent-2'])
        self.assertIn('block-io', events)
        self.assertIn('final-stats', events)

    def test_coverage_boundary_is_not_pass_and_does_not_run_post_tests(self):
        for count in (0, 16384):
            rows = []
            with mock.patch.object(c, 'command', return_value={'exit_code': 0}), \
                 mock.patch.object(c, 'snapshot', return_value={'target_free_blocks': count}), \
                 mock.patch.object(c, 'agent_round', return_value=True) as agent, \
                 mock.patch.object(c, 'run_suite') as suite:
                c.exercise(mock.Mock(), rows, 30)
            self.assertEqual(agent.call_count, 3)
            self.assertEqual(suite.call_count, len(c.BASELINE))
            self.assertEqual(rows[-1]['status'], 'coverage_not_reached')

    def test_missing_feature_and_failed_agent_stop_acceptance(self):
        rows = []
        with mock.patch.object(c, 'command', return_value={'exit_code': 1}), \
             mock.patch.object(c, 'agent_round') as agent, self.assertRaises(ValueError):
            c.exercise(mock.Mock(), rows, 30)
        agent.assert_not_called()
        with mock.patch.object(c, 'command', return_value={'exit_code': 0}), \
             mock.patch.object(c, 'snapshot'), mock.patch.object(c, 'run_suite'), \
             mock.patch.object(c, 'agent_round', return_value=False) as agent, self.assertRaises(ValueError):
            c.exercise(mock.Mock(), [], 30)
        self.assertEqual(agent.call_count, 1)


class TestInstallationContracts(unittest.TestCase):
    def test_fixture_input_is_single_bounded_regular_file(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            fixture = root / 'ext4_inode_identity.img'
            with self.assertRaises(FileNotFoundError):
                c.fixture_input(fixture)
            fixture.write_bytes(b'public fixture')
            self.assertEqual(c.fixture_input(fixture), fixture)
            with self.assertRaises(ValueError):
                c.fixture_input(root)
            fixture.write_bytes(b'')
            with self.assertRaises(ValueError):
                c.fixture_input(fixture)
            with fixture.open('wb') as stream:
                stream.truncate(c.FIXTURE_LIMIT + 1)
            with self.assertRaises(ValueError):
                c.fixture_input(fixture)

    def test_only_new_guest_tree_is_opened_for_nonroot_self_exec(self):
        vm = mock.Mock()
        vm.command.return_value = 0
        c.install_tests(vm, [], 30)
        wire = vm.command.call_args.args[0]
        self.assertTrue(wire.startswith('test ! -e ' + c.GUEST_TEST_ROOT + ' && '))
        self.assertIn('cp /mnt/agent-memory/tests/*_test ' + c.GUEST_TESTS, wire)
        self.assertIn('chmod 0755 ' + c.GUEST_TESTS + '/*_test', wire)
        installed_binary = Path(c.GUEST_TESTS) / 'shared_page_migration_test'
        relative_fixture = installed_binary.parent.parent.parent / 'fixtures/ext4_inode_identity.img'
        self.assertIn(str(relative_fixture), wire)
        for command in wire.split(' && '):
            if command.startswith('chmod '):
                self.assertNotIn('/mnt/agent-memory', command)
                self.assertNotIn('payload', command)
                self.assertNotIn('results', command)
                self.assertTrue(all(path.startswith(c.GUEST_TEST_ROOT + '/') or path == c.GUEST_TEST_ROOT
                                    for path in shlex.split(command)[2:]))

    def test_failed_test_install_never_runs_baseline_or_agent(self):
        with mock.patch.object(c, 'command', return_value={'exit_code': 1}), \
             mock.patch.object(c, 'run_suite') as suite, \
             mock.patch.object(c, 'agent_round') as agent, self.assertRaises(ValueError):
            c.exercise(mock.Mock(), [], 30)
        suite.assert_not_called()
        agent.assert_not_called()


class ResourceContracts(unittest.TestCase):
    def test_cleanup_failure_never_deletes_busy_resources(self):
        vm = mock.Mock()
        vm.close.side_effect = RuntimeError('unmount failed')
        with mock.patch.object(c.shutil, 'rmtree') as remove, self.assertRaises(RuntimeError):
            c.cleanup(vm)
        remove.assert_not_called()
        vm.overlay.unlink.assert_not_called()

    def test_cleanup_rejects_remaining_mount(self):
        vm = mock.Mock(payload_mounted=True)
        with self.assertRaises(RuntimeError):
            c.cleanup(vm)
        vm.overlay.unlink.assert_not_called()

    def test_result_budget_and_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            (root / 'share/results').mkdir(parents=True)
            (root / 'host.jsonl').write_text('')
            vm = mock.Mock(directory=root, share=root / 'share')
            # The sampler belongs to directory.parent for real GuestVM instances.
            vm.directory = root / 'vm'
            vm.directory.mkdir()
            log = vm.directory / 'serial.log'
            log.write_bytes(b'x' * 100)
            with mock.patch.object(c, 'OUTPUT_LIMIT', 50), self.assertRaises(ValueError):
                c.bounded_output(vm)
            log.unlink()
            (root / 'share/results/link').symlink_to(root / 'host.jsonl')
            with self.assertRaises(ValueError):
                c.bounded_output(vm)

    def test_config_drops_provider_and_all_non_whitelist_fields(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            source = {key: str(root / key) for key in c.PATH_KEYS}
            for key in c.PATH_KEYS:
                if key != 'sdk':
                    (root / key).write_text('public')
            (root / 'sdk').mkdir()
            (root / 'sdk/package').write_text('public')
            source.update(provider={'apiKey': 'not-forwarded'}, env={'TOKEN': 'not-forwarded'},
                          cases=[{'vms': 16}], vcpus=2)
            path = root / 'config.json'
            path.write_text(json.dumps(source))
            config = c.load_config(path, root / 'kernel')
            self.assertEqual(set(config), set(c.PATH_KEYS) | {'vcpus', 'overlay_budget_mib'})
            self.assertNotIn('not-forwarded', json.dumps(config))


if __name__ == '__main__':
    unittest.main()
