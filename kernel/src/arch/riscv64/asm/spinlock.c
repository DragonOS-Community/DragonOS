#include <common/spinlock.h>
#include <process/preempt.h>

void __arch_spin_lock(spinlock_t *lock)
{
    while(1);
    rs_preempt_disable();
}

void __arch_spin_unlock(spinlock_t *lock)
{
    while(1);
    rs_preempt_enable();
}

void __arch_spin_lock_no_preempt(spinlock_t *lock)
{
    while(1);
}

void __arch_spin_unlock_no_preempt(spinlock_t *lock)
{
    while(1);
}

long __arch_spin_trylock(spinlock_t *lock)
{
    uint64_t tmp_val = 0;
    rs_preempt_disable();
    // 交换tmp_val和lock的值，若tmp_val==1则证明加锁成功
    while(1);
    if (!tmp_val)
        rs_preempt_enable();
    return tmp_val;
}
