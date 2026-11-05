#!/bin/busybox sh
source /etc/profile

ensure_exec_test_fixtures() {
    # exec_test resolves /bin/echo and /bin/bash directly. The default image is
    # built on FAT32 from BusyBox, which cannot store applet symlinks, so
    # materialize those entry points when the rootfs does not provide them.
    # Existing files (for example a base image's real bash) are never replaced.
    if [ ! -x /bin/busybox ]; then
        echo "[gvisor] cannot prepare exec fixtures: /bin/busybox missing or not executable" >&2
        return 1
    fi

    for applet in echo bash; do
        if [ -e "/bin/$applet" ]; then
            if [ ! -x "/bin/$applet" ]; then
                echo "[gvisor] /bin/$applet exists but is not executable" >&2
                return 1
            fi
            continue
        fi

        if ! /bin/busybox --list | /bin/busybox grep -qx "$applet"; then
            echo "[gvisor] busybox does not provide the '$applet' applet" >&2
            return 1
        fi
        if ! /bin/busybox cp /bin/busybox "/bin/$applet"; then
            echo "[gvisor] failed to install /bin/$applet from busybox" >&2
            return 1
        fi
        if [ ! -x "/bin/$applet" ]; then
            echo "[gvisor] /bin/$applet is not executable after install" >&2
            return 1
        fi
        echo "[gvisor] installed /bin/$applet from busybox"
    done
}

adjust_readv_blocklist() {
    # Constants and variables for readv_test memory-based adjustment
    local THRESHOLD_KB=2621440  # 2.5GiB in kB
    local BLOCKLIST_FILE="${SYSCALL_TEST_DIR}/blocklists/readv_test"
    local HEAVY_TEST="ReadvTestNoFixture.TruncatedAtMax"

    if [ -z "${SYSCALL_TEST_DIR}" ]; then
        echo "[gvisor] SYSCALL_TEST_DIR not set, skip blocklist adjust"
        return 0
    fi

    if [ ! -f /proc/meminfo ]; then
        echo "[gvisor] /proc/meminfo missing, skip blocklist adjust"
        return 0
    fi

    local avail_kb=$(/bin/busybox awk '/^MemAvailable:/ {print $2; exit}' /proc/meminfo 2>/dev/null)
    if [ -z "$avail_kb" ]; then
        avail_kb=$(/bin/busybox awk '/^MemFree:/ {print $2; exit}' /proc/meminfo 2>/dev/null)
    fi
    if [ -z "$avail_kb" ]; then
        echo "[gvisor] cannot read MemAvailable, skip blocklist adjust"
        return 0
    fi

    if [ ! -f "$BLOCKLIST_FILE" ]; then
        echo "[gvisor] blocklist not found: $BLOCKLIST_FILE"
        return 0
    fi

    local unblock=0
    if [ "$avail_kb" -ge "$THRESHOLD_KB" ]; then
        echo "[gvisor] MemAvailable=${avail_kb}kB >= ${THRESHOLD_KB}kB, enable ${HEAVY_TEST}"
        unblock=1
    else
        echo "[gvisor] MemAvailable=${avail_kb}kB < ${THRESHOLD_KB}kB, keep/force ${HEAVY_TEST} blocked"
    fi

    # Normalize the entry in both directions so reusing a persisted rootfs
    # across machines with different memory sizes cannot leave the 2GiB case
    # enabled below the threshold. Rewrite through a sibling file: directing
    # awk output at $BLOCKLIST_FILE truncates the input before awk reads it.
    local tmp_file="${BLOCKLIST_FILE}.tmp.$$"
    if /bin/busybox awk -v t="$HEAVY_TEST" -v unblock="$unblock" '
        {
            line=$0
            trimmed=line
            gsub(/^[ \t]+/, "", trimmed)
            gsub(/[ \t\r]+$/, "", trimmed)
            entry=trimmed
            sub(/^#[ \t]*/, "", entry)
            if (entry == t) {
                print (unblock == 1 ? "# " t : t)
                next
            }
            print $0
        }
    ' "$BLOCKLIST_FILE" > "$tmp_file"; then
        if ! /bin/busybox mv -f "$tmp_file" "$BLOCKLIST_FILE"; then
            /bin/busybox rm -f "$tmp_file"
            echo "[gvisor] failed to replace ${BLOCKLIST_FILE}" >&2
        fi
    else
        /bin/busybox rm -f "$tmp_file"
        echo "[gvisor] failed to rewrite ${BLOCKLIST_FILE}" >&2
    fi

}

ensure_exec_test_fixtures || exit 1
adjust_readv_blocklist

cd "$SYSCALL_TEST_DIR" || exit 1
./gvisor-test-runner
