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

root_folder=$(dirname "$(pwd)")
IMG_NAME="ext4.img"
IMG_SIZE="1G"
LOOP_DEVICE=""

# 创建空白镜像
dd if=/dev/zero of="$IMG_NAME" bs=1M count=1024

# 分区表和分区
parted -s "$IMG_NAME" mklabel msdos
parted -s "$IMG_NAME" mkpart primary ext4 1MiB 100%

# 关联 loop 设备并开启分区识别
LOOP_DEVICE=$(losetup --find --partscan --show "$IMG_NAME")
PARTITION="${LOOP_DEVICE}p1"

echo "loop 设备为 $LOOP_DEVICE，分区为 $PARTITION"

# 等待分区识别
sleep 1

# 格式化为 ext4
mkfs.ext4 "$PARTITION"

# 释放 loop 设备
losetup -d "$LOOP_DEVICE"
# 清理逻辑中不再重复执行
LOOP_DEVICE=""

# 移动镜像
mv "$IMG_NAME" "$root_folder/bin/$IMG_NAME"
