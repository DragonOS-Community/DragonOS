#!/bin/bash
set -euo pipefail

# 自动清理的 trap，确保中途出错能卸载 loop 设备
cleanup() {
    if [[ -n "${LOOP_DEVICE:-}" ]]; then
        echo "清理：释放 loop 设备 $LOOP_DEVICE"
        losetup -d "$LOOP_DEVICE" || echo "警告：无法释放 $LOOP_DEVICE"
    fi
}
trap cleanup EXIT

# 镜像文件保存位置
root_folder=$(dirname "$(pwd)")

# 镜像名称和大小
IMG_NAME="fat.img"
IMG_SIZE="64M"  # FAT 不需要太大空间
LOOP_DEVICE=""

# 创建空白镜像
dd if=/dev/zero of="$IMG_NAME" bs=1M count=64

# 创建分区表和分区（使用 FAT32 格式）
parted -s "$IMG_NAME" mklabel msdos
parted -s "$IMG_NAME" mkpart primary fat32 1MiB 100%

# 关联 loop 设备并启用分区扫描
LOOP_DEVICE=$(losetup --find --partscan --show "$IMG_NAME")
PARTITION="${LOOP_DEVICE}p1"

echo "loop 设备为 $LOOP_DEVICE，分区为 $PARTITION"

# 等待内核识别分区
sleep 1

# 格式化为 FAT32
mkfs.vfat -F 32 "$PARTITION"

# 释放 loop 设备
losetup -d "$LOOP_DEVICE"
LOOP_DEVICE=""

# 移动镜像文件
mv "$IMG_NAME" "$root_folder/bin/$IMG_NAME"
