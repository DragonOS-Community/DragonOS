########################################################################
# 这是一个用于创建磁盘镜像的脚本
# 用法：./create_hdd_image.sh -P MBR/GPT
# 要创建一个MBR分区表的磁盘镜像，请这样运行它： ARCH=x86_64 bash create_hdd_image.sh -P MBR
# 要创建一个GPT分区表的磁盘镜像，请这样运行它： ARCH=x86_64 bash create_hdd_image.sh -P GPT
# 请注意，这个脚本需要root权限
# 请注意，运行这个脚本之前，需要在您的计算机上安装qemu-img和fdisk，以及parted
# 
# 这个脚本会在当前目录下创建一个名为disk-${ARCH}.img的文件，这个文件就是磁盘镜像，
#       在完成后，会将这个文件移动到bin目录下
########################################################################

echo "create_hdd_image.sh: Creating virtual disk image... arch=${ARCH}"

# 给变量赋默认值
export ARCH=${ARCH:=x86_64}

DISK_NAME=disk-${ARCH}.img

format_as_mbr() {
    echo "Formatting as MBR..."
   # 使用fdisk把disk.img的分区表设置为MBR格式(下方的空行请勿删除)
fdisk ${DISK_NAME} << EOF
o
n




a
w
EOF

}

format_as_gpt() {
    echo "Formatting as GPT..."
sudo parted ${DISK_NAME}  << EOF
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
}

echo "Creating virtual disk image..."
ARGS=`getopt -o P: -- "$@"`
# 创建一至少为256MB磁盘镜像（类型选择raw）
qemu-img create -f raw ${DISK_NAME} 2048M
#将规范化后的命令行参数分配至位置参数（$1,$2,...)
eval set -- "${ARGS}"
#echo formatted parameters=[$@]
#根据传入参数进行MBR/GPT分区
case "$1" in
    -P) 
        if [ $2 == "MBR" ];
        then 
            format_as_mbr
        elif [ $2 == "GPT" ];
        then
            format_as_gpt
        else
            echo "Invalid partition type: $2"
            exit 1
        fi
        ;;
    --)
        # 如果没有传入参数-P，则默认为MBR分区
        format_as_mbr
        ;;
    *)
        echo "Invalid option: $1"
        exit 1
        ;;
esac


LOOP_DEVICE=$(sudo losetup -f --show -P ${DISK_NAME}) \
    || exit 1
echo ${LOOP_DEVICE}p1
sudo mkfs.vfat -F 32 ${LOOP_DEVICE}p1
sudo losetup -d ${LOOP_DEVICE}

echo "Successfully created disk image."
mkdir -p ../bin
chmod 777 ${DISK_NAME}
mv ./${DISK_NAME} ../bin/
