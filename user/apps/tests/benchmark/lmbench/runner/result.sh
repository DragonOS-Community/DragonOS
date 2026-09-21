#!/bin/sh
# Shared LMbench output validation for direct wrappers and the suite runner.

lmbench_kv_get() {
    _lm_file=$1 _lm_key=$2
    [ -f "$_lm_file" ] || return 0
    while IFS= read -r _lm_line || [ -n "$_lm_line" ]; do
        case "$_lm_line" in
            "$_lm_key="*) printf '%s\n' "${_lm_line#*=}"; return 0 ;;
        esac
    done < "$_lm_file"
}

lmbench_extract_value() {
    _lm_out=$1 _lm_pat=$2 _lm_idx=$3 _lm_nth=$4
    _lm_line=$(grep -E "$_lm_pat" "$_lm_out" 2>/dev/null | sed -n "${_lm_nth}p")
    [ -n "$_lm_line" ] || return 1
    set -- $_lm_line
    case "$_lm_idx" in
        NF) eval '_lm_value=${'$#'}' ;;
        NF-1)
            _lm_field=$(($# - 1)); [ "$_lm_field" -gt 0 ] || return 1
            eval '_lm_value=${'$_lm_field'}'
            ;;
        *[!0-9]*|'') return 1 ;;
        *) [ "$_lm_idx" -le "$#" ] || return 1; eval '_lm_value=${'"$_lm_idx"'}' ;;
    esac
    printf '%s\n' "$_lm_value"
}

lmbench_awk() {
    if command -v awk >/dev/null 2>&1; then command awk "$@"
    else /bin/busybox awk "$@"; fi
}

lmbench_is_positive_finite() {
    case "$1" in ''|*[!0-9.eE+-]*) return 1 ;; esac
    printf '%s\n' "$1" | lmbench_awk '
        /^[+-]?[0-9]+([.][0-9]+)?([eE][+-]?[0-9]+)?$/ {
            value = $1 + 0
            rendered = tolower(sprintf("%g", value))
            if (value > 0 && value == value && rendered !~ /(inf|nan)/) exit 0
        }
        { exit 1 }
    '
}

lmbench_output_has_fatal() {
    grep -Eiq '(^|[^[:alpha:]])(assert(ion)?([^[:alpha:]]|$).*fail|(error|fatal)[[:space:]]*:|error[[:space:]]+on[[:space:]][^:[:cntrl:]]+:|(send|recv)[[:space:]]+failed[[:space:]]*:|benchmark[^[:cntrl:]]*fail|segmentation fault|core dumped|function not implemented|not implemented([^[:alpha:]]|$)|enosys([^[:alpha:]]|$)|internal[[:space:]]+time[- ]?out|timed[[:space:]]+out|capture truncated)' "$1"
}

lmbench_validate_output() {
    _lm_out=$1 _lm_pat=$2 _lm_idx=$3 _lm_nth=$4
    lmbench_output_has_fatal "$_lm_out" && return 1
    _lm_value=$(lmbench_extract_value "$_lm_out" "$_lm_pat" "$_lm_idx" "$_lm_nth") || return 1
    _lm_value=$(printf '%s' "$_lm_value" | tr -d ' \t\r')
    lmbench_is_positive_finite "$_lm_value"
}

lmbench_cleanup_capture() {
    if [ -n "${LMBENCH_COMMAND_GROUP:-}" ]; then
        kill -TERM "-$LMBENCH_COMMAND_GROUP" 2>/dev/null || true
        kill -KILL "-$LMBENCH_COMMAND_GROUP" 2>/dev/null || true
    fi
    if [ -n "${LMBENCH_COMMAND_PID:-}" ]; then
        wait "$LMBENCH_COMMAND_PID" 2>/dev/null || true
    fi
    LMBENCH_COMMAND_PID=
    LMBENCH_COMMAND_GROUP=
    # With the supervised command group gone, all inherited FIFO writers are
    # closed. The pump's current dd observes EOF and is reaped with the pump.
    if [ -n "${LMBENCH_PUMP_PID:-}" ]; then
        wait "$LMBENCH_PUMP_PID" 2>/dev/null || true
    fi
    LMBENCH_PUMP_PID=
    [ -z "${LMBENCH_CAPTURE_FIFO:-}" ] || rm -f "$LMBENCH_CAPTURE_FIFO"
    [ -z "${LMBENCH_CAPTURE_FILE:-}" ] || rm -f "$LMBENCH_CAPTURE_FILE"
    [ -z "${LMBENCH_CAPTURE_CHUNK:-}" ] || rm -f "$LMBENCH_CAPTURE_CHUNK"
    LMBENCH_CAPTURE_FIFO=
    LMBENCH_CAPTURE_FILE=
    LMBENCH_CAPTURE_CHUNK=
}

lmbench_stream_capture() {
    trap - 0 HUP INT TERM
    _lm_fifo=$1 _lm_capture=$2 _lm_chunk=$3
    _lm_output_limit=1048576
    _lm_capture_data_limit=65500
    _lm_emitted=0 _lm_captured=0 _lm_total=0
    : > "$_lm_capture" || return 1
    exec 3< "$_lm_fifo" || return 1
    while :; do
        dd bs=4096 count=1 <&3 > "$_lm_chunk" 2>/dev/null || true
        _lm_bytes=$(wc -c < "$_lm_chunk")
        [ "$_lm_bytes" -gt 0 ] || break
        _lm_total=$((_lm_total + _lm_bytes))
        if [ "$_lm_emitted" -lt "$_lm_output_limit" ]; then
            _lm_room=$((_lm_output_limit - _lm_emitted))
            if [ "$_lm_bytes" -le "$_lm_room" ]; then
                cat "$_lm_chunk"
                _lm_emitted=$((_lm_emitted + _lm_bytes))
            else
                dd if="$_lm_chunk" bs=1 count="$_lm_room" 2>/dev/null
                _lm_emitted=$_lm_output_limit
            fi
            if [ "$_lm_emitted" -eq "$_lm_output_limit" ]; then
                printf '\nlmbench: output truncated after 1048576 bytes\n'
            fi
        fi
        if [ "$_lm_captured" -lt "$_lm_capture_data_limit" ]; then
            _lm_room=$((_lm_capture_data_limit - _lm_captured))
            if [ "$_lm_bytes" -le "$_lm_room" ]; then
                cat "$_lm_chunk" >> "$_lm_capture"
                _lm_captured=$((_lm_captured + _lm_bytes))
            else
                dd if="$_lm_chunk" bs=1 count="$_lm_room" 2>/dev/null >> "$_lm_capture"
                _lm_captured=$_lm_capture_data_limit
            fi
        fi
    done
    exec 3<&-
    if [ "$_lm_total" -gt "$_lm_capture_data_limit" ]; then
        printf '\nlmbench: capture truncated\n' >> "$_lm_capture"
    fi
    rm -f "$_lm_chunk"
}

run_lmbench_measurement() {
    trap 'lmbench_cleanup_capture; exit 129' HUP
    trap 'lmbench_cleanup_capture; exit 130' INT
    trap 'lmbench_cleanup_capture; exit 143' TERM
    _lm_script=${0##*/}
    _lm_meta="$SCRIPT_DIR/${_lm_script%.sh}.meta"
    _lm_pat=$(lmbench_kv_get "$_lm_meta" SEARCH_PATTERN); [ -n "$_lm_pat" ] || _lm_pat='^[0-9]'
    _lm_idx=$(lmbench_kv_get "$_lm_meta" RESULT_INDEX); [ -n "$_lm_idx" ] || _lm_idx=NF
    _lm_nth=$(lmbench_kv_get "$_lm_meta" NTH_OCCURRENCE); [ -n "$_lm_nth" ] || _lm_nth=1
    _lm_dir=${LMBENCH_RUN_TMP:-${LMBENCH_TMP_DIR:-/tmp}}
    mkdir -p "$_lm_dir" || return 1
    LMBENCH_CAPTURE_FILE="$_lm_dir/.lmbench-measurement.$$"
    LMBENCH_CAPTURE_FIFO="$_lm_dir/.lmbench-stream.$$"
    LMBENCH_CAPTURE_CHUNK="$_lm_dir/.lmbench-chunk.$$"
    rm -f "$LMBENCH_CAPTURE_FILE" "$LMBENCH_CAPTURE_FIFO" "$LMBENCH_CAPTURE_CHUNK"
    mkfifo "$LMBENCH_CAPTURE_FIFO" || { lmbench_cleanup_capture; return 1; }
    if [ -n "${LMBENCH_TIMEOUT:-}" ]; then
        _lm_supervisor=$LMBENCH_TIMEOUT
    elif [ -x /usr/local/bin/timeout ]; then
        _lm_supervisor=/usr/local/bin/timeout
    elif [ -x /usr/bin/timeout ]; then
        _lm_supervisor=/usr/bin/timeout
    else
        _lm_supervisor=timeout
    fi
    command -v "$_lm_supervisor" >/dev/null 2>&1 || { lmbench_cleanup_capture; return 1; }
    exec 4<&0 || { lmbench_cleanup_capture; return 1; }
    lmbench_stream_capture "$LMBENCH_CAPTURE_FIFO" "$LMBENCH_CAPTURE_FILE" "$LMBENCH_CAPTURE_CHUNK" &
    LMBENCH_PUMP_PID=$!
    # GNU timeout creates a process group without setsid. A zero duration adds
    # no deadline; it only gives this helper a bounded command-family target.
    "$_lm_supervisor" 0 "$@" <&4 >"$LMBENCH_CAPTURE_FIFO" 2>&1 &
    LMBENCH_COMMAND_PID=$!
    LMBENCH_COMMAND_GROUP=$LMBENCH_COMMAND_PID
    exec 4<&-
    if wait "$LMBENCH_COMMAND_PID"; then _lm_rc=0; else _lm_rc=$?; fi
    LMBENCH_COMMAND_PID=
    kill -TERM "-$LMBENCH_COMMAND_GROUP" 2>/dev/null || true
    kill -KILL "-$LMBENCH_COMMAND_GROUP" 2>/dev/null || true
    LMBENCH_COMMAND_GROUP=
    if wait "$LMBENCH_PUMP_PID"; then :; elif [ "$_lm_rc" -eq 0 ]; then _lm_rc=1; fi
    LMBENCH_PUMP_PID=
    rm -f "$LMBENCH_CAPTURE_FIFO"
    LMBENCH_CAPTURE_FIFO=
    if [ "$_lm_rc" -eq 0 ] && ! lmbench_validate_output "$LMBENCH_CAPTURE_FILE" "$_lm_pat" "$_lm_idx" "$_lm_nth"; then
        printf '%s\n' 'lmbench: invalid measurement output' >&2
        _lm_rc=1
    fi
    rm -f "$LMBENCH_CAPTURE_FILE" "$LMBENCH_CAPTURE_CHUNK"
    LMBENCH_CAPTURE_FILE=
    LMBENCH_CAPTURE_CHUNK=
    return "$_lm_rc"
}
