# ======检查是否以sudo运行=================
#uid=`id -u`
#if [ ! $uid == "0" ];then
#  echo "请以sudo权限运行"
#  exit
#fi

# 第一个参数如果是--notbuild 那就不构建，直接运行
if [ ! "$1" == "--nobuild" ]; then
    echo "开始构建..."
    make all -j 16
    make clean
fi

IA32_USE_QEMU=0
bochsrc="./bochsrc"
ARCH="x86_64"

# 内核映像
kernel='./bin/kernel/kernel.elf'
iso_boot_grub='./iso/boot/grub'
iso_boot='./iso/boot/'
iso='./DragonOS.iso'
iso_folder='./iso/'


# toolchain
OS=`uname -s`
if [ "${OS}" == "Linux" ]; then
    GRUB_PATH="$(dirname $(which grub-file))"
elif [ "${OS}" == "Darwin" ]; then
    GRUB_PATH="$(pwd)/tools/grub-2.04/build/grub/bin"
fi
export PATH="${GRUB_PATH}:$PATH"

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
    if ${GRUB_PATH}/grub-file --is-x86-multiboot2 ${kernel}; then
        echo Multiboot2 Confirmed!
    else
        echo NOT Multiboot2!
        exit
    fi
fi
# 检测路径是否合法，发生过 rm -rf -f /* 的惨剧
if [ "${iso_boot}" == "" ]; then
    echo iso_boot path error.
else
    mkdir -p ${iso_boot}
    rm -rf -f ${iso_boot}/*
fi

# 设置 grub 相关数据
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    cp ${kernel} ${iso_boot}
    mkdir ${iso_boot_grub}
    touch ${iso_boot_grub}/grub.cfg
    echo 'set timeout=15
    set default=0
    menuentry "DragonOS" {
       multiboot2 /boot/kernel.elf "KERNEL_ELF"
   }' >${iso_boot_grub}/grub.cfg
fi

${GRUB_PATH}/grub-mkrescue -o ${iso} ${iso_folder}
rm -rf ${iso_folder}
# 进行启动前检查
flag_can_run=0

if [ -d "${iso_folder}" ]; then
  flag_can_run=0
  echo "${iso_folder} 文件夹未删除！"
else
  flag_can_run=1
fi

if [ $flag_can_run -eq 1 ]; then
  if [ ${IA32_USE_QEMU} == 0 ]; then
        bochs -q -f ${bochsrc} -rc ./tools/bochsinit
    else
        qemu-system-x86_64 -cdrom ${iso} -m 512M -smp 2,cores=2,threads=1,sockets=1 \
        -monitor stdio -d cpu_reset,guest_errors,trace:check_exception,exec,cpu,out_asm,in_asm -s -S -cpu IvyBridge --enable-kvm \
        -drive id=disk,file=bin/disk.img,if=none \
        -device ahci,id=ahci \
        -device ide-hd,drive=disk,bus=ahci.0    \
        -usb

    fi
else
  echo "不满足运行条件"
fi