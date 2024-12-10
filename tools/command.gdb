set breakpoint pending on
# 设置成QEMU的路径
file /home/fz/qemu-debug/build/install_dir/usr/local/bin/qemu-system-x86_64

handle SIGUSR2 noprint nostop
handle SIGUSR1 noprint nostop

# 仅供参考，请添加任何你需要的断点。
b accel/tcg/cpu-exec.c:996
b accel/tcg/cpu-exec.c:1047
b roms/edk2/OvmfPkg/Library/CcExitLib/CcExitVeHandler.c:752
b target/i386/hvf/hvf.c:116
b target/i386/hvf/hvf.c:117





run -d ../bin/disk-x86_64.img -m 512M -smp 2,cores=2,threads=1,sockets=1 -boot order=d  -d cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq* -s -machine q35,memory-backend=dragonos-qemu-shm.ram -cpu IvyBridge,apic,x2apic,+fpu,check,+vmx, -rtc clock=host,base=localtime -serial file:../serial_opt.txt -drive id=disk,file=../bin/disk-x86_64.img,if=none -device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -netdev user,id=hostnet0,hostfwd=tcp::12580-:12580 -device virtio-net-pci,vectors=5,netdev=hostnet0,id=net0 -usb -device qemu-xhci,id=xhci,p2=8,p3=4 -object memory-backend-file,size=512M,id=dragonos-qemu-shm.ram,mem-path=/dev/shm/dragonos-qemu-shm.ram,share=on -machine accel=kvm -enable-kvm &