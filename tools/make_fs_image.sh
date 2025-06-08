#!/bin/bash
set -euo pipefail

# 使用方法
usage() {
    echo "用法: $0 [ext4|fat]"
    exit 1
}

# 解析参数
FS_TYPE="${1:-ext4}"
case "$FS_TYPE" in
    ext4)
        IMG_NAME="ext4.img"
        IMG_SIZE="1G"
        MKFS_CMD="mkfs.ext4"
        ;;
    fat)
        IMG_NAME="fat.img"
        IMG_SIZE="64M"
        ;;
    *)
        echo "错误: 不支持的文件系统类型 '$FS_TYPE'"
        usage
        ;;
esac

# 设置变量
root_folder=$(dirname "$(pwd)")
LOOP_DEVICE=""

# 自动清理 trap
cleanup() {
    if [[ -n "${LOOP_DEVICE:-}" ]]; then
        echo "清理：释放 loop 设备 $LOOP_DEVICE"
        losetup -d "$LOOP_DEVICE" || echo "警告：无法释放 $LOOP_DEVICE"
    fi
}
trap cleanup EXIT

# 创建空白镜像
echo "创建镜像 $IMG_NAME 大小 $IMG_SIZE"
dd if=/dev/zero of="$IMG_NAME" bs=1M count=$(( $(echo "$IMG_SIZE" | sed 's/M/*1/;s/G/*1024/') )) status=progress

if [[ "$FS_TYPE" == "ext4" ]]; then
    LOOP_DEVICE=$(losetup --find --show "$IMG_NAME")
    echo "loop 设备为 $LOOP_DEVICE"

    echo "格式化为 ext4..."
    $MKFS_CMD "$LOOP_DEVICE"

    losetup -d "$LOOP_DEVICE"
    LOOP_DEVICE=""
elif [[ "$FS_TYPE" == "fat" ]]; then
    # fat 采用带分区表的方式
    # 创建 msdos 分区表和一个 fat32 主分区，起始1MiB到100%
    parted -s "$IMG_NAME" mklabel msdos
    parted -s "$IMG_NAME" mkpart primary fat32 1MiB 100%

    # 关联 loop 设备并启用分区扫描
    LOOP_DEVICE=$(losetup --find --partscan --show "$IMG_NAME")
    PARTITION="${LOOP_DEVICE}p1"

    echo "loop 设备为 $LOOP_DEVICE，分区为 $PARTITION"
    sleep 1  # 等待内核识别分区

    echo "格式化为 fat32..."
    mkfs.vfat -F 32 "$PARTITION"

    losetup -d "$LOOP_DEVICE"
    LOOP_DEVICE=""
fi

# 移动镜像
mv "$IMG_NAME" "$root_folder/bin/$IMG_NAME"
echo "镜像已保存到 $root_folder/bin/$IMG_NAME"
