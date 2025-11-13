#!/bin/busybox sh

# 串口文件路径
SERIAL_FILE="serial_opt.txt"
# qemu进程PID
QEMU_PID=${QEMU_PID}
# 超时配置（秒）
BOOT_TIMEOUT=300        # DragonOS开机超时（5分钟）
TEST_START_TIMEOUT=600  # 测试程序启动超时（10分钟）
TEST_TIMEOUT=1800       # 整个测试超时（30分钟）
IDLE_TIMEOUT=120        # 无输出超时（5分钟）
SINGLE_TEST_TIMEOUT=60 # 单个测试用例超时（1分钟）

# QEMU进程查找条件
QEMU_SEARCH_PATTERN=${QEMU_SEARCH_PATTERN:-"qemu-system-x86_64.*AUTO_TEST=syscall"}

# 如果QEMU_PID未设置，尝试查找QEMU进程
if [ -z "$QEMU_PID" ]; then
    echo "[监控] QEMU_PID未设置，正在查找QEMU进程..."
    echo "[监控] 查找模式: $QEMU_SEARCH_PATTERN"
    # 等待一下确保QEMU已经启动
    sleep 2
    # 查找真正的qemu进程，排除shell包装器和pgrep自身
    # 使用ps和grep来精确匹配qemu可执行文件名
    QEMU_PID=$(ps aux | grep -E "$QEMU_SEARCH_PATTERN" | grep -v "sh -c" | grep -v "grep" | awk '{print $2}' | head -n 1)

    if [ -n "$QEMU_PID" ]; then
        echo "[监控] 找到QEMU进程 (PID: $QEMU_PID)"
    else
        echo "[监控] 警告: 未找到匹配的QEMU进程"
        exit 1
    fi
else
    echo "[监控] 使用指定的QEMU进程PID: $QEMU_PID"
fi

# 记录开始时间
START_TIME=$(date +%s)
LAST_OUTPUT_TIME=$START_TIME
LAST_LINE_COUNT=0
LAST_TEST_TIME=$START_TIME
TEST_STARTED=false
CURRENT_TEST_NAME=""

# 资源清理函数
clean_up() {
    echo "[监控] 正在清理资源..."
    if [ -n "$QEMU_PID" ]; then
        if kill -0 $QEMU_PID 2>/dev/null; then
            echo "[监控] 终止QEMU进程 (PID: $QEMU_PID)"
            # 首先尝试优雅终止
            kill -TERM $QEMU_PID 2>/dev/null
            sleep 3

            # 如果进程还在，检查是否有子进程需要一起杀死
            if kill -0 $QEMU_PID 2>/dev/null; then
                # 查找并杀死所有相关子进程
                CHILD_PIDS=$(pstree -p $QEMU_PID | grep -o '([0-9]\+)' | grep -o '[0-9]\+' | tr '\n' ' ')
                for child_pid in $CHILD_PIDS; do
                    if [ "$child_pid" != "$QEMU_PID" ] && kill -0 $child_pid 2>/dev/null; then
                        echo "[监控] 终止子进程 (PID: $child_pid)"
                        kill -TERM $child_pid 2>/dev/null
                    fi
                done
                sleep 2

                # 强制杀死主进程
                kill -0 $QEMU_PID 2>/dev/null && kill -9 $QEMU_PID 2>/dev/null
                # 再次强制杀死所有子进程
                for child_pid in $CHILD_PIDS; do
                    if [ "$child_pid" != "$QEMU_PID" ] && kill -0 $child_pid 2>/dev/null; then
                        kill -9 $child_pid 2>/dev/null
                    fi
                done
            fi
        else
            echo "[监控] QEMU进程 (PID: $QEMU_PID) 已经退出"
        fi
    else
        # 如果没有特定的PID，尝试终止所有匹配的QEMU进程
        echo "[监控] 查找并终止匹配的QEMU进程..."
        echo "[监控] 查找模式: $QEMU_SEARCH_PATTERN"
        # 使用pkill先杀主进程
        pkill -f "$QEMU_SEARCH_PATTERN" 2>/dev/null
        sleep 2
        # 再次查找并强制杀死任何残留的进程
        pkill -9 -f "$QEMU_SEARCH_PATTERN" 2>/dev/null
    fi

    # 额外清理：查找并杀死所有相关的qemu进程
    echo "[监控] 清理所有残留的QEMU相关进程..."
    # 查找所有匹配的qemu进程
    REMAINING_PIDS=$(ps aux | grep -E "$QEMU_SEARCH_PATTERN" | grep -v "grep" | awk '{print $2}')
    for pid in $REMAINING_PIDS; do
        echo "[监控] 终止残留QEMU进程 (PID: $pid)"
        kill -TERM $pid 2>/dev/null
    done
    sleep 2
    # 强制杀死残留进程
    for pid in $REMAINING_PIDS; do
        if kill -0 $pid 2>/dev/null; then
            echo "[监控] 强制终止残留QEMU进程 (PID: $pid)"
            kill -9 $pid 2>/dev/null
        fi
    done

    # 清理所有可能的子进程
    pkill -P $$ 2>/dev/null
    stty sane 2>/dev/null
}

# 显示详细的诊断信息
show_diagnostic_info() {
    echo "[监控] ========== 诊断信息 =========="
    echo "[监控] 当前时间: $(date)"
    echo "[监控] 已运行时间: $(($(date +%s) - START_TIME)) 秒"
    echo "[监控] 系统启动状态: $BOOT_COMPLETED"
    echo "[监控] 测试启动状态: $TEST_STARTED"

    if [ "$TEST_STARTED" = true ] && [ -f "$SERIAL_FILE" ]; then
        local current_test=$(tail -n 20 "$SERIAL_FILE" 2>/dev/null | grep -a "\[ RUN      \]" | tail -n 1 | sed 's/.*\[ RUN      \] //' || echo "无")
        local last_passed=$(tail -n 100 "$SERIAL_FILE" 2>/dev/null | grep -a "\[  PASSED  \]" | tail -n 1 | sed 's/.*\[  PASSED  \] *\([0-9]*\).*/\1/' || echo "0")
        echo "[监控] 当前测试: $current_test"
        echo "[监控] 已完成测试数: $last_passed"
    fi

    echo "[监控] 串口文件大小: $(du -h "$SERIAL_FILE" 2>/dev/null | cut -f1 || echo "未知")"
    echo "[监控] 最近100行输出:"
    tail -n 100 "$SERIAL_FILE" 2>/dev/null | sed 's/^/  /'
    echo "[监控] ================================"
}

# 检查QEMU进程是否还在运行
check_qemu_alive() {
    if [ -n "$QEMU_PID" ]; then
        # 使用ps检查进程是否存在，避免PID重用问题
        ps aux | grep -E "[q]emu-system-x86_64" | awk '{print $2}' | grep -q "^$QEMU_PID$"
    else
        # 如果没有特定的PID，检查是否有匹配的QEMU进程在运行
        ps aux | grep -E "$QEMU_SEARCH_PATTERN" | grep -v "sh -c" | grep -v "grep" | grep -q .
    fi
}

# 检查系统是否已启动
check_boot_complete() {
    [ -f "$SERIAL_FILE" ] && (grep -aq "[rcS] Running system init script..." "$SERIAL_FILE" 2>/dev/null || \
                             grep -aq "开始运行gvisor系统调用测试" "$SERIAL_FILE" 2>/dev/null)
}

# 检查测试是否已开始执行
check_test_started() {
    [ -f "$SERIAL_FILE" ] && (grep -aq "\[DEBUG\] 开始运行测试:" "$SERIAL_FILE" 2>/dev/null || \
                             grep -aq "\[TEST\]" "$SERIAL_FILE" 2>/dev/null || \
                             grep -aq "开始运行gvisor系统调用测试" "$SERIAL_FILE" 2>/dev/null || \
                             grep -aq "Running.*tests from.*test suites" "$SERIAL_FILE" 2>/dev/null)
}

# 检查单个测试用例是否长时间未完成
check_single_test_progress() {
    if [ -f "$SERIAL_FILE" ] && [ "$TEST_STARTED" = true ]; then
        CURRENT_TIME=$(date +%s)

        # 获取最后一个RUN开始的测试
        LAST_RUN_LINE=$(grep -a "\[ RUN      \]" "$SERIAL_FILE" | tail -n 1)
        if [ -n "$LAST_RUN_LINE" ]; then
            TEST_NAME=$(echo "$LAST_RUN_LINE" | sed 's/.*\[ RUN      \] //')

            # 检查这个测试是否已经完成（有OK或FAILED）
            # 从测试开始行开始查找，避免误判
            RUN_LINE_NUM=$(grep -an "\[ RUN      \].*$TEST_NAME" "$SERIAL_FILE" | tail -n 1 | cut -d: -f1)
            if [ -n "$RUN_LINE_NUM" ]; then
                # 从测试开始行之后查找结果
                if tail -n +$((RUN_LINE_NUM + 1)) "$SERIAL_FILE" | grep -aq "\[       OK \] $TEST_NAME\|\[  FAILED  \] $TEST_NAME"; then
                    # 测试已完成，更新时间戳
                    LAST_TEST_TIME=$CURRENT_TIME
                    return 0
                else
                    # 获取测试开始的时间戳
                    # 如果这是同一个测试，继续使用原来的开始时间
                    if [ "$TEST_NAME" != "$CURRENT_TEST_NAME" ]; then
                        # 新测试开始，重置时间戳
                        CURRENT_TEST_NAME="$TEST_NAME"
                        LAST_TEST_TIME=$CURRENT_TIME
                    else
                        # 同一个测试还在运行，检查是否超时
                        if [ "$((CURRENT_TIME - LAST_TEST_TIME))" -gt "$SINGLE_TEST_TIMEOUT" ]; then
                            echo "[监控] 错误: 单个测试用例执行超时 (${SINGLE_TEST_TIMEOUT}秒)"
                            echo "[监控] 卡住的测试: $TEST_NAME"
                            echo "[监控] 已运行时间: $((CURRENT_TIME - LAST_TEST_TIME)) 秒"
                            return 1
                        fi
                    fi
                fi
            fi
        else
            # 没有正在运行的测试，更新时间戳
            LAST_TEST_TIME=$CURRENT_TIME
            CURRENT_TEST_NAME=""
        fi
    fi
    return 0
}

# 检查是否有新输出
check_activity() {
    if [ -f "$SERIAL_FILE" ]; then
        CURRENT_TIME=$(date +%s)
        CURRENT_LINE_COUNT=$(wc -l < "$SERIAL_FILE" 2>/dev/null || echo 0)
        FILE_MTIME=$(stat -c %Y "$SERIAL_FILE" 2>/dev/null || echo 0)

        # 如果文件有新内容或最近有更新
        if [ "$CURRENT_LINE_COUNT" -gt "$LAST_LINE_COUNT" ] || \
           [ "$FILE_MTIME" -gt "$((CURRENT_TIME - 5))" ]; then
            LAST_OUTPUT_TIME=$CURRENT_TIME
            LAST_LINE_COUNT=$CURRENT_LINE_COUNT
            return 0
        fi

        # 检查是否超过空闲超时
        if [ "$((CURRENT_TIME - LAST_OUTPUT_TIME))" -gt "$IDLE_TIMEOUT" ]; then
            echo "[监控] 错误: 超过 ${IDLE_TIMEOUT} 秒无新输出"
            return 1
        fi
    fi
    return 0
}

# 主监控循环
BOOT_COMPLETED=false
echo "[监控] 开始监控syscall测试 (QEMU PID: $QEMU_PID)"
echo "[监控] 超时配置: 开机${BOOT_TIMEOUT}s, 测试启动${TEST_START_TIMEOUT}s, 单测试${SINGLE_TEST_TIMEOUT}s, 总超时${TEST_TIMEOUT}s"

# 设置信号处理，确保Ctrl+C能正确清理
trap 'clean_up; exit 1' INT TERM

while true; do
    CURRENT_TIME=$(date +%s)
    ELAPSED=$((CURRENT_TIME - START_TIME))

    # 检查总超时
    if [ "$ELAPSED" -gt "$TEST_TIMEOUT" ]; then
        echo "[监控] 错误: 测试总超时 (${TEST_TIMEOUT}秒)"
        clean_up
        exit 1
    fi

    # 检查QEMU进程
    if ! check_qemu_alive; then
        echo "[监控] 错误: QEMU进程已退出"
        clean_up
        exit 1
    fi

    # 检查启动状态
    if [ "$BOOT_COMPLETED" = false ]; then
        if check_boot_complete; then
            BOOT_COMPLETED=true
            echo "[监控] 系统启动完成，等待测试程序启动..."
            LAST_OUTPUT_TIME=$CURRENT_TIME
        elif [ "$ELAPSED" -gt "$BOOT_TIMEOUT" ]; then
            echo "[监控] 错误: 系统启动超时 (${BOOT_TIMEOUT}秒)"
            echo "[监控] 可能的原因: 内核panic、驱动问题或硬件初始化失败"
            show_diagnostic_info
            clean_up
            exit 1
        fi
    fi

    # 检查测试程序是否已启动
    if [ "$BOOT_COMPLETED" = true ] && [ "$TEST_STARTED" = false ]; then
        if check_test_started; then
            TEST_STARTED=true
            echo "[监控] 测试程序已启动，开始执行测试用例"
            LAST_OUTPUT_TIME=$CURRENT_TIME
            LAST_TEST_TIME=$CURRENT_TIME
        elif [ "$ELAPSED" -gt "$TEST_START_TIMEOUT" ]; then
            echo "[监控] 错误: 测试程序启动超时 (${TEST_START_TIMEOUT}秒)"
            echo "[监控] 系统已启动但测试程序未能开始执行"
            echo "[监控] 可能的原因: 自动测试脚本未执行、测试文件缺失或权限问题"
            show_diagnostic_info
            clean_up
            exit 1
        fi
    fi

    # 检查活动状态
    if ! check_activity; then
        echo "[监控] 错误: 系统长时间无响应，可能卡死"
        show_diagnostic_info
        clean_up
        exit 1
    fi

    # 检查单个测试用例进度
    if [ "$TEST_STARTED" = true ]; then
        if ! check_single_test_progress; then
            show_diagnostic_info
            clean_up
            exit 1
        fi
    fi

    # 检查测试完成
    if tail -n 100 "$SERIAL_FILE" | grep -a "测试完成" >/dev/null 2>&1; then
        echo "[监控] 检测到测试完成"
        break
    fi

    # 每60秒报告一次进度
    if [ $((ELAPSED % 60)) -eq 0 ]; then
        if [ "$BOOT_COMPLETED" = false ]; then
            echo "[监控] 等待系统启动... (已运行 ${ELAPSED}s)"
        elif [ "$TEST_STARTED" = false ]; then
            echo "[监控] 等待测试程序启动... (已运行 ${ELAPSED}s)"
        else
            # 显示当前测试进度
            CURRENT_TEST=$(tail -n 50 "$SERIAL_FILE" 2>/dev/null | grep -a "\[ RUN      \]" | tail -n 1 | sed 's/.*\[ RUN      \] //' || echo "未知")
            PASSED_COUNT=$(tail -n 200 "$SERIAL_FILE" 2>/dev/null | grep -a "\[  PASSED  \]" | tail -n 1 | sed 's/.*\[  PASSED  \] *\([0-9]*\).*/\1/' || echo "0")
            echo "[监控] 测试进行中... (已运行 ${ELAPSED}s, 当前测试: ${CURRENT_TEST})"
        fi
    fi

    sleep 10
done

# 提取成功率
success_rate=$(grep -a "成功率" "$SERIAL_FILE" | awk -F'[:%]' '{gsub(/ /,""); print $2}')

# 比较是否等于100
if [ "$success_rate" = "100.00" ]; then
    echo "syscall测试通过, 成功率为 ${success_rate}%"
    echo "[监控] 测试成功完成，总用时: $(($(date +%s) - START_TIME)) 秒"
    clean_up
    stty sane 2>/dev/null
    exit 0
else
    echo "syscall测试失败, 成功率为 ${success_rate}%"
    echo "[监控] 测试未完全通过，总用时: $(($(date +%s) - START_TIME)) 秒"
    clean_up
    exit 1
fi
