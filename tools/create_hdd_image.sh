echo "Creating virtual disk image..."
qemu-img create -f qcow2 disk.img 16M
mkfs.vfat disk.img
echo "Successfully created disk image, please move it to folder ../bin/"
