#![allow(dead_code)]
use super::spinlock::RawSpinlock;
use crate::{
    arch::asm::cmpxchg::try_cmpxchg_q,
    include::bindings::bindings::{ENOTSUP, ETIMEDOUT},
};
use core::{fmt::Debug, intrinsics::size_of};

#[cfg(target_arch = "x86_64")]
/// 由于需要cmpxchg，所以整个lockref按照8字节对齐
#[repr(align(8))]
#[derive(Debug)]
pub struct LockRef {
    pub lock: RawSpinlock,
    pub count: i32,
}

/// 除了x86_64以外的架构，不使用cmpxchg进行优化
#[cfg(not(target_arch = "x86_64"))]
pub struct LockRef {
    lock: RawSpinlock,
    count: i32,
}

enum CmpxchgMode {
    Increase,
    IncreaseNotZero,
    IncreaseNotDead,
    Decrease,
    DecreaseReturn,
    DecreaseNotZero,
    DecreaseOrLockNotZero,
}

impl LockRef {
    pub const INIT: LockRef = LockRef {
        lock: RawSpinlock::INIT,
        count: 0,
    };

    pub fn new() -> LockRef {
        assert_eq!(size_of::<LockRef>(), 8);
        return LockRef::INIT;
    }

    /// @brief 为X86架构实现cmpxchg循环，以支持无锁操作。
    ///
    /// @return 操作成功：返回Ok(new.count)
    /// @return 操作失败，原因：超时 => 返回Err(-ETIMEDOUT)
    /// @return 操作失败，原因：不满足规则 => 返回Err(1)
    #[cfg(target_arch = "x86_64")]
    #[inline]
    fn cmpxchg_loop(&mut self, mode: CmpxchgMode) -> Result<i32, i32> {
        use core::ptr::read_volatile;

        use crate::arch::cpu::cpu_relax;

        let mut old: LockRef = LockRef::INIT;
        old.count = unsafe { read_volatile(&self.count) };
        for _ in 0..100 {
            if !old.lock.is_locked() {
                let mut new = LockRef::INIT;
                unsafe {
                    *(&mut new as *mut LockRef as *mut usize as *mut u64) =
                        read_volatile(&mut old as *mut LockRef as *mut usize as *mut u64);
                    new.lock.set_value(false);
                }

                // 根据不同情况，执行不同代码
                match mode {
                    CmpxchgMode::Increase => {
                        new.count += 1;
                    }
                    CmpxchgMode::IncreaseNotZero => {
                        // 操作失败
                        if old.count <= 0 {
                            return Err(1);
                        }
                        new.count += 1;
                    }

                    CmpxchgMode::IncreaseNotDead => {
                        if old.count < 0 {
                            return Err(1);
                        }
                        new.count += 1;
                    }

                    CmpxchgMode::Decrease | CmpxchgMode::DecreaseReturn => {
                        if old.count <= 0 {
                            return Err(1);
                        }
                        new.count -= 1;
                    }
                    CmpxchgMode::DecreaseNotZero | CmpxchgMode::DecreaseOrLockNotZero => {
                        if old.count <= 1 {
                            return Err(1);
                        }
                        new.count -= 1;
                    }
                }

                if unsafe {
                    try_cmpxchg_q(
                        self as *mut LockRef as *mut usize as *mut u64,
                        &mut old as *mut LockRef as *mut usize as *mut u64,
                        &mut new as *mut LockRef as *mut usize as *mut u64,
                    )
                } {
                    // 无锁操作成功，返回新的值
                    return Ok(new.count);
                }
                cpu_relax();
            }
        }

        return Err(-(ETIMEDOUT as i32));
    }

    /// @brief 对于不支持无锁lockref的架构，直接返回Err(-ENOTSUP)，表示不支持
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    fn cmpxchg_loop(&mut self, mode: CmpxchgMode) -> Result<i32, i32> {
        use crate::include::bindings::bindings::ENOTSUP;

        return Err(-(ENOTSUP as i32));
    }

    /// @brief 原子的将引用计数加1
    pub fn inc(&mut self) {
        let cmpxchg_result = self.cmpxchg_loop(CmpxchgMode::Increase);
        if cmpxchg_result.is_ok() {
            return;
        }

        self.lock.lock();
        self.count += 1;
        self.lock.unlock();
    }

    /**
     * @brief 原子地将引用计数加1.如果原来的count≤0，则操作失败。
     *
     * @return Result<i32, i32>     操作成功=>Ok(self.count)
     *                              操作失败=>Err(-1)
     */
    pub fn inc_not_zero(&mut self) -> Result<i32, i32> {
        {
            let cmpxchg_res = self.cmpxchg_loop(CmpxchgMode::IncreaseNotZero);
            if cmpxchg_res.is_ok() {
                return cmpxchg_res;
            } else if cmpxchg_res.unwrap_err() == 1 {
                // 不满足not zero 的条件
                return Err(-1);
            }
        }

        let mut retval = Err(-1);
        self.lock.lock();

        if self.count > 0 {
            self.count += 1;
            retval = Ok(self.count);
        }

        self.lock.unlock();
        return retval;
    }

    /**
     * @brief 引用计数自增1。（除非该lockref已经被标记为死亡）
     *
     * @return Ok(self.count) 操作成功
     * @return Err(-1) 操作失败，lockref已死亡
     */
    pub fn inc_not_dead(&mut self) -> Result<i32, i32> {
        {
            let cmpxchg_result = self.cmpxchg_loop(CmpxchgMode::IncreaseNotDead);
            if cmpxchg_result.is_ok() {
                return cmpxchg_result;
            } else if cmpxchg_result.unwrap_err() == 1 {
                return Err(-1);
            }
        }
        // 快捷路径操作失败，尝试加锁
        let mut retval = Err(-1);

        self.lock.lock();
        if self.count >= 0 {
            self.count += 1;
            retval = Ok(self.count);
        }
        self.lock.unlock();
        return retval;
    }

    /**
     * @brief 原子地将引用计数-1。如果已处于count≤0的状态，则返回Err(-1)
     *
     * 本函数与lockref_dec_return()的区别在于，当在cmpxchg()中检测到count<=0或已加锁，本函数会再次尝试通过加锁来执行操作
     * 而后者会直接返回错误
     *
     * @return int 操作成功 => 返回新的引用变量值
     *             操作失败lockref处于count≤0的状态 => 返回-1
     */
    pub fn dec(&mut self) -> Result<i32, i32> {
        {
            let cmpxchg_result = self.cmpxchg_loop(CmpxchgMode::Decrease);
            if cmpxchg_result.is_ok() {
                return cmpxchg_result;
            }
        }
        let retval: Result<i32, i32>;
        self.lock.lock();

        if self.count > 0 {
            self.count -= 1;
            retval = Ok(self.count);
        } else {
            retval = Err(-1);
        }

        self.lock.unlock();

        return retval;
    }

    /**
     * @brief 原子地将引用计数减1。如果处于已加锁或count≤0的状态，则返回-1
     *      若当前处理器架构不支持cmpxchg，则退化为self.dec()
     *
     * 本函数与lockref_dec()的区别在于，当在cmpxchg()中检测到count<=0或已加锁，本函数会直接返回错误
     * 而后者会再次尝试通过加锁来执行操作
     *
     * @return int  操作成功 => 返回新的引用变量值
     *              操作失败，lockref处于已加锁或count≤0的状态 => 返回-1
     */
    pub fn dec_return(&mut self) -> Result<i32, i32> {
        let cmpxchg_result = self.cmpxchg_loop(CmpxchgMode::DecreaseReturn);
        if cmpxchg_result.is_ok() {
            return cmpxchg_result;
        } else if cmpxchg_result.unwrap_err() == 1 {
            return Err(-1);
        }

        // 由于cmpxchg超时，操作失败
        if cmpxchg_result.unwrap_err() != -(ENOTSUP as i32) {
            return Err(-1);
        }

        // 能走到这里，代表架构当前不支持cmpxchg
        // 退化为直接dec，加锁
        return self.dec();
    }

    /**
     * @brief 原子地将引用计数减1。若当前的引用计数≤1，则操作失败
     *
     * 该函数与lockref_dec_or_lock_not_zero()的区别在于，当cmpxchg()时发现old.count≤1时，该函数会直接返回Err(-1)
     * 而后者在这种情况下，会尝试加锁来进行操作。
     *
     * @return Ok(self.count) 成功将引用计数减1
     * @return Err(-1) 如果当前的引用计数≤1，操作失败
     */
    pub fn dec_not_zero(&mut self) -> Result<i32, i32> {
        {
            let cmpxchg_result = self.cmpxchg_loop(CmpxchgMode::DecreaseNotZero);
            if cmpxchg_result.is_ok() {
                return cmpxchg_result;
            } else if cmpxchg_result.unwrap_err() == 1 {
                return Err(-1);
            }
        }

        let retval: Result<i32, i32>;
        self.lock.lock();
        if self.count > 1 {
            self.count -= 1;
            retval = Ok(self.count);
        } else {
            retval = Err(-1);
        }
        self.lock.unlock();
        return retval;
    }

    /**
     * @brief 原子地将引用计数减1。若当前的引用计数≤1，则操作失败
     *
     * 该函数与lockref_dec_not_zero()的区别在于，当cmpxchg()时发现old.count≤1时，该函数会尝试加锁来进行操作。
     * 而后者在这种情况下，会直接返回Err(-1).
     *
     * @return Ok(self.count) 成功将引用计数减1
     * @return Err(-1) 如果当前的引用计数≤1，操作失败
     */
    pub fn dec_or_lock_not_zero(&mut self) -> Result<i32, i32> {
        {
            let cmpxchg_result = self.cmpxchg_loop(CmpxchgMode::DecreaseOrLockNotZero);
            if cmpxchg_result.is_ok() {
                return cmpxchg_result;
            }
        }

        let retval: Result<i32, i32>;
        self.lock.lock();
        if self.count > 1 {
            self.count -= 1;
            retval = Ok(self.count);
        } else {
            retval = Err(-1);
        }
        self.lock.unlock();
        return retval;
    }

    /**
     * @brief 原子地将lockref变量标记为已经死亡（将count设置为负值）
     */
    pub fn mark_dead(&mut self) {
        self.lock.lock();
        self.count = -128;
        self.lock.unlock();
    }
}

/*
* 您可以使用以下代码测试lockref

   let mut lockref = LockRef::new();
   kdebug!("lockref={:?}", lockref);
   lockref.inc();
   assert_eq!(lockref.count, 1);
   kdebug!("lockref={:?}", lockref);
   assert!(lockref.dec().is_ok());
   assert_eq!(lockref.count, 0);

   assert!(lockref.dec().is_err());
   assert_eq!(lockref.count, 0);

   lockref.inc();
   assert_eq!(lockref.count, 1);

   assert!(lockref.dec_not_zero().is_err());

   lockref.inc();
   assert_eq!(lockref.count, 2);

   assert!(lockref.dec_not_zero().is_ok());

   lockref.mark_dead();
   assert!(lockref.count < 0);

   assert!(lockref.inc_not_dead().is_err());
   kdebug!("lockref={:?}", lockref);
*/
