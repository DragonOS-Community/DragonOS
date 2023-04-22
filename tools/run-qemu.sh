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
ARCH="x86_64"
#ARCH="i386"
# 请根据自己的需要，在-d 后方加入所需的 trace 事件

# 标准的trace events
qemu_trace_std=cpu_reset,guest_errors,exec,cpu,trace:virtio*
# 调试usb的trace
qemu_trace_usb=trace:usb_xhci_reset,trace:usb_xhci_run,trace:usb_xhci_stop,trace:usb_xhci_irq_msi,trace:usb_xhci_irq_msix,trace:usb_xhci_port_reset,trace:msix_write_config,trace:usb_xhci_irq_msix,trace:usb_xhci_irq_msix_use,trace:usb_xhci_irq_msix_unuse,trace:usb_xhci_irq_msi,trace:usb_xhci_*
qemu_accel=kvm
if [ $(uname) == Darwin ]; then
    qemu_accel=hvf
fi

QEMU=qemu-system-x86_64
QEMU_DISK_IMAGE="../bin/disk.img"
QEMU_MEMORY="512M"
QEMU_SMP="2,cores=2,threads=1,sockets=1"
QEMU_MONITOR="stdio"
QEMU_TRACE="${qemu_trace_std}"
QEMU_CPU_FEATURES="IvyBridge,apic,x2apic,+fpu,check,${allflags}"
QEMU_RTC_CLOCK="clock=host,base=localtime"
QEMU_SERIAL="file:../serial_opt.txt"
QEMU_DRIVE="id=disk,file=${QEMU_DISK_IMAGE},if=none"


# ps: 下面这条使用tap的方式，无法dhcp获取到ip，暂时不知道为什么
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -net nic,netdev=nic0 -netdev tap,id=nic0,model=virtio-net-pci,script=qemu/ifup-nat,downscript=qemu/ifdown-nat -usb -device qemu-xhci,id=xhci,p2=8,p3=4 -machine accel=${qemu_accel} -machine q35 "
QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -nic user,model=virtio-net-pci,hostfwd=tcp::12580-:12580 -usb -device qemu-xhci,id=xhci,p2=8,p3=4 -machine accel=${qemu_accel} -machine q35 "

QEMU_ARGUMENT="-d ${QEMU_DISK_IMAGE} -m ${QEMU_MEMORY} -smp ${QEMU_SMP} -boot order=d -monitor ${QEMU_MONITOR} -d ${qemu_trace_std} "

QEMU_ARGUMENT+="-s -S -cpu ${QEMU_CPU_FEATURES} -rtc ${QEMU_RTC_CLOCK} -serial ${QEMU_SERIAL} -drive ${QEMU_DRIVE} ${QEMU_DEVICES}"

if [ $flag_can_run -eq 1 ]; then
  while true;do
    case "$1" in
        --bios) 
        case "$2" in
              uefi) #uefi启动新增ovmf.fd固件
              BIOS_TYPE=uefi
            ;;
              legacy)
              BIOS_TYPE=lagacy
              ;;
        esac;shift 2;;
        --display)
        case "$2" in
              vnc)
              QEMU_ARGUMENT+=" -display vnc=:00"
              ;;
              window)
              ;;
        esac;shift 2;;
        *) break
      esac 
  done 

if [ ${BIOS_TYPE} == uefi ] ;then
  if [ ${ARCH} == x86_64 ] ;then
    sudo ${QEMU} -bios arch/x86_64/efi/OVMF-pure-efi.fd ${QEMU_ARGUMENT}
  elif [ ${ARCH} == i386 ] ;then
    sudo ${QEMU} -bios arch/i386/efi/OVMF-pure-efi.fd ${QEMU_ARGUMENT}
  fi
else
  sudo ${QEMU} ${QEMU_ARGUMENT}
fi

else
  echo "不满足运行条件"
fi
