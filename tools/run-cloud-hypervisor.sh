#!/bin/bash
#
# DragonOS Cloud Hypervisor 启动脚本
#
# 使用方法:
#   sudo bash run-cloud-hypervisor.sh
#

set -e

# 配置
ARCH=${ARCH:-x86_64}
KERNEL_ELF="../../bin/kernel/kernel.elf"
DISK_IMAGE="../../bin/disk-image-${ARCH}.img"
MEMORY=${MEMORY:-2048}
CPUS=${CPUS:-2}
KERNEL_IMAGE="../../bin/kernel/kernel.elf"

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

echo_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

echo_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 检查依赖
check_dependencies() {
    echo_info "检查依赖..."

    if [ -z "$(which cloud-hypervisor)" ]; then
        echo_error "Cloud Hypervisor 未安装！"
        echo "请安装 Cloud Hypervisor:"
        echo "  cargo install cloud-hypervisor"
        exit 1
    fi

    if [ ! -e /dev/kvm ]; then
        echo_error "KVM 不可用。Cloud Hypervisor 需要 KVM 支持。"
        exit 1
    fi

    if [ ! -f "$KERNEL_IMAGE" ]; then
        echo_error "内核文件不存在: $KERNEL_IMAGE"
        echo "请先编译内核: make kernel"
        exit 1
    fi

    echo_info "依赖检查完成"
}

# 检查内核的 PVH 支持
check_pvh_support() {
    echo_info "检查内核 PVH 支持..."

    # 检查是否有 PT_NOTE program header
    if readelf -l "$KERNEL_IMAGE" 2>/dev/null | grep -q "NOTE"; then
        echo_info "✓ 找到 PT_NOTE program header"
    else
        echo_error "未找到 PT_NOTE program header，内核可能不支持 PVH"
        exit 1
    fi

    # 检查是否有 Xen ELF notes
    if readelf -n "$KERNEL_IMAGE" 2>/dev/null | grep -q "Xen.*0x00000012"; then
        echo_info "✓ 找到 XEN_ELFNOTE_PHYS32_ENTRY note"
    else
        echo_error "未找到 XEN_ELFNOTE_PHYS32_ENTRY note"
        exit 1
    fi

    # 显示 PVH 入口点地址
    ENTRY_ADDR=$(readelf -n "$KERNEL_IMAGE" 2>/dev/null | grep -A1 "0x00000012" | grep "description data" | awk '{print $3}' | head -1)
    if [ -n "$ENTRY_ADDR" ]; then
        echo_info "✓ PVH 入口点地址: 0x$ENTRY_ADDR"
    fi

    echo_info "PVH 支持检查完成"
}

# 设置网络
setup_network() {
    echo_info "配置网络..."

    # 检查是否有可用的 tap 接口
    if [ -d /sys/class/net/tap ]; then
        # 使用现有的 tap 接口
        TAP_DEV=$(ls /sys/class/net/tap/ | head -1)
        echo_info "使用现有 tap 接口: $TAP_DEV"
        NET_ARGS="--net tap=$TAP_DEV"
    else
        echo_warn "未找到 tap 接口，使用用户态网络"
        NET_ARGS=""
    fi
}

# 启动 Cloud Hypervisor
run_cloud_hypervisor() {
    echo_info "启动 Cloud Hypervisor..."
    echo_info "内核: $KERNEL_IMAGE"
    echo_info "内存: ${MEMORY}MB"
    echo_info "CPU: $CPUS"

    # Cloud Hypervisor 参数
    CH_ARGS=(
        --kernel "$KERNEL_IMAGE"
        --cpus "boot=$CPUS"
        --memory "size=${MEMORY}M"
        --serial tty
        --console off
    )

    # 如果有磁盘镜像，添加它
    if [ -f "$DISK_IMAGE" ]; then
        echo_info "磁盘镜像: $DISK_IMAGE"
        CH_ARGS+=(--disk "path=$DISK_IMAGE")
    fi

    # 添加网络参数（如果有）
    if [ -n "$NET_ARGS" ]; then
        CH_ARGS+=($NET_ARGS)
    fi

    # 打印完整命令（用于调试）
    echo "执行命令:"
    echo "cloud-hypervisor ${CH_ARGS[@]}"
    echo ""

    # 启动 Cloud Hypervisor
    sudo cloud-hypervisor "${CH_ARGS[@]}"
}

# 清理函数
cleanup() {
    echo_info "清理资源..."
    # 这里可以添加清理逻辑
}

# 设置信号处理
trap cleanup EXIT INT TERM

# 主函数
main() {
    echo "=================================="
    echo " DragonOS Cloud Hypervisor 启动器"
    echo "=================================="
    echo ""

    check_dependencies
    check_pvh_support
    setup_network

    echo ""
    echo_info "准备就绪，启动虚拟机..."
    echo ""

    run_cloud_hypervisor
}

# 执行主函数
main "$@"
