/// RISC-V 64 ptrace 支持
///
/// 参考 Linux 6.6.21: arch/riscv/include/uapi/asm/ptrace.h
/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/riscv/include/uapi/asm/ptrace.h#24

use super::TrapFrame;

/// Linux 兼容的用户寄存器结构体 (RISC-V 64)
///
/// 该结构体用于 ptrace 系统调用向用户空间暴露寄存器信息。
/// 字段顺序和类型与 Linux 6.6.21 的 user_regs_struct 完全一致。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRegsStruct {
    pub pc: usize,
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
}

impl UserRegsStruct {
    /// 从 TrapFrame 创建 UserRegsStruct
    ///
    /// 这对应 Linux 中从 pt_regs 构建 user_regs_struct 的过程。
    /// RISC-V 的 user_regs_struct 是 pt_regs 的前缀。
    ///
    /// 参考 Linux 6.6.21: arch/riscv/kernel/ptrace.c
    /// - 使用 `user_regset_get` 等函数获取寄存器
    /// - pt_regs 和 user_regs_struct 字段顺序一致
    pub fn from_trap_frame(trap_frame: &TrapFrame) -> Self {
        Self {
            pc: trap_frame.epc,
            ra: trap_frame.ra,
            sp: trap_frame.sp,
            gp: trap_frame.gp,
            tp: trap_frame.tp,
            t0: trap_frame.t0,
            t1: trap_frame.t1,
            t2: trap_frame.t2,
            s0: trap_frame.s0,
            s1: trap_frame.s1,
            a0: trap_frame.a0,
            a1: trap_frame.a1,
            a2: trap_frame.a2,
            a3: trap_frame.a3,
            a4: trap_frame.a4,
            a5: trap_frame.a5,
            a6: trap_frame.a6,
            a7: trap_frame.a7,
            s2: trap_frame.s2,
            s3: trap_frame.s3,
            s4: trap_frame.s4,
            s5: trap_frame.s5,
            s6: trap_frame.s6,
            s7: trap_frame.s7,
            s8: trap_frame.s8,
            s9: trap_frame.s9,
            s10: trap_frame.s10,
            s11: trap_frame.s11,
            t3: trap_frame.t3,
            t4: trap_frame.t4,
            t5: trap_frame.t5,
            t6: trap_frame.t6,
        }
    }

    /// 将 UserRegsStruct 的值写回 TrapFrame
    ///
    /// 用于 PTRACE_SETREGS 操作，允许调试器修改被跟踪进程的寄存器。
    ///
    /// 参考 Linux 6.6.21: arch/riscv/kernel/ptrace.c 中的 `user_regset_set`
    pub fn write_to_trap_frame(&self, trap_frame: &mut TrapFrame) {
        trap_frame.epc = self.pc;
        trap_frame.ra = self.ra;
        trap_frame.sp = self.sp;
        trap_frame.gp = self.gp;
        trap_frame.tp = self.tp;
        trap_frame.t0 = self.t0;
        trap_frame.t1 = self.t1;
        trap_frame.t2 = self.t2;
        trap_frame.s0 = self.s0;
        trap_frame.s1 = self.s1;
        trap_frame.a0 = self.a0;
        trap_frame.a1 = self.a1;
        trap_frame.a2 = self.a2;
        trap_frame.a3 = self.a3;
        trap_frame.a4 = self.a4;
        trap_frame.a5 = self.a5;
        trap_frame.a6 = self.a6;
        trap_frame.a7 = self.a7;
        trap_frame.s2 = self.s2;
        trap_frame.s3 = self.s3;
        trap_frame.s4 = self.s4;
        trap_frame.s5 = self.s5;
        trap_frame.s6 = self.s6;
        trap_frame.s7 = self.s7;
        trap_frame.s8 = self.s8;
        trap_frame.s9 = self.s9;
        trap_frame.s10 = self.s10;
        trap_frame.s11 = self.s11;
        trap_frame.t3 = self.t3;
        trap_frame.t4 = self.t4;
        trap_frame.t5 = self.t5;
        trap_frame.t6 = self.t6;
    }
}
