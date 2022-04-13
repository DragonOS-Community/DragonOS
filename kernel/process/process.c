#include "process.h"

#include "../exception/gate.h"
#include "../common/printk.h"
#include "../common/kprint.h"
#include "../syscall/syscall.h"
#include "../syscall/syscall_num.h"
#include <mm/slab.h>
#include <sched/sched.h>


extern void system_call(void);
ul _stack_start;    // initial proc的栈基地址（虚拟地址）
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
    //set_tss64((uint *)phys_2_virt(TSS64_Table), initial_tss[0].rsp0, initial_tss[0].rsp1, initial_tss[0].rsp2, initial_tss[0].ist1,
     //         initial_tss[0].ist2, initial_tss[0].ist3, initial_tss[0].ist4, initial_tss[0].ist5, initial_tss[0].ist6, initial_tss[0].ist7);

    __asm__ __volatile__("movq	%%fs,	%0 \n\t"
                         : "=a"(prev->thread->fs));
    __asm__ __volatile__("movq	%%gs,	%0 \n\t"
                         : "=a"(prev->thread->gs));

    __asm__ __volatile__("movq	%0,	%%fs \n\t" ::"a"(next->thread->fs));
    __asm__ __volatile__("movq	%0,	%%gs \n\t" ::"a"(next->thread->gs));
    //wrmsr(0x175, next->thread->rbp);
    uint color;
    if(proc_current_cpu_id == 0)
		color = WHITE;
	else
		color = YELLOW;

	
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
    //while(1);
    long ret = 0;
    //	printk_color(RED,BLACK,"user_level_function task is running\n");

    char string[] = "User level process.\n";
    /*
    __asm__ __volatile__("leaq	sysexit_return_address(%%rip),	%%rdx	\n\t"
                         "movq	%%rsp,	%%rcx		\n\t"
                         "sysenter			\n\t"
                         "sysexit_return_address:	\n\t"
                         : "=a"(ret)
                         : "0"(1), "D"(string)
                         : "memory");
                         */

    for (int i = 0;; ++i)
    {
        long err_code;
        ul addr = (ul)string;
        __asm__ __volatile__(
            "movq %2, %%r8 \n\t"
            "int $0x80   \n\t"
            : "=a"(err_code)
            : "a"(SYS_PRINTF), "m"(addr)
            : "memory", "r8");
    }
    // enter_syscall_int(SYS_PRINTF, (ul) "test_sys_printf\n", 0, 0, 0, 0, 0, 0, 0);
    //  kinfo("Return from syscall id 15...");

    while (1)
        ;
}
/**
 * @brief 使当前进程去执行新的代码
 *
 * @param regs 当前进程的寄存器
 * @return ul 错误码
 */
ul do_execve(struct pt_regs *regs)
{
    // 选择这两个寄存器是对应了sysexit指令的需要
    regs->rip = 0x800000; // rip 应用层程序的入口地址   这里的地址选择没有特殊要求，只要是未使用的内存区域即可。
    regs->rsp = 0xa00000; // rsp 应用层程序的栈顶地址
    regs->cs = USER_CS|3;
    regs->ds = USER_DS|3;
    regs->ss = USER_DS |0x3;
    regs->rflags = 0x200246;
    regs->rax = 1;
    regs->es = 0;

    // kdebug("do_execve is running...");

    // 映射起始页面
    // mm_map_proc_page_table(get_CR3(), true, 0x800000, alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys, PAGE_2M_SIZE, PAGE_USER_PAGE, true);

    uint64_t addr = 0x800000UL;

    unsigned long *tmp = phys_2_virt((unsigned long *)((unsigned long)get_CR3() & (~0xfffUL)) + ((addr >> PAGE_GDT_SHIFT) & 0x1ff));

    unsigned long *virtual = kmalloc(PAGE_4K_SIZE, 0);
    set_pml4t(tmp, mk_pml4t(virt_2_phys(virtual), PAGE_USER_PGT));

    tmp = phys_2_virt((unsigned long *)(*tmp & (~0xfffUL)) + ((addr >> PAGE_1G_SHIFT) & 0x1ff));
    virtual = kmalloc(PAGE_4K_SIZE, 0);
    set_pdpt(tmp, mk_pdpt(virt_2_phys(virtual), PAGE_USER_DIR));

    tmp = phys_2_virt((unsigned long *)(*tmp & (~0xfffUL)) + ((addr >> PAGE_2M_SHIFT) & 0x1ff));
    struct Page *p = alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED);
    set_pdt(tmp, mk_pdt(p->addr_phys, PAGE_USER_PAGE));

    flush_tlb();

    /*
        mm_map_phys_addr_user(addr, alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys, PAGE_2M_SIZE, PAGE_USER_PAGE);
     */
    if (!(current_pcb->flags & PF_KTHREAD))
        current_pcb->addr_limit = KERNEL_BASE_LINEAR_ADDR;
    // 将程序代码拷贝到对应的内存中
    memcpy((void *)0x800000, user_level_function, 1024);

    
    // kdebug("program copied!");
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

    struct pt_regs *regs;

    current_pcb->thread->rip = (ul)ret_from_system_call;
    current_pcb->thread->rsp = (ul)current_pcb + STACK_SIZE - sizeof(struct pt_regs);
    // current_pcb->mm->pgd = kmalloc(PAGE_4K_SIZE, 0);
    // memset((void*)current_pcb->mm->pgd, 0, PAGE_4K_SIZE);

    regs = (struct pt_regs *)current_pcb->thread->rsp;
    // kdebug("current_pcb->thread->rsp=%#018lx", current_pcb->thread->rsp);
    current_pcb->flags = 0;
    // 将返回用户层的代码压入堆栈，向rdx传入regs的地址，然后jmp到do_execve这个系统调用api的处理函数  这里的设计思路和switch_proc类似
    __asm__ __volatile__("movq %1, %%rsp   \n\t"
                         "pushq %2    \n\t"
                         "jmp do_execve  \n\t" ::"D"(regs),
                         "m"(current_pcb->thread->rsp), "m"(current_pcb->thread->rip)
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
 * @brief 导出内核线程的执行引导程序
 *  目的是还原执行现场(在kernel_thread中伪造的)
 * 执行到这里时，rsp位于栈顶，然后弹出寄存器值
 * 弹出之后还要向上移动7个unsigned long的大小，从而弹出额外的信息（详见pt_regs）
 */

extern void kernel_thread_func(void);
__asm__(
    "kernel_thread_func:	\n\t"
    "	popq	%r15	\n\t"
    "	popq	%r14	\n\t"
    "	popq	%r13	\n\t"
    "	popq	%r12	\n\t"
    "	popq	%r11	\n\t"
    "	popq	%r10	\n\t"
    "	popq	%r9	\n\t"
    "	popq	%r8	\n\t"
    "	popq	%rbx	\n\t"
    "	popq	%rcx	\n\t"
    "	popq	%rdx	\n\t"
    "	popq	%rsi	\n\t"
    "	popq	%rdi	\n\t"
    "	popq	%rbp	\n\t"
    "	popq	%rax	\n\t"
    "	movq	%rax,	%ds	\n\t"
    "	popq	%rax		\n\t"
    "	movq	%rax,	%es	\n\t"
    "	popq	%rax		\n\t"
    "	addq	$0x38,	%rsp	\n\t"
    /////////////////////////////////
    "	movq	%rdx,	%rdi	\n\t"
    "	callq	*%rbx		\n\t"
    "	movq	%rax,	%rdi	\n\t"
    "	callq	process_thread_do_exit		\n\t");

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

    return do_fork(&regs, flags, 0, 0);
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

    initial_mm.brk_start = 0;
    initial_mm.brk_end = memory_management_struct.kernel_end;

    initial_mm.stack_start = _stack_start;
    
    /*
    // 向MSR寄存器组中的 IA32_SYSENTER_CS寄存器写入内核的代码段的地址
    wrmsr(0x174, KERNEL_CS);
    // 向MSR寄存器组中的 IA32_SYSENTER_ESP寄存器写入内核进程的rbp（在syscall入口中会将rsp减去相应的数值）
    wrmsr(0x175, current_pcb->thread->rbp);

    // 向MSR寄存器组中的 IA32_SYSENTER_EIP寄存器写入系统调用入口的地址。
    wrmsr(0x176, (ul)system_call);
    */
    // 初始化进程和tss
    //set_tss64((uint *)phys_2_virt(TSS64_Table), initial_thread.rbp, initial_tss[0].rsp1, initial_tss[0].rsp2, initial_tss[0].ist1, initial_tss[0].ist2, initial_tss[0].ist3, initial_tss[0].ist4, initial_tss[0].ist5, initial_tss[0].ist6, initial_tss[0].ist7);

    initial_tss[proc_current_cpu_id].rsp0 = initial_thread.rbp;
    /*
    kdebug("initial_thread.rbp=%#018lx", initial_thread.rbp);
    kdebug("initial_tss[0].rsp1=%#018lx", initial_tss[0].rsp1);
    kdebug("initial_tss[0].ist1=%#018lx", initial_tss[0].ist1);
*/
    // 初始化进程的循环链表
    list_init(&initial_proc_union.pcb.list);
    kernel_thread(initial_kernel_thread, 10, CLONE_FS | CLONE_FILES | CLONE_SIGNAL); // 初始化内核进程
    initial_proc_union.pcb.state = PROC_RUNNING;
    initial_proc_union.pcb.preempt_count = 0;
    // 获取新的进程的pcb
    //struct process_control_block *p = container_of(list_next(&current_pcb->list), struct process_control_block, list);

    //kdebug("Ready to switch...");
    // 切换到新的内核线程
    // switch_proc(current_pcb, p);
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
    struct process_control_block *tsk = NULL;

    // 获取一个物理页并在这个物理页内初始化pcb
    struct Page *pp = alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED | PAGE_KERNEL);

    tsk = (struct process_control_block *)phys_2_virt(pp->addr_phys);

    memset(tsk, 0, sizeof(struct process_control_block));

    // 将当前进程的pcb复制到新的pcb内
    *tsk = *current_pcb;

    //kdebug("current_pcb->flags=%#010lx", current_pcb->flags);

    // 将进程加入循环链表
    list_init(&tsk->list);

    // list_add(&initial_proc_union.pcb.list, &tsk->list);
    tsk->priority = 2;
    tsk->preempt_count = 0;
    ++(tsk->pid);
    tsk->cpu_id = proc_current_cpu_id;
    tsk->state = PROC_UNINTERRUPTIBLE;
    list_init(&tsk->list);
    list_add(&initial_proc_union.pcb.list, &tsk->list);

    // 将线程结构体放置在pcb的后面
    struct thread_struct *thd = (struct thread_struct *)(tsk + 1);
    memset(thd, 0, sizeof(struct thread_struct));
    tsk->thread = thd;
    // 将寄存器信息存储到进程的内核栈空间的顶部
    memcpy((void *)((ul)tsk + STACK_SIZE - sizeof(struct pt_regs)), regs, sizeof(struct pt_regs));

    // 设置进程的内核栈
    thd->rbp = (ul)tsk + STACK_SIZE;
    thd->rip = regs->rip;
    thd->rsp = (ul)tsk + STACK_SIZE - sizeof(struct pt_regs);
    thd->fs = KERNEL_DS;
    thd->gs = KERNEL_DS;

    //kdebug("do_fork() thd->rsp=%#018lx", thd->rsp);
    // 若进程不是内核层的进程，则跳转到ret from system call
    if (!(tsk->flags & PF_KTHREAD))
        thd->rip = regs->rip = (ul)ret_from_system_call;
    else
        kdebug("is kernel proc.");

    kdebug("ret_from_system_call=%#018lx", (ul)ret_from_system_call);
    
    tsk->state = PROC_RUNNING;

    sched_cfs_enqueue(tsk);

    return 0;
}
