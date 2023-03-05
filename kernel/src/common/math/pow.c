#include <common/math.h>
#include <common/stddef.h>

int64_t pow(int64_t x, int y)
{
    if (y == 0)
        return 1;
    if (y == 1)
        return x;
    if (y == 2)
        return x * x;
    int64_t res = 1;
    while (y != 0)
    {
        if (y & 1)
            res *= x;
        y >>= 1;
        x *= x;
    }
    return res;
}