echo "Creating virtual disk image..."
qemu-img create -f raw disk.img 16M
mkfs.vfat -f 32 disk.img
echo "Successfully created disk image, please move it to folder ../bin/"
