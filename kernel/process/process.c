#include "process.h"

#include "../exception/gate.h"
#include "../common/printk.h"
#include "../common/kprint.h"
#include "../syscall/syscall.h"
#include "../syscall/syscall_num.h"
#include <mm/slab.h>
#include <sched/sched.h>
#include <filesystem/fat32/fat32.h>
#include <common/stdio.h>
#include <process/spinlock.h>

spinlock_t process_global_pid_write_lock; // 增加pid的写锁
long process_global_pid = 0;              // 系统中最大的pid

extern void system_call(void);
extern void kernel_thread_func(void);

/**
 * @brief 将进程加入到调度器的就绪队列中
 *
 * @param pcb 进程的pcb
 */
static inline void process_wakeup(struct process_control_block *pcb);

ul _stack_start; // initial proc的栈基地址（虚拟地址）
struct mm_struct initial_mm = {0};
struct thread_struct initial_thread =
    {
        .rbp = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),
        .rsp = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),
        .fs = KERNEL_DS,
        .gs = KERNEL_DS,
        .cr2 = 0,
        .trap_num = 0,
        .err_code = 0};

// 初始化 初始进程的union ，并将其链接到.data.init_proc段内
union proc_union initial_proc_union __attribute__((__section__(".data.init_proc_union"))) = {INITIAL_PROC(initial_proc_union.pcb)};

struct process_control_block *initial_proc[MAX_CPU_NUM] = {&initial_proc_union.pcb, 0};

// 为每个核心初始化初始进程的tss
struct tss_struct initial_tss[MAX_CPU_NUM] = {[0 ... MAX_CPU_NUM - 1] = INITIAL_TSS};

/**
 * @brief 拷贝当前进程的标志位
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_flags(uint64_t clone_flags, struct process_control_block *pcb);

/**
 * @brief 拷贝当前进程的文件描述符等信息
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_files(uint64_t clone_flags, struct process_control_block *pcb);

/**
 * @brief 回收进程的所有文件描述符
 *
 * @param pcb 要被回收的进程的pcb
 * @return uint64_t
 */
uint64_t process_exit_files(struct process_control_block *pcb);

/**
 * @brief 拷贝当前进程的内存空间分布结构体信息
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_mm(uint64_t clone_flags, struct process_control_block *pcb);

/**
 * @brief 释放进程的页表
 *
 * @param pcb 要被释放页表的进程
 * @return uint64_t
 */
uint64_t process_exit_mm(struct process_control_block *pcb);

/**
 * @brief 拷贝当前进程的线程结构体
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_thread(uint64_t clone_flags, struct process_control_block *pcb, uint64_t stack_start, uint64_t stack_size, struct pt_regs *current_regs);

void process_exit_thread(struct process_control_block *pcb);
/**
 * @brief 切换进程
 *
 * @param prev 上一个进程的pcb
 * @param next 将要切换到的进程的pcb
 * 由于程序在进入内核的时候已经保存了寄存器，因此这里不需要保存寄存器。
 * 这里切换fs和gs寄存器
 */

void __switch_to(struct process_control_block *prev, struct process_control_block *next)
{
    initial_tss[proc_current_cpu_id].rsp0 = next->thread->rbp;
    // kdebug("next_rsp = %#018lx   ", next->thread->rsp);
    //  set_tss64((uint *)phys_2_virt(TSS64_Table), initial_tss[0].rsp0, initial_tss[0].rsp1, initial_tss[0].rsp2, initial_tss[0].ist1,
    //           initial_tss[0].ist2, initial_tss[0].ist3, initial_tss[0].ist4, initial_tss[0].ist5, initial_tss[0].ist6, initial_tss[0].ist7);

    __asm__ __volatile__("movq	%%fs,	%0 \n\t"
                         : "=a"(prev->thread->fs));
    __asm__ __volatile__("movq	%%gs,	%0 \n\t"
                         : "=a"(prev->thread->gs));

    __asm__ __volatile__("movq	%0,	%%fs \n\t" ::"a"(next->thread->fs));
    __asm__ __volatile__("movq	%0,	%%gs \n\t" ::"a"(next->thread->gs));
    // wrmsr(0x175, next->thread->rbp);
}

/**
 * @brief 这是一个用户态的程序
 *
 */
void user_level_function()
{
    // kinfo("Program (user_level_function) is runing...");
    // kinfo("Try to enter syscall id 15...");
    // enter_syscall(15, 0, 0, 0, 0, 0, 0, 0, 0);

    // enter_syscall(SYS_PRINTF, (ul) "test_sys_printf\n", 0, 0, 0, 0, 0, 0, 0);
    // while(1);
    long ret = 0;
    //	printk_color(RED,BLACK,"user_level_function task is running\n");

    /*
    // 测试sys put string
    char string[] = "User level process.\n";
    long err_code = 1;
    ul addr = (ul)string;
    __asm__ __volatile__(
        "movq %2, %%r8 \n\t"
        "int $0x80   \n\t"
        : "=a"(err_code)
        : "a"(SYS_PUT_STRING), "m"(addr)
        : "memory", "r8");
    */
    while (1)
    {
        // 测试sys_open
        char string[] = "333.txt";
        long err_code = 1;
        int zero = 0;

        uint64_t addr = (ul)string;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_OPEN), "m"(addr), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");

        int fd_num = err_code;

        int count = 128;
        // while (count)
        //{
        uchar buf[128] = {0};
        // Test sys_read
        addr = (uint64_t)&buf;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_READ), "m"(fd_num), "m"(addr), "m"(count), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");
        count = err_code;
        // 将读取到的数据打印出来
        addr = (ul)buf;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_PUT_STRING), "m"(addr)
            : "memory", "r8");
        // SYS_WRITE
        char test1[] = "GGGGHHHHHHHHh112343";

        addr = (uint64_t)&test1;
        count = 19;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_WRITE), "m"(fd_num), "m"(addr), "m"(count), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");

        addr = 1;
        count = SEEK_SET;
        fd_num = 0;
        // Test lseek
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_LSEEK), "m"(fd_num), "m"(addr), "m"(count), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");

        // SYS_WRITE
        char test2[] = "K123456789K";

        addr = (uint64_t)&test2;
        count = 11;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_WRITE), "m"(fd_num), "m"(addr), "m"(count), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");
        // Test sys_close
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_CLOSE), "m"(fd_num), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");

        addr = (ul)string;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_OPEN), "m"(addr), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");
        fd_num = err_code;
        count = 128;
        // Test sys_read
        addr = (uint64_t)&buf;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "movq %3, %%r9 \n\t"
            "movq %4, %%r10 \n\t"
            "movq %5, %%r11 \n\t"
            "movq %6, %%r12 \n\t"
            "movq %7, %%r13 \n\t"
            "movq %8, %%r14 \n\t"
            "movq %9, %%r15 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_READ), "m"(fd_num), "m"(addr), "m"(count), "m"(zero), "m"(zero), "m"(zero), "m"(zero), "m"(zero)
            : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");
        count = err_code;
        // 将读取到的数据打印出来
        addr = (ul)buf;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_PUT_STRING), "m"(addr)
            : "memory", "r8");

        // Test Sys
        //}

        while (1)
            pause();
    }
    while (1)
        pause();
}

/**
 * @brief 打开要执行的程序文件
 *
 * @param path
 * @return struct vfs_file_t*
 */
struct vfs_file_t *process_open_exec_file(char *path)
{
    struct vfs_dir_entry_t *dentry = NULL;
    struct vfs_file_t *filp = NULL;

    dentry = vfs_path_walk(path, 0);

    if (dentry == NULL)
        return (void *)-ENOENT;
    if (dentry->dir_inode->attribute == VFS_ATTR_DIR)
        return (void *)-ENOTDIR;

    filp = (struct vfs_file_t *)kmalloc(sizeof(struct vfs_file_t), 0);
    if (filp == NULL)
        return (void *)-ENOMEM;

    filp->position = 0;
    filp->mode = 0;
    filp->dEntry = dentry;
    filp->mode = ATTR_READ_ONLY;
    filp->file_ops = dentry->dir_inode->file_ops;

    return filp;
}
/**
 * @brief 使当前进程去执行新的代码
 *
 * @param regs 当前进程的寄存器
 * @param path 可执行程序的路径
 * @return ul 错误码
 */
ul do_execve(struct pt_regs *regs, char *path)
{
    // 选择这两个寄存器是对应了sysexit指令的需要
    regs->rip = 0x800000; // rip 应用层程序的入口地址   这里的地址选择没有特殊要求，只要是未使用的内存区域即可。
    regs->rsp = 0xa00000; // rsp 应用层程序的栈顶地址
    regs->cs = USER_CS | 3;
    regs->ds = USER_DS | 3;
    regs->ss = USER_DS | 0x3;
    regs->rflags = 0x200246;
    regs->rax = 1;
    regs->es = 0;
    
    kdebug("do_execve is running...");

    // 当前进程正在与父进程共享地址空间，需要创建
    // 独立的地址空间才能使新程序正常运行
    if (current_pcb->flags & PF_VFORK)
    {
        kdebug("proc:%d  creating new mem space", current_pcb->pid);
        // 分配新的内存空间分布结构体
        struct mm_struct *new_mms = (struct mm_struct *)kmalloc(sizeof(struct mm_struct), 0);
        memset(new_mms, 0, sizeof(struct mm_struct));
        current_pcb->mm = new_mms;

        // 分配顶层页表, 并设置顶层页表的物理地址
        new_mms->pgd = (pml4t_t *)virt_2_phys(kmalloc(PAGE_4K_SIZE, 0));

        // 由于高2K部分为内核空间，在接下来需要覆盖其数据，因此不用清零
        memset(phys_2_virt(new_mms->pgd), 0, PAGE_4K_SIZE / 2);

        // 拷贝内核空间的页表指针
        memcpy(phys_2_virt(new_mms->pgd) + 256, phys_2_virt(initial_proc[proc_current_cpu_id]) + 256, PAGE_4K_SIZE / 2);
    }
    /**
     * @todo: 加载elf文件并映射对应的页
     *
     */
    // 映射1个2MB的物理页
    unsigned long code_start_addr = 0x800000;
    unsigned long stack_start_addr = 0xa00000;

    mm_map_proc_page_table((uint64_t)current_pcb->mm->pgd, true, code_start_addr, alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys, PAGE_2M_SIZE, PAGE_USER_PAGE, true);

    process_switch_mm(current_pcb);

    // 为用户态程序设置地址边界
    if (!(current_pcb->flags & PF_KTHREAD))
        current_pcb->addr_limit = USER_MAX_LINEAR_ADDR;

    current_pcb->mm->code_addr_start = code_start_addr;
    current_pcb->mm->code_addr_end = 0;
    current_pcb->mm->data_addr_start = 0;
    current_pcb->mm->data_addr_end = 0;
    current_pcb->mm->rodata_addr_start = 0;
    current_pcb->mm->rodata_addr_end = 0;
    current_pcb->mm->bss_start = 0;
    current_pcb->mm->bss_end = 0;
    current_pcb->mm->brk_start = 0;
    current_pcb->mm->brk_end = 0;
    current_pcb->mm->stack_start = stack_start_addr;

    // 关闭之前的文件描述符
    process_exit_files(current_pcb);

    // 清除进程的vfork标志位
    current_pcb->flags &= ~PF_VFORK;



    struct vfs_file_t* filp = process_open_exec_file(path);
    if((unsigned long)filp <= 0 )
		return (unsigned long)filp;
    
    memset((void *)code_start_addr, 0, PAGE_2M_SIZE);
    uint64_t pos = 0;
    int retval = filp->file_ops->read(filp, code_start_addr, PAGE_2M_SIZE, &pos);
    kdebug("execve ok");

    return 0;
}

/**
 * @brief 内核init进程
 *
 * @param arg
 * @return ul 参数
 */
ul initial_kernel_thread(ul arg)
{
    // kinfo("initial proc running...\targ:%#018lx", arg);
    fat32_init();

    struct pt_regs *regs;

    current_pcb->thread->rip = (ul)ret_from_system_call;
    current_pcb->thread->rsp = (ul)current_pcb + STACK_SIZE - sizeof(struct pt_regs);
    current_pcb->thread->fs = USER_DS | 0x3;
    current_pcb->thread->gs = USER_DS | 0x3;

    // 主动放弃内核线程身份
    current_pcb->flags &= (~PF_KTHREAD);

    // current_pcb->mm->pgd = kmalloc(PAGE_4K_SIZE, 0);
    // memset((void*)current_pcb->mm->pgd, 0, PAGE_4K_SIZE);

    regs = (struct pt_regs *)current_pcb->thread->rsp;
    // kdebug("current_pcb->thread->rsp=%#018lx", current_pcb->thread->rsp);
    current_pcb->flags = 0;
    // 将返回用户层的代码压入堆栈，向rdx传入regs的地址，然后jmp到do_execve这个系统调用api的处理函数  这里的设计思路和switch_proc类似
    // 加载用户态程序：init.bin
    char init_path[] = "/init.bin";
    uint64_t addr = (uint64_t)&init_path;
    __asm__ __volatile__("movq %1, %%rsp   \n\t"
                         "pushq %2    \n\t"
                         "jmp do_execve  \n\t" ::"D"(current_pcb->thread->rsp),
                         "m"(current_pcb->thread->rsp), "m"(current_pcb->thread->rip), "S"("/init.bin")
                         : "memory");

    return 1;
}

/**
 * @brief 进程退出时执行的函数
 *
 * @param code 返回码
 * @return ul
 */
ul process_thread_do_exit(ul code)
{
    kinfo("thread_exiting..., code is %#018lx.", code);
    while (1)
        ;
}

/**
 * @brief 初始化内核进程
 *
 * @param fn 目标程序的地址
 * @param arg 向目标程序传入的参数
 * @param flags
 * @return int
 */

int kernel_thread(unsigned long (*fn)(unsigned long), unsigned long arg, unsigned long flags)
{
    struct pt_regs regs;
    memset(&regs, 0, sizeof(regs));

    // 在rbx寄存器中保存进程的入口地址
    regs.rbx = (ul)fn;
    // 在rdx寄存器中保存传入的参数
    regs.rdx = (ul)arg;

    regs.ds = KERNEL_DS;
    regs.es = KERNEL_DS;
    regs.cs = KERNEL_CS;
    regs.ss = KERNEL_DS;

    // 置位中断使能标志位
    regs.rflags = (1 << 9);

    // rip寄存器指向内核线程的引导程序
    regs.rip = (ul)kernel_thread_func;
    // kdebug("kernel_thread_func=%#018lx", kernel_thread_func);
    // kdebug("&kernel_thread_func=%#018lx", &kernel_thread_func);
    // kdebug("1111\tregs.rip = %#018lx", regs.rip);
    return do_fork(&regs, flags | CLONE_VM, 0, 0);
}

/**
 * @brief 初始化进程模块
 * ☆前置条件：已完成系统调用模块的初始化
 */
void process_init()
{
    kinfo("Initializing process...");
    initial_mm.pgd = (pml4t_t *)global_CR3;

    initial_mm.code_addr_start = memory_management_struct.kernel_code_start;
    initial_mm.code_addr_end = memory_management_struct.kernel_code_end;

    initial_mm.data_addr_start = (ul)&_data;
    initial_mm.data_addr_end = memory_management_struct.kernel_data_end;

    initial_mm.rodata_addr_start = (ul)&_rodata;
    initial_mm.rodata_addr_end = (ul)&_erodata;
    initial_mm.bss_start = (uint64_t)&_bss;
    initial_mm.bss_end = (uint64_t)&_ebss;

    initial_mm.brk_start = 0;
    initial_mm.brk_end = memory_management_struct.kernel_end;

    initial_mm.stack_start = _stack_start;

    initial_tss[proc_current_cpu_id].rsp0 = initial_thread.rbp;

    // ========= 在IDLE进程的顶层页表中添加对内核地址空间的映射 =====================

    // 由于IDLE进程的顶层页表的高地址部分会被后续进程所复制，为了使所有进程能够共享相同的内核空间，
    //  因此需要先在IDLE进程的顶层页表内映射二级页表

    uint64_t *idle_pml4t_vaddr = (uint64_t *)phys_2_virt((uint64_t)get_CR3() & (~0xfffUL));

    for (int i = 256; i < 512; ++i)
    {
        uint64_t *tmp = idle_pml4t_vaddr + i;
        if (*tmp == 0)
        {
            void *pdpt = kmalloc(PAGE_4K_SIZE, 0);
            memset(pdpt, 0, PAGE_4K_SIZE);
            set_pml4t(tmp, mk_pml4t(virt_2_phys(pdpt), PAGE_KERNEL_PGT));
        }
    }
    /*
    kdebug("initial_thread.rbp=%#018lx", initial_thread.rbp);
    kdebug("initial_tss[0].rsp1=%#018lx", initial_tss[0].rsp1);
    kdebug("initial_tss[0].ist1=%#018lx", initial_tss[0].ist1);
*/
    // 初始化pid的写锁
    spin_init(&process_global_pid_write_lock);

    // 初始化进程的循环链表
    list_init(&initial_proc_union.pcb.list);
    kernel_thread(initial_kernel_thread, 10, CLONE_FS | CLONE_SIGNAL); // 初始化内核进程
    initial_proc_union.pcb.state = PROC_RUNNING;
    initial_proc_union.pcb.preempt_count = 0;
    initial_proc_union.pcb.cpu_id = 0;
    // 获取新的进程的pcb
    // struct process_control_block *p = container_of(list_next(&current_pcb->list), struct process_control_block, list);

    // kdebug("Ready to switch...");
    //  切换到新的内核线程
    //  switch_proc(current_pcb, p);
}

/**
 * @brief fork当前进程
 *
 * @param regs 新的寄存器值
 * @param clone_flags 克隆标志
 * @param stack_start 堆栈开始地址
 * @param stack_size 堆栈大小
 * @return unsigned long
 */

unsigned long do_fork(struct pt_regs *regs, unsigned long clone_flags, unsigned long stack_start, unsigned long stack_size)
{
    int retval = 0;
    struct process_control_block *tsk = NULL;
    // kdebug("222\tregs.rip = %#018lx", regs->rip);

    // 为新的进程分配栈空间，并将pcb放置在底部
    tsk = (struct process_control_block *)kmalloc(STACK_SIZE, 0);
    kdebug("struct process_control_block ADDRESS=%#018lx", (uint64_t)tsk);

    if (tsk == NULL)
    {
        retval = -ENOMEM;
        return retval;
    }

    memset(tsk, 0, sizeof(struct process_control_block));
    // 将当前进程的pcb复制到新的pcb内
    memcpy(tsk, current_pcb, sizeof(struct process_control_block));

    // kdebug("current_pcb->flags=%#010lx", current_pcb->flags);

    // 将进程加入循环链表
    list_init(&tsk->list);

    // list_add(&initial_proc_union.pcb.list, &tsk->list);
    tsk->priority = 2;
    tsk->preempt_count = 0;

    // 增加全局的pid并赋值给新进程的pid
    spin_lock(&process_global_pid_write_lock);
    tsk->pid = process_global_pid++;

    // 加入到进程链表中
    tsk->next_pcb = initial_proc_union.pcb.next_pcb;
    initial_proc_union.pcb.next_pcb = tsk;
    tsk->parent_pcb = current_pcb;

    spin_unlock(&process_global_pid_write_lock);

    tsk->cpu_id = proc_current_cpu_id;
    tsk->state = PROC_UNINTERRUPTIBLE;
    list_init(&tsk->list);
    // list_add(&initial_proc_union.pcb.list, &tsk->list);

    retval = -ENOMEM;

    // 拷贝标志位
    if (process_copy_flags(clone_flags, tsk))
        goto copy_flags_failed;

    // 拷贝内存空间分布结构体
    if (process_copy_mm(clone_flags, tsk))
        goto copy_mm_failed;

    // 拷贝文件
    if (process_copy_files(clone_flags, tsk))
        goto copy_files_failed;

    // 拷贝线程结构体
    if (process_copy_thread(clone_flags, tsk, stack_start, stack_size, regs))
        goto copy_thread_failed;

    // 拷贝成功
    retval = tsk->pid;
    // 唤醒进程
    process_wakeup(tsk);

    return retval;

copy_thread_failed:;
    // 回收线程
    process_exit_thread(tsk);
copy_files_failed:;
    // 回收文件
    process_exit_files(tsk);
copy_mm_failed:;
    // 回收内存空间分布结构体
    process_exit_mm(tsk);
copy_flags_failed:;
    kfree(tsk);
    return retval;

    return 0;
}

/**
 * @brief 根据pid获取进程的pcb
 *
 * @param pid
 * @return struct process_control_block*
 */
struct process_control_block *process_get_pcb(long pid)
{
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
 * @brief 将进程加入到调度器的就绪队列中
 *
 * @param pcb 进程的pcb
 */
static inline void process_wakeup(struct process_control_block *pcb)
{
    pcb->state = PROC_RUNNING;
    sched_cfs_enqueue(pcb);
}

/**
 * @brief 拷贝当前进程的标志位
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_flags(uint64_t clone_flags, struct process_control_block *pcb)
{
    if (clone_flags & CLONE_VM)
        pcb->flags |= PF_VFORK;
    return 0;
}

/**
 * @brief 拷贝当前进程的文件描述符等信息
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_files(uint64_t clone_flags, struct process_control_block *pcb)
{
    int retval = 0;
    // 如果CLONE_FS被置位，那么子进程与父进程共享文件描述符
    // 文件描述符已经在复制pcb时被拷贝
    if (clone_flags & CLONE_FS)
        return retval;

    // 为新进程拷贝新的文件描述符
    for (int i = 0; i < PROC_MAX_FD_NUM; ++i)
    {
        if (current_pcb->fds[i] == NULL)
            continue;

        pcb->fds[i] = (struct vfs_file_t *)kmalloc(sizeof(struct vfs_file_t), 0);
        memcpy(pcb->fds[i], current_pcb->fds[i], sizeof(struct vfs_file_t));
    }

    return retval;
}

/**
 * @brief 回收进程的所有文件描述符
 *
 * @param pcb 要被回收的进程的pcb
 * @return uint64_t
 */
uint64_t process_exit_files(struct process_control_block *pcb)
{
    // 不与父进程共享文件描述符
    if (!(pcb->flags & PF_VFORK))
    {

        for (int i = 0; i < PROC_MAX_FD_NUM; ++i)
        {
            if (pcb->fds[i] == NULL)
                continue;
            kfree(pcb->fds[i]);
        }
    }
    // 清空当前进程的文件描述符列表
    memset(pcb->fds, 0, sizeof(struct vfs_file_t *) * PROC_MAX_FD_NUM);
}

/**
 * @brief 拷贝当前进程的内存空间分布结构体信息
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_mm(uint64_t clone_flags, struct process_control_block *pcb)
{
    int retval = 0;
    // 与父进程共享内存空间
    if (clone_flags & CLONE_VM)
    {
        kdebug("copy_vm\t\t current_pcb->mm->pgd=%#018lx", current_pcb->mm->pgd);
        pcb->mm = current_pcb->mm;

        return retval;
    }

    // 分配新的内存空间分布结构体
    struct mm_struct *new_mms = (struct mm_struct *)kmalloc(sizeof(struct mm_struct), 0);
    memset(new_mms, 0, sizeof(struct mm_struct));

    memcpy(new_mms, current_pcb->mm, sizeof(struct mm_struct));

    pcb->mm = new_mms;

    // 分配顶层页表, 并设置顶层页表的物理地址
    new_mms->pgd = (pml4t_t *)virt_2_phys(kmalloc(PAGE_4K_SIZE, 0));
    // 由于高2K部分为内核空间，在接下来需要覆盖其数据，因此不用清零
    memset(phys_2_virt(new_mms->pgd), 0, PAGE_4K_SIZE / 2);

    // 拷贝内核空间的页表指针
    memcpy(phys_2_virt(new_mms->pgd) + 256, phys_2_virt(initial_proc[proc_current_cpu_id]) + 256, PAGE_4K_SIZE / 2);

    pml4t_t *current_pgd = (pml4t_t *)phys_2_virt(current_pcb->mm->pgd);

    // 迭代地拷贝用户空间
    for (int i = 0; i <= 255; ++i)
    {
        // 当前页表项为空
        if ((current_pgd + i)->pml4t == 0)
            continue;

        // 分配新的二级页表
        pdpt_t *new_pdpt = (pdpt_t *)kmalloc(PAGE_4K_SIZE, 0);
        memset(new_pdpt, 0, PAGE_4K_SIZE);
        // 在新的一级页表中设置新的二级页表表项
        set_pml4t((uint64_t *)(current_pgd + i), mk_pml4t(virt_2_phys(new_pdpt), ((current_pgd + i)->pml4t) & 0xfffUL));

        pdpt_t *current_pdpt = (pdpt_t *)phys_2_virt((current_pgd + i)->pml4t & (~0xfffUL));
        // 设置二级页表
        for (int j = 0; j < 512; ++j)
        {
            if ((current_pdpt + j)->pdpt == 0)
                continue;
            // 分配新的三级页表
            pdt_t *new_pdt = (pdt_t *)kmalloc(PAGE_4K_SIZE, 0);
            memset(new_pdt, 0, PAGE_4K_SIZE);

            // 在新的二级页表中设置三级页表的表项
            set_pdpt((uint64_t *)new_pdpt, mk_pdpt(virt_2_phys(new_pdt), (current_pdpt + j)->pdpt & 0xfffUL));

            pdt_t *current_pdt = (pdt_t *)phys_2_virt((current_pdpt + j)->pdpt & (~0xfffUL));

            // 拷贝内存页
            for (int k = 0; k < 512; ++k)
            {
                if ((current_pdt + k)->pdt == 0)
                    continue;
                // 获取一个新页
                struct Page *pg = alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED);
                set_pdt((uint64_t *)(current_pdt + k), mk_pdt(pg->addr_phys, (current_pdt + k)->pdt & 0x1fffUL));

                // 拷贝数据
                memcpy(phys_2_virt(pg->addr_phys), phys_2_virt((current_pdt + k)->pdt & (~0x1fffUL)), PAGE_2M_SIZE);
            }
        }
    }

    return retval;
}

/**
 * @brief 释放进程的页表
 *
 * @param pcb 要被释放页表的进程
 * @return uint64_t
 */
uint64_t process_exit_mm(struct process_control_block *pcb)
{
    if (pcb->flags & CLONE_VM)
        return 0;
    if (pcb->mm == NULL)
    {
        kdebug("pcb->mm==NULL");
        return 0;
    }
    if (pcb->mm->pgd == NULL)
    {
        kdebug("pcb->mm->pgd==NULL");
        return 0;
    }
    // 获取顶层页表
    pml4t_t *current_pgd = (pml4t_t *)phys_2_virt(pcb->mm->pgd);

    // 迭代地释放用户空间
    for (int i = 0; i <= 255; ++i)
    {
        // 当前页表项为空
        if ((current_pgd + i)->pml4t == 0)
            continue;

        // 二级页表entry
        pdpt_t *current_pdpt = (pdpt_t *)phys_2_virt((current_pgd + i)->pml4t & (~0xfffUL));
        // 遍历二级页表
        for (int j = 0; j < 512; ++j)
        {
            if ((current_pdpt + j)->pdpt == 0)
                continue;

            // 三级页表的entry
            pdt_t *current_pdt = (pdt_t *)phys_2_virt((current_pdpt + j)->pdpt & (~0xfffUL));

            // 释放三级页表的内存页
            for (int k = 0; k < 512; ++k)
            {
                if ((current_pdt + k)->pdt == 0)
                    continue;
                // 释放内存页
                free_pages(Phy_to_2M_Page((current_pdt + k)->pdt & (~0x1fffUL)), 1);
            }
            // 释放三级页表
            kfree(current_pdt);
        }
        // 释放二级页表
        kfree(current_pdpt);
    }
    // 释放顶层页表
    kfree(current_pgd);

    // 释放内存空间分布结构体
    kfree(pcb->mm);

    return 0;
}
/**
 * @brief 拷贝当前进程的线程结构体
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
uint64_t process_copy_thread(uint64_t clone_flags, struct process_control_block *pcb, uint64_t stack_start, uint64_t stack_size, struct pt_regs *current_regs)
{
    // 将线程结构体放置在pcb后方
    struct thread_struct *thd = (struct thread_struct *)(pcb + 1);
    memset(thd, 0, sizeof(struct thread_struct));
    pcb->thread = thd;

    // 拷贝栈空间
    struct pt_regs *child_regs = (struct pt_regs *)((uint64_t)pcb + STACK_SIZE - sizeof(struct pt_regs));
    memcpy(child_regs, current_regs, sizeof(struct pt_regs));

    // 设置子进程的返回值为0
    child_regs->rax = 0;
    child_regs->rsp = stack_start;

    thd->rbp = (uint64_t)pcb + STACK_SIZE;
    thd->rsp = (uint64_t)child_regs;
    thd->fs = current_pcb->thread->fs;
    thd->gs = current_pcb->thread->gs;

    // 根据是否为内核线程，设置进程的开始执行的地址
    if (pcb->flags & PF_KTHREAD)
        thd->rip = (uint64_t)kernel_thread_func;
    else
        thd->rip = (uint64_t)ret_from_system_call;
    kdebug("new proc's ret addr = %#018lx\tchild_regs->rsp = %#018lx", child_regs->rbx, child_regs->rsp);
    return 0;
}

/**
 * @brief todo: 回收线程结构体
 *
 * @param pcb
 */
void process_exit_thread(struct process_control_block *pcb)
{
}