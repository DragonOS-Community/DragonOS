#include <common/math.h>
#include <common/sys/types.h>
#include "libm.h"

double fabs(double x)
{
    union
    {
        double f;
        uint64_t i;
    } u = {x};
    u.i &= -1ULL / 2;
    return u.f;
}


#if __LDBL_MANT_DIG__ == 53 &&  __LDBL_MAX_EXP__ == 1024
long double fabsl(long double x)
{
	return fabs(x);
}
#elif (__LDBL_MANT_DIG__ == 64 || __LDBL_MANT_DIG__ == 113) && __LDBL_MAX_EXP__ == 16384
long double fabsl(long double x)
{
	union ldshape u = {x};

	u.i.se &= 0x7fff;
	return u.f;
}
#endif