#include <time.h>
#include <errno.h>
#include <unistd.h>
#include <libsystem/syscall.h>

/**
 * @brief 休眠指定时间
 *
 * @param rqtp 指定休眠的时间
 * @param rmtp 返回的剩余休眠时间
 * @return int
 */
int nanosleep(const struct timespec *rqtp, struct timespec *rmtp)
{
    return syscall_invoke(SYS_NANOSLEEP, (uint64_t)rqtp, (uint64_t)rmtp, 0, 0, 0, 0);
}

/**
 * @brief 睡眠指定时间
 *
 * @param usec 微秒
 * @return int
 */
int usleep(useconds_t usec)
{
    struct timespec ts = {
        tv_sec : (long int)(usec / 1000000),
        tv_nsec : (long int)(usec % 1000000) * 1000UL
    };

    return nanosleep(&ts, NULL);
}

/**
 * @brief 获取系统当前cpu时间
 *
 * @return clock_t
 */
clock_t clock()
{
    return (clock_t)syscall_invoke(SYS_CLOCK, 0, 0, 0, 0, 0, 0);
}