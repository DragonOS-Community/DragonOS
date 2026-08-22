#!/bin/sh
# LMbench test environment cleanup.

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

remove_test_files() {
    rm -f /tmp/zero_file /tmp/test_file
}

clean_ext4_fs() {
    if [ -d "/ext4" ]; then
        umount /ext4 2>/dev/null || true
        rmdir /ext4 2>/dev/null || true
    fi
    rm -f "$SCRIPT_DIR/ext4.img"
}

main() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "lmbench cleanup requires root privileges" >&2
        return 1
    fi

    remove_test_files
    clean_ext4_fs
}

if main "$@"; then
    echo "lmbench test environment cleaned"
else
    echo "lmbench test environment cleanup failed" >&2
    exit 1
fi
