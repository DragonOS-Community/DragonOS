#!/usr/bin/env python3
"""Unit tests for tools/build_sysconfig_payload.py.

Run with: python3 tools/test_build_sysconfig_payload.py

Coverage:
- byte-identical output under umask 0002/0022/0077;
- tar members carry numeric 0:0, explicit modes, fixed mtime, byte ordering;
- fail closed on undeclared executables, symlinks, special nodes and
  invalid spec paths;
- the fallback parser agrees with tomllib on the real spec file;
- end-to-end build of the real user/sysconfig tree.
"""

from __future__ import annotations

import importlib.util
import io
import os
import stat
import tarfile
import tempfile
import unittest
from pathlib import Path

TOOLS_DIR = Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parent

_spec = importlib.util.spec_from_file_location(
    "build_sysconfig_payload", TOOLS_DIR / "build_sysconfig_payload.py"
)
bsp = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bsp)


SPEC_TEXT = """\
version = 1

[defaults]
uid = 0
gid = 0
directory-mode = "0755"
regular-file-mode = "0644"

[files]
"etc/init.d/rcS" = { mode = "0755" }
"etc/shadow" = { mode = "0600" }
"""


def make_tree(root: Path) -> None:
    """Create a minimal sysconfig tree.

    File modes are left to the current umask (simulating different host
    environments); rcS is created with a 0o777 default mode so it keeps an
    executable bit under any common umask.
    """
    (root / "etc/init.d").mkdir(parents=True)
    (root / "etc/dragonos/network").mkdir(parents=True)
    rcS = root / "etc/init.d/rcS"
    fd = os.open(rcS, os.O_WRONLY | os.O_CREAT, 0o777)
    os.write(fd, b"#!/bin/sh\n")
    os.close(fd)
    (root / "etc/shadow").write_text("root::0:::::::\n")
    (root / "etc/dragonos/network/default.conf").write_text("[network]\n")


def read_members(data: bytes) -> list[tuple]:
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tf:
        return [
            (m.name, m.uid, m.gid, m.mode, m.mtime, m.uname, m.gname, m.isdir())
            for m in tf.getmembers()
        ]


class PayloadBuildTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = Path(self._tmp.name)
        self.src = self.tmp / "sysconfig"
        self.src.mkdir()
        self.spec_path = self.tmp / "spec.toml"
        self.spec_path.write_text(SPEC_TEXT)

    def build(self) -> bytes:
        spec = bsp.load_spec(self.spec_path)
        return bsp.build_tar_bytes(self.src, spec)

    def test_umask_independence(self):
        outputs = []
        for mask in (0o002, 0o022, 0o077):
            tree = self.tmp / f"tree-{mask:03o}"
            tree.mkdir()
            old = os.umask(mask)
            try:
                make_tree(tree)
            finally:
                os.umask(old)
            outputs.append(bsp.build_tar_bytes(tree, bsp.load_spec(self.spec_path)))
        self.assertEqual(outputs[0], outputs[1])
        self.assertEqual(outputs[1], outputs[2])

    def test_member_metadata(self):
        make_tree(self.src)
        members = dict((m[0], m) for m in read_members(self.build()))

        for name, m in members.items():
            self.assertEqual((m[1], m[2]), (0, 0), name)          # uid/gid
            self.assertEqual(m[4], 0, name)                       # mtime
            self.assertEqual((m[5], m[6]), ("", ""), name)        # uname/gname

        self.assertEqual(members["etc/init.d/rcS"][3], 0o755)
        self.assertEqual(members["etc/shadow"][3], 0o600)
        self.assertEqual(members["etc/dragonos/network/default.conf"][3], 0o644)
        self.assertEqual(members["etc"][3], 0o755)
        self.assertTrue(members["etc"][7])                        # is a directory

        names = [m[0] for m in read_members(self.build())]
        self.assertEqual(names, sorted(names, key=lambda p: p.encode()))

    def test_undeclared_executable_fails(self):
        make_tree(self.src)
        extra = self.src / "etc/init.d/extra.sh"
        extra.write_text("#!/bin/sh\n")
        extra.chmod(0o755)
        with self.assertRaises(bsp.PayloadError):
            self.build()

    def test_symlink_fails(self):
        make_tree(self.src)
        os.symlink("shadow", self.src / "etc/shadow.link")
        with self.assertRaises(bsp.PayloadError):
            self.build()

    def test_special_node_fails(self):
        make_tree(self.src)
        os.mkfifo(self.src / "etc/pipe")
        with self.assertRaises(bsp.PayloadError):
            self.build()

    def test_spec_declares_missing_path_fails(self):
        make_tree(self.src)
        self.spec_path.write_text(
            SPEC_TEXT + '"etc/nonexistent" = { mode = "0644" }\n'
        )
        with self.assertRaises(bsp.PayloadError):
            self.build()

    def test_invalid_override_paths_fail(self):
        make_tree(self.src)
        for bad in ("../x", "/abs", "a//b", "a/./b", "a/../b"):
            self.spec_path.write_text(
                SPEC_TEXT + f'"{bad}" = {{ mode = "0644" }}\n'
            )
            with self.assertRaises(bsp.PayloadError, msg=bad):
                self.build()

    def test_special_mode_bits_rejected(self):
        make_tree(self.src)
        self.spec_path.write_text(
            SPEC_TEXT + '"etc/shadow" = { mode = "4755" }\n'
        )
        with self.assertRaises(bsp.PayloadError):
            self.build()

    def test_fallback_parser_matches_tomllib_on_real_spec(self):
        try:
            import tomllib
        except ImportError:
            self.skipTest("requires tomllib (Python 3.11+)")
        real_spec = (REPO_ROOT / "user/sysconfig-metadata.toml").read_text()
        expected = tomllib.loads(real_spec)
        actual = bsp._parse_spec_fallback(real_spec)
        self.assertEqual(expected["version"], actual["version"])
        self.assertEqual(expected["defaults"], actual["defaults"])
        self.assertEqual(expected["files"], actual["files"])

    def test_real_sysconfig_end_to_end(self):
        spec = bsp.load_spec(REPO_ROOT / "user/sysconfig-metadata.toml")
        data = bsp.build_tar_bytes(REPO_ROOT / "user/sysconfig", spec)
        members = dict((m[0], m) for m in read_members(data))

        self.assertEqual(members["etc/init.d/rcS"][3], 0o755)
        self.assertEqual(members["usr/sbin/dragon-network"][3], 0o755)
        self.assertEqual(members["usr/sbin/dragon-network-boot"][3], 0o755)
        self.assertEqual(members["usr/lib/dragon-network/udhcpc.script"][3], 0o755)
        self.assertEqual(members["etc/shadow"][3], 0o600)
        self.assertEqual(members["etc/gshadow"][3], 0o600)
        self.assertEqual(members["etc/resolv.conf"][3], 0o644)
        self.assertFalse(members["etc/resolv.conf"][7])           # regular file
        for name, m in members.items():
            self.assertEqual((m[1], m[2]), (0, 0), name)

    def test_write_if_changed_is_idempotent(self):
        make_tree(self.src)
        out = self.tmp / "out/sysconfig.tar"
        data = self.build()
        self.assertTrue(bsp.write_if_changed(out, data))
        mtime = out.stat().st_mtime_ns
        self.assertFalse(bsp.write_if_changed(out, data))
        self.assertEqual(out.stat().st_mtime_ns, mtime)


if __name__ == "__main__":
    unittest.main(verbosity=2)
