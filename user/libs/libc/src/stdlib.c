#include <ctype.h>
#include <stdlib.h>
#include <unistd.h>
#include <libsystem/syscall.h>
#include <signal.h>

extern void _fini();

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

/**
 * @brief 退出进程
 *
 * @param status
 */
void exit(int status)
{
    _fini();
    syscall_invoke(SYS_EXIT, status, 0, 0, 0, 0, 0);
}

/**
 * @brief 通过发送SIGABRT，从而退出当前进程
 *
 */
void abort()
{
    // step1：设置SIGABRT的处理函数为SIG_DFL
    signal(SIGABRT, SIG_DFL);
    raise(SIGABRT);
}