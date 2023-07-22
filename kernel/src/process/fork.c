#include "process.h"
#include <common/err.h>
#include <common/kthread.h>
#include <common/spinlock.h>

extern spinlock_t process_global_pid_write_lock;
extern long process_global_pid;

extern void kernel_thread_func(void);
extern uint64_t rs_procfs_register_pid(uint64_t);
extern uint64_t rs_procfs_unregister_pid(uint64_t);
extern void *rs_dup_fpstate();

extern int process_copy_files(uint64_t clone_flags, struct process_control_block *pcb);
int process_copy_flags(uint64_t clone_flags, struct process_control_block *pcb);
int process_copy_mm(uint64_t clone_flags, struct process_control_block *pcb);
int process_copy_thread(uint64_t clone_flags, struct process_control_block *pcb, uint64_t stack_start,
                        uint64_t stack_size, struct pt_regs *current_regs);

extern int process_copy_sighand(uint64_t clone_flags, struct process_control_block *pcb);
extern int process_copy_signal(uint64_t clone_flags, struct process_control_block *pcb);
extern void process_exit_sighand(struct process_control_block *pcb);
extern void process_exit_signal(struct process_control_block *pcb);

/**
 * @brief fork当前进程
 *
 * @param regs 新的寄存器值
 * @param clone_flags 克隆标志
 * @param stack_start 堆栈开始地址
 * @param stack_size 堆栈大小
 * @return unsigned long
 */
unsigned long do_fork(struct pt_regs *regs, unsigned long clone_flags, unsigned long stack_start,
                      unsigned long stack_size)
{
    int retval = 0;
    struct process_control_block *tsk = NULL;

    // 为新的进程分配栈空间，并将pcb放置在底部
    tsk = (struct process_control_block *)kzalloc(STACK_SIZE, 0);
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
    tsk->worker_private = NULL;
    io_mfence();

    // 初始化进程的循环链表结点
    list_init(&tsk->list);

    io_mfence();
    // 判断是否为内核态调用fork
    if ((current_pcb->flags & PF_KTHREAD) && stack_start != 0)
        tsk->flags |= PF_KFORK;

    if (tsk->flags & PF_KTHREAD)
    {
        // 对于内核线程，设置其worker私有信息
        retval = kthread_set_worker_private(tsk);
        if (IS_ERR_VALUE(retval))
            goto copy_flags_failed;
        tsk->virtual_runtime = 0;
    }
    tsk->priority = 2;
    tsk->preempt_count = 0;

    // 增加全局的pid并赋值给新进程的pid
    spin_lock(&process_global_pid_write_lock);
    tsk->pid = process_global_pid++;
    barrier();
    // 加入到进程链表中
    // todo: 对pcb_list_lock加锁
    tsk->prev_pcb = &initial_proc_union.pcb;
    barrier();
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
    retval = process_copy_flags(clone_flags, tsk);
    if (retval)
        goto copy_flags_failed;

    // 拷贝内存空间分布结构体
    retval = process_copy_mm(clone_flags, tsk);
    if (retval)
        goto copy_mm_failed;

    // 拷贝文件
    retval = process_copy_files(clone_flags, tsk);
    if (retval)
        goto copy_files_failed;

    // 拷贝信号处理函数
    retval = process_copy_sighand(clone_flags, tsk);
    if (retval)
        goto copy_sighand_failed;

    retval = process_copy_signal(clone_flags, tsk);
    if (retval)
        goto copy_signal_failed;

    // 拷贝线程结构体
    retval = process_copy_thread(clone_flags, tsk, stack_start, stack_size, regs);
    if (retval)
        goto copy_thread_failed;

    // 拷贝成功
    retval = tsk->pid;

    tsk->flags &= ~PF_KFORK;

    // 创建对应procfs文件
    rs_procfs_register_pid(tsk->pid);

    // 唤醒进程
    process_wakeup(tsk);

    return retval;

copy_thread_failed:;
    // 回收线程
    process_exit_thread(tsk);
copy_files_failed:;
    // 回收文件
    process_exit_files(tsk);
    rs_procfs_unregister_pid(tsk->pid);
copy_sighand_failed:;
    process_exit_sighand(tsk);
copy_signal_failed:;
    process_exit_signal(tsk);
copy_mm_failed:;
    // 回收内存空间分布结构体
    process_exit_mm(tsk);
copy_flags_failed:;
    kfree(tsk);
    return retval;
}

/**
 * @brief 拷贝当前进程的标志位
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
int process_copy_flags(uint64_t clone_flags, struct process_control_block *pcb)
{
    if (clone_flags & CLONE_VM)
        pcb->flags |= PF_VFORK;
    return 0;
}

/**
 * @brief 拷贝当前进程的内存空间分布结构体信息
 *
 * @param clone_flags 克隆标志位
 * @param pcb 新的进程的pcb
 * @return uint64_t
 */
int process_copy_mm(uint64_t clone_flags, struct process_control_block *pcb)
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
    memcpy(phys_2_virt(new_mms->pgd) + 256, phys_2_virt(initial_proc[proc_current_cpu_id]->mm->pgd) + 256,
           PAGE_4K_SIZE / 2);

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
                int ret = mm_create_vma(new_mms, vma->vm_start + i * PAGE_2M_SIZE, PAGE_2M_SIZE, vma->vm_flags,
                                        vma->vm_ops, &new_vma);
                // 防止内存泄露
                if (unlikely(ret == -EEXIST))
                    free_pages(Phy_to_2M_Page(pa), 1);
                else
                    mm_map_vma(new_vma, pa, 0, PAGE_2M_SIZE);

                memcpy((void *)phys_2_virt(pa), (void *)(vma->vm_start + i * PAGE_2M_SIZE),
                       (vma_size >= PAGE_2M_SIZE) ? PAGE_2M_SIZE : vma_size);
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
                mm_map_vma(new_vma, virt_2_phys(va), 0, map_size);

            memcpy((void *)va, (void *)vma->vm_start, vma_size);
        }
        vma = vma->vm_next;
    }

    return retval;
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
int process_copy_thread(uint64_t clone_flags, struct process_control_block *pcb, uint64_t stack_start,
                        uint64_t stack_size, struct pt_regs *current_regs)
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
        thd->rbp = (uint64_t)(child_regs + 1); // 设置新的内核线程开始执行时的rbp（也就是进入ret_from_intr时的rbp）
    else
        thd->rbp = (uint64_t)pcb + STACK_SIZE;

    // 设置新的内核线程开始执行的时候的rsp
    thd->rsp = (uint64_t)child_regs;
    thd->fs = current_pcb->thread->fs;
    thd->gs = current_pcb->thread->gs;

    // 根据是否为内核线程、是否在内核态fork，设置进程的开始执行的地址
    if (pcb->flags & PF_KFORK)
        thd->rip = (uint64_t)ret_from_intr;
    else if (pcb->flags & PF_KTHREAD && (!(pcb->flags & PF_KFORK)))
        thd->rip = (uint64_t)kernel_thread_func;
    else
        thd->rip = (uint64_t)ret_from_intr;

    pcb->fp_state = rs_dup_fpstate();

    return 0;
}