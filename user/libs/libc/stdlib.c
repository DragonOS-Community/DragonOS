#include <libc/unistd.h>
#include <libc/stdlib.h>

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