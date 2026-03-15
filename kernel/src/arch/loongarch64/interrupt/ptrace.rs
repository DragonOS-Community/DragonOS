/// LoongArch64 ptrace 支持
///
/// 参考 Linux 6.6.21: arch/loongarch/include/uapi/asm/ptrace.h
/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/uapi/asm/ptrace.h#30
use super::TrapFrame;

/// Linux 兼容的用户寄存器结构体 (LoongArch64)
///
/// 该结构体用于 ptrace 系统调用向用户空间暴露寄存器信息。
/// 字段顺序和类型与 Linux 6.6.21 的 user_pt_regs 完全一致。
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRegsStruct {
    /// 主处理器寄存器 (r0-r31)
    pub regs: [usize; 32],
    /// 原始系统调用参数 a0
    pub orig_a0: usize,
    /// CSR ERA (Exception Return Address)
    pub csr_era: usize,
    /// CSR BADV (Bad Virtual Address)
    pub csr_badv: usize,
    /// 保留字段
    pub reserved: [usize; 10],
}

impl UserRegsStruct {
    /// 从 TrapFrame 创建 UserRegsStruct
    ///
    /// 这对应 Linux 中从 pt_regs 构建 user_pt_regs 的过程。
    ///
    /// 参考 Linux 6.6.21: arch/loongarch/kernel/ptrace.c
    /// - 使用 `user_regset_get` 等函数获取寄存器
    /// - pt_regs 包含 32 个通用寄存器和一些 CSR 寄存器
    pub fn from_trap_frame(trap_frame: &TrapFrame) -> Self {
        let mut regs = [0usize; 32];
        // 按照 LoongArch 寄存器编号映射
        regs[0] = trap_frame.r0;
        regs[1] = trap_frame.ra;
        regs[2] = trap_frame.tp;
        regs[3] = trap_frame.usp;
        regs[4] = trap_frame.a0;
        regs[5] = trap_frame.a1;
        regs[6] = trap_frame.a2;
        regs[7] = trap_frame.a3;
        regs[8] = trap_frame.a4;
        regs[9] = trap_frame.a5;
        regs[10] = trap_frame.a6;
        regs[11] = trap_frame.a7;
        regs[12] = trap_frame.t0;
        regs[13] = trap_frame.t1;
        regs[14] = trap_frame.t2;
        regs[15] = trap_frame.t3;
        regs[16] = trap_frame.t4;
        regs[17] = trap_frame.t5;
        regs[18] = trap_frame.t6;
        regs[19] = trap_frame.t7;
        regs[20] = trap_frame.t8;
        regs[21] = trap_frame.r21;
        regs[22] = trap_frame.fp;
        regs[23] = trap_frame.s0;
        regs[24] = trap_frame.s1;
        regs[25] = trap_frame.s2;
        regs[26] = trap_frame.s3;
        regs[27] = trap_frame.s4;
        regs[28] = trap_frame.s5;
        regs[29] = trap_frame.s6;
        regs[30] = trap_frame.s7;
        regs[31] = trap_frame.s8;

        Self {
            regs,
            orig_a0: trap_frame.orig_a0,
            csr_era: trap_frame.csr_era,
            csr_badv: trap_frame.csr_badvaddr,
            reserved: [0; 10],
        }
    }

    /// 将 UserRegsStruct 的值写回 TrapFrame
    ///
    /// 用于 PTRACE_SETREGS 操作，允许调试器修改被跟踪进程的寄存器。
    ///
    /// 参考 Linux 6.6.21: arch/loongarch/kernel/ptrace.c 中的 `user_regset_set`
    pub fn write_to_trap_frame(&self, trap_frame: &mut TrapFrame) {
        trap_frame.r0 = self.regs[0];
        trap_frame.ra = self.regs[1];
        trap_frame.tp = self.regs[2];
        trap_frame.usp = self.regs[3];
        trap_frame.a0 = self.regs[4];
        trap_frame.a1 = self.regs[5];
        trap_frame.a2 = self.regs[6];
        trap_frame.a3 = self.regs[7];
        trap_frame.a4 = self.regs[8];
        trap_frame.a5 = self.regs[9];
        trap_frame.a6 = self.regs[10];
        trap_frame.a7 = self.regs[11];
        trap_frame.t0 = self.regs[12];
        trap_frame.t1 = self.regs[13];
        trap_frame.t2 = self.regs[14];
        trap_frame.t3 = self.regs[15];
        trap_frame.t4 = self.regs[16];
        trap_frame.t5 = self.regs[17];
        trap_frame.t6 = self.regs[18];
        trap_frame.t7 = self.regs[19];
        trap_frame.t8 = self.regs[20];
        trap_frame.r21 = self.regs[21];
        trap_frame.fp = self.regs[22];
        trap_frame.s0 = self.regs[23];
        trap_frame.s1 = self.regs[24];
        trap_frame.s2 = self.regs[25];
        trap_frame.s3 = self.regs[26];
        trap_frame.s4 = self.regs[27];
        trap_frame.s5 = self.regs[28];
        trap_frame.s6 = self.regs[29];
        trap_frame.s7 = self.regs[30];
        trap_frame.s8 = self.regs[31];
        trap_frame.orig_a0 = self.orig_a0;
        trap_frame.csr_era = self.csr_era;
        trap_frame.csr_badvaddr = self.csr_badv;
    }
}
