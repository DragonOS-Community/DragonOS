# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
 echo "请以sudo权限运行"
 exit
fi

LOOP_DEVICE=$(lsblk | grep disk_mount)

LOOP_DEVICE=${LOOP_DEVICE:2:10}
LOOP_DEVICE=${LOOP_DEVICE%%p1*}

umount -f ../bin/disk_mount/
losetup -d /dev/$LOOP_DEVICE
echo $LOOP_DEVICE
