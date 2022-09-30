# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
 echo "请以sudo权限运行"
 exit
fi
GENERATE_ISO=0
IN_DOCKER=0

IA32_USE_QEMU=1
bochsrc="./bochsrc"
ARCH="x86_64"

for i in "$@"
do
    if [ $i == "--no-qemu" ];then
        IA32_USE_QEMU=0
    fi
done

if [ ${IA32_USE_QEMU} == "1" ];then
    export EMULATOR=__QEMU_EMULATION__
else
    export EMULATOR=__NO_EMULATION__
fi

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
        if [ "$?" != "0" ]; then\
            echo "DragonOS编译失败";\
            exit 1;\
        fi;\
        make clean
        GENERATE_ISO=1
    else
        
        make all -j 16
        if [ "$?" != "0" ]; then\
            echo "DragonOS编译失败";\
            exit 1;\
        fi;\
        make clean
        GENERATE_ISO=1
    fi
fi


# 内核映像
root_folder="$(pwd)"
kernel="${root_folder}/bin/kernel/kernel.elf"
boot_folder="${root_folder}/bin/disk_mount/boot"

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

    # 拷贝程序到硬盘
    cd tools
    # 判断是否存在硬盘镜像文件，如果不存在，就创建一个(docker模式下，由于镜像中缺少qemu-img不会创建)
    if [ ! -f "${root_folder}/bin/disk.img" ]; then
        echo "创建硬盘镜像文件..."
        bash ./create_hdd_image.sh
    fi

    mkdir -p ${root_folder}/bin/disk_mount
    bash mount_virt_disk.sh || exit 1
    mkdir -p ${boot_folder}/grub
    cp ${kernel} ${root_folder}/bin/disk_mount/boot
    # 拷贝用户程序到磁盘镜像
    cp -r ${root_folder}/bin/user/* ${root_folder}/bin/disk_mount
    mkdir -p ${root_folder}/bin/disk_mount/dev
    touch ${root_folder}/bin/disk_mount/dev/keyboard.dev
    

    # 设置 grub 相关数据
    if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
        
        touch ${root_folder}/bin/disk_mount/boot/grub/grub.cfg
cfg_content='set timeout=15
        set default=0
        menuentry "DragonOS" {
        multiboot2 /boot/kernel.elf "KERNEL_ELF"
    }'
    echo "echo '${cfg_content}' >  ${boot_folder}/grub/grub.cfg" | sh
    fi


    # ${GRUB_PATH}/grub-mkrescue -o ${iso} ${iso_folder}
    # rm -rf ${iso_folder}
    LOOP_DEVICE=$(lsblk | grep disk_mount|sed 's/.*\(loop[0-9]*\)p1.*/\1/1g'|awk 'END{print $0}')
    echo $LOOP_DEVICE

    grub-install --target=i386-pc --boot-directory=${root_folder}/bin/disk_mount/boot/ /dev/$LOOP_DEVICE

    sync
    bash umount_virt_disk.sh
    cd ..

    if [ "${IN_DOCKER}" == "1" ]; then
        echo "运行在docker中, 构建结束"
        exit 0
    fi
fi


# 进行启动前检查
flag_can_run=1


allflags=$(qemu-system-x86_64 -cpu help | awk '/flags/ {y=1; getline}; y {print}' | tr ' ' '\n' | grep -Ev "^$" | sed -r 's|^|+|' | tr '\n' ',' | sed -r "s|,$||")

# 请根据自己的需要，在-d 后方加入所需的trace事件

# 标准的trace events
qemu_trace_std=cpu_reset,guest_errors,trace:check_exception,exec,cpu
# 调试usb的trace
qemu_trace_usb=trace:usb_xhci_reset,trace:usb_xhci_run,trace:usb_xhci_stop,trace:usb_xhci_irq_msi,trace:usb_xhci_irq_msix,trace:usb_xhci_port_reset,trace:msix_write_config,trace:usb_xhci_irq_msix,trace:usb_xhci_irq_msix_use,trace:usb_xhci_irq_msix_unuse,trace:usb_xhci_irq_msi,trace:usb_xhci_*


qemu_accel=kvm
if [ $(uname) == Darwin ]; then
    qemu_accel=hvf
fi

if [ $flag_can_run -eq 1 ]; then
  if [ ${IA32_USE_QEMU} == 0 ]; then
        bochs -q -f ${bochsrc} -rc ./tools/bochsinit
    else
        qemu-system-x86_64 -d bin/disk.img -m 512M -smp 2,cores=2,threads=1,sockets=1 \
        -boot order=d   \
        -monitor stdio -d ${qemu_trace_std} \
        -s -S -cpu IvyBridge,apic,x2apic,+fpu,check,${allflags} -rtc clock=host,base=localtime -serial file:serial_opt.txt \
        -drive id=disk,file=bin/disk.img,if=none \
        -device ahci,id=ahci \
        -device ide-hd,drive=disk,bus=ahci.0    \
        -usb    \
        -device qemu-xhci,id=xhci,p2=8,p3=4 \
        -machine accel=${qemu_accel}
    fi
else
  echo "不满足运行条件"
fi