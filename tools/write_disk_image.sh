###############################################
# 该脚本用于将文件拷贝到磁盘镜像中，
#       并在磁盘镜像中安装grub引导程序
#
# 用法：bash write_disk_image.sh --bios legacy/uefi
# 如果之前创建的 disk-${ARCH}.img 是MBR分区表，那么请这样运行它：bash write_disk_image.sh --bios legacy
# 如果之前创建的 disk-${ARCH}.img 是GPT分区表，那么请这样运行它：bash write_disk_image.sh --bios uefi
# 通过设置ARCH为x86_64/i386/riscv64，进行64/32位uefi的install，但是请记住该处的ARCH应与run-qemu.sh中的一致
###############################################
set -euo pipefail

echo "ARCH=${ARCH:-}"
# 给ARCH变量赋默认值
export ARCH=${ARCH:=x86_64}
export DADK=${DADK:=dadk}

# 内核映像
root_folder=$(dirname $(pwd))

# CI或纯nographic运行可通过设置SKIP_GRUB=1跳过grub相关检查与安装
export SKIP_GRUB=${SKIP_GRUB:=0}
if [ "${SKIP_GRUB}" = "1" ]; then
    echo "SKIP_GRUB=1: 跳过grub检查与安装，仅准备镜像文件"
fi

ROOTFS_MOUNTED=0
cleanup() {
    if [ "${ROOTFS_MOUNTED}" = "1" ]; then
        $DADK "${DADK_MANIFEST_ARGS[@]}" -w "$root_folder" rootfs umount || true
    fi
}
kernel="${root_folder}/bin/kernel/kernel.elf"
DADK_MANIFEST="${root_folder}/dadk-manifest.generated.toml"
if [ ! -f "${DADK_MANIFEST}" ]; then
    echo "Error: missing generated manifest: ${DADK_MANIFEST}" >&2
    echo "Please run 'make prepare_rootfs_manifest' at project root first." >&2
    exit 1
fi
DADK_MANIFEST_ARGS=(-f "${DADK_MANIFEST}")
echo "Using DADK manifest: ${DADK_MANIFEST}"
trap cleanup EXIT

mount_folder=$($DADK "${DADK_MANIFEST_ARGS[@]}" -w $root_folder rootfs show-mountpoint || exit 1)
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
# 显式要求跳过grub时，不执行相关安装
if [ "${SKIP_GRUB}" = "1" ]; then
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
if [ "${SKIP_GRUB}" != "1" ] && ([ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]); then
    if ${GRUB_PATH_I386_LEGACY_FILE} --is-x86-multiboot2 ${kernel}; then
        echo Multiboot2 Confirmed!
    else
        echo NOT Multiboot2!
        exit
    fi
fi

# 判断是否存在硬盘镜像文件，如果不存在，就创建一个
echo "创建硬盘镜像文件..."
$DADK "${DADK_MANIFEST_ARGS[@]}" -w $root_folder rootfs create --skip-if-exists || exit 1

$DADK "${DADK_MANIFEST_ARGS[@]}" -w $root_folder rootfs mount || exit 1
ROOTFS_MOUNTED=1



LOOP_DEVICE=$($DADK "${DADK_MANIFEST_ARGS[@]}" -w $root_folder rootfs show-loop-device || exit 1)
echo $LOOP_DEVICE
echo ${mount_folder}

if ! ls -ld "${mount_folder}" >/dev/null 2>&1; then
    echo "错误: rootfs挂载点不可访问，可能镜像已损坏: ${mount_folder}"
    exit 1
fi

FS_TYPE=$(findmnt -n -o FSTYPE ${mount_folder} || df -T ${mount_folder} | tail -1 | awk '{print $2}')
echo "FS_TYPE: $FS_TYPE"
# mkdir -p ${GRUB_INSTALL_PATH}

# 检测grub文件夹是否存在
if [ -d "${GRUB_INSTALL_PATH}" ] || [ "${INSTALL_GRUB_TO_IMAGE}" = "0" ]; then
   echo "无需安装grub"
   INSTALL_GRUB_TO_IMAGE="0"
else
    mkdir -p ${GRUB_INSTALL_PATH}
fi

# 拷贝用户程序到磁盘镜像
mkdir -p ${mount_folder}/bin
mkdir -p ${mount_folder}/sbin
mkdir -p ${mount_folder}/dev
mkdir -p ${mount_folder}/proc
mkdir -p ${mount_folder}/usr
mkdir -p ${mount_folder}/root
mkdir -p ${mount_folder}/tmp

is_vfat_target() {
    [ "$FS_TYPE" = "vfat" ] || [ "$FS_TYPE" = "fat32" ]
}

copy_sysroot_to_vfat() {
    # vfat 是大小写不敏感文件系统，且不支持符号链接。
    # 逐条目复制并做大小写折叠去重，避免 PAM.7.gz / pam.7.gz 这类冲突。
    local src_root="${root_folder}/bin/sysroot"
    local rel src_path dst_path key
    local -A casefold_kept=()

    while IFS= read -r -d '' rel; do
        rel="${rel#./}"
        [ -n "$rel" ] || continue

        src_path="${src_root}/${rel}"
        dst_path="${mount_folder}/${rel}"
        key="${rel,,}"

        if [ -d "$src_path" ]; then
            if [ -n "${casefold_kept[$key]+x}" ] && [ "${casefold_kept[$key]}" != "$rel" ]; then
                continue
            fi
            mkdir -p "$dst_path"
            casefold_kept[$key]="$rel"
            continue
        fi

        # 其它非常规节点（例如 fifo/socket）不写入 vfat。
        if [ ! -f "$src_path" ]; then
            continue
        fi

        if [ -n "${casefold_kept[$key]+x}" ] && [ "${casefold_kept[$key]}" != "$rel" ]; then
            continue
        fi

        mkdir -p "$(dirname "$dst_path")"
        cp -fL --remove-destination "$src_path" "$dst_path"
        casefold_kept[$key]="$rel"
    done < <(cd "$src_root" && find -L . -mindepth 1 -print0 | sort -z)
}

copy_one_sysroot_entry() {
    local src="$1"
    local name
    local dst
    name="$(basename "$src")"
    dst="${mount_folder}/${name}"

    # Ubuntu 等基础镜像常把 /bin,/sbin 作为到 /usr/* 的符号链接。
    # 当源是目录、目标是符号链接目录时，拷贝目录内容到符号链接目标，避免 cp 报冲突。
    if [ -d "$src" ] && [ -L "$dst" ] && [ -d "$dst" ]; then
        if is_vfat_target; then
            cp -rfL --remove-destination "${src}/." "${dst}/"
        else
            cp -a "${src}/." "${dst}/"
        fi
        return 0
    fi

    if is_vfat_target; then
        cp -rfL --remove-destination "$src" "${mount_folder}/"
    else
        cp -a "$src" "${mount_folder}/"
    fi
}

if is_vfat_target; then
    copy_sysroot_to_vfat
else
    shopt -s dotglob nullglob
    for item in "${root_folder}"/bin/sysroot/*; do
        copy_one_sysroot_entry "$item"
    done
    shopt -u dotglob nullglob
fi

ensure_boot_dir() {
    # Keep existing /boot when it's a directory or a symlink to a directory.
    if [ -d "${mount_folder}/boot" ]; then
        return 0
    fi

    # If /boot exists but is not directory-like (e.g. regular file), fail fast.
    if [ -e "${mount_folder}/boot" ]; then
        echo "Error: ${mount_folder}/boot exists but is not a directory/symlink-to-directory." >&2
        echo "Please clean/rebuild rootfs and sysroot, then retry." >&2
        return 1
    fi

    mkdir -p "${mount_folder}/boot"
}

# 设置 grub 相关数据
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
ensure_boot_dir || exit 1
cp ${kernel} ${mount_folder}/boot/
mkdir -p ${mount_folder}/boot/grub
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

$DADK "${DADK_MANIFEST_ARGS[@]}" -w $root_folder rootfs umount || exit 1
ROOTFS_MOUNTED=0
