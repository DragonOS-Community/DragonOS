#!/bin/bash

# 启动qemu并在后台运行，将输出重定向到文件描述符3
exec 3< <(bash ./ci-start-${ARCH}.sh 2>&1)

# 读取qemu的输出，直到检测到错误字段
while read -u 3 -r line; do
    # 打印输出到控制台
    echo "$line"
    # 检查输出中是否包含指定的错误字段
    if [[ "$line" == *"Hello, World!"* ]]; then
        echo "启动成功！"
        kill $(ps aux | grep "qemu-system-${ARCH}" | grep -v grep | awk "{print \$2}")
        exit 0
    fi
done
