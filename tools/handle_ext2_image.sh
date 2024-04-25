DISK_NAME=./ext2.img

# qemu-img create -f raw ${DISK_NAME} 256M

fdisk ${DISK_NAME} << EOF
o
n




a
w
EOF



LOOP_DEVICE=$(sudo losetup -f --show -P ${DISK_NAME}) \
    || exit 1
echo ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image."
chmod 777 ${DISK_NAME}

mv ./${DISK_NAME} ../bin/

