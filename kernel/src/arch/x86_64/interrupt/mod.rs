pub(super) mod entry;
mod handle;
pub mod ipi;
pub mod msi;
pub mod trap;

use core::any::Any;
use core::{
    arch::asm,
    sync::atomic::{compiler_fence, Ordering},
};
use kprobe::ProbeArgs;
use log::error;
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber},
};

use super::{
    asm::irqflags::{local_irq_restore, local_irq_save},
    driver::apic::{lapic_vector::arch_early_irq_init, CurrentApic, LocalAPIC},
};

/// @brief 关闭中断
#[inline]
pub fn cli() {
    unsafe {
        asm!("cli");
    }
}

/// @brief 开启中断
#[inline]
pub fn sti() {
    unsafe {
        asm!("sti");
    }
}

pub struct X86_64InterruptArch;

impl InterruptArch for X86_64InterruptArch {
    #[inline(never)]
    unsafe fn arch_irq_init() -> Result<(), SystemError> {
        CurrentIrqArch::interrupt_disable();

        return Ok(());
    }
    unsafe fn interrupt_enable() {
        sti();
    }

    unsafe fn interrupt_disable() {
        cli();
    }

    fn is_irq_enabled() -> bool {
        let rflags: u64;
        unsafe {
            asm!("pushfq; pop {}", out(reg) rflags, options(nomem, preserves_flags));
        }
        return (rflags & (1 << 9)) != 0;
    }

    unsafe fn save_and_disable_irq() -> IrqFlagsGuard {
        compiler_fence(Ordering::SeqCst);
        let rflags = local_irq_save();
        let flags = IrqFlags::new(rflags);
        let guard = IrqFlagsGuard::new(flags);
        compiler_fence(Ordering::SeqCst);
        return guard;
    }

    unsafe fn restore_irq(flags: IrqFlags) {
        compiler_fence(Ordering::SeqCst);
        local_irq_restore(flags.flags());
        compiler_fence(Ordering::SeqCst);
    }

    fn probe_total_irq_num() -> u32 {
        // todo: 从APIC获取
        // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/apic/vector.c?r=&mo=19514&fi=704#704
        256
    }

    fn ack_bad_irq(irq: IrqNumber) {
        error!("Unexpected IRQ trap at vector {}", irq.data());
        CurrentApic.send_eoi();
    }

    fn arch_early_irq_init() -> Result<(), SystemError> {
        arch_early_irq_init()
    }

    fn arch_ap_early_irq_init() -> Result<(), SystemError> {
        if !CurrentApic.init_current_cpu() {
            return Err(SystemError::ENODEV);
        }

        Ok(())
    }
}

/// 中断栈帧结构体
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TrapFrame {
    pub r15: ::core::ffi::c_ulong,
    pub r14: ::core::ffi::c_ulong,
    pub r13: ::core::ffi::c_ulong,
    pub r12: ::core::ffi::c_ulong,
    pub r11: ::core::ffi::c_ulong,
    pub r10: ::core::ffi::c_ulong,
    pub r9: ::core::ffi::c_ulong,
    pub r8: ::core::ffi::c_ulong,
    pub rbx: ::core::ffi::c_ulong,
    pub rcx: ::core::ffi::c_ulong,
    pub rdx: ::core::ffi::c_ulong,
    pub rsi: ::core::ffi::c_ulong,
    pub rdi: ::core::ffi::c_ulong,
    pub rbp: ::core::ffi::c_ulong,
    pub ds: ::core::ffi::c_ulong,
    pub es: ::core::ffi::c_ulong,
    pub rax: ::core::ffi::c_ulong,
    pub func: ::core::ffi::c_ulong,
    /// - 该字段在异常发生时，保存的是错误码
    /// - 在系统调用时，由系统调用入口函数将其设置为系统调用号
    pub errcode: ::core::ffi::c_ulong,
    pub rip: ::core::ffi::c_ulong,
    pub cs: ::core::ffi::c_ulong,
    pub rflags: ::core::ffi::c_ulong,
    pub rsp: ::core::ffi::c_ulong,
    pub ss: ::core::ffi::c_ulong,
}

impl Default for TrapFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl TrapFrame {
    pub fn new() -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            ds: 0,
            es: 0,
            rax: 0,
            func: 0,
            errcode: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            rsp: 0,
            ss: 0,
        }
    }

    /// 设置中断栈帧返回值
    pub fn set_return_value(&mut self, value: usize) {
        self.rax = value as u64;
    }

    /// 判断当前中断是否来自用户模式
    pub fn is_from_user(&self) -> bool {
        return (self.cs & 0x3) != 0;
    }
    /// 设置当前的程序计数器
    pub fn set_pc(&mut self, pc: usize) {
        self.rip = pc as u64;
    }

    /// 获取当前的程序计数器（指令指针）
    #[inline]
    pub fn rip(&self) -> usize {
        self.rip as usize
    }

    /// 设置指令指针
    #[inline]
    pub fn set_rip(&mut self, rip: usize) {
        self.rip = rip as u64;
    }

    /// 返回当前 TrapFrame 对应的用户态栈指针。
    #[inline(always)]
    pub fn stack_pointer(&self) -> usize {
        self.rsp as usize
    }

    /// 获取系统调用号
    ///
    /// # Safety
    /// 该函数只能在系统调用上下文中调用，
    /// 在其他上下文中，该函数返回值未定义
    pub unsafe fn syscall_nr(&self) -> Option<usize> {
        if self.errcode == u64::MAX {
            return None;
        }
        Some(self.errcode as usize)
    }

    /// 获取系统调用错误码
    ///
    /// # Safety
    /// 该函数只能在系统调用上下文中调用，
    /// 在其他上下文中，该函数返回值未定义
    ///
    /// # Returns
    /// 返回一个 `Option<SystemError>`，表示系统调用的错误码。
    pub unsafe fn syscall_error(&self) -> Option<SystemError> {
        let val = self.rax as i32;
        SystemError::from_posix_errno(val)
    }
}

impl ProbeArgs for TrapFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn break_address(&self) -> usize {
        (self.rip - 1) as usize
    }

    fn debug_address(&self) -> usize {
        self.rip as usize
    }
}

impl crate::process::rseq::RseqTrapFrame for TrapFrame {
    #[inline]
    fn rseq_ip(&self) -> usize {
        self.rip as usize
    }

    #[inline]
    fn set_rseq_ip(&mut self, ip: usize) {
        self.rip = ip as u64;
    }
}

/// Linux 兼容的用户寄存器结构体 (x86_64)
///
/// 该结构体用于 ptrace 系统调用向用户空间暴露寄存器信息。
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/include/asm/user_64.h#69
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRegsStruct {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub bp: u64,
    pub bx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub ax: u64,
    pub cx: u64,
    pub dx: u64,
    pub si: u64,
    pub di: u64,
    /// 在系统调用入口时保存原始的 rax（系统调用号）
    pub orig_ax: u64,
    pub ip: u64,
    pub cs: u64,
    pub flags: u64,
    pub sp: u64,
    pub ss: u64,
    /// FS 段基址，来自 task->thread.fsbase
    pub fs_base: u64,
    /// GS 段基址，来自 task->thread.gsbase
    pub gs_base: u64,
    /// DS 段选择器
    pub ds: u64,
    /// ES 段选择器
    pub es: u64,
    /// FS 段选择器
    pub fs: u64,
    /// GS 段选择器
    pub gs: u64,
}

impl UserRegsStruct {
    /// 从 TrapFrame 创建 UserRegsStruct
    ///
    /// 这对应 Linux 中从 pt_regs 构建 user_regs_struct 的过程。
    /// TrapFrame 包含了 pt_regs 的核心字段，额外的段寄存器信息
    /// 需要从进程的 arch_info 中获取。
    ///
    /// # 参数
    /// - `trap_frame`: 中断/异常时保存的寄存器状态
    /// - `fs_base`: FS 段基址（来自 task->thread.fsbase）
    /// - `gs_base`: GS 段基址（来自 task->thread.gsbase）
    /// - `fs`: FS 段选择器（来自 task->thread.fsindex）
    /// - `gs`: GS 段选择器（来自 task->thread.gsindex）
    pub fn from_trap_frame(
        trap_frame: &TrapFrame,
        fs_base: u64,
        gs_base: u64,
        fs: u64,
        gs: u64,
    ) -> Self {
        Self {
            r15: trap_frame.r15,
            r14: trap_frame.r14,
            r13: trap_frame.r13,
            r12: trap_frame.r12,
            bp: trap_frame.rbp,
            bx: trap_frame.rbx,
            r11: trap_frame.r11,
            r10: trap_frame.r10,
            r9: trap_frame.r9,
            r8: trap_frame.r8,
            ax: trap_frame.rax,
            cx: trap_frame.rcx,
            dx: trap_frame.rdx,
            si: trap_frame.rsi,
            di: trap_frame.rdi,
            // errcode 在系统调用上下文中存储系统调用号
            orig_ax: trap_frame.errcode,
            ip: trap_frame.rip,
            cs: trap_frame.cs,
            flags: trap_frame.rflags,
            sp: trap_frame.rsp,
            ss: trap_frame.ss,
            fs_base,
            gs_base,
            // TrapFrame 中的 ds/es 是完整的段选择器值
            ds: trap_frame.ds,
            es: trap_frame.es,
            fs,
            gs,
        }
    }

    /// 将 UserRegsStruct 的值写回 TrapFrame
    ///
    /// 用于 PTRACE_SETREGS 操作，允许调试器修改被跟踪进程的寄存器。
    ///
    /// # 注意
    /// - fs_base, gs_base, fs, gs 需要单独写回到进程的 arch_info
    /// - 某些字段（如 cs, ss）的修改可能受到安全限制
    #[allow(dead_code)]
    pub fn write_to_trap_frame(&self, trap_frame: &mut TrapFrame) {
        trap_frame.r15 = self.r15;
        trap_frame.r14 = self.r14;
        trap_frame.r13 = self.r13;
        trap_frame.r12 = self.r12;
        trap_frame.rbp = self.bp;
        trap_frame.rbx = self.bx;
        trap_frame.r11 = self.r11;
        trap_frame.r10 = self.r10;
        trap_frame.r9 = self.r9;
        trap_frame.r8 = self.r8;
        trap_frame.rax = self.ax;
        trap_frame.rcx = self.cx;
        trap_frame.rdx = self.dx;
        trap_frame.rsi = self.si;
        trap_frame.rdi = self.di;
        trap_frame.errcode = self.orig_ax;
        trap_frame.rip = self.ip;
        // cs 和 ss 的修改需要谨慎，这里暂时允许
        trap_frame.cs = self.cs;
        trap_frame.rflags = self.flags;
        trap_frame.rsp = self.sp;
        trap_frame.ss = self.ss;
        trap_frame.ds = self.ds;
        trap_frame.es = self.es;
    }
}
