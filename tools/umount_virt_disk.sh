LOOP_DEVICE=$(lsblk | grep disk_mount)
sudo umount -f ../bin/disk_mount/
sudo losetup -d /dev/${LOOP_DEVICE:2:5}
echo ${LOOP_DEVICE:2:5}