echo "Creating virtual disk image..."

# 创建一至少为32MB磁盘镜像（类型选择raw）
qemu-img create -f raw disk.img 32M

# 使用fdisk把disk.img的分区表设置为MBR格式(下方的空行请勿删除)
fdisk disk.img << EOF
o
n




w
EOF

LOOP_DEVICE=$(sudo losetup -f --show -P disk.img) \
    || exit 1
echo ${LOOP_DEVICE}p1
sudo mkfs.vfat -F 32 ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image."
mkdir -p ../bin
mv ./disk.img ../bin/
