#!/usr/bin/env python3
"""Host FUSE snapshot regression: snapshot_restore.py BINARY WORK_DIRECTORY.

Uses Linux _IOW(0xe4, 1/2, u64). Requires /dev/fuse, fusermount3,
mkfs.ext4 and a writable work directory outside the tested mount. No skips.
"""
import array
import ctypes
import errno
import fcntl
import mmap
import os
from pathlib import Path
import sys

from xattr_flags import Mount

CHECKPOINT = 0x4008E401
RESTORE = 0x4008E402


def expect_errno(expected, function, *args):
    try:
        function(*args)
    except OSError as error:
        assert error.errno == expected, (error.errno, expected, function)
    else:
        raise AssertionError(('expected errno', expected, function))


def snapshot(fd, command, key):
    fcntl.ioctl(fd, command, array.array('Q', [key]), True)


def main():
    binary, work = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    work.mkdir(parents=True, exist_ok=True)
    mount = Mount(binary, work)
    descriptors = []
    private = None
    try:
        root = mount.point
        path = root / 'before'
        original = b'A' * (16 * 4096)
        path.write_bytes(original)
        (root / 'directory').mkdir()
        (root / 'directory' / 'child').write_bytes(b'child')
        control = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        descriptors.append(control)
        old = os.open(path, os.O_RDWR)
        descriptors.append(old)
        old_dir = os.open(root / 'directory', os.O_RDONLY | os.O_DIRECTORY)
        descriptors.append(old_dir)
        private = mmap.mmap(old, 4096, access=mmap.ACCESS_COPY)
        private[0] = ord('P')  # Private COW memory is outside snapshot rollback.
        expect_errno(errno.ENODEV, mmap.mmap, old, 4096, mmap.MAP_SHARED, mmap.PROT_READ)
        expect_errno(errno.ENOENT, os.stat, root / 'later')
        (root / 'reappears').write_bytes(b'restored after negative lookup')
        snapshot(control, CHECKPOINT, 7)
        expect_errno(errno.EEXIST, snapshot, control, CHECKPOINT, 7)
        expect_errno(errno.EINVAL, fcntl.ioctl, control, 1, array.array('Q', [7]), True)
        expect_errno(errno.ENOTTY, fcntl.ioctl, control, 0x4008E4FF, array.array('Q', [7]), True)
        expect_errno(errno.ENOENT, snapshot, control, RESTORE, 99)
        os.unlink(root / 'reappears')
        expect_errno(errno.ENOENT, os.stat, root / 'reappears')
        os.pwrite(old, b'changed', len(original))
        (root / 'later').write_bytes(b'discarded')
        assert path.stat().st_size == len(original) + 7
        old_ino = os.fstat(old).st_ino
        snapshot(control, RESTORE, 7)
        expect_errno(errno.ENOENT, snapshot, control, RESTORE, 7)
        expect_errno(errno.ESTALE, os.fstat, old)
        expect_errno(errno.ESTALE, os.pread, old, 1, 0)
        expect_errno(errno.ESTALE, os.pwrite, old, b'x', 0)
        assert private[0] == ord('P')
        expect_errno(errno.ENOENT, os.stat, root / 'later')
        assert path.read_bytes() == original
        assert (root / 'reappears').read_bytes() == b'restored after negative lookup'
        assert path.stat().st_ino != old_ino
        assert (root / 'directory' / 'child').read_bytes() == b'child'
        with os.scandir(root) as entries:
            for entry in entries:
                assert entry.inode() == os.stat(entry.path).st_ino
        libc = ctypes.CDLL(None, use_errno=True)
        buffer = ctypes.create_string_buffer(8192)
        for fd in (old_dir, control):
            ctypes.set_errno(0)
            result = libc.getdents64(fd, buffer, len(buffer))
            assert result == -1 and ctypes.get_errno() == errno.ESTALE, (result, ctypes.get_errno())
        # The same root control handle can operate on multiple future epochs.
        (root / 'later').write_bytes(b'new namespace after negative lookup')
        assert (root / 'later').read_bytes() == b'new namespace after negative lookup'
        snapshot(control, CHECKPOINT, 8)
        for index in range(16):
            (root / f'allocation-{index}').write_bytes(bytes([index]) * 16384)
        snapshot(control, RESTORE, 8)
        assert not (root / 'allocation-0').exists()
        (root / 'continued').write_bytes(b'continued after two restores')
        assert (root / 'continued').read_bytes() == b'continued after two restores'
        pending = os.open(root / 'pending', os.O_CREAT | os.O_RDWR, 0o600)
        descriptors.append(pending)
        os.unlink(root / 'pending')
        expect_errno(errno.EBUSY, snapshot, control, CHECKPOINT, 9)
        # Releasing descriptors from discarded epochs must not reclaim new files.
        private.close()
        private = None
        for fd in descriptors:
            os.close(fd)
        descriptors.clear()
        assert path.read_bytes() == original
        print('PASS snapshot restore: owner, epochs, root control, direct I/O, mmap, keys, continued allocation', flush=True)
    finally:
        if private is not None:
            private.close()
        for fd in descriptors:
            os.close(fd)
        mount.close()
    # Clean shutdown and re-open the output image verify durable restored state.
    mount = Mount(binary, work, restore=True)
    try:
        assert (mount.point / 'before').read_bytes() == original
        assert (mount.point / 'continued').read_bytes() == b'continued after two restores'
        assert not (mount.point / 'allocation-0').exists()
        print('PASS snapshot restored image survives clean remount', flush=True)
    finally:
        mount.close()


if __name__ == '__main__':
    main()
