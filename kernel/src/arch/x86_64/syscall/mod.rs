#![allow(function_casts_as_integer)]

use crate::{
    arch::{
        ipc::signal::{Signal, X86_64SignalArch},
        syscall::nr::{SYS_ARCH_PRCTL, SYS_RT_SIGRETURN},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    ipc::signal_types::{ChldCode, OriginCode, SigCode, SigInfo, SigType, SignalArch},
    libs::align::SafeForZero,
    mm::VirtAddr,
    process::{
        ptrace::{PtraceOptions, PTRACE_EVENTMSG_SYSCALL_ENTRY, PTRACE_EVENTMSG_SYSCALL_EXIT},
        ProcessFlags, ProcessManager, RawPid,
    },
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
    fn syscall_exit_to_user_mode(frame: &mut TrapFrame);
}

macro_rules! syscall_return {
    ($val:expr, $regs:expr, $show:expr) => {{
        let ret = $val;
        $regs.rax = ret as u64;

        if $show {
            let pid = ProcessManager::current_pcb().raw_pid();
            debug!("syscall return:pid={:?},ret= {:?}\n", pid, ret as isize);
        }

        // 调用统一的退出路径来处理信号和系统调用重启
        unsafe {
            syscall_exit_to_user_mode($regs);
        }
        return;
    }};
}

fn syscall_trace_exit_code(pcb: &crate::process::ProcessControlBlock) -> usize {
    if pcb.has_ptrace_option(PtraceOptions::TRACESYSGOOD) {
        0x80 | Signal::SIGTRAP as usize
    } else {
        Signal::SIGTRAP as usize
    }
}

#[inline(always)]
fn syscall_nr_from_orig_ax(orig_ax: u64) -> i32 {
    (orig_ax as u32) as i32
}

fn ptrace_report_syscall_stop(
    pcb: &alloc::sync::Arc<crate::process::ProcessControlBlock>,
    message: usize,
    pid: RawPid,
) {
    let exit_code = syscall_trace_exit_code(pcb);
    let mut info = SigInfo::new(
        Signal::SIGTRAP,
        0,
        SigCode::Raw(exit_code as i32),
        SigType::Kill {
            pid,
            uid: pcb.cred().uid.data() as u32,
        },
    );
    pcb.set_ptrace_message(message);

    let signr = pcb.ptrace_stop(exit_code, ChldCode::Trapped, Some(&mut info));
    if signr != 0 {
        let sig = Signal::from(signr as i32);
        if sig.is_valid() {
            // syscall-stop 恢复时如果 tracer 指定了非 0 signal，内核在 stop 返回后补发该信号。
            let mut reinject = SigInfo::new(
                sig,
                0,
                SigCode::Origin(OriginCode::Kernel),
                SigType::Kill {
                    pid: RawPid::new(0),
                    uid: 0,
                },
            );
            if let Err(e) = sig.send_signal_info_to_pcb(
                Some(&mut reinject),
                pcb.clone(),
                crate::process::pid::PidType::PID,
            ) {
                log::error!("ptrace syscall-stop reinject failed for pid={pid:?}: {e:?}");
            }
        }
    }
}

#[no_mangle]
pub extern "sysv64" fn syscall_handler(frame: &mut TrapFrame) {
    // DragonOS 复用 TrapFrame.errcode 对应 Linux pt_regs->orig_ax/orig_rax。
    // 供 ptrace 在 syscall-stop 期间修改“将要尝试的 syscall nr”。
    frame.errcode = frame.rax;

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

    let trace_flags = *pcb.flags();
    let needs_syscall_trace = trace_flags.contains(ProcessFlags::PTRACED)
        && trace_flags.intersects(ProcessFlags::TRACE_SYSCALL | ProcessFlags::TRACE_SYSEMU);
    let is_sysemu = trace_flags.contains(ProcessFlags::TRACE_SYSEMU);
    if needs_syscall_trace {
        frame.rax = SystemError::ENOSYS.to_posix_errno() as u64;
        ptrace_report_syscall_stop(&pcb, PTRACE_EVENTMSG_SYSCALL_ENTRY, pid);

        // 只要 syscall-entry stop 结束后已有 fatal signal pending，
        // 本次系统调用就必须取消；典型场景是 tracer 恢复时注入 SIGKILL。
        if Signal::fatal_signal_pending(&pcb) || is_sysemu {
            syscall_return!(frame.rax, frame, show);
        }
    }

    // 必须在 ptrace_stop 返回之后重新读取系统调用号和参数，因为 tracer 可能通过
    // PTRACE_SETREGS 修改 orig_rax (= errcode)。按 Linux x86 语义，orig_ax 只有低
    // 32 位有意义，并按 int 符号扩展；orig_ax == -1 表示跳过本次 syscall。
    let syscall_num = syscall_nr_from_orig_ax(frame.errcode);
    let args = [
        frame.rdi as usize,
        frame.rsi as usize,
        frame.rdx as usize,
        frame.r10 as usize,
        frame.r8 as usize,
        frame.r9 as usize,
    ];

    let mut syscall_handle = || -> u64 {
        let pcb = ProcessManager::current_pcb();
        let result = match syscall_num {
            -1 => SystemError::ENOSYS.to_posix_errno() as usize,
            n if n == SYS_RT_SIGRETURN as i32 => X86_64SignalArch::sys_rt_sigreturn(frame) as usize,
            n if n == SYS_ARCH_PRCTL as i32 => Syscall::arch_prctl(args[0], args[1])
                .unwrap_or_else(|e| e.to_posix_errno() as usize),
            n => Syscall::catch_handle(n as usize, &args, frame)
                .unwrap_or_else(|e| e.to_posix_errno() as usize),
        } as u64;

        // 先将结果写入 frame.rax，这样 tracer 可以通过 PTRACE_POKEUSER 修改返回值
        frame.rax = result;

        if pcb
            .flags()
            .contains(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL)
        {
            ptrace_report_syscall_stop(&pcb, PTRACE_EVENTMSG_SYSCALL_EXIT, pid);
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
