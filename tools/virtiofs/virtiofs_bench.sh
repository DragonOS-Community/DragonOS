#!/usr/bin/env sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
HELPER="${VIRTIOFS_BENCH_HELPER:-${SCRIPT_DIR}/virtiofs_bench}"
SRC="${SCRIPT_DIR}/../../user/apps/virtiofs_bench/virtiofs_bench.cc"
MOUNT_PATH="${1:-}"

if [ -z "${MOUNT_PATH}" ]; then
    echo "usage: $0 MOUNT_PATH [helper args...]" >&2
    exit 2
fi
shift || true

if [ ! -x "${HELPER}" ]; then
    if command -v c++ >/dev/null 2>&1; then
        c++ -O2 -std=c++17 -pthread "${SRC}" -o "${HELPER}"
    else
        echo "missing helper ${HELPER}; build ${SRC} for this guest or set VIRTIOFS_BENCH_HELPER" >&2
        exit 2
    fi
fi

STATS_PATH="${VIRTIOFS_STATS_PATH:-/sys/kernel/debug/fuse/stats}"
OUT_DIR="${VIRTIOFS_BENCH_OUT:-/tmp/virtiofs_bench_$$}"
mkdir -p "${OUT_DIR}"

snapshot() {
    name="$1"
    if [ -r "${STATS_PATH}" ]; then
        cat "${STATS_PATH}" > "${OUT_DIR}/${name}.stats"
    else
        : > "${OUT_DIR}/${name}.stats"
    fi
}

write_delta() {
    before="$1"
    after="$2"
    out="$3"
    awk '
        FILENAME == ARGV[1] && /^\[/ { section = substr($0, 2, length($0) - 2); next }
        FILENAME == ARGV[2] && /^\[/ { section = substr($0, 2, length($0) - 2); next }
        FILENAME == ARGV[1] && NF == 2 {
            before[section "." $1] = $2
            next
        }
        FILENAME == ARGV[2] && NF == 2 {
            key = section "." $1
            if (key in before) {
                print key, $2 - before[key]
            }
        }
    ' "$before" "$after" > "$out"
}

snapshot before
set +e
"${HELPER}" --mount "${MOUNT_PATH}" "$@" > "${OUT_DIR}/results.txt"
rc=$?
set -e
cat "${OUT_DIR}/results.txt"
snapshot after
write_delta "${OUT_DIR}/before.stats" "${OUT_DIR}/after.stats" "${OUT_DIR}/delta.stats"

echo "bench_output=${OUT_DIR}"
echo "stats_before=${OUT_DIR}/before.stats"
echo "stats_after=${OUT_DIR}/after.stats"
echo "stats_delta=${OUT_DIR}/delta.stats"
exit "$rc"
