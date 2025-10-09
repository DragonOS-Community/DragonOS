#!/bin/busybox sh

# 串口文件路径
SERIAL_FILE="serial_opt.txt"
# qemu进程PID
QEMU_PID=${QEMU_PID}
# 清空串口输出日志文件
> "$SERIAL_FILE"
# 资源清理函数
clean_up() {
    kill -9 $QEMU_PID
    stty sane 2>/dev/null
}

# 每隔10s查看qemu串口输出日志文件serial_opt.txt后100行是否包含“测试完成”
while true; do
    sleep 10
    tail -n 100 "$SERIAL_FILE" | grep -a "测试完成" && break
done

# 提取成功率
success_rate=$(grep -a "成功率" "$SERIAL_FILE" | awk -F'[:%]' '{gsub(/ /,""); print $2}')

# 比较是否等于100
if [ "$success_rate" = "100.00" ]; then
    echo "syscall测试通过, 成功率为 ${success_rate}%"
    clean_up
    stty sane 2>/dev/null
    exit 0
else
    echo "syscall测试失败, 成功率为 ${success_rate}%"
    clean_up
    exit 1
fi