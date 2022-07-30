# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
 echo "请以sudo权限运行"
 exit
fi

LOOP_DEVICE=$(lsblk | grep disk_mount)
umount -f ../bin/disk_mount/
losetup -d /dev/${LOOP_DEVICE:2:5}
echo ${LOOP_DEVICE:2:5}