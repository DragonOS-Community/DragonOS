# ======检查是否以sudo运行=================
#uid=`id -u`
#if [ ! $uid == "0" ];then
#  echo "请以sudo权限运行"
#  exit
#fi
GENERATE_ISO=0
IN_DOCKER=0

# 第一个参数如果是--notbuild 那就不构建，直接运行
if [ ! "$1" == "--nobuild" ]; then
    echo "开始构建..."
    if [ "$1" == "--docker" ]; then
        echo "使用docker构建"
        sudo bash tools/build_in_docker.sh
        GENERATE_ISO=0
    elif [ "$1" == "--current_in_docker" ]; then
        echo "运行在docker内"
        IN_DOCKER=1
        make all -j 16
        make clean
        GENERATE_ISO=1
    else
        
        make all -j 16
        make clean
        GENERATE_ISO=1
    fi
fi

IA32_USE_QEMU=1
bochsrc="./bochsrc"
ARCH="x86_64"

# 内核映像
kernel='./bin/kernel/kernel.elf'
iso_boot_grub='./iso/boot/grub'
iso_boot='./iso/boot/'
iso='./DragonOS.iso'
iso_folder='./iso/'
root_folder="$(pwd)"

if [ "${GENERATE_ISO}" == "1" ]; then
    echo "开始生成iso..."

    # toolchain
    OS=`uname -s`
    if [ "${OS}" == "Linux" ]; then
        GRUB_PATH="$(dirname $(which grub-file))"
    elif [ "${OS}" == "Darwin" ]; then
        GRUB_PATH="$(pwd)/tools/grub-2.06/build/grub/bin"
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
    if [ "${IN_DOCKER}" == "1" ]; then
        echo "运行在docker中, 构建结束"
        exit 0
    fi
fi


# 进行启动前检查
flag_can_run=0

if [ -d "${iso_folder}" ]; then
  flag_can_run=0
  echo "${iso_folder} 文件夹未删除！"
else
  flag_can_run=1
fi

# 拷贝应用程序到硬盘
cd tools
bash m*
sudo mkdir -p ${root_folder}/bin/disk_mount
sudo cp ${root_folder}/bin/user/shell.elf ${root_folder}/bin/disk_mount
sudo cp ${root_folder}/bin/user/about.elf ${root_folder}/bin/disk_mount
sudo mkdir ${root_folder}/bin/disk_mount/dev
sudo touch ${root_folder}/bin/disk_mount/dev/keyboard.dev
sync
bash u*
cd ..
allflags=$(qemu-system-x86_64 -cpu help | awk '/flags/ {y=1; getline}; y {print}' | tr ' ' '\n' | grep -Ev "^$" | sed -r 's|^|+|' | tr '\n' ',' | sed -r "s|,$||")

# 调试usb的trace
qemu_trace_usb=trace:usb_xhci_reset,trace:usb_xhci_run,trace:usb_xhci_stop,trace:usb_xhci_irq_msi,trace:usb_xhci_irq_msix,trace:usb_xhci_port_reset

if [ $flag_can_run -eq 1 ]; then
  if [ ${IA32_USE_QEMU} == 0 ]; then
        bochs -q -f ${bochsrc} -rc ./tools/bochsinit
    else
        qemu-system-x86_64 -cdrom ${iso} -m 512M -smp 2,cores=2,threads=1,sockets=1 \
        -boot order=d   \
        -monitor stdio -d cpu_reset,guest_errors,trace:check_exception,exec,cpu,out_asm,in_asm,${qemu_trace_usb} \
        -s -S -cpu "IvyBridge,+apic,+x2apic,+fpu,check,${allflags}" --enable-kvm -rtc clock=host,base=localtime -serial file:serial_opt.txt \
        -drive id=disk,file=bin/disk.img,if=none \
        -device ahci,id=ahci \
        -device ide-hd,drive=disk,bus=ahci.0    \
        -usb    \
        -device qemu-xhci,id=xhci,p2=8,p3=4

    fi
else
  echo "不满足运行条件"
fi