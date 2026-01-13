target remote localhost:{{GDB_PORT}}
file bin/kernel/kernel.elf
set follow-fork-mode child
