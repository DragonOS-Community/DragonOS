#include "process.h"

#include <DragonOS/signal.h>
#include <common/compiler.h>
#include <common/completion.h>
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
#include <driver/usb/usb.h>
#include <driver/video/video.h>
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
extern struct mm_struct initial_mm;
extern struct signal_struct INITIAL_SIGNALS;
extern struct sighand_struct INITIAL_SIGHAND;

extern void process_exit_sighand(struct process_control_block *pcb);
extern void process_exit_signal(struct process_control_block *pcb);
extern void initial_proc_init_signal(struct process_control_block *pcb);
extern void rs_process_exit_fpstate(struct process_control_block *pcb);
extern int process_init_files();
extern int rs_init_stdio();

// 设置初始进程的PCB
#define INITIAL_PROC(proc)                                                                                             \
    {                                                                                                                  \
        .state = PROC_UNINTERRUPTIBLE, .flags = PF_KTHREAD, .preempt_count = 0, .signal = 0, .cpu_id = 0,              \
        .mm = &initial_mm, .thread = &initial_thread, .addr_limit = 0xffffffffffffffff, .pid = 0, .priority = 2,       \
        .virtual_runtime = 0, .fds = {0}, .next_pcb = &proc, .prev_pcb = &proc, .parent_pcb = &proc, .exit_code = 0,   \
        .wait_child_proc_exit = 0, .worker_private = NULL, .policy = SCHED_NORMAL, .sig_blocked = 0,                   \
        .signal = &INITIAL_SIGNALS, .sighand = &INITIAL_SIGHAND,                                                       \
    }

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
    __attribute__((__section__(".data.init_proc_union"))) = {INITIAL_PROC(initial_proc_union.pcb)};

struct process_control_block *initial_proc[MAX_CPU_NUM] = {&initial_proc_union.pcb, 0};

// 为每个核心初始化初始进程的tss
struct tss_struct initial_tss[MAX_CPU_NUM] = {[0 ... MAX_CPU_NUM - 1] = INITIAL_TSS};

/**
 * @brief 回收进程的所有文件描述符
 *
 * @param pcb 要被回收的进程的pcb
 * @return uint64_t
 */
extern int process_exit_files(struct process_control_block *pcb);

/**
 * @brief 释放进程的页表
 *
 * @param pcb 要被释放页表的进程
 * @return uint64_t
 */
uint64_t process_exit_mm(struct process_control_block *pcb);

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

    __asm__ __volatile__("movq	%%fs,	%0 \n\t" : "=a"(prev->thread->fs));
    __asm__ __volatile__("movq	%%gs,	%0 \n\t" : "=a"(prev->thread->gs));

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
 * @brief 打开要执行的程序文件
 *
 * @param path
 * @return int 文件描述符编号
 */
int process_open_exec_file(char *path)
{
    struct pt_regs tmp = {0};
    tmp.r8 = (uint64_t)path;
    tmp.r9 = O_RDONLY;
    int fd = sys_open(&tmp);
    return fd;
}

/**
 * @brief 加载elf格式的程序文件到内存中，并设置regs
 *
 * @param regs 寄存器
 * @param path 文件路径
 * @return int
 */
static int process_load_elf_file(struct pt_regs *regs, char *path)
{
    int retval = 0;
    int fd = process_open_exec_file(path);

    if ((long)fd < 0)
    {
        kdebug("(long)fd=%ld", (long)fd);
        return (unsigned long)fd;
    }

    void *buf = kzalloc(PAGE_4K_SIZE, 0);
    uint64_t pos = 0;

    struct pt_regs tmp_use_fs = {0};
    tmp_use_fs.r8 = fd;
    tmp_use_fs.r9 = 0;
    tmp_use_fs.r10 = SEEK_SET;
    retval = sys_lseek(&tmp_use_fs);

    // 读取 Elf64_Ehdr
    tmp_use_fs.r8 = fd;
    tmp_use_fs.r9 = (uint64_t)buf;
    tmp_use_fs.r10 = sizeof(Elf64_Ehdr);
    retval = sys_read(&tmp_use_fs);

    tmp_use_fs.r8 = fd;
    tmp_use_fs.r9 = 0;
    tmp_use_fs.r10 = SEEK_CUR;
    pos = sys_lseek(&tmp_use_fs);

    if (retval != sizeof(Elf64_Ehdr))
    {
        kerror("retval=%d, not equal to sizeof(Elf64_Ehdr):%d", retval, sizeof(Elf64_Ehdr));
    }
    retval = 0;
    if (!elf_check(buf))
    {
        kerror("Not an ELF file: %s", path);
        retval = -ENOTSUP;
        goto load_elf_failed;
    }

#if ARCH(X86_64)
    // 暂时只支持64位的文件
    if (((Elf32_Ehdr *)buf)->e_ident[EI_CLASS] != ELFCLASS64)
    {
        kdebug("((Elf32_Ehdr *)buf)->e_ident[EI_CLASS]=%d", ((Elf32_Ehdr *)buf)->e_ident[EI_CLASS]);
        retval = -EUNSUPPORTED;
        goto load_elf_failed;
    }
    Elf64_Ehdr ehdr = *(Elf64_Ehdr *)buf;
    // 暂时只支持AMD64架构
    if (ehdr.e_machine != EM_AMD64)
    {
        kerror("e_machine=%d", ehdr.e_machine);
        retval = -EUNSUPPORTED;
        goto load_elf_failed;
    }
#else
#error Unsupported architecture!
#endif
    if (ehdr.e_type != ET_EXEC)
    {
        kerror("Not executable file! filename=%s\tehdr->e_type=%d", path, ehdr.e_type);
        retval = -EUNSUPPORTED;
        goto load_elf_failed;
    }
    // kdebug("filename=%s:\te_entry=%#018lx", path, ehdr.e_entry);
    regs->rip = ehdr.e_entry;
    current_pcb->mm->code_addr_start = ehdr.e_entry;

    // kdebug("ehdr.e_phoff=%#018lx\t ehdr.e_phentsize=%d, ehdr.e_phnum=%d", ehdr.e_phoff, ehdr.e_phentsize,
    // ehdr.e_phnum); 将指针移动到program header处

    // 读取所有的phdr
    pos = ehdr.e_phoff;
    tmp_use_fs.r8 = fd;
    tmp_use_fs.r9 = pos;
    tmp_use_fs.r10 = SEEK_SET;
    pos = sys_lseek(&tmp_use_fs);

    memset(buf, 0, PAGE_4K_SIZE);
    tmp_use_fs.r8 = fd;
    tmp_use_fs.r9 = (uint64_t)buf;
    tmp_use_fs.r10 = (uint64_t)ehdr.e_phentsize * (uint64_t)ehdr.e_phnum;
    sys_read(&tmp_use_fs);

    tmp_use_fs.r8 = fd;
    tmp_use_fs.r9 = 0;
    tmp_use_fs.r10 = SEEK_CUR;
    pos = sys_lseek(&tmp_use_fs);

    if ((long)retval < 0)
    {
        kdebug("(unsigned long)filp=%d", (long)retval);
        retval = -ENOEXEC;
        goto load_elf_failed;
    }

    Elf64_Phdr *phdr = buf;
    // 将程序加载到内存中
    for (int i = 0; i < ehdr.e_phnum; ++i, ++phdr)
    {
        // kdebug("phdr[%d] phdr->p_offset=%#018lx phdr->p_vaddr=%#018lx phdr->p_memsz=%ld phdr->p_filesz=%ld
        // phdr->p_type=%d", i, phdr->p_offset, phdr->p_vaddr, phdr->p_memsz, phdr->p_filesz, phdr->p_type);

        // 不是可加载的段
        if (phdr->p_type != PT_LOAD)
            continue;

        int64_t remain_mem_size = phdr->p_memsz;
        int64_t remain_file_size = phdr->p_filesz;
        pos = phdr->p_offset;

        uint64_t virt_base = 0;
        uint64_t beginning_offset = 0; // 由于页表映射导致的virtbase与实际的p_vaddr之间的偏移量

        if (remain_mem_size >= PAGE_2M_SIZE) // 接下来存在映射2M页的情况，因此将vaddr按2M向下对齐
            virt_base = phdr->p_vaddr & PAGE_2M_MASK;
        else // 接下来只有4K页的映射
            virt_base = phdr->p_vaddr & PAGE_4K_MASK;

        beginning_offset = phdr->p_vaddr - virt_base;
        remain_mem_size += beginning_offset;

        while (remain_mem_size > 0)
        {
            // kdebug("loading...");
            int64_t map_size = 0;
            if (remain_mem_size >= PAGE_2M_SIZE)
            {
                uint64_t pa = alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys;
                struct vm_area_struct *vma = NULL;
                int ret =
                    mm_create_vma(current_pcb->mm, virt_base, PAGE_2M_SIZE, VM_USER | VM_ACCESS_FLAGS, NULL, &vma);

                // 防止内存泄露
                if (ret == -EEXIST)
                    free_pages(Phy_to_2M_Page(pa), 1);
                else
                    mm_map(current_pcb->mm, virt_base, PAGE_2M_SIZE, pa);
                // mm_map_vma(vma, pa, 0, PAGE_2M_SIZE);
                io_mfence();
                memset((void *)virt_base, 0, PAGE_2M_SIZE);
                map_size = PAGE_2M_SIZE;
            }
            else
            {
                // todo: 使用4K、8K、32K大小内存块混合进行分配，提高空间利用率（减少了bmp的大小）
                map_size = ALIGN(remain_mem_size, PAGE_4K_SIZE);
                // 循环分配4K大小内存块
                for (uint64_t off = 0; off < map_size; off += PAGE_4K_SIZE)
                {
                    uint64_t paddr = virt_2_phys((uint64_t)kmalloc(PAGE_4K_SIZE, 0));

                    struct vm_area_struct *vma = NULL;
                    int val = mm_create_vma(current_pcb->mm, virt_base + off, PAGE_4K_SIZE, VM_USER | VM_ACCESS_FLAGS,
                                            NULL, &vma);
                    // kdebug("virt_base=%#018lx", virt_base + off);
                    if (val == -EEXIST)
                        kfree(phys_2_virt(paddr));
                    else
                        mm_map(current_pcb->mm, virt_base + off, PAGE_4K_SIZE, paddr);
                    // mm_map_vma(vma, paddr, 0, PAGE_4K_SIZE);
                    io_mfence();
                    memset((void *)(virt_base + off), 0, PAGE_4K_SIZE);
                }
            }

            tmp_use_fs.r8 = fd;
            tmp_use_fs.r9 = pos;
            tmp_use_fs.r10 = SEEK_SET;
            pos = sys_lseek(&tmp_use_fs);

            int64_t val = 0;
            if (remain_file_size > 0)
            {
                int64_t to_trans = (remain_file_size > PAGE_2M_SIZE) ? PAGE_2M_SIZE : remain_file_size;

                void *buf3 = kzalloc(PAGE_4K_SIZE, 0);
                while (to_trans > 0)
                {
                    int64_t x = 0;
                    tmp_use_fs.r8 = fd;
                    tmp_use_fs.r9 = (uint64_t)buf3;
                    tmp_use_fs.r10 = to_trans;
                    x = sys_read(&tmp_use_fs);
                    memcpy(virt_base + beginning_offset + val, buf3, x);
                    val += x;
                    to_trans -= x;
                    tmp_use_fs.r8 = fd;
                    tmp_use_fs.r9 = 0;
                    tmp_use_fs.r10 = SEEK_CUR;
                    pos = sys_lseek(&tmp_use_fs);
                }
                kfree(buf3);

                // kdebug("virt_base + beginning_offset=%#018lx, val=%d, to_trans=%d", virt_base + beginning_offset,
                // val,
                //        to_trans);
                // kdebug("to_trans=%d", to_trans);
            }

            if (val < 0)
                goto load_elf_failed;

            remain_mem_size -= map_size;
            remain_file_size -= val;
            virt_base += map_size;
        }
    }

    // 分配2MB的栈内存空间
    regs->rsp = current_pcb->mm->stack_start;
    regs->rbp = current_pcb->mm->stack_start;

    {
        struct vm_area_struct *vma = NULL;
        uint64_t pa = alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys;
        int val = mm_create_vma(current_pcb->mm, current_pcb->mm->stack_start - PAGE_2M_SIZE, PAGE_2M_SIZE,
                                VM_USER | VM_ACCESS_FLAGS, NULL, &vma);
        if (val == -EEXIST)
            free_pages(Phy_to_2M_Page(pa), 1);
        else
            mm_map_vma(vma, pa, 0, PAGE_2M_SIZE);
    }

    // 清空栈空间
    memset((void *)(current_pcb->mm->stack_start - PAGE_2M_SIZE), 0, PAGE_2M_SIZE);

load_elf_failed:;
    {
        struct pt_regs tmp = {0};
        tmp.r8 = fd;
        sys_close(&tmp);
    }

    if (buf != NULL)
        kfree(buf);
    return retval;
}
/**
 * @brief 使当前进程去执行新的代码
 *
 * @param regs 当前进程的寄存器
 * @param path 可执行程序的路径
 * @param argv 参数列表
 * @param envp 环境变量
 * @return ul 错误码
 */
#pragma GCC push_options
#pragma GCC optimize("O0")
ul do_execve(struct pt_regs *regs, char *path, char *argv[], char *envp[])
{

    // 当前进程正在与父进程共享地址空间，需要创建
    // 独立的地址空间才能使新程序正常运行
    if (current_pcb->flags & PF_VFORK)
    {
        // kdebug("proc:%d  creating new mem space", current_pcb->pid);
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

    // 设置用户栈和用户堆的基地址
    unsigned long stack_start_addr = 0x6ffff0a00000UL;
    const uint64_t brk_start_addr = 0x700000000000UL;

    process_switch_mm(current_pcb);

    // 为用户态程序设置地址边界
    if (!(current_pcb->flags & PF_KTHREAD))
        current_pcb->addr_limit = USER_MAX_LINEAR_ADDR;

    current_pcb->mm->code_addr_end = 0;
    current_pcb->mm->data_addr_start = 0;
    current_pcb->mm->data_addr_end = 0;
    current_pcb->mm->rodata_addr_start = 0;
    current_pcb->mm->rodata_addr_end = 0;
    current_pcb->mm->bss_start = 0;
    current_pcb->mm->bss_end = 0;
    current_pcb->mm->brk_start = brk_start_addr;
    current_pcb->mm->brk_end = brk_start_addr;
    current_pcb->mm->stack_start = stack_start_addr;

    // 清除进程的vfork标志位
    current_pcb->flags &= ~PF_VFORK;

    // 加载elf格式的可执行文件
    int tmp = process_load_elf_file(regs, path);
    if (tmp < 0)
        goto exec_failed;

    // 拷贝参数列表
    if (argv != NULL)
    {
        int argc = 0;

        // 目标程序的argv基地址指针，最大8个参数
        char **dst_argv = (char **)(stack_start_addr - (sizeof(char **) << 3));
        uint64_t str_addr = (uint64_t)dst_argv;

        for (argc = 0; argc < 8 && argv[argc] != NULL; ++argc)
        {

            if (*argv[argc] == NULL)
                break;

            // 测量参数的长度（最大1023）
            int argv_len = strnlen_user(argv[argc], 1023) + 1;
            strncpy((char *)(str_addr - argv_len), argv[argc], argv_len - 1);
            str_addr -= argv_len;
            dst_argv[argc] = (char *)str_addr;
            // 字符串加上结尾字符
            ((char *)str_addr)[argv_len] = '\0';
        }

        // 重新设定栈基址，并预留空间防止越界
        stack_start_addr = str_addr - 8;
        current_pcb->mm->stack_start = stack_start_addr;
        regs->rsp = regs->rbp = stack_start_addr;

        // 传递参数
        regs->rdi = argc;
        regs->rsi = (uint64_t)dst_argv;
    }
    // kdebug("execve ok");
    // 设置进程的段选择子为用户态可访问
    regs->cs = USER_CS | 3;
    regs->ds = USER_DS | 3;
    regs->ss = USER_DS | 0x3;
    regs->rflags = 0x200246;
    regs->rax = 1;
    regs->es = 0;

    return 0;

exec_failed:;
    process_do_exit(tmp);
}
#pragma GCC pop_options

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
    
    rs_init_stdio();
    // block_io_scheduler_init();
    ahci_init();
    mount_root_fs();
    c_virtio_probe();
    // 使用单独的内核线程来初始化usb驱动程序
    // 注释：由于目前usb驱动程序不完善，因此先将其注释掉
    // int usb_pid = kernel_thread(usb_init, 0, 0);

    kinfo("LZ4 lib Version=%s", LZ4_versionString());
    __rust_demo_func();
    // while (1)
    // {
    //     /* code */
    // }

    // 对completion完成量进行测试
    // __test_completion();

    // // 对一些组件进行单元测试
    // uint64_t tpid[] = {
    //     ktest_start(ktest_test_bitree, 0), ktest_start(ktest_test_kfifo, 0), ktest_start(ktest_test_mutex, 0),
    //     ktest_start(ktest_test_idr, 0),
    //     // usb_pid,
    // };
    // kinfo("Waiting test thread exit...");
    // // 等待测试进程退出
    // for (int i = 0; i < sizeof(tpid) / sizeof(uint64_t); ++i)
    //     waitpid(tpid[i], NULL, NULL);
    // kinfo("All test done.");

    // 测试实时进程

    // struct process_control_block *test_rt1 = kthread_run_rt(&test, NULL, "test rt");
    // kdebug("process:rt test kthread is created!!!!");

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
                         "jmp do_execve  \n\t" ::"D"(current_pcb->thread->rsp),
                         "m"(current_pcb->thread->rsp), "m"(current_pcb->thread->rip), "S"("/bin/shell.elf"), "c"(NULL),
                         "d"(NULL)
                         : "memory");

    return 1;
}
#pragma GCC pop_options
/**
 * @brief 当子进程退出后向父进程发送通知
 *
 */
void process_exit_notify()
{
    wait_queue_wakeup(&current_pcb->parent_pcb->wait_child_proc_exit, PROC_INTERRUPTIBLE);
}

/**
 * @brief 进程退出时执行的函数
 *
 * @param code 返回码
 * @return ul
 */
ul process_do_exit(ul code)
{
    // kinfo("process exiting..., code is %ld.", (long)code);
    cli();
    struct process_control_block *pcb = current_pcb;

    // 进程退出时释放资源
    process_exit_files(pcb);
    process_exit_thread(pcb);
    // todo: 可否在这里释放内存结构体？（在判断共享页引用问题之后）

    pcb->state = PROC_ZOMBIE;
    pcb->exit_code = code;
    sti();

    process_exit_notify();
    sched();

    while (1)
        pause();
}

/**
 * @brief 初始化内核进程
 *
 * @param fn 目标程序的地址
 * @param arg 向目标程序传入的参数
 * @param flags
 * @return int
 */

pid_t kernel_thread(int (*fn)(void *), void *arg, unsigned long flags)
{
    struct pt_regs regs;
    barrier();
    memset(&regs, 0, sizeof(regs));
    barrier();
    // 在rbx寄存器中保存进程的入口地址
    regs.rbx = (ul)fn;
    // 在rdx寄存器中保存传入的参数
    regs.rdx = (ul)arg;
    barrier();
    regs.ds = KERNEL_DS;
    barrier();
    regs.es = KERNEL_DS;
    barrier();
    regs.cs = KERNEL_CS;
    barrier();
    regs.ss = KERNEL_DS;
    barrier();

    // 置位中断使能标志位
    regs.rflags = (1 << 9);
    barrier();
    // rip寄存器指向内核线程的引导程序
    regs.rip = (ul)kernel_thread_func;
    barrier();
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

    initial_tss[proc_current_cpu_id].rsp0 = initial_thread.rbp;

    // 初始化pid的写锁

    spin_init(&process_global_pid_write_lock);

    // 初始化进程的循环链表
    list_init(&initial_proc_union.pcb.list);
    wait_queue_init(&initial_proc_union.pcb.wait_child_proc_exit, NULL);

    // 初始化init进程的signal相关的信息
    initial_proc_init_signal(current_pcb);
    kdebug("Initial process to init files");
    process_init_files();
    kdebug("Initial process init files ok");

    // 临时设置IDLE进程的的虚拟运行时间为0，防止下面的这些内核线程的虚拟运行时间出错
    current_pcb->virtual_runtime = 0;
    barrier();
    kernel_thread(initial_kernel_thread, 10, CLONE_FS | CLONE_SIGNAL); // 初始化内核线程
    barrier();
    kthread_mechanism_init(); // 初始化kthread机制

    initial_proc_union.pcb.state = PROC_RUNNING;
    initial_proc_union.pcb.preempt_count = 0;
    initial_proc_union.pcb.cpu_id = 0;
    initial_proc_union.pcb.virtual_runtime = (1UL << 60);
    // 将IDLE进程的虚拟运行时间设置为一个很大的数值
    current_pcb->virtual_runtime = (1UL << 60);
}

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
    sched_enqueue(pcb, true);
    return 0;
}

/**
 * @brief 将进程加入到调度器的就绪队列中，并标志当前进程需要被调度
 *
 * @param pcb 进程的pcb
 */
int process_wakeup_immediately(struct process_control_block *pcb)
{
    if (pcb->state & PROC_RUNNING)
        return 0;
    int retval = process_wakeup(pcb);
    if (retval != 0)
        return retval;
    // 将当前进程标志为需要调度，缩短新进程被wakeup的时间
    current_pcb->flags |= PF_NEED_SCHED;

    if (pcb->cpu_id == current_pcb->cpu_id)
        sched();
    else
        kick_cpu(pcb->cpu_id);
    return 0;
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

    // // 获取顶层页表
    pml4t_t *current_pgd = (pml4t_t *)phys_2_virt(pcb->mm->pgd);

    // 循环释放VMA中的内存
    struct vm_area_struct *vma = pcb->mm->vmas;
    while (vma != NULL)
    {

        struct vm_area_struct *cur_vma = vma;
        vma = cur_vma->vm_next;

        uint64_t pa;
        mm_unmap_vma(pcb->mm, cur_vma, &pa);

        uint64_t size = (cur_vma->vm_end - cur_vma->vm_start);

        // 释放内存
        switch (size)
        {
        case PAGE_4K_SIZE:
            kfree(phys_2_virt(pa));
            break;
        default:
            break;
        }
        vm_area_del(cur_vma);
        vm_area_free(cur_vma);
    }

    // 释放顶层页表
    kfree(current_pgd);
    if (unlikely(pcb->mm->vmas != NULL))
    {
        kwarn("pcb.mm.vmas!=NULL");
    }
    // 释放内存空间分布结构体
    kfree(pcb->mm);

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

/**
 * @brief 释放pcb
 *
 * @param pcb 要被释放的pcb
 * @return int
 */
int process_release_pcb(struct process_control_block *pcb)
{
    // 释放子进程的页表
    process_exit_mm(pcb);
    if ((pcb->flags & PF_KTHREAD)) // 释放内核线程的worker private结构体
        free_kthread_struct(pcb);

    // 将pcb从pcb链表中移除
    // todo: 对相关的pcb加锁
    pcb->prev_pcb->next_pcb = pcb->next_pcb;
    pcb->next_pcb->prev_pcb = pcb->prev_pcb;
    process_exit_sighand(pcb);
    process_exit_signal(pcb);
    rs_process_exit_fpstate(pcb);
    rs_procfs_unregister_pid(pcb->pid);
    // 释放当前pcb
    kfree(pcb);
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

/**
 * @brief 给pcb设置名字
 *
 * @param pcb 需要设置名字的pcb
 * @param pcb_name 保存名字的char数组
 */
void process_set_pcb_name(struct process_control_block *pcb, const char *pcb_name)
{
    __set_pcb_name(pcb, pcb_name);
}
