#include <libc/src/unistd.h>
#include <libc/src/stdlib.h>
#include <libc/src/ctype.h>
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

/**
 * @brief 退出进程
 *
 * @param status
 */
void exit(int status)
{
    syscall_invoke(SYS_EXIT, status, 0, 0, 0, 0, 0, 0, 0);
}