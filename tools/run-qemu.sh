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
    
    qemu-system-x86_64 -d ../bin/disk.img -m 512M -smp 2,cores=2,threads=1,sockets=1 \
    -boot order=d   \
    -monitor stdio -d ${qemu_trace_std} \
    -s -S -cpu IvyBridge,apic,x2apic,+fpu,check,${allflags} -rtc clock=host,base=localtime -serial file:../serial_opt.txt \
    -drive id=disk,file=../bin/disk.img,if=none \
    -device ahci,id=ahci \
    -device ide-hd,drive=disk,bus=ahci.0    \
    -net nic,model=virtio \
    -usb    \
    -device qemu-xhci,id=xhci,p2=8,p3=4 \
    -machine accel=${qemu_accel}
    
else
  echo "不满足运行条件"
fi