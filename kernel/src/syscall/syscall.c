#include "syscall.h"
#include <common/errno.h>
#include <common/fcntl.h>
#include <common/kthread.h>
#include <common/string.h>
#include <driver/disk/ahci/ahci.h>
#include <exception/gate.h>
#include <exception/irq.h>
#include <filesystem/vfs/VFS.h>
#include <mm/slab.h>
#include <process/process.h>
#include <time/sleep.h>
// 导出系统调用入口函数，定义在entry.S中
extern void syscall_int(void);

extern uint64_t sys_clock(struct pt_regs *regs);
extern uint64_t sys_mstat(struct pt_regs *regs);
extern uint64_t sys_open(struct pt_regs *regs);
extern uint64_t sys_unlink_at(struct pt_regs *regs);
extern uint64_t sys_kill(struct pt_regs *regs);
extern uint64_t sys_sigaction(struct pt_regs *regs);
extern uint64_t sys_rt_sigreturn(struct pt_regs *regs);
extern uint64_t sys_getpid(struct pt_regs *regs);
extern uint64_t sys_sched(struct pt_regs *regs);
extern int sys_dup(int oldfd);
extern int sys_dup2(int oldfd, int newfd);
extern uint64_t sys_socket(struct pt_regs *regs);
extern uint64_t sys_setsockopt(struct pt_regs *regs);
extern uint64_t sys_getsockopt(struct pt_regs *regs);
extern uint64_t sys_connect(struct pt_regs *regs);
extern uint64_t sys_bind(struct pt_regs *regs);
extern uint64_t sys_sendto(struct pt_regs *regs);
extern uint64_t sys_recvfrom(struct pt_regs *regs);
extern uint64_t sys_recvmsg(struct pt_regs *regs);
extern uint64_t sys_listen(struct pt_regs *regs);
extern uint64_t sys_shutdown(struct pt_regs *regs);
extern uint64_t sys_accept(struct pt_regs *regs);
extern uint64_t sys_getsockname(struct pt_regs *regs);
extern uint64_t sys_getpeername(struct pt_regs *regs);

/**
 * @brief 关闭文件系统调用
 *
 * @param fd_num 文件描述符号
 *
 * @param regs
 * @return uint64_t
 */
extern uint64_t sys_close(struct pt_regs *regs);

/**
 * @brief 从文件中读取数据
 *
 * @param fd_num regs->r8 文件描述符号
 * @param buf regs->r9 输出缓冲区
 * @param count regs->r10 要读取的字节数
 *
 * @return uint64_t
 */
extern uint64_t sys_read(struct pt_regs *regs);

/**
 * @brief 向文件写入数据
 *
 * @param fd_num regs->r8 文件描述符号
 * @param buf regs->r9 输入缓冲区
 * @param count regs->r10 要写入的字节数
 *
 * @return uint64_t
 */
extern uint64_t sys_write(struct pt_regs *regs);

/**
 * @brief 调整文件的访问位置
 *
 * @param fd_num 文件描述符号
 * @param offset 偏移量
 * @param whence 调整模式
 * @return uint64_t 调整结束后的文件访问位置
 */
extern uint64_t sys_lseek(struct pt_regs *regs);

/**
 * @brief 导出系统调用处理函数的符号
 *
 */

/**
 * @brief 系统调用不存在时的处理函数
 *
 * @param regs 进程3特权级下的寄存器
 * @return ul
 */
ul system_call_not_exists(struct pt_regs *regs)
{
    kerror("System call [ ID #%d ] not exists.", regs->rax);
    return ESYSCALL_NOT_EXISTS;
} // 取消前述宏定义

/**
 * @brief 重新定义为：把系统调用函数加入系统调用表
 * @param syscall_num 系统调用号
 * @param symbol 系统调用处理函数
 */
#define SYSCALL_COMMON(syscall_num, symbol) [syscall_num] = symbol,

/**
 * @brief 初始化系统调用模块
 *
 */
void syscall_init()
{
    kinfo("Initializing syscall...");

    set_system_trap_gate(0x80, 0, syscall_int); // 系统调用门
}

/**
 * @brief 通过中断进入系统调用
 *
 * @param syscall_id
 * @param arg0
 * @param arg1
 * @param arg2
 * @param arg3
 * @param arg4
 * @param arg5
 * @param arg6
 * @param arg7
 * @return long
 */

long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5, ul arg6, ul arg7)
{
    long err_code;
    __asm__ __volatile__("movq %2, %%r8 \n\t"
                         "movq %3, %%r9 \n\t"
                         "movq %4, %%r10 \n\t"
                         "movq %5, %%r11 \n\t"
                         "movq %6, %%r12 \n\t"
                         "movq %7, %%r13 \n\t"
                         "movq %8, %%r14 \n\t"
                         "movq %9, %%r15 \n\t"
                         "int $0x80   \n\t"
                         : "=a"(err_code)
                         : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5), "m"(arg6),
                           "m"(arg7)
                         : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");

    return err_code;
}

/**
 * @brief 打印字符串的系统调用
 *
 * 当arg1和arg2均为0时，打印黑底白字，否则按照指定的前景色和背景色来打印
 *
 * @param regs 寄存器
 * @param arg0 要打印的字符串
 * @param arg1 前景色
 * @param arg2 背景色
 * @return ul 返回值
 */
ul sys_put_string(struct pt_regs *regs)
{

    printk_color(regs->r9, regs->r10, (char *)regs->r8);
    // printk_color(BLACK, WHITE, (char *)regs->r8);

    return 0;
}

uint64_t sys_fork(struct pt_regs *regs)
{
    return do_fork(regs, 0, regs->rsp, 0);
}
uint64_t sys_vfork(struct pt_regs *regs)
{
    return do_fork(regs, CLONE_VM | CLONE_FS | CLONE_SIGNAL, regs->rsp, 0);
}

/**
 * @brief 将堆内存调整为arg0
 *
 * @param arg0 新的堆区域的结束地址
 * arg0=-1  ===> 返回堆区域的起始地址
 * arg0=-2  ===> 返回堆区域的结束地址
 * @return uint64_t 错误码
 *
 */
uint64_t sys_brk(struct pt_regs *regs)
{
    uint64_t new_brk = PAGE_2M_ALIGN(regs->r8);
    // kdebug("sys_brk input= %#010lx ,  new_brk= %#010lx bytes current_pcb->mm->brk_start=%#018lx
    // current->end_brk=%#018lx", regs->r8, new_brk, current_pcb->mm->brk_start, current_pcb->mm->brk_end);
    struct mm_struct *mm = current_pcb->mm;
    if (new_brk < mm->brk_start || new_brk > new_brk >= current_pcb->addr_limit)
        return mm->brk_end;

    if (mm->brk_end == new_brk)
        return new_brk;

    int64_t offset;
    if (new_brk >= current_pcb->mm->brk_end)
        offset = (int64_t)(new_brk - current_pcb->mm->brk_end);
    else
        offset = -(int64_t)(current_pcb->mm->brk_end - new_brk);

    new_brk = mm_do_brk(current_pcb->mm->brk_end, offset); // 扩展堆内存空间

    current_pcb->mm->brk_end = new_brk;
    return mm->brk_end;
}

/**
 * @brief 将堆内存空间加上offset（注意，该系统调用只应在普通进程中调用，而不能是内核线程）
 *
 * @param arg0 offset偏移量
 * @return uint64_t the previous program break
 */
uint64_t sys_sbrk(struct pt_regs *regs)
{
    uint64_t retval = current_pcb->mm->brk_end;
    if ((int64_t)regs->r8 > 0)
    {

        uint64_t new_brk = PAGE_2M_ALIGN(retval + regs->r8);
        if (new_brk > current_pcb->addr_limit) // 堆地址空间超过限制
        {
            kdebug("exceed mem limit, new_brk = %#018lx", new_brk);
            return -ENOMEM;
        }
    }
    else
    {
        if ((__int128_t)current_pcb->mm->brk_end + (__int128_t)regs->r8 < current_pcb->mm->brk_start)
            return retval;
    }
    // kdebug("do brk");
    uint64_t new_brk = mm_do_brk(current_pcb->mm->brk_end, (int64_t)regs->r8); // 调整堆内存空间
    // kdebug("do brk done, new_brk = %#018lx", new_brk);
    current_pcb->mm->brk_end = new_brk;
    return retval;
}

/**
 * @brief 重启计算机
 *
 * @return
 */
uint64_t sys_reboot(struct pt_regs *regs)
{
    // 重启计算机
    io_out8(0x64, 0xfe);

    return 0;
}

/**
 * @brief 切换工作目录
 *
 * @param dest_path 目标路径
 * @return
+--------------+------------------------+
|    返回码    |          描述          |
+--------------+------------------------+
|      0       |          成功          |
|   EACCESS    |        权限不足        |
|    ELOOP     | 解析path时遇到路径循环 |
| ENAMETOOLONG |       路径名过长       |
|    ENOENT    |  目标文件或目录不存在  |
|    ENODIR    |  检索期间发现非目录项  |
|    ENOMEM    |      系统内存不足      |
|    EFAULT    |       错误的地址       |
| ENAMETOOLONG |        路径过长        |
+--------------+------------------------+
 */
extern uint64_t sys_chdir(struct pt_regs *regs);

/**
 * @brief 获取目录中的数据
 *
 * @param fd 文件描述符号
 * @return uint64_t dirent的总大小
 */
extern uint64_t sys_getdents(struct pt_regs *regs);

/**
 * @brief 执行新的程序
 *
 * @param user_path(r8寄存器) 文件路径
 * @param argv(r9寄存器) 参数列表
 * @return uint64_t
 */
uint64_t sys_execve(struct pt_regs *regs)
{

    char *user_path = (char *)regs->r8;
    char **argv = (char **)regs->r9;

    int path_len = strnlen_user(user_path, PAGE_4K_SIZE);

    if (path_len >= PAGE_4K_SIZE)
        return -ENAMETOOLONG;
    else if (path_len <= 0)
        return -EFAULT;

    char *path = (char *)kmalloc(path_len + 1, 0);
    if (path == NULL)
        return -ENOMEM;

    memset(path, 0, path_len + 1);

    // 拷贝文件路径
    strncpy_from_user(path, user_path, path_len);
    path[path_len] = '\0';

    // 执行新的程序
    uint64_t retval = do_execve(regs, path, argv, NULL);

    kfree(path);
    return retval;
}

/**
 * @brief 等待进程退出
 *
 * @param pid 目标进程id
 * @param status 返回的状态信息
 * @param options 等待选项
 * @param rusage
 * @return uint64_t
 */
uint64_t sys_wait4(struct pt_regs *regs)
{
    uint64_t pid = regs->r8;
    int *status = (int *)regs->r9;
    int options = regs->r10;
    void *rusage = (void *)regs->r11;

    struct process_control_block *proc = NULL;
    struct process_control_block *child_proc = NULL;

    // 查找pid为指定值的进程
    // ps: 这里判断子进程的方法没有按照posix 2008来写。
    // todo: 根据进程树判断是否为当前进程的子进程
    // todo: 当进程管理模块拥有pcblist_lock之后，调用之前，应当对其加锁
    child_proc = process_find_pcb_by_pid(pid);

    if (child_proc == NULL)
        return -ECHILD;

    // 暂时不支持options选项，该值目前必须为0
    if (options != 0)
        return -EINVAL;

    // 如果子进程没有退出，则等待其退出
    // BUG: 这里存在问题，由于未对进程管理模块加锁，因此可能会出现子进程退出后，父进程还在等待的情况
    // （子进程退出后，process_exit_notify消息丢失）
    while (child_proc->state != PROC_ZOMBIE)
        wait_queue_sleep_on_interriptible(&current_pcb->wait_child_proc_exit);

    // 拷贝子进程的返回码
    if (likely(status != NULL))
        *status = child_proc->exit_code;
    // copy_to_user(status, (void*)child_proc->exit_code, sizeof(int));

    process_release_pcb(child_proc);
    return 0;
}

/**
 * @brief 进程退出
 *
 * @param exit_code 退出返回码
 * @return uint64_t
 */
uint64_t sys_exit(struct pt_regs *regs)
{
    return process_do_exit(regs->r8);
}

uint64_t sys_nanosleep(struct pt_regs *regs)
{
    const struct timespec *rqtp = (const struct timespec *)regs->r8;
    struct timespec *rmtp = (struct timespec *)regs->r9;

    return rs_nanosleep(rqtp, rmtp);
}

ul sys_ahci_end_req(struct pt_regs *regs)
{
    // ahci_end_request();
    return 0;
}

// 系统调用的内核入口程序
void do_syscall_int(struct pt_regs *regs, unsigned long error_code)
{
    ul ret = system_call_table[regs->rax](regs);
    regs->rax = ret; // 返回码
}
uint64_t sys_pipe(struct pt_regs *regs)
{
    return -ENOTSUP;
}

extern uint64_t sys_mkdir(struct pt_regs *regs);

system_call_t system_call_table[MAX_SYSTEM_CALL_NUM] = {
    [0] = system_call_not_exists,
    [1] = sys_put_string,
    [2] = sys_open,
    [3] = sys_close,
    [4] = sys_read,
    [5] = sys_write,
    [6] = sys_lseek,
    [7] = sys_fork,
    [8] = sys_vfork,
    [9] = sys_brk,
    [10] = sys_sbrk,
    [11] = sys_reboot,
    [12] = sys_chdir,
    [13] = sys_getdents,
    [14] = sys_execve,
    [15] = sys_wait4,
    [16] = sys_exit,
    [17] = sys_mkdir,
    [18] = sys_nanosleep,
    [19] = sys_clock,
    [20] = sys_pipe,
    [21] = sys_mstat,
    [22] = sys_unlink_at,
    [23] = sys_kill,
    [24] = sys_sigaction,
    [25] = sys_rt_sigreturn,
    [26] = sys_getpid,
    [27] = sys_sched,
    [28] = sys_dup,
    [29] = sys_dup2,
    [30] = sys_socket,
    [31] = sys_setsockopt,
    [32] = sys_getsockopt,
    [33] = sys_connect,
    [34] = sys_bind,
    [35] = sys_sendto,
    [36] = sys_recvfrom,
    [37] = sys_recvmsg,
    [38] = sys_listen,
    [39] = sys_shutdown,
    [40] = sys_accept,
    [41] = sys_getsockname,
    [42] = sys_getpeername,
    [43 ... 255] = system_call_not_exists,
};
