use crate::{
    arch::{
        ipc::signal::X86_64SignalArch,
        syscall::nr::{SysCall, SYS_ARCH_PRCTL, SYS_RT_SIGRETURN},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    ipc::signal_types::SignalArch,
    libs::align::SafeForZero,
    mm::VirtAddr,
    process::ProcessManager,
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
            let pid = ProcessManager::current_pcb().pid();
            debug!("[SYS] [Pid: {:?}] [Retn: {:?}]", pid, ret as i64);
        }

        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
        return;
    }};
}

macro_rules! normal_syscall_return {
    ($val:expr, $regs:expr, $show:expr) => {{
        let ret = $val;

        if $show {
            let pid = ProcessManager::current_pcb().pid();
            debug!("[SYS] [Pid: {:?}] [Retn: {:?}]", pid, ret);
        }

        $regs.rax = ret.unwrap_or_else(|e| e.to_posix_errno() as usize) as u64;

        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
        return;
    }};
}

#[no_mangle]
pub extern "sysv64" fn syscall_handler(frame: &mut TrapFrame) {
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
    let pid = ProcessManager::current_pcb().pid();
    let mut show = (syscall_num != SYS_SCHED) && (pid.data() >= 7);
    // let mut show = true;

    let to_print = SysCall::try_from(syscall_num);
    if let Ok(to_print) = to_print {
        use SysCall::*;
        match to_print {
            SYS_ACCEPT | SYS_ACCEPT4 | SYS_BIND | SYS_CONNECT | SYS_SHUTDOWN | SYS_LISTEN => {
                show &= true;
            }
            SYS_RECVFROM | SYS_SENDTO | SYS_SENDMSG | SYS_RECVMSG => {
                show &= true;
            }
            SYS_SOCKET | SYS_GETSOCKNAME | SYS_GETPEERNAME | SYS_SOCKETPAIR | SYS_SETSOCKOPT
            | SYS_GETSOCKOPT => {
                show &= true;
            }
            SYS_OPEN | SYS_OPENAT | SYS_CREAT | SYS_CLOSE => {
                show &= true;
            }
            SYS_READ | SYS_WRITE | SYS_READV | SYS_WRITEV | SYS_PREAD64 | SYS_PWRITE64
            | SYS_PREADV | SYS_PWRITEV | SYS_PREADV2 => {
                show &= true;
            }
            _ => {
                show &= false;
            }
        }

        if show {
            debug!("[SYS] [Pid: {:?}] [Call: {:?}]", pid, to_print);
        }
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
            normal_syscall_return!(Syscall::arch_prctl(args[0], args[1]), frame, show);
        }
        _ => {}
    }
    normal_syscall_return!(Syscall::handle(syscall_num, &args, frame), frame, show);
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
