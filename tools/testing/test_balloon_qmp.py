#!/usr/bin/env python3
"""Focused checks for the host balloon validation procedure, without a VM."""
import argparse
import contextlib
import io
import unittest
from unittest.mock import patch

import balloon_qmp


class FakeQmp:
    def __init__(self, *, active=True, reporting=True, oom=False, stuck=False):
        self.active = active
        self.stuck = stuck
        self.actual = 2 * 1024 ** 3
        self.targets = []
        self.features = []
        if reporting:
            self.features.append('VIRTIO_BALLOON_F_REPORTING: free page reporting')
        if oom:
            self.features.append('VIRTIO_BALLOON_F_DEFLATE_ON_OOM: deflate on OOM')

    def command(self, name, **args):
        if name == 'query-balloon':
            return {'actual': self.actual}
        if name == 'x-query-virtio-status':
            return {'guest-features': {'dev-features': self.features},
                    'status': {'statuses': ['VIRTIO_CONFIG_S_DRIVER_OK: ready']
                               if self.active else []},
                    'broken': False, 'disabled': False, 'started': self.active}
        if name == 'balloon':
            self.targets.append(args['value'])
            if self.active and not self.stuck:
                self.actual = args['value']
            return {}
        raise AssertionError(name)


def arguments(**overrides):
    values = dict(baseline_unsupported=False, reporting='on', deflate_on_oom='off',
                  device='/machine/peripheral/balloon0/virtio-backend',
                  timeout_seconds=.01, delta_mib=64)
    values.update(overrides)
    return argparse.Namespace(**values)


class BalloonValidationTest(unittest.TestCase):
    def setUp(self):
        self.output = contextlib.redirect_stdout(io.StringIO())
        self.output.__enter__()
        self.addCleanup(self.output.__exit__, None, None, None)

    def test_cycles_and_immediate_reversal_restore_initial(self):
        qmp = FakeQmp()
        balloon_qmp.verify(qmp, arguments())
        mib = 1024 ** 2
        self.assertEqual(qmp.targets, [1984*mib, 2048*mib, 1920*mib,
                                      1984*mib, 2048*mib, 2048*mib])
        self.assertEqual(qmp.actual, 2048*mib)

    def test_disabled_reporting_and_enabled_oom_are_checked(self):
        balloon_qmp.verify(FakeQmp(reporting=False, oom=True),
                           arguments(reporting='off', deflate_on_oom='on'))
        with self.assertRaisesRegex(ValueError, 'REPORTING'):
            balloon_qmp.verify(FakeQmp(reporting=False), arguments())

    def test_missing_driver_with_successful_query_is_baseline(self):
        qmp = FakeQmp(active=False)
        with patch.object(balloon_qmp.time, 'sleep'):
            balloon_qmp.verify(qmp, arguments(baseline_unsupported=True))
        self.assertEqual(len(qmp.targets), 2)
        self.assertEqual(qmp.targets[-1], qmp.actual)

    def test_nonconvergence_fails_and_requests_restoration(self):
        qmp = FakeQmp(stuck=True)
        with self.assertRaises(TimeoutError):
            balloon_qmp.verify(qmp, arguments())
        self.assertEqual(qmp.targets[-1], 2 * 1024 ** 3)


if __name__ == '__main__':
    unittest.main()
