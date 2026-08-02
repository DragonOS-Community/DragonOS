#!/bin/sh
# LMbench test environment setup.

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

create_ext4_fs() {
    if [ ! -d "/ext4" ]; then
        mkdir -p /ext4
        if [ -f "$SCRIPT_DIR/ext4.img" ]; then
            rm -f "$SCRIPT_DIR/ext4.img"
        fi
        echo "[lmbench-init] creating ext4 image"
        dd if=/dev/zero of="$SCRIPT_DIR/ext4.img" bs=1M count=1024
        mkfs.ext4 "$SCRIPT_DIR/ext4.img"
        mount -o loop "$SCRIPT_DIR/ext4.img" /ext4
    fi
}

create_one_test_file() {
    file_path="$1"
    # Always recreate: a stale undersized file (e.g. left by an interrupted
    # previous run) would survive a plain "[ -f ]" existence check and break
    # size validation in bw_mmap_rd (size > file size), lat_mmap (< 4m) and
    # lat_pagefault (< 1MB).
    rm -f "$file_path"
    dd if=/dev/zero of="$file_path" bs=1M count=64
}

create_test_file() {
    for file_path in \
        "$LMBENCH_EXT4_DIR/zero_file" \
        "$LMBENCH_EXT4_DIR/test_file" \
        /tmp/zero_file \
        /tmp/test_file
    do
        echo "[lmbench-init] creating $file_path"
        create_one_test_file "$file_path"
    done
}

main() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "lmbench initialization requires root privileges" >&2
        return 1
    fi

    . "$SCRIPT_DIR/env.sh"
    create_ext4_fs

    # The representative MVP whitelist only needs an empty ext4 mount for lat_fs.
    # Avoid creating four 512 MiB fixtures before any metric can be emitted.
    if [ "${LMBENCH_CREATE_TEST_FILES:-0}" = "1" ]; then
        create_test_file
    fi
    # Tolerate dd's "sh: write error: Invalid argument" on serial close (guest
    # shell write EINVAL to serial device) — files are actually created; the
    # spurious non-zero from dd's stderr close would otherwise abort run.sh.
    return 0
}

if main "$@"; then
    echo "lmbench test environment initialized"
else
    echo "lmbench test environment initialization failed" >&2
    exit 1
fi
