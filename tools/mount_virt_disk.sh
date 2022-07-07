LOOP_DEVICE=$(sudo losetup -f --show -P ../bin/disk.img) \
    || exit 1

echo ${LOOP_DEVICE}p1

mkdir -p ../bin/disk_mount/
sudo mount ${LOOP_DEVICE}p1 ../bin/disk_mount/ 
lsblk