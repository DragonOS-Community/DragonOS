/// x86_64 ptrace 支持
///
/// 参考 Linux 6.6.21: arch/x86/include/asm/user_64.h
/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/include/asm/user_64.h#69
use super::TrapFrame;

/// Linux 兼容的用户寄存器结构体 (x86_64)
///
/// 该结构体用于 ptrace 系统调用向用户空间暴露寄存器信息。
/// 字段顺序和类型与 Linux 6.6.21 的 user_regs_struct 完全一致。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRegsStruct {
    pub r15: ::core::ffi::c_ulong,
    pub r14: ::core::ffi::c_ulong,
    pub r13: ::core::ffi::c_ulong,
    pub r12: ::core::ffi::c_ulong,
    pub bp: ::core::ffi::c_ulong,
    pub bx: ::core::ffi::c_ulong,
    pub r11: ::core::ffi::c_ulong,
    pub r10: ::core::ffi::c_ulong,
    pub r9: ::core::ffi::c_ulong,
    pub r8: ::core::ffi::c_ulong,
    pub ax: ::core::ffi::c_ulong,
    pub cx: ::core::ffi::c_ulong,
    pub dx: ::core::ffi::c_ulong,
    pub si: ::core::ffi::c_ulong,
    pub di: ::core::ffi::c_ulong,
    /// 在系统调用入口时保存原始的 rax（系统调用号）
    pub orig_ax: ::core::ffi::c_ulong,
    pub ip: ::core::ffi::c_ulong,
    pub cs: ::core::ffi::c_ulong,
    pub flags: ::core::ffi::c_ulong,
    pub sp: ::core::ffi::c_ulong,
    pub ss: ::core::ffi::c_ulong,
    /// FS 段基址，来自 task->thread.fsbase
    pub fs_base: ::core::ffi::c_ulong,
    /// GS 段基址，来自 task->thread.gsbase
    pub gs_base: ::core::ffi::c_ulong,
    /// DS 段选择器
    pub ds: ::core::ffi::c_ulong,
    /// ES 段选择器
    pub es: ::core::ffi::c_ulong,
    /// FS 段选择器
    pub fs: ::core::ffi::c_ulong,
    /// GS 段选择器
    pub gs: ::core::ffi::c_ulong,
}

impl UserRegsStruct {
    /// 从 TrapFrame 创建 UserRegsStruct
    ///
    /// 这对应 Linux 中从 pt_regs 构建 user_regs_struct 的过程。
    /// TrapFrame 包含了 pt_regs 的核心字段，额外的段寄存器信息
    /// 需要从进程的 arch_info 中获取。
    ///
    /// 参考 Linux 6.6.21: arch/x86/kernel/ptrace.c
    /// - 使用 `user_regset_get` 等函数获取寄存器
    /// - 段基址来自 task->thread.fsbase 和 task->thread.gsbase
    /// - 段选择器来自 pt_regs 或 task->thread
    ///
    /// # 参数
    /// - `trap_frame`: 中断/异常时保存的寄存器状态
    /// - `fs_base`: FS 段基址（来自 task->thread.fsbase）
    /// - `gs_base`: GS 段基址（来自 task->thread.gsbase）
    /// - `fs`: FS 段选择器（来自 task->thread.fsindex）
    /// - `gs`: GS 段选择器（来自 task->thread.gsindex）
    pub fn from_trap_frame(
        trap_frame: &TrapFrame,
        fs_base: ::core::ffi::c_ulong,
        gs_base: ::core::ffi::c_ulong,
        fs: ::core::ffi::c_ulong,
        gs: ::core::ffi::c_ulong,
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
