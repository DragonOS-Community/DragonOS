#include "process.h"

#include <common/printk.h>
#include <common/kprint.h>
#include <common/stdio.h>
#include <common/string.h>
#include <common/compiler.h>
#include <common/libELF/elf.h>
#include <common/time.h>
#include <common/sys/wait.h>
#include <driver/video/video.h>
#include <driver/usb/usb.h>
#include <exception/gate.h>
#include <filesystem/fat32/fat32.h>
#include <filesystem/devfs/devfs.h>
#include <mm/slab.h>
#include <common/spinlock.h>
#include <syscall/syscall.h>
#include <syscall/syscall_num.h>
#include <sched/sched.h>
#include <common/unistd.h>
#include <debug/traceback/traceback.h>
#include <driver/disk/ahci/ahci.h>

#include <ktest/ktest.h>

#include <mm/mmio.h>

#include <common/lz4.h>

// #pragma GCC push_options
// #pragma GCC optimize("O0")

spinlock_t process_global_pid_write_lock; // 增加pid的写锁
long process_global_pid = 1;              // 系统中最大的pid

extern void system_call(void);
extern void kernel_thread_func(void);

ul _stack_start; // initial proc的栈基地址（虚拟地址）
extern struct mm_struct initial_mm;
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
#pragma GCC push_options
#pragma GCC optimize("O0")
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
}
#pragma GCC pop_options

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

    if (dentry->dir_inode->attribute == VFS_IF_DIR)
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
 * @brief 加载elf格式的程序文件到内存中，并设置regs
 *
 * @param regs 寄存器
 * @param path 文件路径
 * @return int
 */
static int process_load_elf_file(struct pt_regs *regs, char *path)
{
    int retval = 0;
    struct vfs_file_t *filp = process_open_exec_file(path);

    if ((long)filp <= 0 && (long)filp >= -255)
    {
        // kdebug("(long)filp=%ld", (long)filp);
        return (unsigned long)filp;
    }

    void *buf = kmalloc(PAGE_4K_SIZE, 0);
    memset(buf, 0, PAGE_4K_SIZE);
    uint64_t pos = 0;
    pos = filp->file_ops->lseek(filp, 0, SEEK_SET);
    retval = filp->file_ops->read(filp, (char *)buf, sizeof(Elf64_Ehdr), &pos);
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

    // kdebug("ehdr.e_phoff=%#018lx\t ehdr.e_phentsize=%d, ehdr.e_phnum=%d", ehdr.e_phoff, ehdr.e_phentsize, ehdr.e_phnum);
    // 将指针移动到program header处
    pos = ehdr.e_phoff;
    // 读取所有的phdr
    pos = filp->file_ops->lseek(filp, pos, SEEK_SET);
    filp->file_ops->read(filp, (char *)buf, (uint64_t)ehdr.e_phentsize * (uint64_t)ehdr.e_phnum, &pos);
    if ((unsigned long)filp <= 0)
    {
        kdebug("(unsigned long)filp=%d", (long)filp);
        retval = -ENOEXEC;
        goto load_elf_failed;
    }
    Elf64_Phdr *phdr = buf;

    // 将程序加载到内存中
    for (int i = 0; i < ehdr.e_phnum; ++i, ++phdr)
    {
        // kdebug("phdr[%d] phdr->p_offset=%#018lx phdr->p_vaddr=%#018lx phdr->p_memsz=%ld phdr->p_filesz=%ld  phdr->p_type=%d", i, phdr->p_offset, phdr->p_vaddr, phdr->p_memsz, phdr->p_filesz, phdr->p_type);

        // 不是可加载的段
        if (phdr->p_type != PT_LOAD)
            continue;

        int64_t remain_mem_size = phdr->p_memsz;
        int64_t remain_file_size = phdr->p_filesz;
        pos = phdr->p_offset;

        uint64_t virt_base = 0;
        uint64_t beginning_offset = 0;       // 由于页表映射导致的virtbase与实际的p_vaddr之间的偏移量

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
                int ret = mm_create_vma(current_pcb->mm, virt_base, PAGE_2M_SIZE, VM_USER | VM_ACCESS_FLAGS, NULL, &vma);
                // 防止内存泄露
                if (ret == -EEXIST)
                    free_pages(Phy_to_2M_Page(pa), 1);
                else
                    mm_map_vma(vma, pa);
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
                    int val = mm_create_vma(current_pcb->mm, virt_base + off, PAGE_4K_SIZE, VM_USER | VM_ACCESS_FLAGS, NULL, &vma);
                    if (val == -EEXIST)
                        kfree(phys_2_virt(paddr));
                    else
                        mm_map_vma(vma, paddr);
                    io_mfence();
                    memset((void *)(virt_base + off), 0, PAGE_4K_SIZE);
                }
            }

            pos = filp->file_ops->lseek(filp, pos, SEEK_SET);
            int64_t val = 0;
            if (remain_file_size > 0)
            {
                int64_t to_trans = (remain_file_size > PAGE_2M_SIZE) ? PAGE_2M_SIZE : remain_file_size;
                val = filp->file_ops->read(filp, (char *)(virt_base + beginning_offset), to_trans, &pos);
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
        int val = mm_create_vma(current_pcb->mm, current_pcb->mm->stack_start - PAGE_2M_SIZE, PAGE_2M_SIZE, VM_USER | VM_ACCESS_FLAGS, NULL, &vma);
        if (val == -EEXIST)
            free_pages(Phy_to_2M_Page(pa), 1);
        else
            mm_map_vma(vma, pa);
    }

    // 清空栈空间
    memset((void *)(current_pcb->mm->stack_start - PAGE_2M_SIZE), 0, PAGE_2M_SIZE);

load_elf_failed:;
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

    // kdebug("do_execve is running...");

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

    // 关闭之前的文件描述符
    process_exit_files(current_pcb);

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
 * @brief 内核init进程
 *
 * @param arg
 * @return ul 参数
 */
#pragma GCC push_options
#pragma GCC optimize("O0")
ul initial_kernel_thread(ul arg)
{
    // kinfo("initial proc running...\targ:%#018lx", arg);

    ahci_init();
    fat32_init();
    rootfs_umount();

    // 使用单独的内核线程来初始化usb驱动程序
    int usb_pid = kernel_thread(usb_init, 0, 0);

    kinfo("LZ4 lib Version=%s", LZ4_versionString());

    // 对一些组件进行单元测试
    uint64_t tpid[] = {
        ktest_start(ktest_test_bitree, 0),
        ktest_start(ktest_test_kfifo, 0),
        ktest_start(ktest_test_mutex, 0),
        usb_pid,
    };
    kinfo("Waiting test thread exit...");
    // 等待测试进程退出
    for (int i = 0; i < sizeof(tpid) / sizeof(uint64_t); ++i)
        waitpid(tpid[i], NULL, NULL);
    kinfo("All test done.");

    // 准备切换到用户态
    struct pt_regs *regs;

    // 若在后面这段代码中触发中断，return时会导致段选择子错误，从而触发#GP，因此这里需要cli
    cli();
    current_pcb->thread->rip = (ul)ret_from_system_call;
    current_pcb->thread->rsp = (ul)current_pcb + STACK_SIZE - sizeof(struct pt_regs);
    current_pcb->thread->fs = USER_DS | 0x3;
    barrier();
    current_pcb->thread->gs = USER_DS | 0x3;

    // 主动放弃内核线程身份
    current_pcb->flags &= (~PF_KTHREAD);
    kdebug("in initial_kernel_thread: flags=%ld", current_pcb->flags);

    regs = (struct pt_regs *)current_pcb->thread->rsp;
    // kdebug("current_pcb->thread->rsp=%#018lx", current_pcb->thread->rsp);
    current_pcb->flags = 0;
    // 将返回用户层的代码压入堆栈，向rdx传入regs的地址，然后jmp到do_execve这个系统调用api的处理函数  这里的设计思路和switch_proc类似
    // 加载用户态程序：shell.elf
    char init_path[] = "/shell.elf";
    uint64_t addr = (uint64_t)&init_path;
    __asm__ __volatile__("movq %1, %%rsp   \n\t"
                         "pushq %2    \n\t"
                         "jmp do_execve  \n\t" ::"D"(current_pcb->thread->rsp),
                         "m"(current_pcb->thread->rsp), "m"(current_pcb->thread->rip), "S"("/shell.elf"), "c"(NULL), "d"(NULL)
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

int kernel_thread(unsigned long (*fn)(unsigned long), unsigned long arg, unsigned long flags)
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
    initial_mm.pgd = (pml4t_t *)get_CR3();

    initial_mm.code_addr_start = memory_management_struct.kernel_code_start;
    initial_mm.code_addr_end = memory_management_struct.kernel_code_end;

    initial_mm.data_addr_start = (ul)&_data;
    initial_mm.data_addr_end = memory_management_struct.kernel_data_end;

    initial_mm.rodata_addr_start = (ul)&_rodata;
    initial_mm.rodata_addr_end = (ul)&_erodata;
    initial_mm.bss_start = (uint64_t)&_bss;
    initial_mm.bss_end = (uint64_t)&_ebss;

    initial_mm.brk_start = memory_management_struct.start_brk;
    initial_mm.brk_end = current_pcb->addr_limit;

    initial_mm.stack_start = _stack_start;
    initial_mm.vmas = NULL;

    initial_tss[proc_current_cpu_id].rsp0 = initial_thread.rbp;

    // ========= 在IDLE进程的顶层页表中添加对内核地址空间的映射 =====================

    // 由于IDLE进程的顶层页表的高地址部分会被后续进程所复制，为了使所有进程能够共享相同的内核空间，
    //  因此需要先在IDLE进程的顶层页表内映射二级页表

    uint64_t *idle_pml4t_vaddr = (uint64_t *)phys_2_virt((uint64_t)get_CR3() & (~0xfffUL));

    for (int i = 256; i < 512; ++i)
    {
        uint64_t *tmp = idle_pml4t_vaddr + i;
        barrier();
        if (*tmp == 0)
        {
            void *pdpt = kmalloc(PAGE_4K_SIZE, 0);
            barrier();
            memset(pdpt, 0, PAGE_4K_SIZE);
            barrier();
            set_pml4t(tmp, mk_pml4t(virt_2_phys(pdpt), PAGE_KERNEL_PGT));
        }
    }
    barrier();

    flush_tlb();
    /*
    kdebug("initial_thread.rbp=%#018lx", initial_thread.rbp);
    kdebug("initial_tss[0].rsp1=%#018lx", initial_tss[0].rsp1);
    kdebug("initial_tss[0].ist1=%#018lx", initial_tss[0].ist1);
*/
    // 初始化pid的写锁

    spin_init(&process_global_pid_write_lock);

    // 初始化进程的循环链表
    list_init(&initial_proc_union.pcb.list);
    barrier();
    kernel_thread(initial_kernel_thread, 10, CLONE_FS | CLONE_SIGNAL); // 初始化内核线程
    barrier();

    initial_proc_union.pcb.state = PROC_RUNNING;
    initial_proc_union.pcb.preempt_count = 0;
    initial_proc_union.pcb.cpu_id = 0;
    initial_proc_union.pcb.virtual_runtime = (1UL << 60);
    current_pcb->virtual_runtime = (1UL << 60);
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

    // 为新的进程分配栈空间，并将pcb放置在底部
    tsk = (struct process_control_block *)kmalloc(STACK_SIZE, 0);
    barrier();

    if (tsk == NULL)
    {
        retval = -ENOMEM;
        return retval;
    }

    barrier();
    memset(tsk, 0, sizeof(struct process_control_block));
    io_mfence();
    // 将当前进程的pcb复制到新的pcb内
    memcpy(tsk, current_pcb, sizeof(struct process_control_block));
    io_mfence();

    // 初始化进程的循环链表结点
    list_init(&tsk->list);

    io_mfence();
    // 判断是否为内核态调用fork
    if (current_pcb->flags & PF_KTHREAD && stack_start != 0)
        tsk->flags |= PF_KFORK;

    tsk->priority = 2;
    tsk->preempt_count = 0;

    // 增加全局的pid并赋值给新进程的pid
    spin_lock(&process_global_pid_write_lock);
    tsk->pid = process_global_pid++;
    barrier();
    // 加入到进程链表中
    tsk->next_pcb = initial_proc_union.pcb.next_pcb;
    barrier();
    initial_proc_union.pcb.next_pcb = tsk;
    barrier();
    tsk->parent_pcb = current_pcb;
    barrier();

    spin_unlock(&process_global_pid_write_lock);

    tsk->cpu_id = proc_current_cpu_id;
    tsk->state = PROC_UNINTERRUPTIBLE;

    tsk->parent_pcb = current_pcb;
    wait_queue_init(&tsk->wait_child_proc_exit, NULL);
    barrier();
    list_init(&tsk->list);

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

    tsk->flags &= ~PF_KFORK;

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
void process_wakeup(struct process_control_block *pcb)
{
    pcb->state = PROC_RUNNING;
    sched_enqueue(pcb);
}

/**
 * @brief 将进程加入到调度器的就绪队列中，并标志当前进程需要被调度
 *
 * @param pcb 进程的pcb
 */
void process_wakeup_immediately(struct process_control_block *pcb)
{
    pcb->state = PROC_RUNNING;
    sched_enqueue(pcb);
    // 将当前进程标志为需要调度，缩短新进程被wakeup的时间
    current_pcb->flags |= PF_NEED_SCHED;
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
        pcb->mm = current_pcb->mm;

        return retval;
    }

    // 分配新的内存空间分布结构体
    struct mm_struct *new_mms = (struct mm_struct *)kmalloc(sizeof(struct mm_struct), 0);
    memset(new_mms, 0, sizeof(struct mm_struct));

    memcpy(new_mms, current_pcb->mm, sizeof(struct mm_struct));
    new_mms->vmas = NULL;
    pcb->mm = new_mms;

    // 分配顶层页表, 并设置顶层页表的物理地址
    new_mms->pgd = (pml4t_t *)virt_2_phys(kmalloc(PAGE_4K_SIZE, 0));
    // 由于高2K部分为内核空间，在接下来需要覆盖其数据，因此不用清零
    memset(phys_2_virt(new_mms->pgd), 0, PAGE_4K_SIZE / 2);

    // 拷贝内核空间的页表指针
    memcpy(phys_2_virt(new_mms->pgd) + 256, phys_2_virt(initial_proc[proc_current_cpu_id]->mm->pgd) + 256, PAGE_4K_SIZE / 2);

    uint64_t *current_pgd = (uint64_t *)phys_2_virt(current_pcb->mm->pgd);

    uint64_t *new_pml4t = (uint64_t *)phys_2_virt(new_mms->pgd);

    // 拷贝用户空间的vma
    struct vm_area_struct *vma = current_pcb->mm->vmas;
    while (vma != NULL)
    {
        if (vma->vm_end > USER_MAX_LINEAR_ADDR || vma->vm_flags & VM_DONTCOPY)
        {
            vma = vma->vm_next;
            continue;
        }

        int64_t vma_size = vma->vm_end - vma->vm_start;
        // kdebug("vma_size=%ld, vm_start=%#018lx", vma_size, vma->vm_start);
        if (vma_size > PAGE_2M_SIZE / 2)
        {
            int page_to_alloc = (PAGE_2M_ALIGN(vma_size)) >> PAGE_2M_SHIFT;
            for (int i = 0; i < page_to_alloc; ++i)
            {
                uint64_t pa = alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys;

                struct vm_area_struct *new_vma = NULL;
                int ret = mm_create_vma(new_mms, vma->vm_start + i * PAGE_2M_SIZE, PAGE_2M_SIZE, vma->vm_flags, vma->vm_ops, &new_vma);
                // 防止内存泄露
                if (unlikely(ret == -EEXIST))
                    free_pages(Phy_to_2M_Page(pa), 1);
                else
                    mm_map_vma(new_vma, pa);

                memcpy((void *)phys_2_virt(pa), (void *)(vma->vm_start + i * PAGE_2M_SIZE), (vma_size >= PAGE_2M_SIZE) ? PAGE_2M_SIZE : vma_size);
                vma_size -= PAGE_2M_SIZE;
            }
        }
        else
        {
            uint64_t map_size = PAGE_4K_ALIGN(vma_size);
            uint64_t va = (uint64_t)kmalloc(map_size, 0);

            struct vm_area_struct *new_vma = NULL;
            int ret = mm_create_vma(new_mms, vma->vm_start, map_size, vma->vm_flags, vma->vm_ops, &new_vma);
            // 防止内存泄露
            if (unlikely(ret == -EEXIST))
                kfree((void *)va);
            else
                mm_map_vma(new_vma, virt_2_phys(va));

            memcpy((void *)va, (void *)vma->vm_start, vma_size);
        }
        vma = vma->vm_next;
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

    // // 获取顶层页表
    pml4t_t *current_pgd = (pml4t_t *)phys_2_virt(pcb->mm->pgd);

    // 循环释放VMA中的内存
    struct vm_area_struct *vma = pcb->mm->vmas;
    while (vma != NULL)
    {

        struct vm_area_struct *cur_vma = vma;
        vma = cur_vma->vm_next;

        uint64_t pa;
        // kdebug("vm start=%#018lx, sem=%d", cur_vma->vm_start, cur_vma->anon_vma->sem.counter);
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
 * @brief 重写内核栈中的rbp地址
 *
 * @param new_regs 子进程的reg
 * @param new_pcb 子进程的pcb
 * @return int
 */
static int process_rewrite_rbp(struct pt_regs *new_regs, struct process_control_block *new_pcb)
{

    uint64_t new_top = ((uint64_t)new_pcb) + STACK_SIZE;
    uint64_t old_top = (uint64_t)(current_pcb) + STACK_SIZE;

    uint64_t *rbp = &new_regs->rbp;
    uint64_t *tmp = rbp;

    // 超出内核栈范围
    if ((uint64_t)*rbp >= old_top || (uint64_t)*rbp < (old_top - STACK_SIZE))
        return 0;

    while (1)
    {
        // 计算delta
        uint64_t delta = old_top - *rbp;
        // 计算新的rbp值
        uint64_t newVal = new_top - delta;

        // 新的值不合法
        if (unlikely((uint64_t)newVal >= new_top || (uint64_t)newVal < (new_top - STACK_SIZE)))
            break;
        // 将新的值写入对应位置
        *rbp = newVal;
        // 跳转栈帧
        rbp = (uint64_t *)*rbp;
    }

    // 设置内核态fork返回到enter_syscall_int()函数内的时候，rsp寄存器的值
    new_regs->rsp = new_top - (old_top - new_regs->rsp);
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

    struct pt_regs *child_regs = NULL;
    // 拷贝栈空间
    if (pcb->flags & PF_KFORK) // 内核态下的fork
    {
        // 内核态下则拷贝整个内核栈
        uint32_t size = ((uint64_t)current_pcb) + STACK_SIZE - (uint64_t)(current_regs);

        child_regs = (struct pt_regs *)(((uint64_t)pcb) + STACK_SIZE - size);
        memcpy(child_regs, (void *)current_regs, size);
        barrier();
        // 然后重写新的栈中，每个栈帧的rbp值
        process_rewrite_rbp(child_regs, pcb);
    }
    else
    {
        child_regs = (struct pt_regs *)((uint64_t)pcb + STACK_SIZE - sizeof(struct pt_regs));
        memcpy(child_regs, current_regs, sizeof(struct pt_regs));
        barrier();
        child_regs->rsp = stack_start;
    }

    // 设置子进程的返回值为0
    child_regs->rax = 0;
    if (pcb->flags & PF_KFORK)
        thd->rbp = (uint64_t)(child_regs + 1); // 设置新的内核线程开始执行时的rbp（也就是进入ret_from_system_call时的rbp）
    else
        thd->rbp = (uint64_t)pcb + STACK_SIZE;

    // 设置新的内核线程开始执行的时候的rsp
    thd->rsp = (uint64_t)child_regs;
    thd->fs = current_pcb->thread->fs;
    thd->gs = current_pcb->thread->gs;

    // 根据是否为内核线程、是否在内核态fork，设置进程的开始执行的地址
    if (pcb->flags & PF_KFORK)
        thd->rip = (uint64_t)ret_from_system_call;
    else if (pcb->flags & PF_KTHREAD && (!(pcb->flags & PF_KFORK)))
        thd->rip = (uint64_t)kernel_thread_func;
    else
        thd->rip = (uint64_t)ret_from_system_call;

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
 * @brief 申请可用的文件句柄
 *
 * @return int
 */
int process_fd_alloc(struct vfs_file_t *file)
{
    int fd_num = -1;
    struct vfs_file_t **f = current_pcb->fds;

    for (int i = 0; i < PROC_MAX_FD_NUM; ++i)
    {
        /* 找到指针数组中的空位 */
        if (f[i] == NULL)
        {
            fd_num = i;
            f[i] = file;
            break;
        }
    }
    return fd_num;
}