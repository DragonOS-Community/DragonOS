#!/bin/bash
set -e

root_folder=$(dirname $(pwd))

IMG_NAME="ext4.img"
IMG_SIZE="1G"
LOOP_DEVICE=""

dd if=/dev/zero of=$IMG_NAME bs=1M count=1024

parted -s $IMG_NAME mklabel msdos
parted -s $IMG_NAME mkpart primary ext4 1MiB 100%

LOOP_DEVICE=$(losetup --find --partscan --show "$IMG_NAME")
PARTITION="${LOOP_DEVICE}p1"

echo "loop 设备为 $LOOP_DEVICE，分区为 $PARTITION"

sleep 1  # 等待内核识别分区（必要）

mkfs.ext4 $PARTITION

# 卸载
losetup -d ${LOOP_DEVICE}

mv $IMG_NAME $root_folder/bin/$IMG_NAME