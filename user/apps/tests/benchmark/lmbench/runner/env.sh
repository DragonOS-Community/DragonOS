# Defaults for DragonOS; callers may select Linux binaries and isolated fixtures.
export LMBENCH_BIN_DIR="${LMBENCH_BIN_DIR:-/lib/lmbench/bin/x86_64-linux-gnu}"
export LMBENCH_EXT4_DIR="${LMBENCH_EXT4_DIR:-/ext4}"
export LMBENCH_TMP_DIR="${LMBENCH_TMP_DIR:-/tmp}"
export LMBENCH_TEST_FILE="${LMBENCH_TEST_FILE:-test_file}"
export LMBENCH_ZERO_FILE="${LMBENCH_ZERO_FILE:-zero_file}"
export LMBENCH_CREATE_TEST_FILES="${LMBENCH_CREATE_TEST_FILES:-1}"
export LMBENCH_NET_SERVER="${LMBENCH_NET_SERVER:-10.0.2.15}"
# Set to 0 when supplying an existing filesystem directory (the Linux entry).
export LMBENCH_MANAGE_EXT4="${LMBENCH_MANAGE_EXT4:-1}"

# SCRIPT_DIR is the wrapper's test_cases directory during direct invocation,
# and the runner directory when sourced by run.sh.
if [ -f "$SCRIPT_DIR/../result.sh" ]; then
    . "$SCRIPT_DIR/../result.sh"
elif [ -f "$SCRIPT_DIR/result.sh" ]; then
    . "$SCRIPT_DIR/result.sh"
fi
