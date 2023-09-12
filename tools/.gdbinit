target remote localhost:1234
file bin/kernel/kernel.elf
set follow-fork-mode child
b sched/syscall.rs:26
b sched/core.rs:79
b sched/core.rs:82
b sched/core.rs:109
