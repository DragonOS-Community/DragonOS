#!/bin/busybox sh
# Host-side monitor for the LMbench benchmark run.
#
# Boots are driven by the Makefile `test-benchmark` target, which starts QEMU in
# the background (serial -> serial_opt.txt, PID -> $VMSTATE_DIR/pid) and then runs
# this script. We poll the serial log for boot / run-begin / progress / completion
# and enforce timeouts, then tear QEMU down. Unlike the syscall suite this is a
# performance run, so individual metric failures are DATA, not harness failures:
# we exit 0 as long as the run reaches "benchmark测试完成".

if [ -z "${ROOT_PATH}" ]; then
    echo "[monitor] 错误: ROOT_PATH 未设置(请通过 Makefile 运行)"; exit 1
fi
if [ -z "${VMSTATE_DIR}" ]; then
    echo "[monitor] 错误: VMSTATE_DIR 未设置(请通过 Makefile 运行)"; exit 1
fi

SERIAL_FILE="${SERIAL_FILE:-serial_opt.txt}"
BOOT_TIMEOUT="${BENCH_BOOT_TIMEOUT:-300}"       # DragonOS boot
RUN_START_TIMEOUT="${BENCH_RUN_START_TIMEOUT:-600}"  # until run begins
TOTAL_TIMEOUT="${BENCH_TOTAL_TIMEOUT:-3600}"    # whole run (benchmarks are slow)
IDLE_TIMEOUT="${BENCH_IDLE_TIMEOUT:-600}"       # no serial output

get_qemu_pid() { [ -f "${VMSTATE_DIR}/pid" ] && cat "${VMSTATE_DIR}/pid" || echo ""; }

clean_up() {
    echo "[monitor] 清理资源..."
    if [ -f "${VMSTATE_DIR}/pid" ]; then
        QEMU_PID=$(cat "${VMSTATE_DIR}/pid")
        if [ -n "$QEMU_PID" ] && sudo kill -0 "$QEMU_PID" 2>/dev/null; then
            echo "[monitor] 终止 QEMU (PID: $QEMU_PID)"
            sudo kill -TERM "$QEMU_PID" 2>/dev/null
            sleep 3
            sudo kill -0 "$QEMU_PID" 2>/dev/null && sudo kill -9 "$QEMU_PID" 2>/dev/null
        fi
        rm -f "${VMSTATE_DIR}/pid"
    fi
    pkill -P $$ 2>/dev/null
    stty sane 2>/dev/null
}

show_diag() {
    echo "[monitor] ===== 诊断 ====="
    echo "[monitor] 已运行: $(($(date +%s) - START_TIME))s  boot=$BOOT_DONE run=$RUN_STARTED"
    if [ -f "$SERIAL_FILE" ]; then
        echo "[monitor] 已产出指标: $(grep -ac 'LMBENCH_JSON ' "$SERIAL_FILE" 2>/dev/null)"
        echo "[monitor] 串口尾部:"; tail -n 15 "$SERIAL_FILE" 2>/dev/null | sed 's/^/[monitor]   /'
    fi
    echo "[monitor] ================"
}

check_qemu_alive() {
    p=$(get_qemu_pid); [ -n "$p" ] && sudo kill -0 "$p" 2>/dev/null
}
check_boot_done() {
    [ -f "$SERIAL_FILE" ] && ( grep -aq "\[rcS\] Running system init script" "$SERIAL_FILE" 2>/dev/null || \
                               grep -aq "===LMBENCH_RUN_BEGIN===" "$SERIAL_FILE" 2>/dev/null )
}
check_run_started() {
    [ -f "$SERIAL_FILE" ] && ( grep -aq "===LMBENCH_RUN_BEGIN===" "$SERIAL_FILE" 2>/dev/null || \
                               grep -aq "LMBENCH_JSON " "$SERIAL_FILE" 2>/dev/null )
}
check_done() {
    [ -f "$SERIAL_FILE" ] && grep -aq "benchmark测试完成" "$SERIAL_FILE" 2>/dev/null
}

# ---- wait for QEMU PID ----
echo "[monitor] 等待 QEMU 写入 PID 文件..."
WAITED=0
QEMU_PID=$(get_qemu_pid)
while [ -z "$QEMU_PID" ] && [ $WAITED -lt 30 ]; do
    sleep 1; WAITED=$((WAITED + 1)); QEMU_PID=$(get_qemu_pid)
done
if [ -z "$QEMU_PID" ]; then echo "[monitor] 错误: 超时未找到 PID 文件"; exit 1; fi
if ! sudo kill -0 "$QEMU_PID" 2>/dev/null; then echo "[monitor] 错误: QEMU (PID $QEMU_PID) 不存在"; exit 1; fi
echo "[monitor] QEMU PID: $QEMU_PID"

START_TIME=$(date +%s)
LAST_OUTPUT_TIME=$START_TIME
LAST_LINES=0
BOOT_DONE=false
RUN_STARTED=false

trap 'clean_up; exit 1' INT TERM

echo "[monitor] 开始监控 (boot=${BOOT_TIMEOUT}s run_start=${RUN_START_TIMEOUT}s total=${TOTAL_TIMEOUT}s idle=${IDLE_TIMEOUT}s)"
while true; do
    NOW=$(date +%s); ELAPSED=$((NOW - START_TIME))

    if [ "$ELAPSED" -gt "$TOTAL_TIMEOUT" ]; then
        echo "[monitor] 错误: 总超时 (${TOTAL_TIMEOUT}s)"; show_diag; clean_up; exit 1
    fi
    if ! check_qemu_alive; then
        # QEMU may have exited right after printing completion; re-check done.
        if check_done; then break; fi
        echo "[monitor] 错误: QEMU 进程已退出"; show_diag; clean_up; exit 1
    fi

    if [ "$BOOT_DONE" = false ]; then
        if check_boot_done; then BOOT_DONE=true; echo "[monitor] 系统已启动"; LAST_OUTPUT_TIME=$NOW
        elif [ "$ELAPSED" -gt "$BOOT_TIMEOUT" ]; then
            echo "[monitor] 错误: 启动超时 (${BOOT_TIMEOUT}s)"; show_diag; clean_up; exit 1
        fi
    fi
    if [ "$BOOT_DONE" = true ] && [ "$RUN_STARTED" = false ]; then
        if check_run_started; then RUN_STARTED=true; echo "[monitor] 基准运行已开始"; LAST_OUTPUT_TIME=$NOW
        elif [ "$ELAPSED" -gt "$RUN_START_TIMEOUT" ]; then
            echo "[monitor] 错误: 基准程序启动超时 (${RUN_START_TIMEOUT}s)"; show_diag; clean_up; exit 1
        fi
    fi

    # idle detection
    if [ -f "$SERIAL_FILE" ]; then
        CUR_LINES=$(wc -l < "$SERIAL_FILE" 2>/dev/null || echo 0)
        MTIME=$(stat -c %Y "$SERIAL_FILE" 2>/dev/null || echo 0)
        if [ "$CUR_LINES" -gt "$LAST_LINES" ] || [ "$MTIME" -gt "$((NOW - 5))" ]; then
            LAST_OUTPUT_TIME=$NOW; LAST_LINES=$CUR_LINES
        elif [ "$((NOW - LAST_OUTPUT_TIME))" -gt "$IDLE_TIMEOUT" ]; then
            echo "[monitor] 错误: ${IDLE_TIMEOUT}s 无输出,疑似卡死"; show_diag; clean_up; exit 1
        fi
    fi

    if check_done; then echo "[monitor] 检测到 benchmark测试完成"; break; fi

    if [ $((ELAPSED % 60)) -eq 0 ]; then
        n=$(grep -ac 'LMBENCH_JSON ' "$SERIAL_FILE" 2>/dev/null || echo 0)
        echo "[monitor] 运行中... (${ELAPSED}s, 已产出 ${n} 个指标)"
    fi
    sleep 10
done

# summary from serial (informational; does not gate exit)
SUMMARY=$(grep -a 'LMBENCH_SUMMARY ' "$SERIAL_FILE" 2>/dev/null | tail -n1)
echo "[monitor] 运行完成,用时 $(($(date +%s) - START_TIME))s"
[ -n "$SUMMARY" ] && echo "[monitor] ${SUMMARY}"
clean_up
stty sane 2>/dev/null
echo "[monitor] OK: 基准运行结束,结果待 collect_results.py 收集"
exit 0
