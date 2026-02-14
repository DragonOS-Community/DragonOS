#!/usr/bin/env bash

set -euo pipefail

if [ -z "${ROOT_PATH:-}" ]; then
    echo "[dunit-monitor] 错误: ROOT_PATH 环境变量未设置"
    exit 1
fi

if [ -z "${VMSTATE_DIR:-}" ]; then
    echo "[dunit-monitor] 错误: VMSTATE_DIR 环境变量未设置"
    exit 1
fi

SERIAL_FILE="serial_opt.txt"
BOOT_TIMEOUT=300
TEST_START_TIMEOUT=600
TEST_TIMEOUT=1800
IDLE_TIMEOUT=120

get_qemu_pid() {
    if [ -f "${VMSTATE_DIR}/pid" ]; then
        cat "${VMSTATE_DIR}/pid"
    else
        echo ""
    fi
}

cleanup() {
    local qemu_pid
    qemu_pid="$(get_qemu_pid)"

    if [ -n "$qemu_pid" ] && sudo kill -0 "$qemu_pid" 2>/dev/null; then
        echo "[dunit-monitor] 终止QEMU进程 (PID: $qemu_pid)"
        sudo kill -TERM "$qemu_pid" 2>/dev/null || true
        sleep 3
        if sudo kill -0 "$qemu_pid" 2>/dev/null; then
            echo "[dunit-monitor] 强制终止QEMU进程 (PID: $qemu_pid)"
            sudo kill -9 "$qemu_pid" 2>/dev/null || true
        fi
    fi

    rm -f "${VMSTATE_DIR}/pid"
    pkill -P $$ 2>/dev/null || true
    stty sane 2>/dev/null || true
}

check_qemu_alive() {
    local qemu_pid
    qemu_pid="$(get_qemu_pid)"
    [ -n "$qemu_pid" ] && sudo kill -0 "$qemu_pid" 2>/dev/null
}

echo "[dunit-monitor] 等待QEMU进程启动..."
qemu_pid=""
for _ in $(seq 1 30); do
    qemu_pid="$(get_qemu_pid)"
    if [ -n "$qemu_pid" ]; then
        break
    fi
    sleep 1
done

if [ -z "$qemu_pid" ]; then
    echo "[dunit-monitor] 错误: 未发现QEMU PID文件"
    exit 1
fi

if ! sudo kill -0 "$qemu_pid" 2>/dev/null; then
    echo "[dunit-monitor] 错误: QEMU进程不存在 (PID: $qemu_pid)"
    exit 1
fi

trap 'cleanup; exit 1' INT TERM

start_time="$(date +%s)"
last_output_time="$start_time"
last_line_count=0
boot_completed=false
test_started=false

echo "[dunit-monitor] 开始监控 dunitest (QEMU PID: $qemu_pid)"
echo "[dunit-monitor] 超时配置: 开机${BOOT_TIMEOUT}s, 测试启动${TEST_START_TIMEOUT}s, 总超时${TEST_TIMEOUT}s"

while true; do
    now="$(date +%s)"
    elapsed="$((now - start_time))"

    if [ "$elapsed" -gt "$TEST_TIMEOUT" ]; then
        echo "[dunit-monitor] 错误: 测试总超时 (${TEST_TIMEOUT}秒)"
        cleanup
        exit 1
    fi

    if ! check_qemu_alive; then
        echo "[dunit-monitor] 错误: QEMU进程已退出"
        cleanup
        exit 1
    fi

    serial_exists=false
    if [ -f "$SERIAL_FILE" ]; then
        serial_exists=true
        current_line_count="$(wc -l < "$SERIAL_FILE" 2>/dev/null || echo 0)"
        file_mtime="$(stat -c %Y "$SERIAL_FILE" 2>/dev/null || echo 0)"

        if [ "$current_line_count" -gt "$last_line_count" ] || [ "$file_mtime" -gt "$((now - 5))" ]; then
            last_output_time="$now"
            last_line_count="$current_line_count"
        fi
    fi

    if [ "$serial_exists" = true ] && [ "$((now - last_output_time))" -gt "$IDLE_TIMEOUT" ]; then
        echo "[dunit-monitor] 错误: 超过 ${IDLE_TIMEOUT} 秒无新输出"
        cleanup
        exit 1
    fi

    if [ "$boot_completed" = false ]; then
        if [ -f "$SERIAL_FILE" ] && grep -aq "\[rcS\] Running system init script..." "$SERIAL_FILE" 2>/dev/null; then
            boot_completed=true
            echo "[dunit-monitor] 系统启动完成，等待dunitest启动..."
        elif [ "$elapsed" -gt "$BOOT_TIMEOUT" ]; then
            echo "[dunit-monitor] 错误: 系统启动超时 (${BOOT_TIMEOUT}秒)"
            cleanup
            exit 1
        fi
    fi

    if [ "$boot_completed" = true ] && [ "$test_started" = false ]; then
        if [ -f "$SERIAL_FILE" ] && grep -aq "\[dunit\] start running tests..." "$SERIAL_FILE" 2>/dev/null; then
            test_started=true
            echo "[dunit-monitor] dunitest已启动"
        elif [ "$elapsed" -gt "$TEST_START_TIMEOUT" ]; then
            echo "[dunit-monitor] 错误: dunitest启动超时 (${TEST_START_TIMEOUT}秒)"
            cleanup
            exit 1
        fi
    fi

    if [ -f "$SERIAL_FILE" ] && grep -aq "\[dunit\] 测试完成, status=" "$SERIAL_FILE" 2>/dev/null; then
        status_line="$(grep -a "\[dunit\] 测试完成, status=" "$SERIAL_FILE" | tail -n 1 || true)"
        status_value="$(echo "$status_line" | sed -n 's/.*status=\([0-9][0-9]*\).*/\1/p')"
        if [ "$status_value" = "0" ]; then
            echo "[dunit-monitor] dunitest测试通过"
            cleanup
            exit 0
        fi
        echo "[dunit-monitor] dunitest测试失败, status=${status_value:-unknown}"
        cleanup
        exit 1
    fi

    sleep 10
done
