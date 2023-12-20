use core::ffi::c_void;

use crate::{
    arch::{
        ipc::signal::X86_64SignalArch,
        syscall::nr::{SYS_ARCH_PRCTL, SYS_RT_SIGRETURN},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    include::bindings::bindings::set_system_trap_gate,
    ipc::signal_types::SignalArch,
    libs::align::SafeForZero,
    mm::VirtAddr,
    process::ProcessManager,
    syscall::{Syscall, SystemError, SYS_SCHED},
};
use alloc::string::String;

use super::{interrupt::TrapFrame, mm::barrier::mfence};

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
            crate::kdebug!("syscall return:pid={:?},ret= {:?}\n", pid, ret as isize);
        }

        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
        return;
    }};
}

#[no_mangle]
pub extern "sysv64" fn syscall_handler(frame: &mut TrapFrame) -> () {
    let syscall_num = frame.rax as usize;
    // 防止sys_sched由于超时无法退出导致的死锁
    if syscall_num != SYS_SCHED {
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
    let show = false;
    // let show = if syscall_num != SYS_SCHED && pid.data() > 3 {
    //     true
    // } else {
    //     false
    // };

    if show {
        crate::kdebug!("syscall: pid: {:?}, num={:?}\n", pid, syscall_num);
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
    syscall_return!(
        Syscall::handle(syscall_num, &args, frame).unwrap_or_else(|e| e.to_posix_errno() as usize)
            as u64,
        frame,
        show
    );
}

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    // kinfo!("arch_syscall_init\n");
    unsafe { set_system_trap_gate(0x80, 0, syscall_int as *mut c_void) }; // 系统调用门
    unsafe { init_syscall_64() };
    return Ok(());
}

/// 执行第一个用户进程的函数（只应该被调用一次）
///
/// 当进程管理重构完成后，这个函数应该被删除。调整为别的函数。
#[no_mangle]
pub extern "C" fn rs_exec_init_process(frame: &mut TrapFrame) -> usize {
    let path = String::from("/bin/shell.elf");
    let argv = vec![String::from("/bin/shell.elf")];
    let envp = vec![String::from("PATH=/bin")];
    let r = Syscall::do_execve(path, argv, envp, frame);
    // kdebug!("rs_exec_init_process: r: {:?}\n", r);
    return r.map(|_| 0).unwrap_or_else(|e| e.to_posix_errno() as usize);
}

/// syscall指令初始化函数
pub(super) unsafe fn init_syscall_64() {
    let mut efer = x86::msr::rdmsr(x86::msr::IA32_EFER);
    efer |= 0x1;
    x86::msr::wrmsr(x86::msr::IA32_EFER, efer);

    let syscall_base = (1 as u16) << 3;
    let sysret_base = ((4 as u16) << 3) | 3;
    let high = (u32::from(sysret_base) << 16) | u32::from(syscall_base);
    // 初始化STAR寄存器
    x86::msr::wrmsr(x86::msr::IA32_STAR, u64::from(high) << 32);

    // 初始化LSTAR,该寄存器存储syscall指令入口
    x86::msr::wrmsr(x86::msr::IA32_LSTAR, syscall_64 as u64);
    x86::msr::wrmsr(x86::msr::IA32_FMASK, 0xfffffffe);
}
