#!/bin/sh
# Test: vfs_open_lat
# Binary: lat_syscall
# Description: VFS open syscall latency test

set -e

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

testfile="$LMBENCH_TMP_DIR/vfs_open_lat.$$"
cleanup() {
    /bin/busybox rm -f "$testfile"
}
trap cleanup EXIT HUP INT TERM

echo "=== Running VFS open latency test ==="
touch "$testfile"
${LMBENCH_BIN_DIR}/lat_syscall -P 1 -W 1 -N 3 open "$testfile"
echo "Test completed successfully"
