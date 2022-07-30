# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
 echo "请以sudo权限运行"
 exit
fi

LOOP_DEVICE=$(losetup -f --show -P ../bin/disk.img) \
    || exit 1

echo ${LOOP_DEVICE}p1

mkdir -p ../bin/disk_mount/
mount ${LOOP_DEVICE}p1 ../bin/disk_mount/ 
lsblk