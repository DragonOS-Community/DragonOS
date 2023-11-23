#pragma once
#include "stddef.h"
#include <arch/arch.h>
#if ARCH(I386) || ARCH(X86_64)

#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/math/bitcount.h>
#else
#error Arch not supported.
#endif
#endif

int64_t pow(int64_t x, int y);
