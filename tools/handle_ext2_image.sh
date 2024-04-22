DISK_NAME=../bin/ext2.img


fdisk ${DISK_NAME} << EOF
o
n




a
w
EOF



LOOP_DEVICE=$(sudo losetup -f --show -P ${DISK_NAME}) \
    || exit 1
echo ${LOOP_DEVICE}p1
sudo mkfs.ext2 ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image."
chmod 777 ${DISK_NAME}
