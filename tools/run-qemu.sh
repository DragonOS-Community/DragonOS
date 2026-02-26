#!/bin/bash
#
# DragonOS QEMU启动脚本
#
# 环境变量支持:
# - DRAGONOS_LOGLEVEL: 设置内核日志级别 (0-7)
#   0: EMERG   1: ALERT   2: CRIT   3: ERR
#   4: WARN    5: NOTICE  6: INFO   7: DEBUG
#   示例: export DRAGONOS_LOGLEVEL=4  # 只显示WARN及以上级别的日志
#
# - AUTO_TEST: 自动测试选项
# - SYSCALL_TEST_DIR: 系统调用测试目录
#

check_dependencies()
{
    # Check if qemu is installed
    if [ -z "$(which qemu-system-x86_64)" ]; then
        echo "Please install qemu first!"
        exit 1
    fi

    if [ -z "$(which ${QEMU})" ]; then
      if [ "$ARCH" == "loongarch64" ]; then
        echo -e "\nPlease install qemu-system-loongarch64 first!"
        echo -e "\nYou can install it by running:  (if you are using ubuntu)"
        echo -e "    ${ROOT_PATH}/tools/qemu/build-qemu-la64-for-ubuntu.sh"
        echo -e ""
        exit 1
      fi
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
EXT4_DISK_NAME="ext4.img"
FAT_DISK_NAME="fat.img"

QEMU=$(which qemu-system-${ARCH})
QEMU_DISK_IMAGE="../bin/${DISK_NAME}"
QEMU_EXT4_DISK_IMAGE="../bin/${EXT4_DISK_NAME}"
QEMU_FAT_DISK_IMAGE="../bin/${FAT_DISK_NAME}"
QEMU_MEMORY="2G"

# 检查必要的环境变量
if [ -z "${ROOT_PATH}" ]; then
    echo "[错误] ROOT_PATH 环境变量未设置"
    echo "[错误] 请通过 Makefile 运行本脚本 (make qemu, make run 等)"
    exit 1
fi

# 状态文件目录（优先使用环境变量，否则使用默认值）
VMSTATE_DIR="${VMSTATE_DIR:-${ROOT_PATH}/bin/vmstate}"
mkdir -p "${VMSTATE_DIR}"

# 分配可用端口的函数
find_free_port() {
    local start_port=$1
    local port=$start_port
    while netstat -tuln 2>/dev/null | grep -q ":${port} " || \
          ss -tuln 2>/dev/null | grep -q ":${port} "; do
        port=$((port + 1))
    done
    echo $port
}

# 先分配网络端口
HOST_PORT=$(find_free_port 12580)
# GDB端口从网络端口的下一位开始搜索，确保不重复
GDB_PORT=$(find_free_port $((HOST_PORT + 1)))

# 写入状态文件
echo "${HOST_PORT}" > "${VMSTATE_DIR}/port"
echo "${GDB_PORT}" > "${VMSTATE_DIR}/gdb"

QEMU_SMP="2,cores=2,threads=1,sockets=1"
QEMU_MONITOR_ARGS=(-monitor stdio)
QEMU_TRACE="${qemu_trace_std}"
QEMU_CPU_FEATURES=""
QEMU_RTC_CLOCK=""
QEMU_SERIAL_LOG_FILE="../serial_opt.txt"
QEMU_SERIAL_ARGS=(-serial "file:${QEMU_SERIAL_LOG_FILE}")
QEMU_DRIVE_ARGS=(-drive "id=disk,file=${QEMU_DISK_IMAGE},if=none,format=raw")
QEMU_ACCEL_ARGS=()
QEMU_DEVICE_ARGS=()
QEMU_DISPLAY_ARGS=()
QEMU_ARGS=()

# vsock 固定配置（按需直接修改脚本）：
# - QEMU_ENABLE_VSOCK=1: 默认启用
# - QEMU_VSOCK_GUEST_CID: guest CID（不能与 host CID=2 冲突）
QEMU_ENABLE_VSOCK=1
QEMU_VSOCK_GUEST_CID=3
QEMU_ATTACH_VSOCK=0
# 推荐 non-transitional 模型，PCI device id 对应 0x1053 (VSOCK)。
QEMU_VSOCK_DEVICE_MODEL="vhost-vsock-pci-non-transitional"
# GDB调试支持：
# - QEMU_GDB_WAIT=1: QEMU 启动后立即暂停CPU（等同 -S），等待 GDB/monitor 手动继续
# - QEMU_GDB_WAIT=0: 默认不暂停
QEMU_GDB_WAIT=1

if [ -f "${QEMU_EXT4_DISK_IMAGE}" ]; then
  QEMU_DRIVE_ARGS+=(-drive "id=ext4disk,file=${QEMU_EXT4_DISK_IMAGE},if=none,format=raw")
fi
if [ -f "${QEMU_FAT_DISK_IMAGE}" ]; then
  QEMU_DRIVE_ARGS+=(-drive "id=fatdisk,file=${QEMU_FAT_DISK_IMAGE},if=none,format=raw")
fi

check_dependencies

# 设置无图形界面模式
QEMU_NOGRAPHIC=false

KERNEL_CMDLINE=" "

# 自动测试选项，支持的选项：
# - none: 不进行自动测试
# - syscall: 进行gvisor系统调用测试
# - dunit: 进行dunitest测试
AUTO_TEST=${AUTO_TEST:=none}
# gvisor测试目录
SYSCALL_TEST_DIR=${SYSCALL_TEST_DIR:=/opt/tests/gvisor}
# dunitest测试目录
DUNITEST_DIR=${DUNITEST_DIR:=/opt/tests/dunitest}

BIOS_TYPE=""
#这个变量为true则使用virtio磁盘
VIRTIO_BLK_DEVICE=true

# 如果qemu_accel不为空
if [ -n "${qemu_accel}" ]; then
    QEMU_ACCEL_ARGS=(-machine "accel=${qemu_accel}")
  if [ "${qemu_accel}" == "kvm" ]; then
    QEMU_ACCEL_ARGS+=(-enable-kvm)
  fi
fi

if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    # 根据加速方式设置机器参数
    # 仅在KVM加速时禁用HPET，以使用KVM clock作为时钟源
    # 非KVM加速（如TCG）时保留HPET，因为kvm-clock不可用
    if [ "${qemu_accel}" == "kvm" ]; then
        # KVM加速：禁用HPET，使用kvm-clock（性能更好且延迟更低）
        QEMU_MACHINE_ARGS=(-machine "q35,hpet=off")
    else
        # 非KVM加速
        QEMU_MACHINE_ARGS=(-machine q35)
    fi
    # 根据加速方式选择CPU型号：KVM使用host，TCG使用IvyBridge
    cpu_model=$([ "${qemu_accel}" == "kvm" ] && echo "host" || echo "IvyBridge")
    if [ -n "${allflags}" ]; then
      QEMU_CPU_ARGS=(-cpu "${cpu_model},apic,x2apic,+fpu,check,+vmx,${allflags}")
    else
      QEMU_CPU_ARGS=(-cpu "${cpu_model},apic,x2apic,+fpu,check,+vmx")
    fi
    # RTC配置：clock=host 使guest使用host的时钟源，支持kvm-clock
    # base=localtime 设置RTC基准时间为本地时间
    QEMU_RTC_ARGS=(-rtc clock=host,base=localtime)
    if [ ${VIRTIO_BLK_DEVICE} == false ]; then
      QEMU_DEVICE_DISK_ARGS=(-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0)
    else
      QEMU_DEVICE_DISK_ARGS=(-device virtio-blk-pci,drive=disk -device pci-bridge,chassis_nr=1,id=pci.1 -device pcie-root-port)
    fi
    if [ -f "${QEMU_EXT4_DISK_IMAGE}" ]; then
      QEMU_DEVICE_DISK_ARGS+=(-device virtio-blk-pci,drive=ext4disk)
    fi
    if [ -f "${QEMU_FAT_DISK_IMAGE}" ]; then
      QEMU_DEVICE_DISK_ARGS+=(-device virtio-blk-pci,drive=fatdisk)
    fi

    # 可选启用 vsock 设备（默认关闭）。
    if [ "${QEMU_ENABLE_VSOCK}" = "1" ]; then
      if [ "${QEMU_VSOCK_GUEST_CID}" = "2" ]; then
        echo "[WARN] guest CID=2 conflicts with host CID=2; skip vhost-vsock-pci"
      elif [ ! -e /dev/vhost-vsock ]; then
        echo "[WARN] /dev/vhost-vsock not found; skip vsock device"
        echo "[WARN] Hint: sudo modprobe vhost_vsock"
      else
        QEMU_ATTACH_VSOCK=1
      fi
    else
      echo "[INFO] vsock disabled by script config (QEMU_ENABLE_VSOCK=0)"
    fi

elif [ ${ARCH} == "riscv64" ]; then
    QEMU_MACHINE_ARGS=(-machine virt)
    QEMU_CPU_ARGS=(-cpu sifive-u54)
    QEMU_RTC_ARGS=()
    QEMU_DEVICE_DISK_ARGS=(-device virtio-blk-device,drive=disk)
elif [ ${ARCH} == "loongarch64" ]; then
    QEMU_MACHINE_ARGS=(-machine virt)
    QEMU_CPU_ARGS=()
    QEMU_RTC_ARGS=()
    QEMU_DEVICE_DISK_ARGS=(-device virtio-blk-pci,drive=disk -device pci-bridge,chassis_nr=1,id=pci.1 -device pcie-root-port)
else
    echo "Unsupported architecture: ${ARCH}"
    exit 1
fi

if [ ${ARCH} == "riscv64" ]; then
# 如果是riscv64架构，就不需要图形界面
    QEMU_NOGRAPHIC=true
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
              QEMU_DISPLAY_ARGS=(-display vnc=:00)
              ;;
              window)
              ;;
              nographic)
              QEMU_NOGRAPHIC=true

              ;;
        esac;shift 2;;
        *) break
      esac
  done

setup_kernel_init_program() {
    if [ ${ARCH} == "x86_64" ]; then
        KERNEL_CMDLINE+=" init=/bin/busybox init AUTO_TEST=${AUTO_TEST} SYSCALL_TEST_DIR=${SYSCALL_TEST_DIR} DUNITEST_DIR=${DUNITEST_DIR} "
        # KERNEL_CMDLINE+=" init=/bin/dragonreach "
    elif [ ${ARCH} == "riscv64" ]; then
        KERNEL_CMDLINE+=" init=/bin/riscv_rust_init "
    fi
}

# 检测环境变量并设置内核命令行参数
setup_kernel_cmdline_from_env() {
    # 检测 DRAGONOS_LOGLEVEL 环境变量
    # 设置内核日志级别，支持0-7:
    # 0: EMERG   1: ALERT   2: CRIT   3: ERR
    # 4: WARN    5: NOTICE  6: INFO   7: DEBUG
    if [ -n "${DRAGONOS_LOGLEVEL}" ]; then
        KERNEL_CMDLINE+=" loglevel=${DRAGONOS_LOGLEVEL} "
        echo "[INFO] Setting kernel loglevel to ${DRAGONOS_LOGLEVEL} from environment variable"
    fi

    # 检测其他环境变量可以在这里添加
    # 例如：
    # if [ -n "${DRAGONOS_DEBUG}" ]; then
    #     KERNEL_CMDLINE+=" debug "
    # fi
}

# 设置内核init程序
setup_kernel_init_program

# 从环境变量设置内核命令行参数
setup_kernel_cmdline_from_env


if [ ${QEMU_NOGRAPHIC} == true ]; then
    QEMU_SERIAL_ARGS=(-serial chardev:mux -monitor chardev:mux -chardev "stdio,id=mux,mux=on,signal=off,logfile=${QEMU_SERIAL_LOG_FILE}")

    # 添加 virtio console 设备
    if [ ${ARCH} == "x86_64" ]; then
      QEMU_DEVICE_ARGS+=(-device virtio-serial -device virtconsole,chardev=mux)
    elif [ ${ARCH} == "loongarch64" ]; then
      QEMU_DEVICE_ARGS+=(-device virtio-serial -device virtconsole,chardev=mux)
    elif [ ${ARCH} == "riscv64" ]; then
      QEMU_DEVICE_ARGS+=(-device virtio-serial-device -device virtconsole,chardev=mux)
    fi

    KERNEL_CMDLINE=" console=/dev/hvc0 ${KERNEL_CMDLINE}"
    QEMU_MONITOR_ARGS=()
    QEMU_ARGS+=(--nographic)

    KERNEL_CMDLINE=$(echo "${KERNEL_CMDLINE}" | sed 's/^[ \t]*//;s/[ \t]*$//')

    if [ ${ARCH} == "x86_64" ]; then
      QEMU_ARGS+=(-kernel ../bin/kernel/kernel.elf -append "${KERNEL_CMDLINE}")
    elif [ ${ARCH} == "loongarch64" ]; then
      QEMU_ARGS+=(-kernel ../bin/kernel/kernel.elf -append "${KERNEL_CMDLINE}")
    elif [ ${ARCH} == "riscv64" ]; then
      QEMU_ARGS+=(-append "${KERNEL_CMDLINE}")
    fi
fi


# ps: 下面这条使用tap的方式，无法dhcp获取到ip，暂时不知道为什么
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -net nic,netdev=nic0 -netdev tap,id=nic0,model=virtio-net-pci,script=qemu/ifup-nat,downscript=qemu/ifdown-nat -usb -device qemu-xhci,id=xhci,p2=8,p3=4 "
QEMU_DEVICE_ARGS+=("${QEMU_DEVICE_DISK_ARGS[@]}")
QEMU_DEVICE_ARGS+=(
  -netdev "user,id=hostnet0,hostfwd=tcp::${HOST_PORT}-:12580"
  -device "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0"
  -usb
  -device "qemu-xhci,id=xhci,p2=8,p3=4"
) 

if [ "${QEMU_ATTACH_VSOCK}" = "1" ]; then
  QEMU_DEVICE_ARGS+=(-device "${QEMU_VSOCK_DEVICE_MODEL},guest-cid=${QEMU_VSOCK_GUEST_CID}")
  echo "[INFO] enable vsock device: ${QEMU_VSOCK_DEVICE_MODEL},guest-cid=${QEMU_VSOCK_GUEST_CID}"
fi
# E1000E
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -netdev user,id=hostnet0,hostfwd=tcp::12580-:12580 -net nic,model=e1000e,netdev=hostnet0,id=net0 -netdev user,id=hostnet1,hostfwd=tcp::12581-:12581 -device virtio-net-pci,vectors=5,netdev=hostnet1,id=net1 -usb -device qemu-xhci,id=xhci,p2=8,p3=4 " 


QEMU_ARGS+=(
  -m "${QEMU_MEMORY}"
  -smp "${QEMU_SMP}"
  -boot order=d
)
QEMU_ARGS+=("${QEMU_MONITOR_ARGS[@]}")
QEMU_ARGS+=("${QEMU_DISPLAY_ARGS[@]}")
QEMU_ARGS+=(-d "${qemu_trace_std}")

QEMU_ARGS+=(
  "${QEMU_MACHINE_ARGS[@]}"
  "${QEMU_CPU_ARGS[@]}"
  "${QEMU_RTC_ARGS[@]}"
  "${QEMU_SERIAL_ARGS[@]}"
  "${QEMU_DRIVE_ARGS[@]}"
  "${QEMU_DEVICE_ARGS[@]}"
)
QEMU_ARGS+=("${QEMU_ACCEL_ARGS[@]}")

QEMU_ARGS+=(-D ../qemu.log)

# GDB调试支持（默认不暂停CPU；需要暂停请显式设置 QEMU_GDB_WAIT=1）
QEMU_ARGS+=(-gdb "tcp::${GDB_PORT}")
if [ "${QEMU_GDB_WAIT}" == "1" ]; then
  QEMU_ARGS+=(-S)
fi


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

  # 清理旧的PID文件
  rm -f "${VMSTATE_DIR}/pid"

  # 启动QEMU的函数
  launch_qemu() {
    local -a bios_args=()
    if [ $# -gt 0 ]; then
      bios_args=("$@")
    fi
    echo "[QEMU] 启动中... (网络端口: ${HOST_PORT}, GDB端口: ${GDB_PORT})"
    if [ "${QEMU_GDB_WAIT}" == "1" ]; then
      echo "[QEMU] 等待GDB连接... (使用 'make gdb' 连接)"
    fi
    local -a cmd=("${QEMU}" "${bios_args[@]}" "${QEMU_ARGS[@]}")
    printf '[QEMU] 执行: sudo ' >&2
    printf '%q ' "${cmd[@]}" >&2
    printf '\n' >&2
    sudo bash -c 'pidfile="$1"; shift; echo $$ > "$pidfile"; exec "$@"' bash "${VMSTATE_DIR}/pid" "${cmd[@]}"
  }

  if [ ${BIOS_TYPE} == uefi ] ;then
    if [ ${ARCH} == x86_64 ] ;then
      launch_qemu -bios arch/x86_64/efi/OVMF-pure-efi.fd
    elif [ ${ARCH} == i386 ] ;then
      launch_qemu -bios arch/i386/efi/OVMF-pure-efi.fd
    elif [ ${ARCH} == riscv64 ] ;then
      install_riscv_uboot
      launch_qemu -kernel "${RISCV64_UBOOT_PATH}/u-boot.bin"
    else
      echo "不支持的架构: ${ARCH}"
    fi
  else
    # 如果是i386架构或者x86_64架构，就直接启动
    if [ ${ARCH} == x86_64 ] || [ ${ARCH} == i386 ] ;then
      launch_qemu
    elif [ ${ARCH} == riscv64 ] ;then
      # 如果是riscv64架构，就与efi启动一样
      install_riscv_uboot
      launch_qemu -kernel "${RISCV64_UBOOT_PATH}/u-boot.bin"
    elif [ ${ARCH} == loongarch64 ] ;then
      launch_qemu
    else
      echo "不支持的架构: ${ARCH}"
    fi
  fi
else
  echo "不满足运行条件"
fi
