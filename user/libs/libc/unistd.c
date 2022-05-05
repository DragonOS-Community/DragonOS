#include <libc/unistd.h>
#include <libsystem/syscall.h>


/**
 * @brief 往屏幕上输出字符串
 * 
 * @param str 字符串指针
 * @param front_color 前景色
 * @param bg_color 背景色
 * @return int64_t 
 */
int64_t put_string(char* str, uint64_t front_color, uint64_t bg_color)
{
    return syscall_invoke(SYS_PUT_STRING, str, front_color, bg_color,0,0,0,0,0);
}
/**
 * @brief 关闭文件接口
 *
 * @param fd 文件描述符
 * @return int
 */
int close(int fd)
{
    return syscall_invoke(SYS_CLOSE, fd, 0, 0, 0, 0, 0, 0, 0);
}

/**
 * @brief 从文件读取数据的接口
 *
 * @param fd 文件描述符
 * @param buf 缓冲区
 * @param count 待读取数据的字节数
 * @return ssize_t 成功读取的字节数
 */
ssize_t read(int fd, void *buf, size_t count)
{
    return (ssize_t)syscall_invoke(SYS_READ, fd, buf, count,0,0,0,0,0);
}

/**
 * @brief 向文件写入数据的接口
 *
 * @param fd 文件描述符
 * @param buf 缓冲区
 * @param count 待写入数据的字节数
 * @return ssize_t 成功写入的字节数
 */
ssize_t write(int fd, void const *buf, size_t count)
{
    return (ssize_t)syscall_invoke(SYS_WRITE, fd, buf, count,0,0,0,0,0);
}

/**
 * @brief 调整文件的访问位置
 *
 * @param fd 文件描述符号
 * @param offset 偏移量
 * @param whence 调整模式
 * @return uint64_t 调整结束后的文件访问位置
 */
off_t lseek(int fd, off_t offset, int whence)
{
    return (off_t)syscall_invoke(SYS_LSEEK, fd, offset, whence, 0,0,0,0,0);
}