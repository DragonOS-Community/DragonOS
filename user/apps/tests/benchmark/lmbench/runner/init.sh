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

create_test_file() {
    ext4_zero_file_path="$LMBENCH_EXT4_DIR/zero_file"
    ext4_test_file_path="$LMBENCH_EXT4_DIR/test_file"
    tmp_zero_file_path=/tmp/zero_file
    tmp_test_file_path=/tmp/test_file

    if [ ! -f "$ext4_zero_file_path" ]; then
        echo "[lmbench-init] creating $ext4_zero_file_path"
        dd if=/dev/zero of="$ext4_zero_file_path" bs=1M count=512
    fi

    if [ ! -f "$ext4_test_file_path" ]; then
        echo "[lmbench-init] creating $ext4_test_file_path"
        dd if=/dev/zero of="$ext4_test_file_path" bs=1M count=512
    fi

    if [ ! -f "$tmp_zero_file_path" ]; then
        echo "[lmbench-init] creating $tmp_zero_file_path"
        dd if=/dev/zero of="$tmp_zero_file_path" bs=1M count=512
    fi

    if [ ! -f "$tmp_test_file_path" ]; then
        echo "[lmbench-init] creating $tmp_test_file_path"
        dd if=/dev/zero of="$tmp_test_file_path" bs=1M count=512
    fi
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
}

if main "$@"; then
    echo "lmbench test environment initialized"
else
    echo "lmbench test environment initialization failed" >&2
    exit 1
fi
