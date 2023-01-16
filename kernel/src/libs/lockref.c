#include <common/lockref.h>
#include <common/compiler.h>

#ifdef __LOCKREF_ENABLE_CMPXCHG__
#include <asm/cmpxchg.h>

#define CMPXCHG_LOOP(__lock_ref, CODE, SUCCESS)                                                     \
    {                                                                                               \
        int retry = 100;                                                                            \
        struct lockref old;                                                                         \
        BUILD_BUG_ON(sizeof(old) != sizeof(uint64_t));                                              \
        old.lock_count = READ_ONCE(__lock_ref->lock_count);                                         \
        while (likely(!spin_is_locked(&old.lock)))                                                  \
        {                                                                                           \
            struct lockref new = old;                                                               \
            CODE;                                                                                   \
            if (likely(arch_try_cmpxchg(&__lock_ref->lock_count, &old.lock_count, new.lock_count))) \
            {                                                                                       \
                SUCCESS;                                                                            \
            }                                                                                       \
            if (!--retry)                                                                           \
                break;                                                                              \
            pause();                                                                                \
        }                                                                                           \
    }
#else

#define CMPXCHG_LOOP(__lock_ref, CODE, SUCCESS) \
    do                                          \
    {                                           \
    } while (0)

#endif

/**
 * @brief 原子的将引用计数加1
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 */
void lockref_inc(struct lockref *lock_ref)
{
    // 先尝试使用cmpxchg进行无锁操作，若成功则返回
    CMPXCHG_LOOP(lock_ref, ++new.count;, return;);

    // 无锁操作超时，或当前是上锁的状态，则退化为有锁操作
    spin_lock(&lock_ref->lock);
    ++lock_ref->count;
    spin_unlock(&lock_ref->lock);
}

/**
 * @brief 原子地将引用计数加1.如果原来的count≤0，则操作失败。
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return bool  操作成功=>true
 *              操作失败=>false
 */
bool lockref_inc_not_zero(struct lockref *lock_ref)
{
    CMPXCHG_LOOP(
        lock_ref,
        {
            if (old.count <= 0)
                return false;
            ++new.count;
        },
        { return true; })

    bool retval;
    spin_lock(&lock_ref->lock);
    retval = false;
    if (lock_ref->count > 0)
    {
        ++lock_ref->count;
        retval = true;
    }
    spin_unlock(&lock_ref->lock);
    return retval;
}

/**
 * @brief 原子地减少引用计数。如果已处于count≤0的状态，则返回-1
 *
 * 本函数与lockref_dec_return()的区别在于，当在cmpxchg()中检测到count<=0或已加锁，本函数会再次尝试通过加锁来执行操作
 * 而后者会直接返回错误
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return int 操作成功 => 返回新的引用变量值
 *             lockref处于count≤0的状态 => 返回-1
 */
int lockref_dec(struct lockref *lock_ref)
{
    CMPXCHG_LOOP(
        lock_ref,
        {
            if (old.count <= 0)
                break;
            --new.count;
        },
        { return new.count; })

    // 如果xchg时，处于已加锁的状态或者检测到old.count <= 0，则采取加锁处理
    int retval = -1;
    spin_lock(&lock_ref->lock);
    if (lock_ref->count > 0)
    {
        --lock_ref->count;
        retval = lock_ref->count;
    }
    spin_unlock(&lock_ref->lock);

    return retval;
}

/**
 * @brief 原子地减少引用计数。如果处于已加锁或count≤0的状态，则返回-1
 *
 * 本函数与lockref_dec()的区别在于，当在cmpxchg()中检测到count<=0或已加锁，本函数会直接返回错误
 * 而后者会再次尝试通过加锁来执行操作
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return int  操作成功 => 返回新的引用变量值
 *              lockref处于已加锁或count≤0的状态 => 返回-1
 */
int lockref_dec_return(struct lockref *lock_ref)
{
    CMPXCHG_LOOP(
        lock_ref,
        {
            if (old.count <= 0)
                return -1;
            --new.count;
        },
        { return new.count; })

    return -1;
}

/**
 * @brief 原子地减少引用计数。若当前的引用计数≤1，则操作失败
 *
 * 该函数与lockref_dec_or_lock_not_zero()的区别在于，当cmpxchg()时发现old.count≤1时，该函数会直接返回false.
 * 而后者在这种情况下，会尝试加锁来进行操作。
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return true 成功将引用计数减1
 * @return false 如果当前的引用计数≤1，操作失败
 */
bool lockref_dec_not_zero(struct lockref *lock_ref)
{
    CMPXCHG_LOOP(
        lock_ref,
        {
            if (old.count <= 1)
                return false;
            --new.count;
        },
        { return true; })

    bool retval = false;
    spin_lock(&lock_ref->lock);
    if (lock_ref->count > 1)
    {
        --lock_ref->count;
        retval = true;
    }
    spin_unlock(&lock_ref->lock);
    return retval;
}

/**
 * @brief 原子地减少引用计数。若当前的引用计数≤1，则操作失败
 *
 * 该函数与lockref_dec_not_zero()的区别在于，当cmpxchg()时发现old.count≤1时，该函数会尝试加锁来进行操作。
 * 而后者在这种情况下，会直接返回false.
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return true 成功将引用计数减1
 * @return false 如果当前的引用计数≤1，操作失败
 */
bool lockref_dec_or_lock_not_zero(struct lockref *lock_ref)
{
    CMPXCHG_LOOP(
        lock_ref,
        {
            if (old.count <= 1)
                break;
            --new.count;
        },
        { return true; });

    bool retval = false;
    spin_lock(&lock_ref->lock);
    if (lock_ref->count > 1)
    {
        --lock_ref->count;
        retval = true;
    }
    spin_unlock(&lock_ref->lock);
    return retval;
}

/**
 * @brief 将lockref变量标记为已经死亡（将count设置为负值）
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 */
void lockref_mark_dead(struct lockref *lock_ref)
{
    // 需要自旋锁先被加锁，若没有被加锁，则会抛出错误信息
    assert_spin_locked(&lock_ref->lock);
    lock_ref->count = -128;
}

/**
 * @brief 自增引用计数。（除非该lockref已经死亡）
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return true 操作成功
 * @return false 操作失败，lockref已死亡
 */
bool lockref_inc_not_dead(struct lockref *lock_ref)
{
    CMPXCHG_LOOP(
        lock_ref,
        {
            if (old.count < 0)
                return false;
            ++new.count;
        },
        { return true; })

    bool retval = false;
    // 快捷路径操作失败，尝试加锁
    spin_lock(&lock_ref->lock);
    if (lock_ref->count >= 0)
    {
        ++lock_ref->count;
        retval = true;
    }
    spin_unlock(&lock_ref->lock);
    return retval;
}
