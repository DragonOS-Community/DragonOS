echo "Creating virtual disk image..."
# qemu-img create -f raw disk.img 16M
# 输入o m w即可
fdisk disk.img
LOOP_DEVICE=$(sudo losetup -f --show -P disk.img) \
    || exit 1

sudo losetup -P /dev/loop1 --show disk.img
# lsblk
echo ${LOOP_DEVICE}p1

sudo mkfs.vfat -F 32 ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image, please make a FAT32 filesystem on it and move it to folder ../bin/"
