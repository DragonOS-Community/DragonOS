target remote localhost:1234
file bin/kernel/kernel.elf
set follow-fork-mode child 
b src/libs/libUI/screen_manager.rs:388