#include "process.h"

#include <DragonOS/signal.h>
#include <common/compiler.h>
#include <common/elf.h>
#include <common/kprint.h>
#include <common/kthread.h>
#include <common/lz4.h>
#include <common/printk.h>
#include <common/spinlock.h>
#include <common/stdio.h>
#include <common/string.h>
#include <common/sys/wait.h>
#include <common/time.h>
#include <common/unistd.h>
#include <debug/bug.h>
#include <debug/traceback/traceback.h>
#include <driver/disk/ahci/ahci.h>
#include <exception/gate.h>
#include <ktest/ktest.h>
#include <mm/mmio.h>
#include <mm/slab.h>
#include <sched/sched.h>
#include <syscall/syscall.h>
#include <syscall/syscall_num.h>
extern int __rust_demo_func();
// #pragma GCC push_options
// #pragma GCC optimize("O0")

spinlock_t process_global_pid_write_lock; // 增加pid的写锁
long process_global_pid = 1;              // 系统中最大的pid

extern void system_call(void);
extern void kernel_thread_func(void);
extern void rs_procfs_unregister_pid(uint64_t);

ul _stack_start; // initial proc的栈基地址（虚拟地址）

extern void process_exit_sighand(struct process_control_block *pcb);
extern void process_exit_signal(struct process_control_block *pcb);
extern void initial_proc_init_signal(struct process_control_block *pcb);
extern void rs_process_exit_fpstate(struct process_control_block *pcb);
extern void rs_drop_address_space(struct process_control_block *pcb);

extern int rs_init_stdio();
extern uint64_t rs_exec_init_process(struct pt_regs *regs);


// 初始化 初始进程的union ，并将其链接到.data.init_proc段内
union proc_union initial_proc_union
    __attribute__((__section__(".data.init_proc_union"))) = {0};

struct process_control_block *initial_proc[MAX_CPU_NUM] = {&initial_proc_union.pcb, 0};

// 为每个核心初始化初始进程的tss
struct tss_struct initial_tss[MAX_CPU_NUM] = {[0 ... MAX_CPU_NUM - 1] = INITIAL_TSS};
