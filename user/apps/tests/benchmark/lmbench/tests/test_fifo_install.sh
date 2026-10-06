#!/bin/sh
# The installed suite must carry its cleanup driver and export its selection.
set -eu
SUITE=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_SHELL=${TEST_SHELL:-/bin/sh}
work=$(mktemp -d)
trap 'rm -rf "$work"' 0
make -s -C "$SUITE" install DADK_CURRENT_BUILD_DIR="$work/install"
[ -x "$work/install/fifo_cleanup.sh" ] || {
    echo 'FAIL: installed suite has no FIFO cleanup driver' >&2
    exit 1
}
# Run the installed runner without filesystem fixture preparation. A tiny
# testcase observes what a dispatched child actually inherits, even when the
# caller disabled cleanup. Direct wrapper invocations remain useful controls.
rm "$work/install/init.sh" "$work/install/clean_up.sh"
cat > "$work/install/test_cases/cleanup_selection.sh" <<'CASE'
#!/bin/sh
[ "${LMBENCH_FIFO_CLEANUP:-0}" = 1 ] || exit 17
printf 'value 1\n'
CASE
printf 'SEARCH_PATTERN=^value\nRESULT_INDEX=2\n' > "$work/install/test_cases/cleanup_selection.meta"
mkdir "$work/tmp"
LMBENCH_FIFO_CLEANUP=0 ENOUGH=50000 LMBENCH_SH="$TEST_SHELL" \
    LMBENCH_TMP_DIR="$work/tmp" LMBENCH_RUN_TMP="$work/raw" \
    "$TEST_SHELL" "$work/install/run.sh" --only cleanup_selection --samples 1 --timeout 2 \
    > "$work/runner.log" 2>&1 || { cat "$work/runner.log"; exit 1; }
grep -q '"status":"ok"' "$work/runner.log"
echo "FIFO installed runner regression: PASS ($TEST_SHELL)"
