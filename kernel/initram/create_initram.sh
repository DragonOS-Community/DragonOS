#!/bin/bash

echo "Start build initram.cpio.xz......"

if [ $# -ne 1 ]; then
    echo "仅接受一个参数: arg1(arch)"
    exit 1
fi

# Arch 使用整数区分
# 0: x86_64
# 1: riscv64
# 2: loongarch64
arch = $1

if [ "$arch" = 0 ]; then
    echo "开始构建用于 x86_64 的initram"
    x86_64_build()
else
    echo "当前架构未实现 initram build: $arch"
fi

x86_64_build() {
    check_go_env()

}

check_go_env() {
    echo "检查 Go 语言环境..."
    
    # 检查 go 命令是否存在
    if ! command -v go &> /dev/null; then
        echo "错误: 未找到 Go 语言环境, 请自行安装或使用 go_install.sh"
        exit 1
    fi
    
    # 获取 Go 版本
    go_version=$(go version | awk '{print $3}')
    echo "✓ 找到 Go 环境: $go_version"
}
