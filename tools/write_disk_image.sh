###############################################
# 该脚本用于将文件拷贝到磁盘镜像中，
#       并在磁盘镜像中安装grub引导程序
#
# 用法：bash write_disk_image.sh --bios legacy/uefi
# 如果之前创建的 disk-${ARCH}.img 是MBR分区表，那么请这样运行它：bash write_disk_image.sh --bios legacy
# 如果之前创建的 disk-${ARCH}.img 是GPT分区表，那么请这样运行它：bash write_disk_image.sh --bios uefi
# 通过设置ARCH为x86_64/i386/riscv64，进行64/32位uefi的install，但是请记住该处的ARCH应与run-qemu.sh中的一致
###############################################

echo "ARCH=${ARCH}"
# 给ARCH变量赋默认值
export ARCH=${ARCH:=x86_64}
export DADK=${DADK:=dadk}


# 内核映像
root_folder=$(dirname $(pwd))
kernel="${root_folder}/bin/kernel/kernel.elf"
mount_folder=$($DADK -w $root_folder rootfs show-mountpoint || exit 1)
boot_folder="${mount_folder}/boot"
GRUB_INSTALL_PATH="${boot_folder}/grub"

ARGS=`getopt -o p -l bios: -- "$@"`
eval set -- "${ARGS}"
#echo formatted parameters=[$@]
echo "开始写入磁盘镜像..."

if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then

INSTALL_GRUB_TO_IMAGE="1"

else
INSTALL_GRUB_TO_IMAGE="0"
fi


# toolchain
GRUB_ABS_PREFIX=/opt/dragonos-grub
GRUB_PATH_I386_LEGACY_INSTALL=${GRUB_ABS_PREFIX}/arch/i386/legacy/grub/sbin/grub-install
GRUB_PATH_I386_EFI_INSTALL=${GRUB_ABS_PREFIX}/arch/i386/efi/grub/sbin/grub-install
GRUB_PATH_X86_64_EFI_INSTALL=${GRUB_ABS_PREFIX}/arch/x86_64/efi/grub/sbin/grub-install
GRUB_PATH_RISCV64_EFI_INSTALL=${GRUB_ABS_PREFIX}/arch/riscv64/efi/grub/sbin/grub-install

GRUB_PATH_I386_LEGACY_FILE=${GRUB_ABS_PREFIX}/arch/i386/legacy/grub/bin/grub-file


# ==============检查文件是否齐全================

bins[0]=${kernel}

for file in ${bins[*]};do
if [ ! -x $file ]; then
echo "$file 不存在！"
exit 
fi
done

# ===============文件检查完毕===================

# 如果是 i386/x86_64，需要判断是否符合 multiboot2 标准
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    if ${GRUB_PATH_I386_LEGACY_FILE} --is-x86-multiboot2 ${kernel}; then
        echo Multiboot2 Confirmed!
    else
        echo NOT Multiboot2!
        exit
    fi
fi

# 判断是否存在硬盘镜像文件，如果不存在，就创建一个
echo "创建硬盘镜像文件..."
$DADK -w $root_folder rootfs create --skip-if-exists || exit 1

$DADK -w $root_folder rootfs mount || exit 1



LOOP_DEVICE=$($DADK -w $root_folder rootfs show-loop-device || exit 1)
echo $LOOP_DEVICE
echo ${mount_folder}
# mkdir -p ${GRUB_INSTALL_PATH}

# 检测grub文件夹是否存在
if [ -d "${GRUB_INSTALL_PATH}" ] || [ "${INSTALL_GRUB_TO_IMAGE}" = "0" ]; then
   echo "无需安装grub"
   INSTALL_GRUB_TO_IMAGE="0"
else
    mkdir -p ${GRUB_INSTALL_PATH}
fi


if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    cp ${kernel} ${mount_folder}/boot/
fi

# 拷贝用户程序到磁盘镜像
mkdir -p ${mount_folder}/bin
mkdir -p ${mount_folder}/dev
mkdir -p ${mount_folder}/proc
mkdir -p ${mount_folder}/usr
cp -r ${root_folder}/bin/sysroot/* ${mount_folder}/

# 设置 grub 相关数据
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    
    touch ${mount_folder}/boot/grub/grub.cfg
cfg_content='set timeout=15
    set default=0
    insmod efi_gop
    menuentry "DragonOS" {
    multiboot2 /boot/kernel.elf init=/bin/dragonreach
}'
# 增加insmod efi_gop防止32位uefi启动报错
echo "echo '${cfg_content}' >  ${boot_folder}/grub/grub.cfg" | sh
fi

install_riscv64_efi(){
    ${GRUB_PATH_RISCV64_EFI_INSTALL} --target=riscv64-efi --efi-directory=${mount_folder}  --boot-directory=${boot_folder}  --removable
}

if [ "${INSTALL_GRUB_TO_IMAGE}" = "1" ];then

    case "$1" in
        --bios) 
            case "$2" in
                    uefi) #uefi
                    if [ ${ARCH} == "i386" ];then
                        ${GRUB_PATH_I386_EFI_INSTALL} --target=i386-efi  --efi-directory=${mount_folder}  --boot-directory=${boot_folder}  --removable
                    elif [ ${ARCH} == "x86_64" ];then
                        ${GRUB_PATH_X86_64_EFI_INSTALL} --target=x86_64-efi --efi-directory=${mount_folder}  --boot-directory=${boot_folder}   --removable
                    elif [ ${ARCH} == "riscv64" ];then
                        install_riscv64_efi
                    else
                        echo "grub install: 不支持的架构"
                    fi
                ;;
                    legacy) #传统bios
                    if [ ${ARCH} == "x86_64" ];then
                        ${GRUB_PATH_I386_LEGACY_INSTALL} --target=i386-pc --boot-directory=${boot_folder} $LOOP_DEVICE
                    elif [ ${ARCH} == "riscv64" ];then
                        install_riscv64_efi
                    else
                        echo "grub install: 不支持的架构"
                    fi      
                ;;
            esac
            ;;
        *)
        #传统bios
        ${GRUB_PATH_I386_LEGACY_INSTALL} --target=i386-pc --boot-directory=${boot_folder} $LOOP_DEVICE
        ;;

    esac
fi

sync

$DADK -w $root_folder rootfs umount || exit 1
