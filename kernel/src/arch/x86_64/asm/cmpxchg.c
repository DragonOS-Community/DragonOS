#include <arch/x86_64/include/asm/cmpxchg.h>

bool __try_cmpxchg_q(uint64_t *ptr, uint64_t *old_ptr, uint64_t *new_ptr)
{
    bool success = __raw_try_cmpxchg(ptr, old_ptr, *new_ptr, 8);
    return success;
}