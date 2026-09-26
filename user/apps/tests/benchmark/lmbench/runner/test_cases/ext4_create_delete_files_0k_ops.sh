#!/bin/sh
# Test: ext4_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ext4 filesystem

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running ext4_create_delete_files_0k_ops test ==="
# tempnam prefers TMPDIR over its directory argument. Keep both on the fixture.
export TMPDIR="$LMBENCH_EXT4_DIR"
# Unmodified lat_fs includes 0k in its default size sweep; metadata selects
# only that row. Do not rely on -s 0k being distinguished from no size option.
run_lmbench_measurement "$LMBENCH_BIN_DIR/lat_fs" -P 1 "${LMBENCH_EXT4_DIR}"

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
