#include <libc/unistd.h>
#include <libc/stdlib.h>
#include <libc/ctype.h>
#include <libsystem/syscall.h>

int abs(int i)
{
    return i < 0 ? -i : i;
}

long labs(long i)
{
    return i < 0 ? -i : i;
}

long long llabs(long long i)
{
    return i < 0 ? -i : i;
}

int atoi(const char *str)
{
    int n = 0, neg = 0;

    while (isspace(*str))
    {
        str++;
    }

    switch (*str)
    {
    case '-':
        neg = 1;
        break;
    case '+':
        str++;
        break;
    }

    /* Compute n as a negative number to avoid overflow on INT_MIN */
    while (isdigit(*str))
    {
        n = 10 * n - (*str++ - '0');
    }

    return neg ? n : -n;
}
// 种子
unsigned long long _seed_status = 0;
/**
 * @brief 生成随机数
 *
 * @return int 随机数
 */
int rand(void)
{
    _seed_status = (214013 * _seed_status + 2531011) % RAND_MAX;
    return (int)_seed_status;
}
/**
 * @brief 设置随机数种子
 *
 * @param seed 种子
 */
void srand(unsigned seed)
{
    _seed_status = (unsigned long long)seed;
}
/**
 * @brief 退出进程
 *
 * @param status
 */
void exit(int status)
{
    syscall_invoke(SYS_EXIT, status, 0, 0, 0, 0, 0, 0, 0);
}