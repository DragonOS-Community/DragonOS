sudo losetup -P /dev/loop1 --show ../bin/disk.img
lsblk
mkdir -p ../bin/disk_mount/ 
sudo mount /dev/loop1p1 ../bin/disk_mount/ 