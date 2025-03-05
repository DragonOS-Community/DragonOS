target remote localhost:1234
file bin/kernel/kernel.elf
set follow-fork-mode child

b kernel/crates/rust-slabmalloc/src/sc.rs:254