#include "syscall.h"
#include "../process/process.h"
#include <exception/gate.h>
#include <exception/irq.h>
#include <driver/disk/ahci/ahci.h>
#include <mm/slab.h>
#include <common/errno.h>
#include <common/fcntl.h>
#include <filesystem/fat32/fat32.h>

// 导出系统调用入口函数，定义在entry.S中
extern void system_call(void);
extern void syscall_int(void);

/**
 * @brief 导出系统调用处理函数的符号
 *
 */
#define SYSCALL_COMMON(syscall_num, symbol) extern unsigned long symbol(struct pt_regs *regs);
SYSCALL_COMMON(0, system_call_not_exists); // 导出system_call_not_exists函数
#undef SYSCALL_COMMON                      // 取消前述宏定义

/**
 * @brief 重新定义为：把系统调用函数加入系统调用表
 * @param syscall_num 系统调用号
 * @param symbol 系统调用处理函数
 */
#define SYSCALL_COMMON(syscall_num, symbol) [syscall_num] = symbol,

/**
 * @brief sysenter的系统调用函数，从entry.S中跳转到这里
 *
 * @param regs 3特权级下的寄存器值,rax存储系统调用号
 * @return ul 对应的系统调用函数的地址
 */
ul system_call_function(struct pt_regs *regs)
{
    return system_call_table[regs->rax](regs);
}

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
        : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5), "m"(arg6), "m"(arg7)
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

    if (regs->r9 == 0 && regs->r10 == 0)
        printk((char *)regs->r8);
    else
        printk_color(regs->r9, regs->r10, (char *)regs->r8);
    // printk_color(BLACK, WHITE, (char *)regs->r8);

    return 0;
}

uint64_t sys_open(struct pt_regs *regs)
{

    char *filename = (char *)(regs->r8);
    int flags = (int)(regs->r9);

    long path_len = strnlen_user(filename, PAGE_4K_SIZE);

    if (path_len <= 0) // 地址空间错误
    {
        return -EFAULT;
    }
    else if (path_len >= PAGE_4K_SIZE) // 名称过长
    {
        return -ENAMETOOLONG;
    }

    // 为待拷贝文件路径字符串分配内存空间
    char *path = (char *)kmalloc(path_len, 0);
    if (path == NULL)
        return -ENOMEM;
    memset(path, 0, path_len);

    strncpy_from_user(path, filename, path_len);

    // 寻找文件
    struct vfs_dir_entry_t *dentry = vfs_path_walk(path, 0);
    kfree(path);

    if (dentry != NULL)
        printk_color(ORANGE, BLACK, "Found %s\nDIR_FstClus:%#018lx\tDIR_FileSize:%#018lx\n", path, ((struct fat32_inode_info_t *)(dentry->dir_inode->private_inode_info))->first_clus, dentry->dir_inode->file_size);
    else
        printk_color(ORANGE, BLACK, "Can`t find file\n");

    if (dentry == NULL)
        return -ENOENT;

    // 暂时认为目标是目录是一种错误
    if (dentry->dir_inode->attribute == VFS_ATTR_DIR)
        return -EISDIR;

    // 创建文件描述符
    struct vfs_file_t *file_ptr = (struct vfs_file_t *)kmalloc(sizeof(struct vfs_file_t), 0);
    memset(file_ptr, 0, sizeof(struct vfs_file_t));

    int errcode = -1;

    file_ptr->dEntry = dentry;
    file_ptr->mode = flags;
    file_ptr->file_ops = dentry->dir_inode->file_ops;

    // 如果文件系统实现了打开文件的函数
    if (file_ptr->file_ops && file_ptr->file_ops->open)
        errcode = file_ptr->file_ops->open(dentry->dir_inode, file_ptr);

    if (errcode != VFS_SUCCESS)
    {
        kfree(file_ptr);
        return -EFAULT;
    }

    if (file_ptr->mode & O_TRUNC) // 清空文件
        file_ptr->dEntry->dir_inode->file_size = 0;

    if (file_ptr->mode & O_APPEND)
        file_ptr->position = file_ptr->dEntry->dir_inode->file_size;
    else
        file_ptr->position = 0;

    struct vfs_file_t **f = current_pcb->fds;

    int fd_num = -1;

    // 在指针数组中寻找空位
    // todo: 当pcb中的指针数组改为动态指针数组之后，需要更改这里（目前还是静态指针数组）
    for (int i = 0; i < PROC_MAX_FD_NUM; ++i)
    {
        if (f[i] == NULL) // 找到指针数组中的空位
        {
            fd_num = i;
            break;
        }
    }

    // 指针数组没有空位了
    if (fd_num == -1)
    {
        kfree(file_ptr);
        return -EMFILE;
    }
    // 保存文件描述符
    f[fd_num] = file_ptr;

    return fd_num;
}

/**
 * @brief 关闭文件系统调用
 *
 * @param fd_num 文件描述符号
 *
 * @param regs
 * @return uint64_t
 */
uint64_t sys_close(struct pt_regs *regs)
{
    int fd_num = (int)regs->r8;

    kdebug("sys close: fd=%d", fd_num);
    // 校验文件描述符范围
    if (fd_num < 0 || fd_num > PROC_MAX_FD_NUM)
        return -EBADF;
    // 文件描述符不存在
    if (current_pcb->fds[fd_num] == NULL)
        return -EBADF;
    struct vfs_file_t *file_ptr = current_pcb->fds[fd_num];
    uint64_t ret;
    // If there is a valid close function
    if (file_ptr->file_ops && file_ptr->file_ops->close)
        ret = file_ptr->file_ops->close(file_ptr->dEntry->dir_inode, file_ptr);

    kfree(file_ptr);
    current_pcb->fds[fd_num] = NULL;
    return 0;
}

/**
 * @brief 从文件中读取数据
 *
 * @param fd_num regs->r8 文件描述符号
 * @param buf regs->r9 输出缓冲区
 * @param count regs->r10 要读取的字节数
 *
 * @return uint64_t
 */
uint64_t sys_read(struct pt_regs *regs)
{
    int fd_num = (int)regs->r8;
    void *buf = (void *)regs->r9;
    int64_t count = (int64_t)regs->r10;

    // kdebug("sys read: fd=%d", fd_num);

    // 校验文件描述符范围
    if (fd_num < 0 || fd_num > PROC_MAX_FD_NUM)
        return -EBADF;

    // 文件描述符不存在
    if (current_pcb->fds[fd_num] == NULL)
        return -EBADF;

    if (count < 0)
        return -EINVAL;

    struct vfs_file_t *file_ptr = current_pcb->fds[fd_num];
    uint64_t ret;
    if (file_ptr->file_ops && file_ptr->file_ops->read)
        ret = file_ptr->file_ops->read(file_ptr, (char *)buf, count, &(file_ptr->position));

    return ret;
}

/**
 * @brief 向文件写入数据
 *
 * @param fd_num regs->r8 文件描述符号
 * @param buf regs->r9 输入缓冲区
 * @param count regs->r10 要写入的字节数
 *
 * @return uint64_t
 */
uint64_t sys_write(struct pt_regs *regs)
{
    int fd_num = (int)regs->r8;
    void *buf = (void *)regs->r9;
    int64_t count = (int64_t)regs->r10;

    kdebug("sys write: fd=%d", fd_num);

    // 校验文件描述符范围
    if (fd_num < 0 || fd_num > PROC_MAX_FD_NUM)
        return -EBADF;

    // 文件描述符不存在
    if (current_pcb->fds[fd_num] == NULL)
        return -EBADF;

    if (count < 0)
        return -EINVAL;

    struct vfs_file_t *file_ptr = current_pcb->fds[fd_num];
    uint64_t ret;
    if (file_ptr->file_ops && file_ptr->file_ops->write)
        ret = file_ptr->file_ops->write(file_ptr, (char *)buf, count, &(file_ptr->position));

    return ret;
}

/**
 * @brief 调整文件的访问位置
 *
 * @param fd_num 文件描述符号
 * @param offset 偏移量
 * @param whence 调整模式
 * @return uint64_t
 */
uint64_t sys_lseek(struct pt_regs *regs)
{
    int fd_num = (int)regs->r8;
    long offset = (long)regs->r9;
    int whence = (int)regs->r10;

    kdebug("sys_lseek: fd=%d", fd_num);
    uint64_t retval = 0;

    // 校验文件描述符范围
    if (fd_num < 0 || fd_num > PROC_MAX_FD_NUM)
        return -EBADF;

    // 文件描述符不存在
    if (current_pcb->fds[fd_num] == NULL)
        return -EBADF;

    struct vfs_file_t *file_ptr = current_pcb->fds[fd_num];
    if (file_ptr->file_ops && file_ptr->file_ops->lseek)
        retval = file_ptr->file_ops->lseek(file_ptr, offset, whence);

    return retval;
}

uint64_t sys_fork(struct pt_regs *regs)
{
    kdebug("sys_fork");
    return do_fork(regs, 0, regs->rsp, 0);
}
uint64_t sys_vfork(struct pt_regs *regs)
{
    kdebug("sys vfork");
    return do_fork(regs, CLONE_VM | CLONE_FS | CLONE_SIGNAL, regs->rsp, 0);
}

ul sys_ahci_end_req(struct pt_regs *regs)
{
    ahci_end_request();
    return 0;
}

// 系统调用的内核入口程序
void do_syscall_int(struct pt_regs *regs, unsigned long error_code)
{

    ul ret = system_call_table[regs->rax](regs);
    regs->rax = ret; // 返回码
}

system_call_t system_call_table[MAX_SYSTEM_CALL_NUM] =
    {
        [0] = system_call_not_exists,
        [1] = sys_put_string,
        [2] = sys_open,
        [3] = sys_close,
        [4] = sys_read,
        [5] = sys_write,
        [6] = sys_lseek,
        [7 ... 254] = system_call_not_exists,
        [255] = sys_ahci_end_req};
