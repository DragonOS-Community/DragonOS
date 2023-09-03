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
#include <driver/virtio/virtio.h>
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

struct thread_struct initial_thread = {
    .rbp = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),
    .rsp = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),
    .fs = KERNEL_DS,
    .gs = KERNEL_DS,
    .cr2 = 0,
    .trap_num = 0,
    .err_code = 0,
};

// 初始化 初始进程的union ，并将其链接到.data.init_proc段内
union proc_union initial_proc_union
    __attribute__((__section__(".data.init_proc_union"))) = {0};

struct process_control_block *initial_proc[MAX_CPU_NUM] = {&initial_proc_union.pcb, 0};

// 为每个核心初始化初始进程的tss
struct tss_struct initial_tss[MAX_CPU_NUM] = {[0 ... MAX_CPU_NUM - 1] = INITIAL_TSS};

/**
 * @brief 切换进程
 *
 * @param prev 上一个进程的pcb
 * @param next 将要切换到的进程的pcb
 * 由于程序在进入内核的时候已经保存了寄存器，因此这里不需要保存寄存器。
 * 这里切换fs和gs寄存器
 */
#pragma GCC push_options
#pragma GCC optimize("O0")
void __switch_to(struct process_control_block *prev, struct process_control_block *next)
{
    initial_tss[proc_current_cpu_id].rsp0 = next->thread->rbp;
    // kdebug("next_rsp = %#018lx   ", next->thread->rsp);
    //  set_tss64((uint *)phys_2_virt(TSS64_Table), initial_tss[0].rsp0, initial_tss[0].rsp1, initial_tss[0].rsp2,
    //  initial_tss[0].ist1,
    //           initial_tss[0].ist2, initial_tss[0].ist3, initial_tss[0].ist4, initial_tss[0].ist5,
    //           initial_tss[0].ist6, initial_tss[0].ist7);

    __asm__ __volatile__("movq	%%fs,	%0 \n\t"
                         : "=a"(prev->thread->fs));
    __asm__ __volatile__("movq	%%gs,	%0 \n\t"
                         : "=a"(prev->thread->gs));

    __asm__ __volatile__("movq	%0,	%%fs \n\t" ::"a"(next->thread->fs));
    __asm__ __volatile__("movq	%0,	%%gs \n\t" ::"a"(next->thread->gs));
}
#pragma GCC pop_options

/**
 * @brief 切换进程的fs、gs寄存器
 * 注意，fs、gs的值在return的时候才会生效，因此本函数不能简化为一个单独的宏
 * @param fs 目标fs值
 * @param gs 目标gs值
 */
void process_switch_fsgs(uint64_t fs, uint64_t gs)
{
    asm volatile("movq	%0,	%%fs \n\t" ::"a"(fs));
    asm volatile("movq	%0,	%%gs \n\t" ::"a"(gs));
}

/**
 * @brief 初始化实时进程rt_pcb
 *
 * @return 初始化后的进程
 *
 */
struct process_control_block *process_init_rt_pcb(struct process_control_block *rt_pcb)
{
    // 暂时将实时进程的优先级设置为10
    rt_pcb->priority = 10;
    rt_pcb->policy = SCHED_RR;
    rt_pcb->rt_time_slice = 80;
    rt_pcb->virtual_runtime = 0x7fffffffffffffff;
    return rt_pcb;
}

/**
 * @brief 内核init进程
 *
 * @param arg
 * @return ul 参数
 */
#pragma GCC push_options
#pragma GCC optimize("O0")
ul initial_kernel_thread(ul arg)
{
    kinfo("initial proc running...\targ:%#018lx, vruntime=%d", arg, current_pcb->virtual_runtime);
    int val = 0;
    val = scm_enable_double_buffer();
    io_mfence();
    rs_init_stdio();
    io_mfence();
    // block_io_scheduler_init();
    ahci_init();
    mount_root_fs();
    io_mfence();
    rs_virtio_probe();
    io_mfence();

    // 使用单独的内核线程来初始化usb驱动程序
    // 注释：由于目前usb驱动程序不完善，因此先将其注释掉
    // int usb_pid = kernel_thread(usb_init, 0, 0);

    kinfo("LZ4 lib Version=%s", LZ4_versionString());
    io_mfence();
    __rust_demo_func();
    io_mfence();

    // 准备切换到用户态
    struct pt_regs *regs;

    // 若在后面这段代码中触发中断，return时会导致段选择子错误，从而触发#GP，因此这里需要cli
    cli();
    current_pcb->thread->rip = (ul)ret_from_intr;
    current_pcb->thread->rsp = (ul)current_pcb + STACK_SIZE - sizeof(struct pt_regs);
    current_pcb->thread->fs = USER_DS | 0x3;
    barrier();
    current_pcb->thread->gs = USER_DS | 0x3;
    process_switch_fsgs(current_pcb->thread->fs, current_pcb->thread->gs);

    // 主动放弃内核线程身份
    current_pcb->flags &= (~PF_KTHREAD);
    kdebug("in initial_kernel_thread: flags=%ld", current_pcb->flags);

    regs = (struct pt_regs *)current_pcb->thread->rsp;
    // kdebug("current_pcb->thread->rsp=%#018lx", current_pcb->thread->rsp);
    current_pcb->flags = 0;
    // 将返回用户层的代码压入堆栈，向rdx传入regs的地址，然后jmp到do_execve这个系统调用api的处理函数
    // 这里的设计思路和switch_to类似 加载用户态程序：shell.elf
    __asm__ __volatile__("movq %1, %%rsp   \n\t"
                         "pushq %2    \n\t"
                         "jmp rs_exec_init_process  \n\t" ::"D"(current_pcb->thread->rsp),
                         "m"(current_pcb->thread->rsp), "m"(current_pcb->thread->rip), "c"(NULL),
                         "d"(NULL)
                         : "memory");

    return 1;
}
#pragma GCC pop_options



/**
 * @brief 根据pid获取进程的pcb。存在对应的pcb时，返回对应的pcb的指针，否则返回NULL
 *  当进程管理模块拥有pcblist_lock之后，调用本函数之前，应当对其加锁
 * @param pid
 * @return struct process_control_block*
 */
struct process_control_block *process_find_pcb_by_pid(pid_t pid)
{
    // todo: 当进程管理模块拥有pcblist_lock之后，对其加锁
    struct process_control_block *pcb = initial_proc_union.pcb.next_pcb;
    // 使用蛮力法搜索指定pid的pcb
    // todo: 使用哈希表来管理pcb
    for (; pcb != &initial_proc_union.pcb; pcb = pcb->next_pcb)
    {
        if (pcb->pid == pid)
            return pcb;
    }
    return NULL;
}

/**
 * @brief 将进程加入到调度器的就绪队列中.
 *
 * @param pcb 进程的pcb
 *
 * @return true 成功加入调度队列
 * @return false 进程已经在运行
 */
int process_wakeup(struct process_control_block *pcb)
{

    BUG_ON(pcb == NULL);
    if (pcb == NULL)
        return -EINVAL;
    // 如果pcb正在调度队列中，则不重复加入调度队列
    if (pcb->state & PROC_RUNNING)
        return 0;

    pcb->state |= PROC_RUNNING;
    sched_enqueue_old(pcb, true);
    return 0;
}

/**
 * @brief 将进程加入到调度器的就绪队列中，并标志当前进程需要被调度
 *
 * @param pcb 进程的pcb
 */
int process_wakeup_immediately(struct process_control_block *pcb)
{
    kerror("FIXME: process_wakeup_immediately");
    while (1)
        ;

    return 0;
}

/**
 * @brief 给pcb设置名字
 *
 * @param pcb 需要设置名字的pcb
 * @param pcb_name 保存名字的char数组
 */
static void __set_pcb_name(struct process_control_block *pcb, const char *pcb_name)
{
    // todo:给pcb加锁
    //  spin_lock(&pcb->alloc_lock);
    strncpy(pcb->name, pcb_name, PCB_NAME_LEN);
    // spin_unlock(&pcb->alloc_lock);
}

