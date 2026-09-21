#!/bin/sh
# Test: vfs_stat_lat
# Binary: lat_syscall
# Description: VFS stat syscall latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

testfile="$LMBENCH_TMP_DIR/vfs_stat_lat.$$"
cleanup() { lmbench_cleanup_capture; rm -f "$testfile"; }
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "=== Running VFS stat latency test ==="
touch "$testfile"
run_lmbench_measurement "${LMBENCH_BIN_DIR}/lat_syscall" -P 1 -W 1000 -N 11 stat "$testfile"

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
