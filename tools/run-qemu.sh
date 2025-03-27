check_dependencies()
{
    # Check if qemu is installed
    if [ -z "$(which qemu-system-x86_64)" ]; then
        echo "Please install qemu first!"
        exit 1
    fi

    # Check if brctl is installed
    if [ -z "$(which brctl)" ]; then
        echo "Please install bridge-utils first!"
        exit 1
    fi

    # Check if dnsmasq is installed
    if [ -z "$(which dnsmasq)" ]; then
        echo "Please install dnsmasq first!"
        exit 1
    fi

    # Check if iptable is installed
    if [ -z "$(which iptables)" ]; then
        echo "Please install iptables first!"
        exit 1
    fi

}

check_dependencies

# 进行启动前检查
flag_can_run=1
ARGS=`getopt -o p -l bios:,display: -- "$@"`
eval set -- "${ARGS}"
echo "$@"
allflags= 
# allflags=$(qemu-system-x86_64 -cpu help | awk '/flags/ {y=1; getline}; y {print}' | tr ' ' '\n' | grep -Ev "^$" | sed -r 's|^|+|' | tr '\n' ',' | sed -r "s|,$||")
# 设置ARCH环境变量，如果没有设置，就默认为x86_64
export ARCH=${ARCH:=x86_64}
echo "ARCH=${ARCH}"
#ARCH="i386"
# 请根据自己的需要，在-d 后方加入所需的 trace 事件

# 标准的trace events
qemu_trace_std=cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*
# 调试usb的trace
qemu_trace_usb=trace:usb_xhci_reset,trace:usb_xhci_run,trace:usb_xhci_stop,trace:usb_xhci_irq_msi,trace:usb_xhci_irq_msix,trace:usb_xhci_port_reset,trace:msix_write_config,trace:usb_xhci_irq_msix,trace:usb_xhci_irq_msix_use,trace:usb_xhci_irq_msix_unuse,trace:usb_xhci_irq_msi,trace:usb_xhci_*

# 根据架构设置qemu的加速方式
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
  qemu_accel="kvm"
  if [ $(uname) == Darwin ]; then
    qemu_accel=hvf
  else
    # 判断系统kvm模块是否加载
    if [ ! -e /dev/kvm ]; then
      # kvm模块未加载，使用tcg加速
      qemu_accel="tcg"
    fi
  fi
fi

# uboot版本
UBOOT_VERSION="v2023.10"
RISCV64_UBOOT_PATH="arch/riscv64/u-boot-${UBOOT_VERSION}-riscv64"


DISK_NAME="disk-image-${ARCH}.img"

QEMU=qemu-system-${ARCH}
QEMU_DISK_IMAGE="../bin/${DISK_NAME}"
QEMU_MEMORY="512M"
QEMU_MEMORY_BACKEND="dragonos-qemu-shm.ram"
QEMU_MEMORY_BACKEND_PATH_PREFIX="/dev/shm"
QEMU_SHM_OBJECT="-object memory-backend-file,size=${QEMU_MEMORY},id=${QEMU_MEMORY_BACKEND},mem-path=${QEMU_MEMORY_BACKEND_PATH_PREFIX}/${QEMU_MEMORY_BACKEND},share=on "
QEMU_SMP="2,cores=2,threads=1,sockets=1"
QEMU_MONITOR="-monitor stdio"
QEMU_TRACE="${qemu_trace_std}"
QEMU_CPU_FEATURES=""
QEMU_RTC_CLOCK=""
QEMU_SERIAL_LOG_FILE="../serial_opt.txt"
QEMU_SERIAL="-serial file:${QEMU_SERIAL_LOG_FILE}"
QEMU_DRIVE="id=disk,file=${QEMU_DISK_IMAGE},if=none"
QEMU_ACCELARATE=""
QEMU_ARGUMENT=" -no-reboot "
QEMU_DEVICES=""

KERNEL_CMDLINE=""

BIOS_TYPE=""
#这个变量为true则使用virtio磁盘
VIRTIO_BLK_DEVICE=true
# 如果qemu_accel不为空
if [ -n "${qemu_accel}" ]; then
    QEMU_ACCELARATE=" -machine accel=${qemu_accel} "
  if [ "${qemu_accel}" == "kvm" ]; then
    QEMU_ACCELARATE+=" -enable-kvm "
  fi
fi

if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    QEMU_MACHINE=" -machine q35,memory-backend=${QEMU_MEMORY_BACKEND} "
    QEMU_CPU_FEATURES+="-cpu IvyBridge,apic,x2apic,+fpu,check,+vmx,${allflags}"
    QEMU_RTC_CLOCK+=" -rtc clock=host,base=localtime"
    if [ ${VIRTIO_BLK_DEVICE} == false ]; then
      QEMU_DEVICES_DISK+="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 "
    else
      QEMU_DEVICES_DISK="-device virtio-blk-pci,drive=disk -device pci-bridge,chassis_nr=1,id=pci.1 -device pcie-root-port "
    fi

else
    QEMU_MACHINE=" -machine virt,memory-backend=${QEMU_MEMORY_BACKEND} -cpu sifive-u54 "
    QEMU_DEVICES_DISK="-device virtio-blk-device,drive=disk "

fi

if [ ${ARCH} == "riscv64" ]; then
# 如果是riscv64架构，就不需要图形界面
    QEMU_ARGUMENT+=" --nographic "
    # 从控制台显示
    QEMU_MONITOR=""
    QEMU_SERIAL=""
fi

while true;do
    case "$1" in
        --bios) 
        case "$2" in
              uefi) #uefi启动新增ovmf.fd固件
              BIOS_TYPE=uefi
            ;;
              legacy)
              BIOS_TYPE=legacy
              ;;
        esac;shift 2;;
        --display)
        case "$2" in
              vnc)
              QEMU_ARGUMENT+=" -display vnc=:00 "
              ;;
              window)
              ;;
              nographic)
              QEMU_SERIAL=" -serial chardev:mux -monitor chardev:mux -chardev stdio,id=mux,mux=on,signal=off,logfile=${QEMU_SERIAL_LOG_FILE} "
              # 添加 virtio console 设备
              QEMU_DEVICES+=" -device virtio-serial -device virtconsole,chardev=mux "
              KERNEL_CMDLINE+=" console=/dev/hvc0 "
              QEMU_MONITOR=""
              QEMU_ARGUMENT+=" --nographic "
              QEMU_ARGUMENT+=" -kernel ../bin/kernel/kernel.elf "
              QEMU_ARGUMENT+="-append ${KERNEL_CMDLINE}"

              ;;
        esac;shift 2;;
        *) break
      esac 
  done


# ps: 下面这条使用tap的方式，无法dhcp获取到ip，暂时不知道为什么
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -net nic,netdev=nic0 -netdev tap,id=nic0,model=virtio-net-pci,script=qemu/ifup-nat,downscript=qemu/ifdown-nat -usb -device qemu-xhci,id=xhci,p2=8,p3=4 "
QEMU_DEVICES+="${QEMU_DEVICES_DISK} "
QEMU_DEVICES+=" -netdev user,id=hostnet0,hostfwd=tcp::12580-:12580 -device virtio-net-pci,vectors=5,netdev=hostnet0,id=net0 -usb -device qemu-xhci,id=xhci,p2=8,p3=4 " 
# E1000E
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -netdev user,id=hostnet0,hostfwd=tcp::12580-:12580 -net nic,model=e1000e,netdev=hostnet0,id=net0 -netdev user,id=hostnet1,hostfwd=tcp::12581-:12581 -device virtio-net-pci,vectors=5,netdev=hostnet1,id=net1 -usb -device qemu-xhci,id=xhci,p2=8,p3=4 " 


QEMU_ARGUMENT+="-d ${QEMU_DISK_IMAGE} -m ${QEMU_MEMORY} -smp ${QEMU_SMP} -boot order=d ${QEMU_MONITOR} -d ${qemu_trace_std} "

QEMU_ARGUMENT+="-s ${QEMU_MACHINE} ${QEMU_CPU_FEATURES} ${QEMU_RTC_CLOCK} ${QEMU_SERIAL} -drive ${QEMU_DRIVE} ${QEMU_DEVICES} "
QEMU_ARGUMENT+=" ${QEMU_SHM_OBJECT} "
QEMU_ARGUMENT+=" ${QEMU_ACCELARATE} "

QEMU_ARGUMENT+=" -D ../qemu.log "


# 安装riscv64的uboot
install_riscv_uboot()
{

    if [ ! -d ${RISCV64_UBOOT_PATH} ]; then
        echo "正在下载u-boot..."
        uboot_tar_name="u-boot-${UBOOT_VERSION}-riscv64.tar.xz"
        
        uboot_parent_path=$(dirname ${RISCV64_UBOOT_PATH}) || (echo "获取riscv u-boot 版本 ${UBOOT_VERSION} 的父目录失败" && exit 1)

        if [ ! -f ${uboot_tar_name} ]; then
            wget https://mirrors.dragonos.org.cn/pub/third_party/u-boot/${uboot_tar_name} || (echo "下载riscv u-boot 版本 ${UBOOT_VERSION} 失败" && exit 1)
        fi
        echo "下载完成"
        echo "正在解压u-boot到 '$uboot_parent_path'..."
        mkdir -p $uboot_parent_path
        tar xvf u-boot-${UBOOT_VERSION}-riscv64.tar.xz -C ${uboot_parent_path} || (echo "解压riscv u-boot 版本 ${UBOOT_VERSION} 失败" && exit 1)
        echo "解压完成"
        rm -rf u-boot-${UBOOT_VERSION}-riscv64.tar.xz
    fi
    echo "riscv u-boot 版本 ${UBOOT_VERSION} 已经安装"
} 


if [ $flag_can_run -eq 1 ]; then
   

# 删除共享内存
sudo rm -rf ${QEMU_MEMORY_BACKEND_PATH_PREFIX}/${QEMU_MEMORY_BACKEND}

if [ ${BIOS_TYPE} == uefi ] ;then
  if [ ${ARCH} == x86_64 ] ;then
    sudo ${QEMU} -bios arch/x86_64/efi/OVMF-pure-efi.fd ${QEMU_ARGUMENT}
  elif [ ${ARCH} == i386 ] ;then
    sudo ${QEMU} -bios arch/i386/efi/OVMF-pure-efi.fd ${QEMU_ARGUMENT}
  elif [ ${ARCH} == riscv64 ] ;then
    install_riscv_uboot
    sudo ${QEMU} -kernel ${RISCV64_UBOOT_PATH}/u-boot.bin ${QEMU_ARGUMENT}
  else
    echo "不支持的架构: ${ARCH}"
  fi
else
  # 如果是i386架构或者x86_64架构，就直接启动
  if [ ${ARCH} == x86_64 ] || [ ${ARCH} == i386 ] ;then
    sudo ${QEMU} ${QEMU_ARGUMENT}
  elif [ ${ARCH} == riscv64 ] ;then
    # 如果是riscv64架构，就与efi启动一样
    install_riscv_uboot
    sudo ${QEMU} -kernel ${RISCV64_UBOOT_PATH}/u-boot.bin ${QEMU_ARGUMENT}
  else
    echo "不支持的架构: ${ARCH}"
  fi
fi

# 删除共享内存
sudo rm -rf ${QEMU_MEMORY_BACKEND_PATH_PREFIX}/${QEMU_MEMORY_BACKEND}
else
  echo "不满足运行条件"
fi
