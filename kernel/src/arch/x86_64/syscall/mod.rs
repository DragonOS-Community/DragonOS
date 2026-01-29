use crate::{
    arch::{
        ipc::signal::{Signal, X86_64SignalArch},
        syscall::nr::{SYS_ARCH_PRCTL, SYS_RT_SIGRETURN},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    ipc::signal_types::{ChldCode, SignalArch},
    libs::align::SafeForZero,
    mm::VirtAddr,
    process::{ptrace::PtraceStopReason, ProcessFlags, ProcessManager},
    syscall::Syscall,
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

        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
        return;
    }};
}

#[no_mangle]
pub extern "sysv64" fn syscall_handler(frame: &mut TrapFrame) {
    // 系统调用进入时，把系统调用号存入 orig_rax 字段
    // 用于恢复被 ptrace 修改的系统调用号
    // frame.orig_rax = frame.rax;

    // 系统调用进入时，始终开中断
    unsafe {
        CurrentIrqArch::interrupt_enable();
    };

    mfence();
    let pid = ProcessManager::current_pcb().raw_pid();
    let show = false;
    if show {
        debug!("syscall: pid: {:?}, num={:?}\n", pid, frame.rax as usize);
    }

    let pcb = ProcessManager::current_pcb();

    // 注意：必须同时检查 PTRACED 和 TRACE_SYSCALL 标志
    let needs_syscall_trace = pcb.flags().contains(ProcessFlags::TRACE_SYSCALL);
    if needs_syscall_trace {
        // 设置停止原因
        pcb.ptrace_state_mut().stop_reason = PtraceStopReason::SyscallEntry;
        // 构造 syscall entry 的 exit_code: 0x80 | SIGTRAP
        // 0x80 表示 PTRACE_SYSCALL_TRACE (PT_TRACESYSGOOD)
        let exit_code = 0x80 | Signal::SIGTRAP as usize;

        // 同步调用 ptrace_stop，阻塞直到 tracer 唤醒
        // 这与 Linux 6.6.21 的 ptrace_report_syscall_entry() 行为一致
        let _signr = pcb.ptrace_stop(exit_code, ChldCode::Trapped, None);

        // ptrace_stop 返回后，检查 tracer 是否注入了信号
        // 如果有致命信号，需要立即处理
        // TODO: 处理注入信号
    }

    // 关键：必须在 ptrace_stop 返回之后重新读取系统调用号和参数！
    // 因为 tracer 可能在我们睡眠时修改了寄存器。
    let syscall_num = frame.rax as usize;
    let args = [
        frame.rdi as usize,
        frame.rsi as usize,
        frame.rdx as usize,
        frame.r10 as usize,
        frame.r8 as usize,
        frame.r9 as usize,
    ];

    // 保存系统调用入口信息（用于 PTRACE_GETSIGINFO）
    pcb.on_syscall_entry(syscall_num, &args);

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
        let pcb = ProcessManager::current_pcb();
        let result = Syscall::catch_handle(syscall_num, &args, frame)
            .unwrap_or_else(|e| e.to_posix_errno() as usize) as u64;

        // 先将结果写入 frame.rax，这样 tracer 可以通过 PTRACE_POKEUSER 修改返回值
        frame.rax = result;

        // 按照 Linux 6.6.21 的同步 ptrace 模型处理系统调用出口
        // 在 syscall_exit_work() 中调用 ptrace_report_syscall_exit()
        if pcb.flags().contains(ProcessFlags::TRACE_SYSCALL) {
            // 设置停止原因
            pcb.ptrace_state_mut().stop_reason = PtraceStopReason::SyscallExit;

            // 构造 syscall exit 的 exit_code: 0x80 | SIGTRAP
            let exit_code = 0x80 | Signal::SIGTRAP as usize;

            // 同步调用 ptrace_stop，阻塞直到 tracer 唤醒
            // 这与 Linux 6.6.21 的 ptrace_report_syscall_exit() 行为一致
            let _signr =
                pcb.ptrace_stop(exit_code, crate::ipc::signal_types::ChldCode::Trapped, None);

            // ptrace_stop 返回后，tracer 可能修改了 frame.rax
            // 必须返回 frame.rax 而不是原始 result
            // TODO: 处理注入信号
        }

        // 返回 frame.rax，包含 tracer 可能的修改
        frame.rax
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
