pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSigStackFlags as SigStackFlags;
pub use crate::ipc::generic_signal::GenericSignal as Signal;
pub use crate::ipc::generic_signal::GENERIC_MAX_SIG_NUM as MAX_SIG_NUM;

use crate::{
    arch::{
        interrupt::TrapFrame, process::FpDExtState, syscall::nr::SYS_RESTART_SYSCALL,
        CurrentIrqArch, MMArch,
    },
    exception::InterruptArch,
    ipc::{
        signal::{
            force_kernel_default_signal_to_current, force_kernel_signal_to_current,
            restore_saved_sigmask, set_current_blocked,
        },
        signal_types::{
            PosixSigInfo, SaHandlerType, SigInfo, Sigaction, SigactionType, SignalArch, SignalFlags,
        },
    },
    mm::{MemoryManagementArch, VirtAddr},
    process::{ptrace, rseq::Rseq, ProcessFlags, ProcessManager},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use core::{
    ffi::c_void,
    mem::{offset_of, size_of},
};
use defer::defer;
use log::error;
use system_error::SystemError;

/// RV64 用户栈 16 字节对齐
pub const STACK_ALIGN: usize = 16;
pub const MINSIGSTKSZ: usize = 2048;

/// UserGpRegs 布局：第 0 项为 PC，之后依次为 x1 ... x31
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserGpRegs {
    pub regs: [usize; 32],
}

impl UserGpRegs {
    pub fn from_trapframe(frame: &TrapFrame) -> Self {
        Self {
            regs: [
                frame.epc, frame.ra, frame.sp, frame.gp, frame.tp, frame.t0, frame.t1, frame.t2,
                frame.s0, frame.s1, frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5,
                frame.a6, frame.a7, frame.s2, frame.s3, frame.s4, frame.s5, frame.s6, frame.s7,
                frame.s8, frame.s9, frame.s10, frame.s11, frame.t3, frame.t4, frame.t5, frame.t6,
            ],
        }
    }

    pub fn restore_to_trapframe(&self, frame: &mut TrapFrame) {
        [
            frame.epc, frame.ra, frame.sp, frame.gp, frame.tp, frame.t0, frame.t1, frame.t2,
            frame.s0, frame.s1, frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5,
            frame.a6, frame.a7, frame.s2, frame.s3, frame.s4, frame.s5, frame.s6, frame.s7,
            frame.s8, frame.s9, frame.s10, frame.s11, frame.t3, frame.t4, frame.t5, frame.t6,
        ] = self.regs;
    }
}

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
struct UserFpState {
    pub f: [u64; 32],
    pub fcsr: u32,
    pub padding: [u32; 64],
    pub reserved: u32,
    pub magic: u32,
    pub size: u32,
}

impl Default for UserFpState {
    fn default() -> Self {
        Self {
            f: [0; 32],
            fcsr: 0,
            padding: [0; 64],
            reserved: 0,
            magic: 0,
            size: 0,
        }
    }
}

impl UserFpState {
    fn from_kernel_fpstate(state: &FpDExtState) -> Self {
        Self {
            f: state.f,
            fcsr: state.fcsr,
            ..Self::default()
        }
    }

    fn to_kernel_fpstate(self) -> FpDExtState {
        FpDExtState {
            f: self.f,
            fcsr: self.fcsr & 0xff,
        }
    }

    pub fn has_supported_extensions(&self) -> bool {
        self.reserved == 0 && self.magic == 0 && self.size == 0
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserSigContext {
    pub gregs: UserGpRegs,
    pub fpregs: UserFpState,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct StackT {
    pub ss_sp: *mut c_void,
    pub ss_flags: i32,
    pub ss_size: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserSigSet {
    pub __val: [u64; 16],
}

impl UserSigSet {
    /// 从内核 SigSet (64-bit) 转换到用户态 sigset_t (1024-bit)
    pub fn from_kernel_sigset(kernel_sigset: &SigSet) -> Self {
        let mut val = [0; 16];
        val[0] = kernel_sigset.bits();
        Self { __val: val }
    }

    pub fn to_kernel_sigset(self) -> SigSet {
        // 内核当前仅支持 64 个信号
        SigSet::from_bits_truncate(self.__val[0])
    }
}

/// RV64 用户上下文。uc_sigmask 位于 uc_mcontext 之前，字段顺序不能照搬 x86。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserUContext {
    pub uc_flags: u64,
    pub uc_link: *mut UserUContext,
    pub uc_stack: StackT,
    pub uc_sigmask: UserSigSet,
    pub uc_mcontext: UserSigContext,
}

/// Linux 兼容信号栈帧
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SigFrame {
    pub siginfo: PosixSigInfo,
    pub ucontext: UserUContext,
}

// 编译期固定用户 ABI 布局，防止字段调整改变帧大小或偏移
const _: () = {
    assert!(core::mem::size_of::<UserGpRegs>() == 256);
    assert!(core::mem::size_of::<UserFpState>() == 528);
    assert!(core::mem::align_of::<UserFpState>() == 16);
    assert!(core::mem::offset_of!(UserFpState, reserved) == 516);
    assert!(core::mem::size_of::<StackT>() == 24);
    assert!(core::mem::offset_of!(StackT, ss_size) == 16);
    assert!(core::mem::offset_of!(UserUContext, uc_stack) == 16);
    assert!(core::mem::offset_of!(UserUContext, uc_sigmask) == 40);
    assert!(core::mem::offset_of!(UserUContext, uc_mcontext) == 176);
    assert!(core::mem::size_of::<UserUContext>() == 960);
    assert!(core::mem::offset_of!(SigFrame, ucontext) == 128);
    assert!(core::mem::size_of::<SigFrame>() == 1088);
};

impl SigFrame {
    /// 按字段组装字节缓冲区，再一次性写入用户栈
    /// 由于 StackT 和 UserUContext 有隐式字节填充，不能把整个结构直接当作已初始化字节复制
    fn copy_to_user(&self, writer: &mut UserBufferWriter<'_>) -> Result<(), SystemError> {
        let mut bytes = [0u8; size_of::<Self>()];
        // 仅复制没有隐式填充的字段
        macro_rules! copy_field {
            ($($field:ident).+) => {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        core::ptr::addr_of!(self.$($field).+).cast::<u8>(),
                        bytes.as_mut_ptr().add(offset_of!(Self, $($field).+)),
                        core::mem::size_of_val(&self.$($field).+),
                    );
                }
            };
        }
        copy_field!(siginfo);
        copy_field!(ucontext.uc_flags);
        copy_field!(ucontext.uc_link);
        copy_field!(ucontext.uc_stack.ss_sp);
        copy_field!(ucontext.uc_stack.ss_flags);
        copy_field!(ucontext.uc_stack.ss_size);
        copy_field!(ucontext.uc_sigmask);
        copy_field!(ucontext.uc_mcontext);
        writer.copy_one_to_user(&bytes, 0)
    }
}

impl UserUContext {
    /// 保存返回现场、原信号掩码和备用栈配置, 浮点状态由调用方补齐
    pub fn from_trapframe(frame: &TrapFrame, oldset: &SigSet, stack: &RiscV64SigStack) -> Self {
        Self {
            uc_flags: 0,
            uc_link: core::ptr::null_mut(),
            uc_stack: stack.to_user_stack(),
            uc_sigmask: UserSigSet::from_kernel_sigset(oldset),
            uc_mcontext: UserSigContext {
                gregs: UserGpRegs::from_trapframe(frame),
                fpregs: UserFpState::default(),
            },
        }
    }

    /// 修改进程状态前检查用户提供的恢复地址及扩展描述
    fn validate_for_sigreturn(&self) -> Result<(), SystemError> {
        if !user_pc(self.uc_mcontext.gregs.regs[0])
            || !VirtAddr::new(self.uc_mcontext.gregs.regs[2]).check_user()
            || !self.uc_mcontext.fpregs.has_supported_extensions()
        {
            return Err(SystemError::EFAULT);
        }
        Ok(())
    }

    pub fn restore_to_trapframe(&self, frame: &mut TrapFrame) {
        self.uc_mcontext.gregs.restore_to_trapframe(frame);
    }
}

bitflags! {
    #[repr(C,align(8))]
    #[derive(Default)]
    pub struct SigFlags:u32{
        const SA_NOCLDSTOP =  1;
        const SA_NOCLDWAIT = 2;
        const SA_SIGINFO   = 4;
        const SA_ONSTACK   = 0x08000000;
        const SA_RESTART   = 0x10000000;
        const SA_NODEFER  = 0x40000000;
        const SA_RESETHAND = 0x80000000;
        const SA_RESTORER   =0x04000000;
        const SA_ALL = Self::SA_NOCLDSTOP.bits()|Self::SA_NOCLDWAIT.bits()|Self::SA_NODEFER.bits()|Self::SA_ONSTACK.bits()|Self::SA_RESETHAND.bits()|Self::SA_RESTART.bits()|Self::SA_SIGINFO.bits()|Self::SA_RESTORER.bits();
    }

}

/// 信号处理备用栈的信息，用于 sigaltstack 和嵌套信号递送
#[derive(Debug, Clone, Copy)]
pub struct RiscV64SigStack {
    pub sp: usize,
    pub flags: SigStackFlags,
    pub size: usize,
}

impl RiscV64SigStack {
    pub fn new() -> Self {
        Self {
            sp: 0,
            flags: SigStackFlags::SS_DISABLE,
            size: 0,
        }
    }

    /// 按备用栈语义判断当前位置
    #[inline]
    pub fn on_sig_stack(&self, sp: usize) -> bool {
        !self.flags.contains(SigStackFlags::SS_AUTODISARM) && self.contains_sp(sp)
    }

    /// 检查地址范围，在分配信号帧时检测备用栈是否溢出
    #[inline]
    fn contains_sp(&self, sp: usize) -> bool {
        self.sp != 0 && self.size != 0 && sp > self.sp && sp.wrapping_sub(self.sp) <= self.size
    }

    #[inline]
    fn to_user_stack(self) -> StackT {
        StackT {
            ss_sp: self.sp as *mut c_void,
            ss_flags: self.flags.bits() as i32,
            ss_size: self.size,
        }
    }

    fn from_user_stack(user_stack: StackT, current_sp: usize) -> Result<Self, SystemError> {
        let flags =
            SigStackFlags::from_bits(user_stack.ss_flags as u32).ok_or(SystemError::EINVAL)?;
        let ss_mode = flags.difference(SigStackFlags::SS_AUTODISARM);
        if !(ss_mode.is_empty()
            || ss_mode == SigStackFlags::SS_DISABLE
            || ss_mode == SigStackFlags::SS_ONSTACK)
        {
            return Err(SystemError::EINVAL);
        }
        let pcb = ProcessManager::current_pcb();
        let current_stack = pcb.sig_altstack();
        if current_stack.on_sig_stack(current_sp) {
            return Err(SystemError::EPERM);
        }
        drop(current_stack);
        if flags.contains(SigStackFlags::SS_DISABLE) {
            Ok(Self {
                sp: 0,
                flags,
                size: 0,
            })
        } else {
            if user_stack.ss_size < MINSIGSTKSZ {
                return Err(SystemError::ENOMEM);
            }
            Ok(Self {
                sp: user_stack.ss_sp as usize,
                flags,
                size: user_stack.ss_size,
            })
        }
    }

    fn reset_for_autodisarm(&mut self) {
        self.sp = 0;
        self.flags = SigStackFlags::SS_DISABLE;
        self.size = 0;
    }
}

impl Default for RiscV64SigStack {
    fn default() -> Self {
        Self::new()
    }
}

unsafe fn do_signal(frame: &mut TrapFrame, got_signal: &mut bool) {
    let (sig_number, sigaction, info, sig_block, frame_oldset) = loop {
        let pcb = ProcessManager::current_pcb();
        let (mut sig_block, mut frame_oldset) = {
            let siginfo = pcb.sig_info_irqsave();
            let blocked = *siginfo.sig_blocked();
            let oldset = if pcb.flags().contains(ProcessFlags::RESTORE_SIG_MASK) {
                *siginfo.saved_sigmask()
            } else {
                blocked
            };
            (blocked, oldset)
        };
        let (mut sig_number, mut info) = pcb.dequeue_pending_signal(&sig_block);
        if sig_number == Signal::INVALID {
            return;
        }
        if pcb.is_traced() && sig_number != Signal::SIGKILL {
            match ptrace::ptrace_signal(&pcb, sig_number, &mut info) {
                Some(new_sig) => sig_number = new_sig,
                None => continue,
            }
        }
        if sig_number.kernel_only() {
            drop(pcb);
            sig_number.handle_default();
            continue;
        }
        let Some(sigaction) = pcb.sighand().handler(sig_number) else {
            continue;
        };
        match sigaction.action() {
            SigactionType::SaHandler(SaHandlerType::Ignore) => continue,
            SigactionType::SaHandler(SaHandlerType::Default) => {
                if pcb.process_signal().flags_contains(SignalFlags::UNKILLABLE) {
                    continue;
                }
                drop(pcb);
                sig_number.handle_default();
                continue;
            }
            SigactionType::SaHandler(SaHandlerType::Customized(_)) => {}
            _ => {
                error!(
                    "Unsupported signal action for signal: {}, pid={:?}",
                    sig_number as i32,
                    pcb.raw_pid()
                );
                drop(pcb);
                *got_signal = true;
                let _ = force_kernel_default_signal_to_current(Signal::SIGSEGV);
                return;
            }
        }
        if pcb.is_traced() {
            let siginfo = pcb.sig_info_irqsave();
            sig_block = *siginfo.sig_blocked();
            frame_oldset = if pcb.flags().contains(ProcessFlags::RESTORE_SIG_MASK) {
                *siginfo.saved_sigmask()
            } else {
                sig_block
            };
        }
        break (
            sig_number,
            sigaction,
            info.unwrap(),
            sig_block,
            frame_oldset,
        );
    };

    *got_signal = true;
    let mut blocked = sig_block | sigaction.mask();
    if !sigaction.flags().contains(SigFlags::SA_NODEFER) {
        blocked.insert(sig_number.into_sigset());
    }
    set_current_blocked(&mut blocked);
    ProcessManager::current_pcb()
        .flags()
        .remove(ProcessFlags::RESTORE_SIG_MASK);

    let res = handle_signal(sig_number, &sigaction, &info, &frame_oldset, frame);
    if let Err(e) = res {
        // 连 SIGSEGV 的用户帧也无法建立时，强制默认动作，避免反复递送失败。
        let _ = if sig_number == Signal::SIGSEGV {
            force_kernel_default_signal_to_current(Signal::SIGSEGV)
        } else {
            force_kernel_signal_to_current(Signal::SIGSEGV)
        };
        if e != SystemError::EFAULT {
            error!(
                "Error occurred when handling signal: {}, pid={:?}, errcode={:?}",
                sig_number as i32,
                ProcessManager::current_pcb().raw_pid(),
                e
            );
        }
    }
}

fn take_syscall_error(frame: &mut TrapFrame) -> Option<SystemError> {
    if frame.cause.bits() != 8 {
        return None;
    }
    // 系统调用出口和公共用户返回入口都可能调用此处，需要防止重复回退 pc
    frame.cause = unsafe { core::mem::zeroed() };
    let errno = i32::try_from(frame.a0 as isize).ok()?;
    SystemError::from_posix_errno(errno)
}

/// 没有进入用户 handler 时，处理内部重启错误码并恢复临时信号掩码
fn try_restart_syscall(frame: &mut TrapFrame) {
    defer!({
        restore_saved_sigmask();
    });
    match take_syscall_error(frame) {
        Some(
            SystemError::ERESTARTSYS | SystemError::ERESTARTNOHAND | SystemError::ERESTARTNOINTR,
        ) => {
            frame.a0 = frame.origin_a0;
            frame.epc -= 4;
        }
        Some(SystemError::ERESTART_RESTARTBLOCK) => {
            frame.a0 = frame.origin_a0;
            frame.a7 = SYS_RESTART_SYSCALL;
            frame.epc -= 4;
        }
        _ => {}
    }
}

pub struct RiscV64SignalArch;

impl SignalArch for RiscV64SignalArch {
    unsafe fn do_signal_or_restart(frame: &mut TrapFrame) {
        if !frame.is_from_user() {
            return;
        }
        CurrentIrqArch::interrupt_enable();
        crate::security::keys::apply_pending_session_keyring();
        while ProcessManager::current_pcb().ptrace_handle_pending_stop() {}

        let mut got_signal = false;
        do_signal(frame, &mut got_signal);
        if got_signal {
            return;
        }
        try_restart_syscall(frame);
    }

    fn sys_rt_sigreturn(trap_frame: &mut TrapFrame) -> u64 {
        let _ = ProcessManager::current_pcb().restart_block().take();
        trap_frame.cause = unsafe { core::mem::zeroed() };
        let ucontext = (|| {
            if trap_frame.sp & (STACK_ALIGN - 1) != 0 {
                return Err(SystemError::EFAULT);
            }
            let ucontext_ptr = trap_frame
                .sp
                .checked_add(offset_of!(SigFrame, ucontext))
                .ok_or(SystemError::EFAULT)? as *const UserUContext;
            let reader = UserBufferReader::new(ucontext_ptr, size_of::<UserUContext>(), true)?;
            let ucontext = reader.read_one_from_user::<UserUContext>(0)?;
            ucontext.validate_for_sigreturn()?;
            Ok(ucontext)
        })();
        let ucontext = match ucontext {
            Ok(ucontext) => ucontext,
            Err(err) => {
                error!("sys_rt_sigreturn: invalid signal frame: {:?}", err);
                let _ = force_kernel_default_signal_to_current(Signal::SIGSEGV);
                return 0;
            }
        };
        // 1. 恢复原信号掩码，set_current_blocked 会移除不可屏蔽的 SIGKILL/SIGSTOP
        let mut sigmask = ucontext.uc_sigmask.to_kernel_sigset();
        set_current_blocked(&mut sigmask);
        // 2. 用即将恢复的用户 sp 判断是否允许更新备用栈，保留嵌套信号的栈状态
        if let Ok(restored_stack) =
            RiscV64SigStack::from_user_stack(ucontext.uc_stack, ucontext.uc_mcontext.gregs.regs[2])
        {
            *ProcessManager::current_pcb().sig_altstack_mut() = restored_stack;
        }
        // 3. 恢复通用寄存器；用户帧不包含可覆盖特权状态的 CSR
        ucontext.restore_to_trapframe(trap_frame);
        // 4. 将用户浮点格式转回 PCB 保存区，再装入硬件寄存器
        let pcb = ProcessManager::current_pcb();
        let mut archinfo = pcb.arch_info_irqsave();
        *archinfo.fp_state_mut() = ucontext.uc_mcontext.fpregs.to_kernel_fpstate();
        archinfo.restore_fp_state(trap_frame);
        // 通用系统调用出口会写回返回值，必须返回刚恢复的 a0
        trap_frame.a0 as u64
    }
}

fn handle_signal(
    sig: Signal,
    sigaction: &Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    frame: &mut TrapFrame,
) -> Result<(), SystemError> {
    if let Some(syscall_err) = take_syscall_error(frame) {
        match syscall_err {
            SystemError::ERESTARTNOHAND | SystemError::ERESTART_RESTARTBLOCK => {
                frame.a0 = SystemError::EINTR.to_posix_errno() as usize;
            }
            SystemError::ERESTARTSYS => {
                // 只有支持重启且设置 SA_RESTART 的调用才回退到 ecall
                if !sigaction.flags().contains(SigFlags::SA_RESTART) {
                    frame.a0 = SystemError::EINTR.to_posix_errno() as usize;
                } else {
                    frame.a0 = frame.origin_a0;
                    frame.epc -= 4;
                }
            }
            SystemError::ERESTARTNOINTR => {
                frame.a0 = frame.origin_a0;
                frame.epc -= 4;
            }
            _ => {}
        }
    }
    setup_frame(sig, sigaction, info, oldset, frame)
}

/// 构造用户信号帧
fn setup_frame(
    sig: Signal,
    sigaction: &Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    trap_frame: &mut TrapFrame,
) -> Result<(), SystemError> {
    // 在保存 PC 前完成 rseq 修正，避免 sigreturn 恢复到已被打断的临界区
    Rseq::on_signal(trap_frame).map_err(|_| SystemError::EFAULT)?;
    let handler_addr = match sigaction.action() {
        SigactionType::SaHandler(SaHandlerType::Customized(addr)) => addr.data(),
        _ => return Err(SystemError::EINVAL),
    };
    if !sigaction.flags().contains(SigFlags::SA_RESTORER) {
        return Err(SystemError::EINVAL);
    }
    let ret_code_ptr = sigaction.restorer().ok_or(SystemError::EINVAL)?.data();
    if !user_pc(handler_addr) || !user_pc(ret_code_ptr) {
        return Err(SystemError::EFAULT);
    }
    let frame_ptr = get_stack(sigaction, trap_frame, size_of::<SigFrame>())?;
    let mut writer = UserBufferWriter::new(frame_ptr, size_of::<SigFrame>(), true)?;
    let pcb = ProcessManager::current_pcb();
    let sig_altstack = *pcb.sig_altstack();
    let mut user_ucontext = UserUContext::from_trapframe(trap_frame, oldset, &sig_altstack);
    {
        let mut archinfo = pcb.arch_info_irqsave();
        archinfo.save_fp_state(trap_frame);
        user_ucontext.uc_mcontext.fpregs = UserFpState::from_kernel_fpstate(archinfo.fp_state());
    }
    let frame = SigFrame {
        siginfo: info.convert_to_posix_siginfo(),
        ucontext: user_ucontext,
    };
    if sig_altstack.flags.contains(SigStackFlags::SS_AUTODISARM) {
        pcb.sig_altstack_mut().reset_for_autodisarm();
    }
    frame.copy_to_user(&mut writer)?;
    trap_frame.epc = handler_addr;
    trap_frame.ra = ret_code_ptr;
    trap_frame.sp = frame_ptr as usize;
    trap_frame.a0 = sig as usize;
    trap_frame.a1 = frame_ptr as usize + offset_of!(SigFrame, siginfo);
    trap_frame.a2 = frame_ptr as usize + offset_of!(SigFrame, ucontext);
    Ok(())
}

/// 为信号帧向低地址分配空间
fn get_stack(
    sigaction: &Sigaction,
    frame: &TrapFrame,
    size: usize,
) -> Result<*mut SigFrame, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let stack = pcb.sig_altstack();
    let nested_altstack = stack.on_sig_stack(frame.sp);
    let entering_altstack = sigaction.flags().contains(SigFlags::SA_ONSTACK)
        && !stack.flags.contains(SigStackFlags::SS_DISABLE)
        && !nested_altstack;
    let sp = if entering_altstack {
        stack
            .sp
            .checked_add(stack.size)
            .ok_or(SystemError::EFAULT)?
    } else {
        frame.sp
    };
    let sp = sp.checked_sub(size).ok_or(SystemError::EFAULT)? & !(STACK_ALIGN - 1);
    if (nested_altstack || entering_altstack) && !stack.contains_sp(sp) {
        return Err(SystemError::EFAULT);
    }
    Ok(sp as *mut SigFrame)
}

/// 校验代码地址的用户范围及半字对齐
fn user_pc(pc: usize) -> bool {
    pc != 0 && pc < MMArch::USER_END_VADDR.data() && pc & 1 == 0
}
