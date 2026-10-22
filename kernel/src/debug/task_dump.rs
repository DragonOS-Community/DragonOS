//! 系统停顿看门狗与任务状态转储（Linux SysRQ-T 的等价物）。
//!
//! 触发条件：连续多个观察窗口内上下文切换计数零增长（系统级停顿——
//! 所有任务被阻塞、CPU 空闲），且系统内任务数达到阈值。
//! 输出走关键通道（`println_critical!`），在控制台背压下也保证可见。
//! 用于捕获"静默楔死"类缺陷的现场（哪些任务、什么状态、阻塞在哪）。

use alloc::string::ToString;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::init::initcall::INITCALL_DEVICE;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::ProcessManager;
use crate::time::{sleep::nanosleep, PosixTimeSpec};
use core::sync::atomic::Ordering;

/// 观察窗口长度（秒）
const WATCH_INTERVAL_S: i64 = 10;
/// 连续零增长窗口数达到该值视为停顿
const STALL_WINDOWS: u32 = 2;
/// 系统内任务数低于该值不转储（避免启动早期误报）
const MIN_TASKS: usize = 8;

fn stall_watchdog_thread() -> i32 {
    let mut stalled_windows: u32 = 0;
    let mut last_switches = crate::process::nr_context_switches();
    // 转储后需重新观察到进度才会再次武装，防止刷屏
    let mut armed = true;
    // B+（不可中断阻塞）持续检测：dockerd/containerd 类 daemon 卡死后系统
    // 整体仍健康（ctxsw 正常增长），停摆检测抓不到——但被卡任务会长期停留
    // 在 uninterruptible 状态。连续多个窗口观察到即转储一次（触发不依赖
    // 任何控制台输入路径）。
    let mut uninterruptible_windows: u32 = 0;
    let mut uninterruptible_armed = true;
    loop {
        let _ = nanosleep(PosixTimeSpec::new(WATCH_INTERVAL_S, 0));
        let now_switches = crate::process::nr_context_switches();
        if now_switches == last_switches {
            stalled_windows = stalled_windows.saturating_add(1);
        } else {
            stalled_windows = 0;
            armed = true;
        }
        last_switches = now_switches;

        if stalled_windows >= STALL_WINDOWS
            && armed
            && crate::process::ProcessManager::get_all_processes().len() >= MIN_TASKS
        {
            dump_all_tasks();
            armed = false;
        }

        // B+ 持续检测（与停摆检测互补）
        let unint_count = crate::process::snapshot_all_processes()
            .map(|procs| {
                procs
                    .iter()
                    .filter(|p| {
                        !p.is_kthread()
                            && p.raw_pid().data() > 100
                            && matches!(
                                p.sched_info().state(),
                                crate::process::ProcessState::Blocked(_)
                            )
                    })
                    .count()
            })
            .unwrap_or(0);
        // 用户态阻塞计数（含可中断阻塞）：docker 链楔死时 43 个、空闲
        // 系统 ~15 个以下；阈值 25 + 连续窗口判定，避免空闲误触发。
        if unint_count >= 25 {
            uninterruptible_windows = uninterruptible_windows.saturating_add(1);
        } else {
            uninterruptible_windows = 0;
            uninterruptible_armed = true;
        }
        if uninterruptible_windows >= STALL_WINDOWS && uninterruptible_armed {
            println_critical!(
                "==== USER BLOCK STORM: {} user tasks blocked for >{}s ====",
                unint_count,
                uninterruptible_windows * (WATCH_INTERVAL_S as u32)
            );
            dump_all_tasks();
            uninterruptible_armed = false;
        }
    }
}

/// 经关键通道转储所有任务的 pid / 名称 / 调度状态
pub(crate) fn dump_all_tasks() {
    let procs = match crate::process::snapshot_all_processes() {
        Ok(p) => p,
        Err(_) => {
            println_critical!("TASK DUMP: failed to snapshot processes");
            return;
        }
    };
    println_critical!(
        "==== TASK DUMP: kernel stall detected, {} tasks ====",
        procs.len()
    );
    for pcb in procs.iter() {
        let state = pcb.sched_info().state();
        println_critical!(
            "[pid {:>6}] {:<16} state={:?}",
            pcb.raw_pid().data(),
            pcb.basic().name(),
            state
        );
        // 用户态 Blocked 任务：打印其内核栈顶 TrapFrame 的关键寄存器，
        // 离线用 nm 反解符号定位卡死的系统调用路径（dockerd B+ 取证）。
        if matches!(state, crate::process::ProcessState::Blocked(_))
            && pcb.raw_pid().data() > 100
            && pcb.basic().name() != "idle"
        {
            unsafe {
                let kstack = pcb.kernel_stack_force_ref();
                let sp = kstack.stack_max_address().data();
                let tf = sp - core::mem::size_of::<crate::arch::interrupt::TrapFrame>();
                let tf_ptr = tf as *const crate::arch::interrupt::TrapFrame;
                if tf_ptr as usize >= kstack.start_address().data() {
                    let tfv = &*tf_ptr;
                    println_critical!(
                        "[pid {:>6}] {:<16} rip={:#x} LSYSCALL={} errcode={:#x} rdi={:#x} rsi={:#x} rdx={:#x} r10={:#x}\n",
                        pcb.raw_pid().data(),
                        pcb.basic().name(),
                        core::ptr::read_volatile(&tfv.rip),
                        pcb.last_syscall.load(Ordering::Relaxed),
                        core::ptr::read_volatile(&tfv.errcode),
                        core::ptr::read_volatile(&tfv.rdi),
                        core::ptr::read_volatile(&tfv.rsi),
                        core::ptr::read_volatile(&tfv.rdx),
                        core::ptr::read_volatile(&tfv.r10)
                    );
                    // 自 TrapFrame 向下扫描内核栈，打印内核文本区间的
                    // 返回地址（离线 nm 反解得内核侧卡点调用链）。
                    let base = kstack.start_address().data();
                    let mut a = tf - 8;
                    let mut found = 0;
                    while a >= base && found < 12 {
                        let v = core::ptr::read_volatile(a as *const u64);
                        if (0xffff_8000_0000_0000..0xffff_8000_0200_0000).contains(&v) {
                            println_critical!("    kret -{:#x}: {:#x}", tf - a, v);
                            found += 1;
                        }
                        a -= 8;
                    }
                }
            }
        }
    }
    println_critical!("==== END TASK DUMP ====");
}

#[unified_init(INITCALL_DEVICE)]
fn stall_watchdog_init() -> Result<(), SystemError> {
    let closure =
        KernelThreadClosure::StaticEmptyClosure((&(stall_watchdog_thread as fn() -> i32), ()));
    KernelThreadMechanism::create_and_run(closure, "stall_watchdog".to_string())
        .ok_or(SystemError::ENOMEM)
        .expect("create stall_watchdog thread failed");
    Ok(())
}
