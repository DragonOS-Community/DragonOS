#!/bin/sh
# Exercise wrapper arguments, cleanup and exit status without benchmarking.
# Usage: sh test_wrapper_portability.sh [shell [shell-options ...]]
set -eu

TEST_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/lmbench-wrappers.XXXXXX")
trap 'rm -rf "$WORK_DIR"' 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
[ "$#" -gt 0 ] || set -- sh

mkdir -p "$WORK_DIR/runner/test_cases" "$WORK_DIR/bin" "$WORK_DIR/tmp" "$WORK_DIR/ext4" "$WORK_DIR/cwd"
cp "$TEST_DIR"/*.sh "$WORK_DIR/runner/test_cases/"
cp "$TEST_DIR"/*.meta "$WORK_DIR/runner/test_cases/"
[ ! -f "$TEST_DIR/../result.sh" ] || cp "$TEST_DIR/../result.sh" "$WORK_DIR/runner/result.sh"
# Wrappers still source env.sh; isolate defaults while retaining shared result
# validation exactly as the installed flat layout does.
cat > "$WORK_DIR/runner/env.sh" <<'ENV'
. "$SCRIPT_DIR/../result.sh"
ENV
export LMBENCH_BIN_DIR="$WORK_DIR/bin" LMBENCH_TMP_DIR="$WORK_DIR/tmp" LMBENCH_EXT4_DIR="$WORK_DIR/ext4"
export LMBENCH_TEST_FILE=test_file LMBENCH_ZERO_FILE=zero_file LMBENCH_NET_SERVER=192.0.2.7
export WRAPPER_LOG="$WORK_DIR/invocations"
export FORK_PID_FILE="$WORK_DIR/forked-writer.pid"
cat > "$WORK_DIR/bin/benchmark-stub" <<'STUB'
#!/bin/sh
printf '%s' "${0##*/}" >> "$WRAPPER_LOG"
for arg do printf ' <%s>' "$arg" >> "$WRAPPER_LOG"; done
printf '\n' >> "$WRAPPER_LOG"
case "${1:-}" in -s|-S) exit 0 ;; esac
if [ "${NOISY_HANG:-0}" = 1 ]; then
    while :; do printf '0123456789abcdef'; done
fi
if [ "${FORK_WRITER:-0}" = 1 ]; then
    (
        trap '' TERM
        while :; do printf 'forked writer still holds stdout\n'; sleep 1; done
    ) &
    printf '%s\n' "$!" > "$FORK_PID_FILE"
fi
if [ "${FAIL_CLIENT:-0}" != 0 ]; then
    printf 'RAW_FAILURE_MARKER\n'
    exit 23
fi
case "${BAD_OUTPUT:-}" in
    assertion) printf 'assertion failed: clock went backwards\n' ;;
    error) printf 'error: benchmark child failed\n' ;;
    semaphore_read_error) printf '(r) error on semaphore: Invalid argument\n' ;;
    semaphore_write_error) printf '(w) error on initial semaphore: Permission denied\n' ;;
    send_failed) printf 'lat_udp client: send failed: Permission denied\n' ;;
    recv_failed) printf 'lat_udp client: recv failed: Invalid argument\n' ;;
    timeout) printf 'UDP benchmark internal timeout\n' ;;
    enosys) printf 'Function not implemented\n' ;;
    missing) exit 0 ;;
esac
case "${0##*/}" in
    lat_sem) printf 'Semaphore latency: 1.0 microseconds\n' ;;
    lat_udp) printf 'UDP latency using 192.0.2.7: 1.0 microseconds\n' ;;
    lat_tcp) printf 'TCP latency using 127.0.0.1: 1.0 microseconds\n' ;;
    lat_unix_connect) printf 'UNIX connection cost: 1.0 microseconds\n' ;;
    bw_tcp) printf '0.000128 1.0 MB/sec\n' ;;
    lat_connect) printf 'TCP/IP connection cost to 192.0.2.7: 1.0 microseconds\n' ;;
    lat_syscall)
        case " $* " in
            *' fstat '*) printf 'Simple fstat: 1.0 microseconds\n' ;;
            *) printf 'Simple stat: 1.0 microseconds\n' ;;
        esac
        ;;
    lat_http)
        http_input=$(cat)
        [ "$http_input" = "file1k
file4k
file16k" ] || { printf 'error: HTTP file list stdin missing\n'; exit 42; }
        printf 'Avg xfer: 0.5 MB/sec\n'
        ;;
    *) printf 'benchmark result: 1.0 microseconds\n' ;;
esac
STUB
chmod +x "$WORK_DIR/bin/benchmark-stub"
for binary in lat_sem lat_unix_connect lat_udp lat_tcp bw_tcp lat_connect lat_syscall lat_http lmhttp; do
    ln -s benchmark-stub "$WORK_DIR/bin/$binary"
done
ln -s /bin/true "$WORK_DIR/bin/sleep"
PATH="$WORK_DIR/bin:$PATH"
export PATH
cd "$WORK_DIR/cwd"
FAILURES=0
fail() { printf 'FAIL: %s\n' "$*" >&2; FAILURES=$((FAILURES + 1)); }
run_wrapper() {
    wrapper_name=$1; shift
    : > "$WRAPPER_LOG"
    WRAPPER_RC=0
    "$@" "$WORK_DIR/runner/test_cases/$wrapper_name.sh" > "$WORK_DIR/output" 2>&1 || WRAPPER_RC=$?
}

for wrapper in semaphore_lat unix_connect_lat; do
    run_wrapper "$wrapper" "$@"
    [ "$WRAPPER_RC" = 0 ] || fail "$wrapper must use the runner timeout and return success (got $WRAPPER_RC)"
done

grep -q '^lat_unix_connect <-S>$' "$WRAPPER_LOG" || fail 'Unix connection wrapper must stop its forked server'

for wrapper in tcp_virtio_lat tcp_virtio_bw_128 tcp_virtio_bw_64k tcp_virtio_connect_lat udp_virtio_lat; do
    run_wrapper "$wrapper" "$@"
    [ "$WRAPPER_RC" = 0 ] || fail "$wrapper failed ($WRAPPER_RC)"
    grep -q '<192.0.2.7>' "$WRAPPER_LOG" || fail "$wrapper must honor the configured target"
    grep -q ' <-S> <192.0.2.7>$' "$WRAPPER_LOG" || fail "$wrapper must stop its server"
    if [ "$wrapper" = udp_virtio_lat ]; then
        grep -q '^lat_udp <-P> <1> <192.0.2.7>$' "$WRAPPER_LOG" || fail 'UDP must run a latency client'
    fi
done

for scenario in assertion error semaphore_read_error semaphore_write_error send_failed recv_failed timeout enosys missing; do
    BAD_OUTPUT=$scenario; export BAD_OUTPUT
    run_wrapper semaphore_lat "$@"
    [ "$WRAPPER_RC" -ne 0 ] || fail "direct wrapper accepted $scenario output"
done
unset BAD_OUTPUT

# The HTTP wrapper has distinct multi-field output and stdin handling.
run_wrapper tcp_loopback_http_bw "$@"
[ "$WRAPPER_RC" = 0 ] || fail "HTTP wrapper rejected a valid metric ($WRAPPER_RC)"
grep -q '^lat_http <127.0.0.1> <8080>$' "$WRAPPER_LOG" || fail 'HTTP measurement command did not consume file_list stdin'
rm -rf "$LMBENCH_TMP_DIR/lmbench_http"

for wrapper in vfs_stat_lat vfs_fstat_lat; do
    printf 'existing fixture\n' > test_file
    printf 'existing fixture\n' > testfile
    run_wrapper "$wrapper" "$@"
    [ "$WRAPPER_RC" = 0 ] || fail "$wrapper failed ($WRAPPER_RC)"
    [ -f test_file ] && [ -f testfile ] || fail "$wrapper must not remove existing files in the caller directory"
    [ -z "$(ls -A "$LMBENCH_TMP_DIR")" ] || fail "$wrapper must remove its temporary file"
done

FAIL_CLIENT=1; export FAIL_CLIENT
run_wrapper tcp_loopback_lat "$@"
[ "$WRAPPER_RC" = 23 ] || fail "cleanup must preserve the client exit code (got $WRAPPER_RC)"
grep -q 'RAW_FAILURE_MARKER' "$WORK_DIR/output" || fail 'failed client raw diagnostics must be replayed'
if find "$LMBENCH_TMP_DIR" -name '.lmbench-*' | grep -q .; then
    fail 'failed client left measurement capture files'
fi
grep -q '^lat_tcp <-S> <127.0.0.1>$' "$WRAPPER_LOG" || fail 'failed client must stop its server'
unset FAIL_CLIENT

NOISY_HANG=1; export NOISY_HANG
WRAPPER_RC=0
"${LMBENCH_TIMEOUT:-/usr/bin/timeout}" -k 2s 1s "$@" "$WORK_DIR/runner/test_cases/semaphore_lat.sh" > "$WORK_DIR/noisy-output" 2>&1 || WRAPPER_RC=$?
[ "$WRAPPER_RC" = 124 ] || fail "noisy hung wrapper must time out (got $WRAPPER_RC)"
[ "$(wc -c < "$WORK_DIR/noisy-output")" -le 1100000 ] || fail 'noisy wrapper output exceeded its bound'
grep -q 'output truncated after 1048576 bytes' "$WORK_DIR/noisy-output" || fail 'noisy wrapper did not report output truncation'
if find "$LMBENCH_TMP_DIR" -name '.lmbench-*' | grep -q .; then
    find "$LMBENCH_TMP_DIR" -name '.lmbench-*' >&2
    fail 'timed-out wrapper left measurement capture files'
fi
if ps -eo args= | grep -F "$WORK_DIR" | grep -v 'grep -F' | grep -q .; then
    ps -eo pid=,args= | grep -F "$WORK_DIR" | grep -v 'grep -F' >&2 || true
    fail 'timed-out wrapper left measurement or pump processes'
fi
unset NOISY_HANG

FORK_WRITER=1; export FORK_WRITER
WRAPPER_RC=0
"${LMBENCH_TIMEOUT:-/usr/bin/timeout}" -k 2s 3s "$@" "$WORK_DIR/runner/test_cases/semaphore_lat.sh" > "$WORK_DIR/fork-output" 2>&1 || WRAPPER_RC=$?
[ "$WRAPPER_RC" = 0 ] || fail "forking measurement wrapper did not finish cleanly (got $WRAPPER_RC)"
forked_pid=$(cat "$WORK_DIR/forked-writer.pid")
if kill -0 "$forked_pid" 2>/dev/null; then fail 'forked FIFO writer survived wrapper completion'; fi
if find "$LMBENCH_TMP_DIR" -name '.lmbench-*' | grep -q .; then
    fail 'forking measurement left helper paths'
fi
if ps -eo args= | grep -F "$WORK_DIR" | grep -v 'grep -F' | grep -q .; then
    fail 'forking measurement left command-family or pump processes'
fi
unset FORK_WRITER

NOISY_HANG=1; export NOISY_HANG
: > "$WRAPPER_LOG"
WRAPPER_RC=0
"$@" "$WORK_DIR/runner/test_cases/tcp_loopback_lat.sh" > "$WORK_DIR/output" 2>&1 &
wrapper_pid=$!
i=0
while [ "$i" -lt 50 ] && ! find "$LMBENCH_TMP_DIR" -name '.lmbench-stream.*' | grep -q .; do
    /bin/sleep 0.02
    i=$((i + 1))
done
kill -TERM "$wrapper_pid"
wait "$wrapper_pid" || WRAPPER_RC=$?
[ "$WRAPPER_RC" = 143 ] || fail "TERM must terminate the wrapper (got $WRAPPER_RC)"
grep -q '^lat_tcp <-S> <127.0.0.1>$' "$WRAPPER_LOG" || fail 'TERM must stop the server'
if find "$LMBENCH_TMP_DIR" -name '.lmbench-*' | grep -q .; then
    fail 'signaled wrapper left measurement capture files'
fi
if ps -eo args= | grep -F "$WORK_DIR" | grep -v 'grep -F' | grep -q .; then
    fail 'signaled wrapper left measurement or pump processes'
fi
unset NOISY_HANG

mv "$WORK_DIR/bin" "$WORK_DIR/bin tools"
LMBENCH_BIN_DIR="$WORK_DIR/bin tools"
export LMBENCH_BIN_DIR
run_wrapper semaphore_lat "$@"
[ "$WRAPPER_RC" = 0 ] || fail 'binary paths with spaces must work'

[ "$FAILURES" = 0 ] || exit 1
printf 'wrapper portability regression: PASS\n'
