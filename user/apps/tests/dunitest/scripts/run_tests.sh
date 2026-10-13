#!/bin/busybox sh

set -u

SCRIPT_DIR="$(cd -- "$(dirname "$0")" && pwd)"
if [ -x "$SCRIPT_DIR/dunitest-runner" ] && [ -d "$SCRIPT_DIR/bin" ]; then
    BASE_DIR="$SCRIPT_DIR"
else
    BASE_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
fi
RUNNER="$BASE_DIR/dunitest-runner"
BIN_DIR="$BASE_DIR/bin"
RESULTS="$BASE_DIR/results"
WHITELIST="$BASE_DIR/whitelist.txt"
NO_SKIP="$BASE_DIR/no_skip.txt"

# Cache protection requires disk-backed files, not the guest's tmpfs /tmp.
# The installed test directory exists on the rootfs even in minimal images
# without /var/tmp. The test verifies its filesystem rather than skipping.
export DUNITEST_MEMORY_FILE_DIR="${DUNITEST_MEMORY_FILE_DIR:-$BASE_DIR}"

RUNNER_ARGS="--bin-dir $BIN_DIR --results-dir $RESULTS --whitelist $WHITELIST --no-skip $NO_SKIP"

echo "[dunit] start running tests..."
if [ "${DUNITEST_PATTERN:-}" != "" ]; then
    "$RUNNER" $RUNNER_ARGS --pattern "$DUNITEST_PATTERN"
else
    "$RUNNER" $RUNNER_ARGS
fi
status=$?
echo "[dunit] 测试完成, status=$status"
exit $status
