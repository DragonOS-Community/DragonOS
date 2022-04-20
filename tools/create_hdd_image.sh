echo "Creating virtual disk image..."
qemu-img create -f raw disk.img 16M
fdisk disk.img
sudo losetup -P /dev/loop1 --show disk.img
lsblk
#mkfs.vfat -F 32 /dev/loop1p1
echo "Successfully created disk image, please make a FAT32 filesystem on it and move it to folder ../bin/"
