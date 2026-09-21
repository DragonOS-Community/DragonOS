#!/bin/sh
# Shared POSIX-sh LMbench benchmark runner.
#
# Entry point invoked by /etc/init.d/rcS when AUTO_TEST=benchmark. Runs each
# whitelisted case N times, extracts a numeric result per test_cases/<name>.meta,
# computes summary statistics, and emits ONE JSON line per metric (prefixed with
# "LMBENCH_JSON ") to the serial console for the host-side collector to harvest.
#
# Output contract (host collect_results.py depends on it):
#   ===LMBENCH_RUN_BEGIN===
#   LMBENCH_META    {json}          # run-level info known inside the guest
#   LMBENCH_JSON    {json}          # one per metric (JSONL, single line each)
#   LMBENCH_SUMMARY {json}          # totals
#   ===LMBENCH_RUN_END===
#   benchmark测试完成                # completion marker (mirrors gvisor "测试完成")
set -u

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
CASES_DIR="$SCRIPT_DIR/test_cases"
CONFIG_FILE="$SCRIPT_DIR/config"
WHITELIST_FILE="$SCRIPT_DIR/whitelist.txt"
# Support both the installed flat layout and running directly from the checkout.
[ -f "$CONFIG_FILE" ] || CONFIG_FILE="$SCRIPT_DIR/../config"
[ -f "$WHITELIST_FILE" ] || WHITELIST_FILE="$SCRIPT_DIR/../whitelist.txt"
WORK_TMP="${LMBENCH_RUN_TMP:-/tmp/lmbench_run}"
LMBENCH_SH=${LMBENCH_SH:-sh}
CLEANUP_DONE=0
CLEANUP_STATUS=0
# Prefer the guest's GNU coreutils installation; Linux normally uses PATH.
if [ -z "${LMBENCH_TIMEOUT:-}" ]; then
    if [ -x /usr/local/bin/timeout ]; then
        LMBENCH_TIMEOUT=/usr/local/bin/timeout
    elif [ -x /usr/bin/timeout ]; then
        # An absolute path avoids BusyBox ash selecting its timeout applet.
        LMBENCH_TIMEOUT=/usr/bin/timeout
    else
        LMBENCH_TIMEOUT=timeout
    fi
fi

# ---- global config (overridable via config file) ----
SAMPLES=5
TIMEOUT_SEC=120
WARMUP=0
SUITE_VERSION="3.0-a9"

# CLI intent variables (set by parse_args, applied after load_config).
CLI_SAMPLES=""; CLI_TIMEOUT=""; CLI_WARMUP=""
CLI_WHITELIST=""; CLI_CONFIG=""; ONLY_NAME=""; LIST_ONLY=""; HELP_ONLY=""

log() { echo "[lmbench-runner] $*"; }

usage() {
    cat >&2 <<'EOF'
usage: run.sh [--samples N] [--timeout S] [--warmup N]
              [--whitelist FILE] [--config FILE] [--only NAME] [--list]
EOF
}

# Parse CLI args into CLI_* intent variables (applied after load_config).
# Returns 2 on unknown arg (caller exits); does not exit the shell itself
# so it can be unit-tested via `LMBENCH_RUNNER_NO_MAIN=1 . run.sh`.
parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --samples|--timeout|--warmup|--whitelist|--config|--only)
                if [ "$#" -lt 2 ] || [ -z "$2" ]; then usage; return 2; fi ;;
        esac
        case "$1" in
            --samples)    CLI_SAMPLES=$2;    shift 2 ;;
            --timeout)    CLI_TIMEOUT=$2;    shift 2 ;;
            --warmup)     CLI_WARMUP=$2;     shift 2 ;;
            --whitelist)  CLI_WHITELIST=$2;  shift 2 ;;
            --config)     CLI_CONFIG=$2;     shift 2 ;;
            --only)       ONLY_NAME=$2;      shift 2 ;;
            --list)       LIST_ONLY=1;       shift ;;
            -h|--help)    HELP_ONLY=1; usage; return 0 ;;
            *)            usage; return 2 ;;
        esac
    done
}

# Apply CLI overrides on top of config-file values.
apply_cli_overrides() {
    [ -n "$CLI_SAMPLES" ]   && SAMPLES=$CLI_SAMPLES
    [ -n "$CLI_TIMEOUT" ]   && TIMEOUT_SEC=$CLI_TIMEOUT
    [ -n "$CLI_WARMUP" ]    && WARMUP=$CLI_WARMUP
    [ -n "$CLI_WHITELIST" ] && WHITELIST_FILE=$CLI_WHITELIST
}

is_positive_integer() {
    case "$1" in ''|0*|*[!0-9]*) return 1 ;; *) return 0 ;; esac
}

# Read KEY=VALUE from a file without sourcing it (avoids clobbering runner state).
# Use shell builtins only: DragonOS can stall when repeatedly constructing short
# grep|head|cut pipelines during early boot.
kv_get() {
    _kv_file=$1
    _kv_key=$2
    [ -f "$_kv_file" ] || return 0
    while IFS= read -r _kv_line || [ -n "$_kv_line" ]; do
        case "$_kv_line" in
            "$_kv_key="*)
                printf '%s\n' "${_kv_line#*=}"
                return 0
                ;;
        esac
    done < "$_kv_file"
}

load_config() {
    [ -f "$CONFIG_FILE" ] || return 0
    v=$(kv_get "$CONFIG_FILE" SAMPLES);     [ -n "$v" ] && SAMPLES=$v
    v=$(kv_get "$CONFIG_FILE" TIMEOUT_SEC); [ -n "$v" ] && TIMEOUT_SEC=$v
    v=$(kv_get "$CONFIG_FILE" WARMUP);      [ -n "$v" ] && WARMUP=$v
}

# Minimal JSON string escaper: backslash, quote, then collapse control chars.
json_escape() {
    printf '%s' "$1" \
        | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' \
        | tr '\n\r\t' '   '
}

# Is the argument a plain decimal number?
is_number() {
    case "$1" in
        ''|*[!0-9.eE+-]*) return 1 ;;
        *) printf '%s\n' "$1" | grep -Eq '^[-+]?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?$' ;;
    esac
}

# Run "$@" in a dedicated process group with a wall-clock timeout.
# GNU timeout uses setpgid (not setsid), avoiding DragonOS's setsid fault while
# still terminating background servers spawned by a benchmark wrapper.
run_with_timeout() {
    secs=$1; outf=$2; shift 2
    "$LMBENCH_TIMEOUT" -k 2s "${secs}s" "$@" >"$outf" 2>&1
    rc=$?
    case "$rc" in
        124|137) return 124 ;;
        *) return "$rc" ;;
    esac
}

# Extract the metric value from a captured-output file.
# PAT/IDX/NTH are metadata globals. Keep field selection in shell instead of
# interpolating $(NF-1) into awk; DragonOS BusyBox awk rejects that construct.
extract_value() {
    lmbench_extract_value "$1" "$PAT" "$IDX" "$NTH"
}

# Some DragonOS images install BusyBox without standalone applet links.
runner_sort() {
    if command -v sort >/dev/null 2>&1; then command sort "$@"
    else /bin/busybox sort "$@"; fi
}
runner_awk() {
    if command -v awk >/dev/null 2>&1; then command awk "$@"
    else /bin/busybox awk "$@"; fi
}

# Compute stats from space-separated numbers on $1. Echoes:
#   count mean median stddev min max cv
# sqrt is done via Newton's method so we never depend on busybox awk's optional
# libm (CONFIG_FEATURE_AWK_LIBM); only + - * / are used.
compute_stats() {
    printf '%s\n' $1 | runner_sort -n | runner_awk '
        function nsqrt(x,   g, i) {
            if (x <= 0) return 0
            g = x
            for (i = 0; i < 60; i++) g = (g + x / g) / 2
            return g
        }
        { v[NR] = $1 + 0; sum += $1 }
        END {
            n = NR; if (n == 0) { exit }
            mean = sum / n
            for (i = 1; i <= n; i++) { d = v[i] - mean; ss += d * d }
            sd = (n > 1) ? nsqrt(ss / (n - 1)) : 0
            median = (n % 2) ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2
            cv = (mean != 0) ? sd / mean : 0
            printf "%d %.6f %.6f %.6f %.6f %.6f %.6f", n, mean, median, sd, v[1], v[n], cv
        }'
}

# Common metric JSON prefix built from .meta fields (no trailing brace).
metric_head() {
    _big=false; [ "$BIGGER" = "1" ] && _big=true
    printf '{"name":"%s","category":"%s","binary":"%s","metric_type":"%s","unit":"%s","bigger_is_better":%s,"description":"%s"' \
        "$NAME" "$CATEGORY" "$BINARY" "$MTYPE" "$UNIT" "$_big" "$(json_escape "$DESC")"
}

# Run one whitelisted case; emit its LMBENCH_JSON line.
# Returns 0=ok, 1=failed, 2=skipped.
run_one_case() {
    NAME=$1
    case_sh="$CASES_DIR/$NAME.sh"
    meta="$CASES_DIR/$NAME.meta"

    if [ ! -f "$case_sh" ]; then
        log "SKIP $NAME: test_cases/$NAME.sh not found"
        printf 'LMBENCH_JSON {"name":"%s","metric_type":"other","unit":"","bigger_is_better":false,"status":"skipped","error":"missing script"}\n' "$NAME"
        return 2
    fi

    # metadata (with defaults)
    log "loading metadata for $NAME"
    CATEGORY=$(kv_get "$meta" CATEGORY);       [ -n "$CATEGORY" ] || CATEGORY=other
    BINARY=$(kv_get "$meta" BINARY);           [ -n "$BINARY" ]   || BINARY=""
    MTYPE=$(kv_get "$meta" METRIC_TYPE);       [ -n "$MTYPE" ]    || MTYPE=other
    UNIT=$(kv_get "$meta" UNIT);               [ -n "$UNIT" ]     || UNIT=""
    BIGGER=$(kv_get "$meta" BIGGER_IS_BETTER); [ -n "$BIGGER" ]   || BIGGER=0
    PAT=$(kv_get "$meta" SEARCH_PATTERN);      [ -n "$PAT" ]      || PAT='^[0-9]'
    IDX=$(kv_get "$meta" RESULT_INDEX);        [ -n "$IDX" ]      || IDX=NF
    NTH=$(kv_get "$meta" NTH_OCCURRENCE);      [ -n "$NTH" ]      || NTH=1
    DESC=$(kv_get "$meta" DESCRIPTION)
    n_samples=$(kv_get "$meta" SAMPLES);       [ -n "$n_samples" ] || n_samples=$SAMPLES

    if ! is_positive_integer "$n_samples"; then
        printf 'LMBENCH_JSON %s,"status":"failed","error":"invalid sample count"}\n' "$(metric_head)"
        log "FAIL $NAME: invalid sample count"
        return 1
    fi
    log "RUN  $NAME (samples=$n_samples, timeout=${TIMEOUT_SEC}s)"

    samples=""
    last_out="$WORK_TMP/$NAME.out"
    last_rc=0
    valid_samples=0
    i=1
    total_iter=$((WARMUP + n_samples))
    idx_iter=0
    while [ "$idx_iter" -lt "$total_iter" ]; do
        idx_iter=$((idx_iter + 1))
        last_out="$WORK_TMP/$NAME.$idx_iter.out"
        run_with_timeout "$TIMEOUT_SEC" "$last_out" "$LMBENCH_SH" "$case_sh"
        last_rc=$?
        printf '%s\n' "$last_rc" > "$WORK_TMP/$NAME.$idx_iter.rc"
        [ "$idx_iter" -le "$WARMUP" ] && continue     # discard warmup rounds
        val=$(extract_value "$last_out")
        val=$(printf '%s' "$val" | tr -d ' \t\r')
        if [ "$last_rc" -eq 0 ] && ! lmbench_output_has_fatal "$last_out" && lmbench_is_positive_finite "$val"; then
            samples="$samples $val"
            valid_samples=$((valid_samples + 1))
        else
            log "     $NAME sample $i: no valid numeric result (rc=$last_rc)"
        fi
        i=$((i + 1))
    done

    samples=$(printf '%s' "$samples" | sed 's/^ *//')

    if [ "$valid_samples" -ne "$n_samples" ]; then
        err="incomplete samples ($valid_samples/$n_samples)"
        [ "$last_rc" -eq 124 ] && err="$err: timeout"
        raw=$(tail -c 300 "$last_out" 2>/dev/null)
        printf 'LMBENCH_JSON %s,"status":"failed","error":"%s","raw_tail":"%s"}\n' \
            "$(metric_head)" "$err" "$(json_escape "$raw")"
        log "FAIL $NAME: $err"
        return 1
    fi

    stats=$(compute_stats "$samples")
    if [ -z "$stats" ]; then
        printf 'LMBENCH_JSON %s,"status":"failed","error":"stats computation failed"}\n' "$(metric_head)"
        log "FAIL $NAME: stats computation failed"
        return 1
    fi
    set -- $stats
    s_count=$1; s_mean=$2; s_median=$3; s_sd=$4; s_min=$5; s_max=$6; s_cv=$7
    samples_json="[$(printf '%s' "$samples" | tr ' ' ',')]"
    printf 'LMBENCH_JSON %s,"status":"ok","samples":%s,"stats":{"count":%s,"mean":%s,"median":%s,"stddev":%s,"min":%s,"max":%s,"cv":%s}}\n' \
        "$(metric_head)" "$samples_json" \
        "$s_count" "$s_mean" "$s_median" "$s_sd" "$s_min" "$s_max" "$s_cv"
    log "OK   $NAME: mean=$s_mean $UNIT (n=$s_count, cv=$s_cv)"
    return 0
}

# ============================== main ==============================
run_main() {
    parse_args "$@" || return $?
    [ "$HELP_ONLY" = 1 ] && return 0

    # --config may redirect the config file before load_config reads it.
    if [ -n "$CLI_CONFIG" ]; then
        CONFIG_FILE=$CLI_CONFIG
        [ -f "$CONFIG_FILE" ] || { log "ERROR: config not found: $CONFIG_FILE"; return 2; }
    fi
    load_config
    apply_cli_overrides
    for positive in "$SAMPLES" "$TIMEOUT_SEC"; do
        is_positive_integer "$positive" || { usage; return 2; }
    done
    if [ "$WARMUP" != 0 ] && ! is_positive_integer "$WARMUP"; then usage; return 2; fi
    mkdir -p "$WORK_TMP" || return 1

    if [ "$LIST_ONLY" = "1" ]; then
        ls "$CASES_DIR"/*.meta 2>/dev/null | sed 's|.*/||; s|\.meta$||'
        return 0
    fi

    # Resolve caller-supplied relative paths before moving to the fixture CWD.
    if [ -f "$WHITELIST_FILE" ]; then
        WHITELIST_FILE=$(CDPATH='' cd -- "$(dirname -- "$WHITELIST_FILE")" && pwd)/$(basename -- "$WHITELIST_FILE")
    fi

    # --only: replace whitelist with a single-line file, reusing run_one_case
    # unchanged (no second dispatch branch).
    if [ -n "$ONLY_NAME" ]; then
        wl_tmp="$WORK_TMP/only_whitelist"
        printf '%s\n' "$ONLY_NAME" > "$wl_tmp"
        WHITELIST_FILE="$wl_tmp"
    fi

    . "$SCRIPT_DIR/env.sh"
    export LMBENCH_RUN_TMP="$WORK_TMP"
    command -v "$LMBENCH_TIMEOUT" >/dev/null 2>&1 || {
        log "ERROR: timeout command not found: $LMBENCH_TIMEOUT"; return 1;
    }
    trap 'runner_cleanup' 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

log "LMbench benchmark run starting"
echo "===LMBENCH_RUN_BEGIN==="
printf 'LMBENCH_META {"suite":"lmbench","suite_version":"%s","samples":%s,"timeout_sec":%s,"warmup":%s}\n' \
    "$SUITE_VERSION" "$SAMPLES" "$TIMEOUT_SEC" "$WARMUP"

    if [ -f "$SCRIPT_DIR/init.sh" ]; then
        log "initializing test environment (init.sh)..."
        if ! "$LMBENCH_SH" "$SCRIPT_DIR/init.sh"; then
            log "ERROR: test environment initialization failed"
            printf 'LMBENCH_SUMMARY {"total":0,"ok":0,"failed":0,"skipped":0}\n'
            echo "===LMBENCH_RUN_END==="
            perform_cleanup || true
echo "benchmark测试完成"
            exit 1
        fi
    fi

    cd "$LMBENCH_TMP_DIR" || return 1

    # Pre-compute ENOUGH once, as lmbench's scripts/config-run does. Keep the
    # timeout bounded and fall back to REAL_SHORT when calibration fails.
    if [ -z "${ENOUGH:-}" ]; then
        . "$SCRIPT_DIR/env.sh" 2>/dev/null || true
        if [ -x "${LMBENCH_BIN_DIR:-}/enough" ]; then
            if calibrated_enough=$("$LMBENCH_TIMEOUT" -k 2s 10s "${LMBENCH_BIN_DIR}/enough" 2>/dev/null); then
                case "$calibrated_enough" in
                    '')
                        ENOUGH=50000
                        enough_note='fallback: enough tool returned empty output'
                        ;;
                    *[!0-9]*)
                        ENOUGH=50000
                        enough_note='fallback: enough tool returned non-decimal output'
                        ;;
                    *[!0]*)
                        ENOUGH=$calibrated_enough
                        enough_note='calibrated by enough tool'
                        ;;
                    *)
                        ENOUGH=50000
                        enough_note='fallback: enough tool returned zero'
                        ;;
                esac
            else
                ENOUGH=50000
                enough_note='fallback: enough tool failed'
            fi
        else
            ENOUGH=50000
            enough_note='fallback: enough tool not found'
        fi
        export ENOUGH
        log "ENOUGH=$ENOUGH ($enough_note)"
    fi

if [ ! -f "$WHITELIST_FILE" ]; then
    log "ERROR: whitelist not found: $WHITELIST_FILE"
    echo "===LMBENCH_RUN_END==="
    perform_cleanup || true
echo "benchmark测试完成"
    exit 1
fi

    total=0; ok=0; failed=0; skipped=0
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|\#*) continue ;; esac
        total=$((total + 1))
        log "dispatching $line"
        run_one_case "$line"
    case $? in
        0) ok=$((ok + 1)) ;;
        2) skipped=$((skipped + 1)) ;;
        *) failed=$((failed + 1)) ;;
    esac
    echo "---"
done < "$WHITELIST_FILE"

printf 'LMBENCH_SUMMARY {"total":%s,"ok":%s,"failed":%s,"skipped":%s}\n' \
    "$total" "$ok" "$failed" "$skipped"
echo "===LMBENCH_RUN_END==="

log "done: total=$total ok=$ok failed=$failed skipped=$skipped"
perform_cleanup || true
echo "benchmark测试完成"
[ "$total" -gt 0 ] && [ "$failed" -eq 0 ] && [ "$skipped" -eq 0 ]
}

perform_cleanup() {
    if [ "$CLEANUP_DONE" -eq 0 ]; then
        CLEANUP_DONE=1
        if [ -f "$SCRIPT_DIR/clean_up.sh" ]; then
            "$LMBENCH_SH" "$SCRIPT_DIR/clean_up.sh" || CLEANUP_STATUS=$?
            [ "$CLEANUP_STATUS" -eq 0 ] || log "ERROR: cleanup failed"
        fi
    fi
    return "$CLEANUP_STATUS"
}

runner_cleanup() {
    cleanup_rc=$?
    trap - 0 HUP INT TERM
    if ! perform_cleanup && [ "$cleanup_rc" -eq 0 ]; then cleanup_rc=1; fi
    exit "$cleanup_rc"
}

# Allow sourcing for host-side unit tests without executing the run.
[ "${LMBENCH_RUNNER_NO_MAIN:-0}" = "1" ] || run_main "$@"
