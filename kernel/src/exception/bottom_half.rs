//! Bottom-half (softirq/tasklet) 屏蔽机制（BH）
//!
//! 目标：提供类似 Linux `local_bh_disable/enable` 与 `*_bh` 锁语义的最小实现。
//!
//! ## 设计要点（对齐 Linux，适配 DragonOS 现状）
//! - DragonOS 当前在硬中断退出路径无条件调用 `do_softirq()`，且 softirq 回调期间会重新打开本地中断。
//! - 如果进程上下文持有普通自旋锁/读写锁时被中断打断，硬中断退出触发 softirq 再取同一把锁，会发生经典死锁。
//! - 因此，我们引入"本 CPU 的 BH 禁用计数"，在 IRQ 退出路径检测到 BH disabled 时跳过 softirq 执行；
//!   在 BH enable（计数归零）且处于 task context 时补跑 pending softirq。
//!
//! ## 重要边界
//! - `lock_bh` 只解决"进程态 vs softirq/tasklet"并发。
//! - 若同一把锁也会在 hardirq 中获取，仍必须使用 `*_irqsave`（`lock_bh` 不关硬中断）。
//! - `local_bh_disable()` **不禁止抢占**，与 Linux 行为一致。`lock_bh()` 会额外禁用抢占。

use core::sync::atomic::{AtomicUsize, Ordering};

use system_error::SystemError;

use alloc::vec::Vec;

use crate::{
    arch::CurrentIrqArch,
    exception::{softirq, tasklet, InterruptArch},
    mm::percpu::{PerCpu, PerCpuVar},
};

lazy_static! {
    /// 每个 CPU 的 BH 禁用计数
    static ref BH_DISABLE_COUNT: PerCpuVar<AtomicUsize> = {
        let mut v = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
        v.resize_with(PerCpu::MAX_CPU_NUM as usize, || AtomicUsize::new(0));
        PerCpuVar::new(v).expect("PerCpuVar length mismatch")
    };
}

#[inline(always)]
fn local_cnt() -> &'static AtomicUsize {
    BH_DISABLE_COUNT.get()
}

/// 返回本 CPU 是否允许执行 softirq/tasklet（bottom half）。
#[inline(always)]
pub fn is_local_bh_disabled() -> bool {
    local_cnt().load(Ordering::SeqCst) != 0
}

/// 初始化中断下半部（bottom half）子系统。
///
/// 该函数按正确顺序依次初始化：
/// 1. softirq：软中断核心机制
/// 3. tasklet：基于 softirq 的 tasklet 机制
///
/// # Returns
///
/// 成功返回 `Ok(())`，失败返回相应错误。
#[inline(never)]
pub fn irq_bottom_half_init() -> Result<(), SystemError> {
    softirq::softirq_init()?;
    tasklet::tasklet_init()?;
    Ok(())
}

/// 一个"禁用本 CPU bottom half"的 RAII 守卫
///
/// - 构造时：`bh_disable_cnt++`
/// - Drop 时：`bh_disable_cnt--`；若归零且处于 task context（当前约束为：本地 IRQ enabled），则补跑 pending softirq。
///
/// ## 注意
/// 与 Linux 行为一致，`local_bh_disable()` **不禁止抢占**。如果需要同时禁用抢占，
/// 应使用 `lock_bh()` 或显式调用 `preempt_disable()`。
pub struct LocalBhDisableGuard;

/// 禁用本 CPU bottom half，返回 RAII 守卫。
///
/// 注意：该接口只屏蔽本 CPU 的 softirq/tasklet 执行，不关硬中断，也不禁止抢占。
#[inline(always)]
pub fn local_bh_disable() -> LocalBhDisableGuard {
    local_cnt().fetch_add(1, Ordering::SeqCst);
    LocalBhDisableGuard
}

impl Drop for LocalBhDisableGuard {
    fn drop(&mut self) {
        // 计数归零时，尝试在当前线程上下文补跑 pending softirq。
        //
        // 约束：只有在本地 IRQ enabled 时才做补跑；若此时 IRQ disabled（例如持有 irqsave 锁），
        // 则只负责降低计数，pending 会在之后的 IRQ 退出点或未来某次 enable 点处理。
        // 注意：
        // - 必须在 IRQ-off 区间完成“递减 + 是否归零”的判定，避免硬中断退出路径观察到中间态。
        // - 不能允许计数下溢；release 下也必须安全，因此用 checked decrement。
        let irq_was_enabled = CurrentIrqArch::is_irq_enabled();
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        let prev =
            local_cnt().fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| x.checked_sub(1));

        match prev {
            Ok(1) if irq_was_enabled => {
                // 本次 drop 让计数归零，且来自 task context（IRQ was enabled）=> 补跑 softirq。
                softirq::do_softirq();
            }
            Ok(_) => {}
            Err(_old /* == 0 */) => {
                // 发生了 enable 次数多于 disable 的逻辑错误；保持计数为 0，避免 wrap 下溢。
                debug_assert!(false, "local_bh_enable underflow");
            }
        }
    }
}
