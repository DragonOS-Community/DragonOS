# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
 echo "请以sudo权限运行"
 exit
fi

# 检查是否设置ARCH环境变量

if [ ! ${ARCH} ];then
 echo "请设置ARCH环境变量"
 exit
fi


DISK_NAME=disk-${ARCH}.img

echo "Mounting virtual disk image '${DISK_NAME}'..."

LOOP_DEVICE=$(losetup -f --show -P ../bin/${DISK_NAME}) \
    || exit 1

echo ${LOOP_DEVICE}p1

mkdir -p ../bin/disk_mount/
mount ${LOOP_DEVICE}p1 ../bin/disk_mount/ 
lsblk