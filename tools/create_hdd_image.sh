echo "Creating virtual disk image..."
qemu-img create -f raw disk.img 16M
# 分别输入o、n, 然后按4次回车，直到回到fdisk的默认界面，
# 再输入w即可
# 按顺序输入，并且，每次输入完成后要按下回车）
fdisk disk.img

LOOP_DEVICE=$(sudo losetup -f --show -P disk.img) \
    || exit 1
echo ${LOOP_DEVICE}p1
sudo mkfs.vfat -F 32 ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image."
mkdir -p ../bin
mv ./disk.img ../bin/
