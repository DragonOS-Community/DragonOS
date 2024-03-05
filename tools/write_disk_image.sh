###############################################
# 该脚本用于将disk_mount目录下的文件写入到disk-${ARCH}.img的第一个分区中，
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

DISK_NAME=disk-${ARCH}.img

# 内核映像
root_folder=$(dirname $(pwd))
kernel="${root_folder}/bin/kernel/kernel.elf"
boot_folder="${root_folder}/bin/disk_mount/boot"
GRUB_INSTALL_PATH="${boot_folder}/grub"
mount_folder="${root_folder}/bin/disk_mount"
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

# 判断是否存在硬盘镜像文件，如果不存在，就创建一个(docker模式下，由于镜像中缺少qemu-img不会创建)
if [ ! -f "${root_folder}/bin/${DISK_NAME}" ]; then
    echo "创建硬盘镜像文件..."
    case "$1" in
        --bios) 
        case "$2" in
                uefi)
            sudo ARCH=${ARCH} bash ./create_hdd_image.sh -P MBR #GPT分区    用GPT分区uefi启动不了 内核没有针对gpt分区表来做处理
            ;;
                legacy)
            sudo ARCH=${ARCH} bash ./create_hdd_image.sh -P MBR #MBR分区
            ;;
            esac       
        ;;
    *)
        # 默认创建MBR分区
        sudo ARCH=${ARCH} bash ./create_hdd_image.sh -P MBR #MBR分区
        ;;
    esac
fi

# 拷贝程序到硬盘
mkdir -p ${root_folder}/bin/disk_mount
bash mount_virt_disk.sh || exit 1

LOOP_DEVICE=$(lsblk | grep disk_mount|sed 's/.*\(loop[0-9]*\)p1.*/\1/1g'|awk 'END{print $0}')
echo $LOOP_DEVICE

# mkdir -p ${GRUB_INSTALL_PATH}

# 检测grub文件夹是否存在
if [ -d "${GRUB_INSTALL_PATH}" ] || [ "${INSTALL_GRUB_TO_IMAGE}" = "0" ]; then
   echo "无需安装grub"
   INSTALL_GRUB_TO_IMAGE="0"
else
    mkdir -p ${GRUB_INSTALL_PATH}
fi


if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    cp ${kernel} ${root_folder}/bin/disk_mount/boot/
fi

# 拷贝用户程序到磁盘镜像
mkdir -p ${root_folder}/bin/disk_mount/bin
mkdir -p ${root_folder}/bin/disk_mount/dev
mkdir -p ${root_folder}/bin/disk_mount/proc
mkdir -p ${root_folder}/bin/disk_mount/usr
touch ${root_folder}/bin/disk_mount/dev/keyboard.dev
cp -r ${root_folder}/bin/sysroot/* ${root_folder}/bin/disk_mount/

# 设置 grub 相关数据
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    
    touch ${root_folder}/bin/disk_mount/boot/grub/grub.cfg
cfg_content='set timeout=15
    set default=0
    insmod efi_gop
    menuentry "DragonOS" {
    multiboot2 /boot/kernel.elf "KERNEL_ELF"
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
                        ${GRUB_PATH_I386_LEGACY_INSTALL} --target=i386-pc --boot-directory=${boot_folder} /dev/$LOOP_DEVICE
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
        ${GRUB_PATH_I386_LEGACY_INSTALL} --target=i386-pc --boot-directory=${boot_folder} /dev/$LOOP_DEVICE
        ;;
            
    esac
fi

sync
bash umount_virt_disk.sh
