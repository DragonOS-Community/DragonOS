#!/bin/sh
# Failures must not be converted to success by the FIFO cleanup driver.
set -eu
SUITE=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_SHELL=${TEST_SHELL:-/bin/sh}
work=$(mktemp -d)
trap 'rm -rf "$work"' 0
cat > "$work/lat_fifo" <<'STUB'
#!/bin/sh
case "$MODE" in
    ok) echo 'Fifo latency: 1.0 microseconds' ;;
    exit) echo 'Fifo latency: 1.0 microseconds'; exit 23 ;;
    writer) echo '(w) read/write on pipe: Invalid argument'; echo 'Fifo latency: 1.0 microseconds' ;;
    reader) echo '(r) read/write on pipe: Invalid argument'; echo 'Fifo latency: 1.0 microseconds' ;;
esac
STUB
chmod +x "$work/lat_fifo"
for MODE in ok exit writer reader; do
    export MODE
    rc=0
    LMBENCH_TMP_DIR="$work" "$TEST_SHELL" "$SUITE/runner/fifo_cleanup.sh" "$work/lat_fifo" > "$work/$MODE.log" 2>&1 || rc=$?
    case "$MODE" in
        ok) [ "$rc" = 0 ] ;;
        exit) [ "$rc" = 23 ] ;;
        writer|reader) [ "$rc" != 0 ] ;;
    esac
    ! grep -q 'reaped FIFO writer' "$work/$MODE.log"
done
# No helper pipe/directory survives any of these paths.
[ "$(find "$work" -type p | wc -l)" -eq 0 ]
[ "$(find "$work" -type d | wc -l)" -eq 1 ]
echo "FIFO driver failure regression: PASS ($TEST_SHELL)"
