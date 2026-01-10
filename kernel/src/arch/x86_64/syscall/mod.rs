use crate::{
    arch::{
        ipc::signal::{Signal, X86_64SignalArch},
        syscall::nr::{SYS_ARCH_PRCTL, SYS_RT_SIGRETURN},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    ipc::signal_types::{
        OriginCode, SigCode, SigFaultInfo, SigInfo, SigType, SignalArch, TrapCode,
    },
    libs::align::SafeForZero,
    mm::VirtAddr,
    process::{ProcessFlags, ProcessManager},
    syscall::{Syscall, SYS_SCHED},
};
use log::debug;
use system_error::SystemError;

use super::{
    interrupt::{entry::set_system_trap_gate, TrapFrame},
    mm::barrier::mfence,
};

pub mod nr;

/// ### 存储PCB系统调用栈以及在syscall过程中暂存用户态rsp的结构体
///
/// 在syscall指令中将会从该结构体中读取系统调用栈和暂存rsp,
/// 使用`gsbase`寄存器实现，后续如果需要使用gsbase寄存器，需要相应设置正确的偏移量
#[repr(C)]
#[derive(Debug, Clone)]
pub(super) struct X86_64GSData {
    pub(super) kaddr: VirtAddr,
    pub(super) uaddr: VirtAddr,
}

impl X86_64GSData {
    /// ### 设置系统调用栈，将会在下一个调度后写入KernelGsbase
    pub fn set_kstack(&mut self, kstack: VirtAddr) {
        self.kaddr = kstack;
    }
}

unsafe impl SafeForZero for X86_64GSData {}

extern "C" {
    fn syscall_int();
    fn syscall_64();
}

macro_rules! syscall_return {
    ($val:expr, $regs:expr, $show:expr) => {{
        let ret = $val;
        $regs.rax = ret as u64;

        if $show {
            let pid = ProcessManager::current_pcb().raw_pid();
            debug!("syscall return:pid={:?},ret= {:?}\n", pid, ret as isize);
        }

        // 系统调用返回前检查并处理信号
        // 这是 ptrace 和普通信号处理的关键入口
        // 如果 HAS_PENDING_SIGNAL 标志被设置，需要调用 do_signal_or_restart
        let pcb = ProcessManager::current_pcb();
        if pcb.flags().contains(ProcessFlags::HAS_PENDING_SIGNAL) {
            drop(pcb);
            // 调用退出到用户态的处理流程，会检查并处理信号
            // irqentry_exit 会检查 is_from_user() 并调用 exit_to_user_mode_prepare
            unsafe { crate::exception::entry::irqentry_exit($regs) };
            // irqentry_exit 已经处理了退出流程，直接返回
            return;
        }
        drop(pcb);

        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
        return;
    }};
}

#[no_mangle]
pub extern "sysv64" fn syscall_handler(frame: &mut TrapFrame) {
    // 系统调用进入时，把系统调用号存入errcode字段，以便在syscall_handler退出后，仍能获取到系统调用号
    frame.errcode = frame.rax;
    let syscall_num = frame.rax as usize;
    // 防止sys_sched由于超时无法退出导致的死锁
    if syscall_num == SYS_SCHED {
        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
    } else {
        unsafe {
            CurrentIrqArch::interrupt_enable();
        }
    }

    let args = [
        frame.rdi as usize,
        frame.rsi as usize,
        frame.rdx as usize,
        frame.r10 as usize,
        frame.r8 as usize,
        frame.r9 as usize,
    ];
    mfence();
    let pid = ProcessManager::current_pcb().raw_pid();
    let show = false;
    // let show = if syscall_num != SYS_SCHED && pid.data() >= 9{
    //     true
    // } else {
    //     false
    // };

    if show {
        debug!("syscall: pid: {:?}, num={:?}\n", pid, syscall_num);
    }

    // 检查是否需要 ptrace syscall trace (系统调用入口)
    let pcb = ProcessManager::current_pcb();
    let needs_syscall_trace = pcb
        .flags()
        .contains(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL);

    if needs_syscall_trace {
        // 按照 Linux 6.6.21 的 ptrace_report_syscall_entry 语义：
        // 在系统调用入口停止，而不是在系统调用执行后停止
        //
        // 实现方式：
        // 1. 设置 ERESTARTNOHAND 错误码，让 try_restart_syscall 稍后重启
        // 2. 发送 SIGTRAP 给自己，触发 ptrace 停止
        // 3. 直接返回，不执行系统调用
        // 4. 当 ptrace_stop 返回后，try_restart_syscall 会将 rip -= 2 重新执行系统调用
        // 5. 第二次执行时，由于 needs_syscall_entry_stop 已被设置为 false，执行系统调用并在出口停止

        let needs_entry_stop = pcb.needs_syscall_entry_stop();
        drop(pcb);

        if needs_entry_stop {
            let pcb = ProcessManager::current_pcb();
            // 第一次进入：需要在系统调用入口停止
            // 保存系统调用信息
            pcb.on_syscall_entry(syscall_num, &args);

            // 设置重启条件：使用 ERESTARTNOHAND 让 try_restart_syscall 稍后重启
            frame.rax = SystemError::ERESTARTNOHAND.to_posix_errno() as u64;
            // errcode 已经保存了原始系统调用号（line 69）

            // 标记已经完成入口停止，下次通过时将执行系统调用
            pcb.set_needs_syscall_entry_stop(false);

            // 设置 HAS_PENDING_SIGNAL 标志，确保在返回用户态前处理信号
            pcb.flags().insert(ProcessFlags::HAS_PENDING_SIGNAL);

            // 发送 SIGTRAP 给自己，触发 ptrace 停止 (syscall entry)
            // 使用 TRAP_TRACE (2) 表示系统调用跟踪
            let mut info = SigInfo::new(
                Signal::SIGTRAP,
                TrapCode::TrapTrace as i32,
                SigCode::SigFault(SigFaultInfo {
                    addr: 0,
                    trapno: crate::ipc::signal_types::TrapCode::TrapTrace as i32,
                }),
                SigType::SigFault(SigFaultInfo {
                    addr: 0,
                    trapno: crate::ipc::signal_types::TrapCode::TrapTrace as i32,
                }),
            );
            let _ = Signal::SIGTRAP.send_signal_info_to_pcb(
                Some(&mut info),
                pcb.clone(),
                crate::process::pid::PidType::PID,
            );

            // 直接返回，不执行系统调用
            // try_restart_syscall 稍后会在信号处理后重启系统调用
            syscall_return!(frame.rax, frame, show);
        }
        // needs_entry_stop == false 表示已经完成入口停止，这次应该执行系统调用
    }

    // Arch specific syscall
    match syscall_num {
        SYS_RT_SIGRETURN => {
            syscall_return!(
                X86_64SignalArch::sys_rt_sigreturn(frame) as usize,
                frame,
                show
            );
        }
        SYS_ARCH_PRCTL => {
            syscall_return!(
                Syscall::arch_prctl(args[0], args[1])
                    .unwrap_or_else(|e| e.to_posix_errno() as usize),
                frame,
                show
            );
        }
        _ => {}
    }
    let mut syscall_handle = || -> u64 {
        let result = Syscall::catch_handle(syscall_num, &args, frame)
            .unwrap_or_else(|e| e.to_posix_errno() as usize) as u64;

        // 系统调用出口 ptrace trace
        let pcb = ProcessManager::current_pcb();
        if pcb
            .flags()
            .contains(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL)
        {
            // 保存系统调用结果
            pcb.on_syscall_exit(result as isize);

            // 重置入口停止标志，以便下一个系统调用也在入口停止
            pcb.set_needs_syscall_entry_stop(true);

            // 设置 HAS_PENDING_SIGNAL 标志，确保在返回用户态前处理信号
            pcb.flags().insert(ProcessFlags::HAS_PENDING_SIGNAL);

            // 发送 SIGTRAP 给自己，触发 ptrace 停止 (syscall exit)
            let mut info = SigInfo::new(
                Signal::SIGTRAP,
                1, // PTRACE_EVENTMSG_SYSCALL_EXIT
                SigCode::Origin(OriginCode::Kernel),
                SigType::SigFault(crate::ipc::signal_types::SigFaultInfo { addr: 0, trapno: 0 }),
            );
            let _ = Signal::SIGTRAP.send_signal_info_to_pcb(
                Some(&mut info),
                pcb.clone(),
                crate::process::pid::PidType::PID,
            );
        }

        result
    };
    syscall_return!(syscall_handle(), frame, show);
}

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    // info!("arch_syscall_init\n");
    unsafe { set_system_trap_gate(0x80, 0, VirtAddr::new(syscall_int as usize)) }; // 系统调用门
    unsafe { init_syscall_64() };
    return Ok(());
}

/// syscall指令初始化函数
pub(super) unsafe fn init_syscall_64() {
    let mut efer = x86::msr::rdmsr(x86::msr::IA32_EFER);
    efer |= 0x1;
    x86::msr::wrmsr(x86::msr::IA32_EFER, efer);

    let syscall_base = (1_u16) << 3;
    let sysret_base = ((4_u16) << 3) | 3;
    let high = (u32::from(sysret_base) << 16) | u32::from(syscall_base);
    // 初始化STAR寄存器
    x86::msr::wrmsr(x86::msr::IA32_STAR, u64::from(high) << 32);

    // 初始化LSTAR,该寄存器存储syscall指令入口
    x86::msr::wrmsr(x86::msr::IA32_LSTAR, syscall_64 as usize as u64);
    x86::msr::wrmsr(x86::msr::IA32_FMASK, 0xfffffffe);
}
