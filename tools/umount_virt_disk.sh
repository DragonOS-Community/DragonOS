# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
 echo "请以sudo权限运行"
 exit
fi

if [ ! ${ARCH} ];then
 echo "请设置ARCH环境变量"
 exit
fi

DISK_NAME=disk-${ARCH}.img

LOOP_DEVICE=$(lsblk | grep disk_mount|sed 's/.*\(loop[0-9]*\)p1.*/\1/1g'|awk 'END{print $0}')

umount -f ../bin/disk_mount/
losetup -d /dev/$LOOP_DEVICE
echo $LOOP_DEVICE