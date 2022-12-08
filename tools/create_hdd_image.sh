echo "Creating virtual disk image..."
ARGS=`getopt -o P: -- "$@"`
# 创建一至少为64MB磁盘镜像（类型选择raw）
qemu-img create -f raw disk.img 64M
#将规范化后的命令行参数分配至位置参数（$1,$2,...)
eval set -- "${ARGS}"
#echo formatted parameters=[$@]
#根据传入参数进行MBR/GPT分区
case "$1" in
        -P) 
if [ $2 == "MBR" ];
then 
# 使用fdisk把disk.img的分区表设置为MBR格式(下方的空行请勿删除)
fdisk disk.img << EOF
o
n




w
EOF
elif [ $2 == "GPT" ];
then
sudo parted disk.img  << EOF
mklabel gpt
y
mkpart
p1
FAT32
0
-1
I
set
1
boot
on
print
q
EOF
fi
esac
LOOP_DEVICE=$(sudo losetup -f --show -P disk.img) \
    || exit 1
echo ${LOOP_DEVICE}p1
sudo mkfs.vfat -F 32 ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image."
mkdir -p ../bin
mv ./disk.img ../bin/
