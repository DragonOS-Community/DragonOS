target remote localhost:1234
file bin/kernel/kernel.elf
set follow-fork-mode child 
b:user/apps/shell/shell.c
