#include <common/spinlock.h>
#include <process/preempt.h>

void __arch_spin_lock(spinlock_t *lock)
{
    __asm__ __volatile__("1:    \n\t"
                         "lock decb %0   \n\t" // 尝试-1
                         "jns 3f    \n\t"      // 加锁成功，跳转到步骤3
                         "2:    \n\t"          // 加锁失败，稍后再试
                         "pause \n\t"
                         "cmpb $0, %0   \n\t"
                         "jle   2b  \n\t" // 若锁被占用，则继续重试
                         "jmp 1b    \n\t" // 尝试加锁
                         "3:"
                         : "=m"(lock->lock)::"memory");
    rs_preempt_disable();
}

void __arch_spin_unlock(spinlock_t *lock)
{
    __asm__ __volatile__("movb $1, %0   \n\t" : "=m"(lock->lock)::"memory");
    rs_preempt_enable();
}

void __arch_spin_lock_no_preempt(spinlock_t *lock)
{
    __asm__ __volatile__("1:    \n\t"
                         "lock decb %0   \n\t" // 尝试-1
                         "jns 3f    \n\t"      // 加锁成功，跳转到步骤3
                         "2:    \n\t"          // 加锁失败，稍后再试
                         "pause \n\t"
                         "cmpb $0, %0   \n\t"
                         "jle   2b  \n\t" // 若锁被占用，则继续重试
                         "jmp 1b    \n\t" // 尝试加锁
                         "3:"
                         : "=m"(lock->lock)::"memory");
}

void __arch_spin_unlock_no_preempt(spinlock_t *lock)
{
    __asm__ __volatile__("movb $1, %0   \n\t" : "=m"(lock->lock)::"memory");
}

long __arch_spin_trylock(spinlock_t *lock)
{
    uint64_t tmp_val = 0;
    rs_preempt_disable();
    // 交换tmp_val和lock的值，若tmp_val==1则证明加锁成功
    asm volatile("lock xchg %%bx, %1  \n\t" // 确保只有1个进程能得到锁
                 : "=q"(tmp_val), "=m"(lock->lock)
                 : "b"(0)
                 : "memory");
    if (!tmp_val)
        rs_preempt_enable();
    return tmp_val;
}
