#include <common/mutex.h>
#include <mm/slab.h>
#include <sched/sched.h>
#include <common/current_pcb_compat.h>>

extern void rs_current_pcb_set_state(uint32_t state);
extern uint32_t rs_current_pcb_flags();
extern rs_current_pcb_set_flags(uint32_t new_flags);
extern void* rs_get_current_pcb();
/**
 * @brief 初始化互斥量
 *
 * @param lock mutex结构体
 */
void mutex_init(mutex_t *lock)
{
    atomic_set(&lock->count, 1);
    spin_init(&lock->wait_lock);
    list_init(&lock->wait_list);
}

static void __mutex_sleep()
{
    rs_current_pcb_set_state(PROC_UNINTERRUPTIBLE);
    uint32_t flags = rs_current_pcb_flags();
    rs_current_pcb_set_flags(flags|= PF_NEED_SCHED);
    sched();
}

static void __mutex_acquire(mutex_t *lock)
{
}
/**
 * @brief 对互斥量加锁
 *
 * @param lock mutex结构体
 */
void mutex_lock(mutex_t *lock)
{
    bool lock_ok = 0;

    while (lock_ok == false)
    {
        spin_lock(&lock->wait_lock);
        if (likely(mutex_is_locked(lock)))
        {
            struct mutex_waiter_t *waiter = (struct mutex_waiter_t *)kzalloc(sizeof(struct mutex_waiter_t), 0);
            if (waiter == NULL)
            {
                kerror("In mutex_lock: no memory to alloc waiter. Program's behaviour might be indetermined!");
                spin_unlock(&lock->wait_lock);
                return;
            }
            // memset(waiter, 0, sizeof(struct mutex_waiter_t));
            waiter->pcb = rs_get_current_pcb();
            list_init(&waiter->list);
            list_append(&lock->wait_list, &waiter->list);

            spin_unlock(&lock->wait_lock);

            __mutex_sleep();
        }
        else
        {
            atomic_dec(&lock->count);
            spin_unlock(&lock->wait_lock);
            lock_ok = true;
        }
    }
}

/**
 * @brief 对互斥量解锁
 *
 * @param lock mutex结构体
 */
void mutex_unlock(mutex_t *lock)
{
    if (unlikely(!mutex_is_locked(lock)))
        return;

    spin_lock(&lock->wait_lock);
    struct mutex_waiter_t *wt = NULL;
    if (mutex_is_locked(lock))
    {
        if (!list_empty(&lock->wait_list))
            wt = container_of(list_next(&lock->wait_list), struct mutex_waiter_t, list);

        atomic_inc(&lock->count);
        if (wt != NULL)
            list_del(&wt->list);
    }

    spin_unlock(&lock->wait_lock);

    if (wt != NULL)
    {
        process_wakeup(wt->pcb);
        kfree(wt);
    }
}

/**
 * @brief 尝试对互斥量加锁
 *
 * @param lock mutex结构体
 *
 * @return 成功加锁->1, 加锁失败->0
 */
int mutex_trylock(mutex_t *lock)
{
    if (mutex_is_locked(lock))
        return 0;

    spin_lock(&lock->wait_lock);
    if (mutex_is_locked(lock))
    {
        spin_unlock(&lock->wait_lock);
        return 0;
    }
    else
    {
        atomic_dec(&lock->count);
        spin_unlock(&lock->wait_lock);
        return 1;
    }
}