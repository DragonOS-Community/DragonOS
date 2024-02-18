#include <math.h>
#include <stddef.h>

int64_t pow(int64_t x, int y)
{
    int64_t res = 1;
    for (int i = 0; i < y; ++i)
        res *= x;
    return res;
}