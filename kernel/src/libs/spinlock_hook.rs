//! 关中断自旋等待期间的全局钩子。
//!
//! 背景：IRQ-off 的自旋锁等待者无法响应 IPI。若另一个 CPU 正在等待它的 TLB
//! shootdown ack，而发起者恰好持有该等待者需要的锁，就会形成系统级死锁
//! （实测：CPU1 在 flush_tlb_multi 等 ack，CPU0 在某个关中断自旋锁上）。
//!
//! 这里允许 mm 层注册“服务 pending shootdown”的回调，由 `SpinLock` 在**争用**
//! 循环中周期性调用，使等待者仍能应答。无争用时零开销（不在快路径调用）。

use core::sync::atomic::{AtomicUsize, Ordering};

static HOOK: AtomicUsize = AtomicUsize::new(0);

/// 注册钩子；应在 mm 的 TLB 子系统初始化后调用。
pub fn set_hook(hook: fn(usize)) {
    HOOK.store(hook as usize, Ordering::Release);
}

/// 在自旋等待的争用循环中周期调用。
#[inline]
pub fn run(lock_addr: usize) {
    let hook = HOOK.load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: 只由 `set_hook` 写入合法的 `fn()` 指针，生命周期为进程全局。
        let f: fn(usize) = unsafe { core::mem::transmute(hook) };
        f(lock_addr);
    }
}
