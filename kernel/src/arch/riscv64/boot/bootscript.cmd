load ${devtype} ${devnum}:${distro_bootpart} ${kernel_addr_r} /boot/kernel.elf
if fdt addr -q ${fdt_addr_r}; then bootelf ${kernel_addr_r} ${fdt_addr_r};else bootelf ${kernel_addr_r} ${fdtcontroladdr};fi
