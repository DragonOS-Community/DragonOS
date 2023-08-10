target remote localhost:1234
add-symbol-file bin/kernel/kernel.elf
set follow-fork-mode child 
b Start_Kernel