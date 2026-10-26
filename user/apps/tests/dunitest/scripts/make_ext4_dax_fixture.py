#!/usr/bin/env python3
"""Create a disposable, offline-only ext4 DAX fixture; never accepts a block device.

Dependencies: Python 3, mke2fs and debugfs (e2fsprogs). No mount/root required.
The image and its numeric audit JSON must not exist. Attach the image to a test
VM's virtio-pmem device; the guest suite only mounts it read-only.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import subprocess
import tempfile

BLOCK = 4096
MARKER = b"DragonOS-ext4-dax-fixture-v1\n"


def run(*args):
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env={**os.environ, "LC_ALL": "C"})
    return result.stdout + result.stderr


def wrap_partition(output, mode):
    """Wrap this invocation's new filesystem in a fixed test-only disk layout."""
    filesystem_bytes = output.stat().st_size
    start, extra = (1, 0) if mode == "unaligned-start" else (8, 1)
    sectors = filesystem_bytes // 512 + extra
    image_bytes = ((start * 512 + sectors * 512 + BLOCK - 1) // BLOCK) * BLOCK
    header = bytearray(512)
    struct.pack_into("<B3sB3sII", header, 446, 0, b"\0" * 3,
                     0x83, b"\0" * 3, start, sectors)
    header[510:512] = b"\x55\xaa"
    # Keep the replacement on the output filesystem even when TMPDIR differs.
    fd, name = tempfile.mkstemp(prefix=".ext4-dax-partition-", dir=output.parent)
    wrapped = Path(name)
    try:
        with os.fdopen(fd, "wb") as target, output.open("rb") as source:
            target.truncate(image_bytes)
            target.write(header)
            target.seek(start * 512)
            shutil.copyfileobj(source, target)
        os.replace(wrapped, output)
    finally:
        wrapped.unlink(missing_ok=True)
    return {"filesystem_image_bytes": filesystem_bytes,
            "partition": {"start_lba": start, "length_lba": sectors}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--partition", choices=("unaligned-start", "unaligned-length"),
                        help="wrap the fixture for the DAX partition-geometry regression")
    args = parser.parse_args()
    output = args.output.absolute()
    audit = output.with_suffix(output.suffix + ".json")
    if output.exists() or output.is_symlink() or audit.exists() or audit.is_symlink():
        parser.error("output and audit must not exist")
    for tool in ("mke2fs", "debugfs"):
        if shutil.which(tool) is None:
            parser.error("requires e2fsprogs mke2fs and debugfs")
    output.parent.mkdir(parents=True, exist_ok=True)
    created = False
    try:
        with tempfile.TemporaryDirectory(prefix="ext4-dax-fixture-") as scratch:
            root = Path(scratch)
            (root / "fixture-marker").write_bytes(MARKER)
            pattern = bytes((i * 17 + 3) % 251 for i in range(3 * BLOCK))
            (root / "full-pattern").write_bytes(pattern)
            (root / "inode-dax").write_bytes(pattern)
            (root / "tail").write_bytes(pattern[:BLOCK + 17])
            (root / "sparse").write_bytes(pattern)
            (root / "unwritten").write_bytes(b"")
            fd = os.open(output, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
            created = True
            try:
                if not stat.S_ISREG(os.fstat(fd).st_mode):
                    raise RuntimeError("fixture must be a new regular file")
                os.ftruncate(fd, 64 * 1024 * 1024)
            finally:
                os.close(fd)
            run("mke2fs", "-q", "-F", "-t", "ext4", "-b", str(BLOCK),
                "-O", "^orphan_file", "-E", "lazy_itable_init=0,lazy_journal_init=0",
                "-d", str(root), str(output))

            def debug(command, write=False):
                return run("debugfs", *( ["-w"] if write else []), "-R", command, str(output))

            debug("punch /sparse 1 1", True)
            debug("fallocate /unwritten 0 2", True)
            debug("sif /unwritten size 12288", True)
            debug("sif /inode-dax flags 0x02080000", True)
            flags = re.search(r"Flags:\s*(0x[0-9a-fA-F]+)", debug("stat /inode-dax"))
            size = re.search(r"\bSize:\s*([0-9]+)", debug("stat /unwritten"))
            if flags is None or int(flags[1], 16) != 0x02080000:
                raise RuntimeError("inode DAX flags were not established")
            if size is None or int(size[1]) != 3 * BLOCK:
                raise RuntimeError("unwritten file must have an explicit i_size")
            hole = re.findall(r"(?m)^\s*([0-9]+)\s*$", debug("bmap /sparse 1"))
            if hole != ["0"]:
                raise RuntimeError("sparse file's middle block is not a hole")
            uninit = debug("extents /unwritten")
            if not re.search(r"uninit|unwritten", uninit, re.IGNORECASE):
                raise RuntimeError("debugfs did not establish unwritten extents")
            # bmap must identify exactly the second file block, not a guessed
            # inode/data offset. Only bytes after i_size in that data block change.
            mapped = debug("bmap /tail 1")
            matches = re.findall(r"(?m)^\s*([0-9]+)\s*$", mapped)
            if len(matches) != 1:
                raise RuntimeError("ambiguous tail block mapping")
            tail_block = int(matches[0])
            if not 0 < tail_block < 64 * 1024 * 1024 // BLOCK:
                raise RuntimeError("tail block outside fixture")
            with output.open("r+b", buffering=0) as image:
                image.seek(tail_block * BLOCK + 17)
                image.write(b"\xa5" * (BLOCK - 17))
                image.seek(tail_block * BLOCK)
                observed = image.read(BLOCK)
                if observed != pattern[BLOCK:BLOCK + 17] + b"\xa5" * (BLOCK - 17):
                    raise RuntimeError("tail data poison verification failed")
            # Data blocks carry no ext4 metadata checksum; no super/inode/extent
            # or bitmap bytes are modified by the poison write above.
            record = {"schema": 1, "image_bytes": 64 * 1024 * 1024,
                      "block_bytes": BLOCK, "full_bytes": len(pattern),
                      "tail_bytes": BLOCK + 17, "tail_logical_block": 1,
                      "tail_physical_block": tail_block, "poison_byte": 165,
                      "poison_offset_in_block": 17, "poison_bytes": BLOCK - 17,
                      "sparse_zero_block": 1, "unwritten_bytes": 3 * BLOCK,
                      "inode_dax_flags": 0x02080000}
            if args.partition is not None:
                record.update(wrap_partition(output, args.partition))
                record["image_bytes"] = output.stat().st_size
            with audit.open("x") as stream:
                json.dump(record, stream, indent=2)
                stream.write("\n")
    except BaseException:
        if created:
            output.unlink(missing_ok=True)
        raise


if __name__ == "__main__":
    main()
