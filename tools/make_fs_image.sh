#!/bin/bash
set -euo pipefail

# 检查是否以 root 权限运行
if [[ $EUID -ne 0 ]]; then
    echo "错误：此脚本必须以 root 权限运行！"
    exit 1
fi


# 获取项目根目录（无论从哪里调用脚本）
root_folder="$(cd "$(dirname "$0")/.." && pwd)"
echo "项目根目录：$root_folder"
mkdir -p "$root_folder/bin"

LOOP_DEVICE=""

# 自动清理 trap
cleanup() {
    if [[ -n "${LOOP_DEVICE:-}" ]]; then
        echo "清理：释放 loop 设备 $LOOP_DEVICE"
        losetup -d "$LOOP_DEVICE" || echo "警告：无法释放 $LOOP_DEVICE"
    fi
}
trap cleanup EXIT

# 创建 ext4 镜像
EXT4_IMG="ext4.img"
EXT4_SIZE="1G"
echo "创建 ext4 镜像 $EXT4_IMG 大小 $EXT4_SIZE"
dd if=/dev/zero of="$EXT4_IMG" bs=1M count=1024 status=progress

LOOP_DEVICE=$(losetup --find --show "$EXT4_IMG")
echo "loop 设备为 $LOOP_DEVICE"
echo "格式化为 ext4..."
mkfs.ext4 "$LOOP_DEVICE"
losetup -d "$LOOP_DEVICE"
LOOP_DEVICE=""

mv "$EXT4_IMG" "$root_folder/bin/$EXT4_IMG"
echo "ext4 镜像已保存到 $root_folder/bin/$EXT4_IMG"

# 创建 fat 镜像
FAT_IMG="fat.img"
FAT_SIZE="64M"
echo "创建 fat 镜像 $FAT_IMG 大小 $FAT_SIZE"
dd if=/dev/zero of="$FAT_IMG" bs=1M count=64 status=progress

# 创建分区表和分区
parted -s "$FAT_IMG" mklabel msdos
parted -s "$FAT_IMG" mkpart primary fat32 1MiB 100%

LOOP_DEVICE=$(losetup --find --partscan --show "$FAT_IMG")
PARTITION="${LOOP_DEVICE}p1"
echo "loop 设备为 $LOOP_DEVICE，分区为 $PARTITION"
sleep 1  # 等待内核识别分区

echo "格式化为 fat32..."
mkfs.vfat -F 32 "$PARTITION"
losetup -d "$LOOP_DEVICE"
LOOP_DEVICE=""

mv "$FAT_IMG" "$root_folder/bin/$FAT_IMG"
echo "fat 镜像已保存到 $root_folder/bin/$FAT_IMG"
