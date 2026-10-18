#!/usr/bin/env python3
"""Real host FUSE regression: python3 tests/xattr_flags.py BINARY WORK_DIRECTORY.

Requires /dev/fuse, fusermount3 and mkfs.ext4. WORK_DIRECTORY must be a
writable local directory, outside the tested FUSE mount. No tests are skipped.
"""
import concurrent.futures
import errno
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def set_result(path, name, value, flags):
    try:
        os.setxattr(path, name, value, flags)
        return 0
    except OSError as error:
        return error.errno


def value_result(path, name):
    try:
        return os.getxattr(path, name)
    except OSError as error:
        return error.errno


class Mount:
    def __init__(self, binary, work, restore=False):
        self.point = work / 'mount'
        self.point.mkdir(exist_ok=True)
        self.log = (work / ('restore.log' if restore else 'initial.log')).open('w')
        args = [str(binary), '--mountpoint', str(self.point), '--log', 'warn']
        args += ['--image', str(work / 'saved.ext4')] if restore else ['--block', '8192']
        if not restore:
            args += ['--output', str(work / 'saved.ext4')]
        self.process = subprocess.Popen(args, cwd=work, stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 20
        try:
            while not os.path.ismount(self.point):
                if self.process.poll() is not None:
                    raise RuntimeError('FUSE daemon exited; see ' + self.log.name)
                if time.monotonic() > deadline:
                    raise RuntimeError('FUSE mount timed out; see ' + self.log.name)
                time.sleep(0.02)
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                try:
                    subprocess.run(['fusermount3', '-u', str(self.point)], check=True)
                finally:
                    self.process.kill()
                    self.process.wait()
                raise
        if os.path.ismount(self.point):
            subprocess.run(['fusermount3', '-u', str(self.point)], check=True)
        self.log.close()
        assert self.process.returncode == 0, 'FUSE daemon failed'


def check_matrix(path):
    cases = [
        ('zero creates', b'user.zero', b'old', 0, 0, b'old'),
        ('zero replaces', b'user.zero', b'new', 0, 0, b'new'),
        ('create creates', b'user.create', b'created', 1, 0, b'created'),
        ('create existing', b'user.create', b'bad', 1, errno.EEXIST, b'created'),
        ('replace absent', b'user.absent', b'bad', 2, errno.ENODATA, errno.ENODATA),
        ('replace existing', b'user.create', b'replaced', 2, 0, b'replaced'),
        ('both existing', b'user.create', b'bad', 3, errno.EEXIST, b'replaced'),
        ('both absent', b'user.both', b'bad', 3, errno.ENODATA, errno.ENODATA),
        ('unknown absent', b'user.unknown', b'bad', 4, errno.EINVAL, errno.ENODATA),
        ('unknown existing', b'user.zero', b'bad', 4, errno.EINVAL, b'new'),
        ('empty replace', b'user.zero', b'', 2, 0, b''),
        ('byte name', b'user.\xff', b'byte-name', 1, 0, b'byte-name'),
        ('byte name replaces', b'user.\xff', b'changed', 2, 0, b'changed'),
    ]
    for label, name, value, flags, expected, retained in cases:
        result = set_result(path, name, value, flags)
        actual = value_result(path, name)
        assert (result, actual) == (expected, retained), (label, result, actual)
        print('PASS', label, flush=True)


def check_concurrent(path):
    # Multiple callers compete on the same absent name: exactly one CREATE wins.
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        for iteration in range(20):
            name = f'user.race{iteration}'.encode()
            values = [bytes([index]) * 256 for index in range(8)]
            results = list(pool.map(lambda value: set_result(path, name, value, 1), values))
            assert results.count(0) == 1 and results.count(errno.EEXIST) == 7, results
            assert value_result(path, name) == values[results.index(0)]
            # Every successful replacement publishes a complete value.
            def replace(value):
                assert set_result(path, name, value, 2) == 0
                assert value_result(path, name) in values
            list(pool.map(replace, values))
            os.removexattr(path, name)
    print('PASS 20 concurrent CREATE/REPLACE rounds', flush=True)


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    work_root = Path(sys.argv[2]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix='ext4-fuse-xattr-', dir=work_root) as directory:
        work = Path(directory)
        mounted = Mount(binary, work)
        try:
            path = mounted.point / 'attributes'
            path.write_bytes(b'')
            check_matrix(path)
            check_concurrent(path)
        finally:
            mounted.close()
        # ext4 INCOMPAT_RECOVER must be cleared by shutdown before --output
        # captures the image. A writable reload alone could conceal replay.
        with (work / 'saved.ext4').open('rb') as image:
            image.seek(1024 + 0x60)
            assert int.from_bytes(image.read(4), 'little') & 0x4 == 0
        print('PASS saved journal image is clean before reload', flush=True)
        restored = Mount(binary, work, restore=True)
        try:
            path = restored.point / 'attributes'
            assert value_result(path, b'user.zero') == b''
            assert value_result(path, b'user.create') == b'replaced'
            assert value_result(path, b'user.\xff') == b'changed'
            assert value_result(path, b'user.absent') == errno.ENODATA
            print('PASS saved image reload retains values and absence', flush=True)
        finally:
            restored.close()


if __name__ == '__main__':
    main()
