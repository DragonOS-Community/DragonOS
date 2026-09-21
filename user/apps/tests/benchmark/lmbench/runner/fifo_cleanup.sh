#!/bin/sh
# Linux driver for the unmodified packaged lat_fifo (-P 1).
# Its writer inherits a SIGTERM handler and loops on EOF during cleanup.
# Only reap that writer AFTER both FIFO paths are unlinked. The benchmark
# has already sent its measured samples to its controller at that point.
# The caller supervises this entire process group (runner/result.sh).
set -eu
# Private Linux tmpfs, not persistent raw results: namespace teardown also
# reclaims this pipe if the supervisor must escalate to SIGKILL.
work=$(mktemp -d "${LMBENCH_TMP_DIR:-/tmp}/fifo-cleanup.XXXXXXXX")
trap 'rm -rf "$work"' 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkfifo "$work/output"
"$1" -P 1 > "$work/output" 2>&1 &
bench=$!
reaped=0
repeated=0
invalid=0
while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
        '(w) read/write on pipe:'*)
            if [ "$reaped" = 1 ]; then
                repeated=$((repeated + 1))
                continue
            fi
            printf '%s\n' "$line"
            # Restrict action to this invocation's worker and its own child;
            # a diagnostic alone is never sufficient to terminate a process.
            workers=$(cat "/proc/$bench/task/$bench/children" 2>/dev/null) || workers=
            for worker in $workers; do
                writers=$(cat "/proc/$worker/task/$worker/children" 2>/dev/null) || writers=
                for writer in $writers; do
                    # BusyBox readlink accepts one path per invocation.
                    links=$(for fd in /proc/"$writer"/fd/*; do
                        readlink "$fd" 2>/dev/null || true
                    done)
                    case "$links" in *"/var/tmp/lmbench/lmbench_f1.$worker (deleted)"*) ;; *) continue ;; esac
                    case "$links" in *"/var/tmp/lmbench/lmbench_f2.$worker (deleted)"*) ;; *) continue ;; esac
                    kill -KILL "$writer"
                    reaped=1
                    printf 'lmbench: reaped FIFO writer %s after both FIFO paths were unlinked\n' "$writer"
                done
            done
            if [ "$reaped" = 0 ]; then
                printf 'lmbench: error: writer diagnostic outside verified FIFO cleanup\n'
                invalid=1
            fi
            ;;
        '(r) read/write on pipe:'*|'(i) read/write on pipe:'*)
            invalid=1; printf '%s\n' "$line" ;;
        *) printf '%s\n' "$line" ;;
    esac
done < "$work/output"
rc=0
wait "$bench" || rc=$?
if [ "$repeated" -gt 0 ]; then
    printf 'lmbench: coalesced %s repeated writer EOF diagnostics after verified cleanup\n' "$repeated"
fi
# Keep the real controller's exit status; result.sh additionally requires a
# positive latency and rejects fatal diagnostics. No timeout becomes success.
[ "$invalid" = 0 ] || rc=1
exit "$rc"
