import unittest
from unittest.mock import patch


import offline


class CounterBoundaries(unittest.TestCase):
    def sample(self, ticks=10, start=20, errors=None):
        return {'errors': errors or [], 'monotonic_ns': 1_000_000_000,
                'processes': [{'pid': 1, 'start_ticks': start, 'cpu_ticks': ticks, 'role': 'qemu'}]}

    def test_idle_rejects_pid_reuse_and_missing_samples(self):
        with self.assertRaises(ValueError):
            offline.idle_delta(self.sample(), self.sample(start=21))
        with self.assertRaises(ValueError):
            offline.idle_delta(self.sample(), self.sample(errors=['unavailable']))
        end = self.sample()
        end['processes'] = []
        with self.assertRaises(ValueError):
            offline.idle_delta(self.sample(), end)

    def test_workload_limit_is_checked_before_loading_config_or_booting(self):
        for iterations in ('0', '17'):
            arguments = ['offline.py', '--config', '/nonexistent-config',
                         '--kernel', 'test=' + __file__, '--output', '/unused-output',
                         '--iterations', iterations]
            with patch('sys.argv', arguments), self.assertRaises(SystemExit) as error:
                offline.main()
            self.assertEqual(error.exception.code, 2)

    def test_counters_cannot_silently_become_zero(self):
        with self.assertRaises(ValueError):
            offline.idle_delta(self.sample(), self.sample(ticks=9))
        helper = {'baseline': {'cpu_stat': dict(usage_usec=10, user_usec=2, system_usec=8)},
                  'final': {'cpu_stat': dict(usage_usec=20, user_usec=4, system_usec=16)}}
        self.assertEqual(offline.cpu_delta(helper), dict(usage_usec=10, user_usec=2, system_usec=8))
        helper['final']['cpu_stat']['system_usec'] = None
        with self.assertRaises(ValueError):
            offline.cpu_delta(helper)


if __name__ == '__main__':
    unittest.main()
