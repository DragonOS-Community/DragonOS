use core::sync::atomic::{compiler_fence, Ordering};
use core::{ffi::c_void, intrinsics::unlikely, mem::size_of};

use defer::defer;
use log::error;
use system_error::SystemError;

pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSigStackFlags as SigStackFlags;
pub use crate::ipc::generic_signal::GenericSignal as Signal;

use crate::process::rseq::Rseq;
use crate::{
    arch::{
        fpu::{FpState, XFEATURE_AVX, XFEATURE_SSE, XFEATURE_X87},
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
        syscall::nr::SYS_RESTART_SYSCALL,
        CurrentIrqArch, MMArch,
    },
    exception::InterruptArch,
    ipc::{
        signal::{restore_saved_sigmask, set_current_blocked},
        signal_types::{
            PosixSigInfo, SaHandlerType, SigInfo, Sigaction, SigactionType, SignalArch, SignalFlags,
        },
    },
    mm::MemoryManagementArch,
    process::{ProcessFlags, ProcessManager},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

/// 信号处理的栈的栈指针的最小对齐数量
pub const STACK_ALIGN: u64 = 16;
/// 信号最大值
pub const MAX_SIG_NUM: usize = 64;

const UC_FP_XSTATE: u64 = 0x1;
const UC_SIGCONTEXT_SS: u64 = 0x2;
const UC_STRICT_RESTORE_SS: u64 = 0x4;

const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
const FP_XSTATE_MAGIC2_SIZE: usize = size_of::<u32>();
const FPSTATE_FRAME_SIZE: usize = size_of::<UserXState>() + FP_XSTATE_MAGIC2_SIZE;
pub const MINSIGSTKSZ: usize = 2048;

const X86_EFLAGS_CF: u64 = 1 << 0;
const X86_EFLAGS_PF: u64 = 1 << 2;
const X86_EFLAGS_AF: u64 = 1 << 4;
const X86_EFLAGS_ZF: u64 = 1 << 6;
const X86_EFLAGS_SF: u64 = 1 << 7;
const X86_EFLAGS_TF: u64 = 1 << 8;
const X86_EFLAGS_DF: u64 = 1 << 10;
const X86_EFLAGS_OF: u64 = 1 << 11;
const X86_EFLAGS_RF: u64 = 1 << 16;
const X86_EFLAGS_AC: u64 = 1 << 18;
const FIX_EFLAGS: u64 = X86_EFLAGS_AC
    | X86_EFLAGS_OF
    | X86_EFLAGS_DF
    | X86_EFLAGS_TF
    | X86_EFLAGS_SF
    | X86_EFLAGS_ZF
    | X86_EFLAGS_AF
    | X86_EFLAGS_PF
    | X86_EFLAGS_CF
    | X86_EFLAGS_RF;

// ===== Linux 兼容的信号栈帧结构 =====

/// XSAVE header 结构（64 字节，位于偏移 512）
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct XStateHeader {
    /// 表示哪些状态组件已被保存
    pub xfeatures: u64,
    /// 压缩格式标志
    pub xcomp_bv: u64,
    /// 保留字段
    pub reserved: [u64; 6],
}

/// AVX 扩展状态：YMM 寄存器的高 128 位（256 字节）
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AvxState {
    /// YMM0-YMM15 的高 128 位，每个 16 字节
    pub ymmh: [[u8; 16]; 16],
}

/// 与 Linux 兼容的 _fpstate_64 结构（FXSAVE 兼容部分，512 字节）
/// 参考: /usr/include/x86_64-linux-gnu/asm/sigcontext.h
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserFpState64 {
    pub cwd: u16,
    pub swd: u16,
    pub twd: u16,
    pub fop: u16,
    pub rip: u64,
    pub rdp: u64,
    pub mxcsr: u32,
    pub mxcsr_mask: u32,
    pub st_space: [u32; 32],  // 8个 FP 寄存器，每个16字节
    pub xmm_space: [u32; 64], // 16个 XMM 寄存器，每个16字节
    pub reserved2: [u32; 12],
    pub reserved3: [u32; 12],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct FpxSwBytes {
    magic1: u32,
    extended_size: u32,
    xfeatures: u64,
    xstate_size: u32,
    padding: [u32; 7],
}

/// 完整的 XSAVE 状态结构（包含 AVX 扩展）
/// 布局：
/// - 0-511: FXSAVE 兼容区域 (UserFpState64)
/// - 512-575: XSAVE header (XStateHeader)
/// - 576-831: AVX 状态 (AvxState)
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
struct UserXState {
    /// FXSAVE 兼容区域（前 512 字节）
    pub fpstate: UserFpState64,
    /// XSAVE header（64 字节）
    pub header: XStateHeader,
    /// AVX 扩展状态：YMM 高 128 位（256 字节）
    pub avx: AvxState,
}

/// 与 Linux 兼容的 sigcontext 结构 (x86_64)
/// 参考: /usr/include/x86_64-linux-gnu/asm/sigcontext.h
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserSigContext {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rsp: u64,
    pub rip: u64,
    pub eflags: u64,
    pub cs: u16,
    pub gs: u16,
    pub fs: u16,
    pub ss: u16,
    pub err: u64,
    pub trapno: u64,
    pub oldmask: u64,
    pub cr2: u64,
    pub fpstate: *mut UserFpState64, // 指向 fpstate 的指针
    pub reserved1: [u64; 8],
}

/// 与 Linux 兼容的 stack_t 结构
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct StackT {
    pub ss_sp: *mut c_void,
    pub ss_flags: i32,
    pub ss_size: usize,
}

/// 与 Linux 兼容的 sigset_t 结构
/// Linux 定义: unsigned long int __val[_SIGSET_NWORDS]
/// 其中 _SIGSET_NWORDS = 1024 / (8 * sizeof(unsigned long)) = 16 (on x86_64)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserSigSet {
    pub __val: [u64; 16], // 1024 bits total
}

impl UserSigSet {
    /// 从内核 SigSet (64-bit) 转换到用户态 sigset_t (1024-bit)
    pub fn from_kernel_sigset(kernel_sigset: &SigSet) -> Self {
        let mut val = [0u64; 16];
        val[0] = kernel_sigset.bits(); // 只使用第一个 u64
        Self { __val: val }
    }

    /// 从用户态 sigset_t 转换回内核 SigSet
    pub fn to_kernel_sigset(self) -> SigSet {
        // 只取第一个 u64，因为内核目前只支持 64 个信号
        SigSet::from_bits_truncate(self.__val[0])
    }
}

/// 与 Linux x86_64 内核 sigframe 兼容的 ucontext 结构
/// 参考: Linux arch/x86/include/uapi/asm/ucontext.h
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UserUContext {
    pub uc_flags: u64,
    pub uc_link: *mut UserUContext,
    pub uc_stack: StackT,
    pub uc_mcontext: UserSigContext,
    pub uc_sigmask: UserSigSet, // 使用 Linux 兼容的 1024-bit sigset
}

// 编译期校验关键字段偏移量与 Linux 的兼容性
const _: () = {
    assert!(core::mem::offset_of!(UserUContext, uc_stack) == 16);
    assert!(core::mem::offset_of!(UserUContext, uc_mcontext) == 40);
    assert!(core::mem::offset_of!(UserUContext, uc_sigmask) == 296);
    assert!(core::mem::size_of::<UserUContext>() == 424);
    // UserXState = 512 (FXSAVE) + 64 (header) + 256 (AVX) = 832 bytes
    assert!(core::mem::size_of::<UserXState>() == 832);
};

impl UserXState {
    fn user_size() -> usize {
        size_of::<UserXState>()
    }

    fn build_sw_bytes(&self) -> FpxSwBytes {
        FpxSwBytes {
            magic1: FP_XSTATE_MAGIC1,
            extended_size: (Self::user_size() + FP_XSTATE_MAGIC2_SIZE) as u32,
            xfeatures: self.header.xfeatures,
            xstate_size: Self::user_size() as u32,
            padding: [0; 7],
        }
    }

    fn install_sw_bytes(&mut self) {
        let sw_bytes = self.build_sw_bytes();
        self.fpstate.reserved3[0] = sw_bytes.magic1;
        self.fpstate.reserved3[1] = sw_bytes.extended_size;
        self.fpstate.reserved3[2] = sw_bytes.xfeatures as u32;
        self.fpstate.reserved3[3] = (sw_bytes.xfeatures >> 32) as u32;
        self.fpstate.reserved3[4] = sw_bytes.xstate_size;
        self.fpstate.reserved3[5..12].copy_from_slice(&sw_bytes.padding);
    }

    fn validate_for_sigreturn(&self) -> Result<(), SystemError> {
        let mxcsr_mask = FpState::mxcsr_feature_mask();
        if self.fpstate.mxcsr & !mxcsr_mask != 0 {
            error!(
                "invalid signal fpstate mxcsr: value={:#x}, supported_mask={:#x}",
                self.fpstate.mxcsr, mxcsr_mask
            );
            return Err(SystemError::EINVAL);
        }

        if !FpState::is_xsave_enabled() {
            return Ok(());
        }

        let supported_user_xfeatures =
            FpState::xsave_feature_mask() & (XFEATURE_X87 | XFEATURE_SSE | XFEATURE_AVX);
        if self.header.xfeatures & !supported_user_xfeatures != 0 {
            error!(
                "invalid signal xstate features: value={:#x}, supported_mask={:#x}",
                self.header.xfeatures, supported_user_xfeatures
            );
            return Err(SystemError::EINVAL);
        }

        if self.header.xcomp_bv != 0 {
            error!(
                "invalid signal xstate compacted format: xcomp_bv={:#x}",
                self.header.xcomp_bv
            );
            return Err(SystemError::EINVAL);
        }

        if self.header.reserved.iter().any(|value| *value != 0) {
            error!("invalid signal xstate header: reserved bits are non-zero");
            return Err(SystemError::EINVAL);
        }

        Ok(())
    }

    /// 从内核 FpState 转换到用户态完整的 XSAVE 状态
    ///
    /// 包含：
    /// - FXSAVE 兼容区域 (512 字节)
    /// - XSAVE header (64 字节)
    /// - AVX 状态 (256 字节)
    pub fn from_kernel_fpstate(kernel_fp: &FpState) -> Self {
        let bytes = kernel_fp.as_bytes();
        let legacy = kernel_fp.legacy_region();

        // 构建 FXSAVE 兼容部分
        let mut fpstate = UserFpState64 {
            cwd: u16::from_le_bytes([legacy[0], legacy[1]]),
            swd: u16::from_le_bytes([legacy[2], legacy[3]]),
            twd: u16::from_le_bytes([legacy[4], legacy[5]]),
            fop: u16::from_le_bytes([legacy[6], legacy[7]]),
            rip: u64::from_le_bytes(legacy[8..16].try_into().unwrap()),
            rdp: u64::from_le_bytes(legacy[16..24].try_into().unwrap()),
            mxcsr: u32::from_le_bytes(legacy[24..28].try_into().unwrap()),
            mxcsr_mask: u32::from_le_bytes(legacy[28..32].try_into().unwrap()),
            st_space: [0; 32],
            xmm_space: [0; 64],
            reserved2: [0; 12],
            reserved3: [0; 12],
        };

        // 复制 ST 空间 (32-159: 128字节 = 32个u32)
        for i in 0..32 {
            let offset = 32 + i * 4;
            fpstate.st_space[i] =
                u32::from_le_bytes(legacy[offset..offset + 4].try_into().unwrap());
        }

        // 复制 XMM 空间 (160-415: 256字节 = 64个u32)
        for i in 0..64 {
            let offset = 160 + i * 4;
            fpstate.xmm_space[i] =
                u32::from_le_bytes(legacy[offset..offset + 4].try_into().unwrap());
        }

        // 构建 XSAVE header（偏移 512-575）
        let mut header = XStateHeader::default();
        if let Some(hdr) = bytes.get(512..528) {
            header.xfeatures = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
            header.xcomp_bv = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        }

        // 构建 AVX 状态（偏移 576-831）
        let mut avx = AvxState::default();
        if bytes.len() >= 832 {
            for i in 0..16 {
                let offset = 576 + i * 16;
                avx.ymmh[i].copy_from_slice(&bytes[offset..offset + 16]);
            }
        }

        let mut user_xstate = Self {
            fpstate,
            header,
            avx,
        };
        user_xstate.install_sw_bytes();
        user_xstate
    }

    /// 从用户态 XSAVE 状态转换回内核 FpState
    ///
    /// 恢复完整的 XSAVE 状态，包括 AVX
    pub fn to_kernel_fpstate(self) -> Result<FpState, SystemError> {
        self.validate_for_sigreturn()?;

        let mut result = FpState::new();

        // 写入 FXSAVE 兼容区域（前 512 字节）
        let legacy = result.legacy_region_mut();
        legacy[0..2].copy_from_slice(&self.fpstate.cwd.to_le_bytes());
        legacy[2..4].copy_from_slice(&self.fpstate.swd.to_le_bytes());
        legacy[4..6].copy_from_slice(&self.fpstate.twd.to_le_bytes());
        legacy[6..8].copy_from_slice(&self.fpstate.fop.to_le_bytes());
        legacy[8..16].copy_from_slice(&self.fpstate.rip.to_le_bytes());
        legacy[16..24].copy_from_slice(&self.fpstate.rdp.to_le_bytes());
        legacy[24..28].copy_from_slice(&self.fpstate.mxcsr.to_le_bytes());
        legacy[28..32].copy_from_slice(&self.fpstate.mxcsr_mask.to_le_bytes());

        for i in 0..32 {
            let offset = 32 + i * 4;
            legacy[offset..offset + 4].copy_from_slice(&self.fpstate.st_space[i].to_le_bytes());
        }

        for i in 0..64 {
            let offset = 160 + i * 4;
            legacy[offset..offset + 4].copy_from_slice(&self.fpstate.xmm_space[i].to_le_bytes());
        }

        // 写入 XSAVE header（偏移 512-575）
        let result_bytes = result.as_bytes_mut();
        if let Some(hdr) = result_bytes.get_mut(512..528) {
            hdr[0..8].copy_from_slice(&self.header.xfeatures.to_le_bytes());
            hdr[8..16].copy_from_slice(&self.header.xcomp_bv.to_le_bytes());
        }

        // 写入 AVX 状态（偏移 576-831）
        if result_bytes.len() >= 832 {
            for i in 0..16 {
                let offset = 576 + i * 16;
                result_bytes[offset..offset + 16].copy_from_slice(&self.avx.ymmh[i]);
            }
        }

        Ok(result)
    }
}

impl Default for UserXState {
    fn default() -> Self {
        Self {
            fpstate: UserFpState64 {
                cwd: 0x037F, // 默认 FPU 控制字
                swd: 0,
                twd: 0,
                fop: 0,
                rip: 0,
                rdp: 0,
                mxcsr: 0x1F80, // 默认 MXCSR
                mxcsr_mask: 0,
                st_space: [0; 32],
                xmm_space: [0; 64],
                reserved2: [0; 12],
                reserved3: [0; 12],
            },
            header: XStateHeader::default(),
            avx: AvxState::default(),
        }
    }
}

impl UserFpState64 {
    /// 从内核 FpState 转换到用户态可见的 FpState64
    ///
    /// FXSAVE 格式布局:
    /// - 0-1: FCW
    /// - 2-3: FSW
    /// - 4-5: FTW (abridged)
    /// - 6-7: FOP
    /// - 8-15: FIP (rip)
    /// - 16-23: FDP (rdp)
    /// - 24-27: MXCSR
    /// - 28-31: MXCSR_MASK
    /// - 32-159: ST0-ST7 (128 bytes, 8*16)
    /// - 160-415: XMM0-XMM15 (256 bytes, 16*16)
    ///
    /// 注意：此方法仅用于兼容旧代码，新代码应使用 UserXState::from_kernel_fpstate
    #[allow(dead_code)]
    pub fn from_kernel_fpstate(kernel_fp: &FpState) -> Self {
        // 使用 legacy_region() 获取 FXSAVE 兼容的前 512 字节
        let bytes = kernel_fp.legacy_region();

        let mut result = Self {
            cwd: 0,
            swd: 0,
            twd: 0,
            fop: 0,
            rip: 0,
            rdp: 0,
            mxcsr: 0,
            mxcsr_mask: 0,
            st_space: [0; 32],
            xmm_space: [0; 64],
            reserved2: [0; 12],
            reserved3: [0; 12],
        };

        // 读取控制字段
        result.cwd = u16::from_le_bytes([bytes[0], bytes[1]]);
        result.swd = u16::from_le_bytes([bytes[2], bytes[3]]);
        result.twd = u16::from_le_bytes([bytes[4], bytes[5]]);
        result.fop = u16::from_le_bytes([bytes[6], bytes[7]]);
        result.rip = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        result.rdp = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        result.mxcsr = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        result.mxcsr_mask = u32::from_le_bytes(bytes[28..32].try_into().unwrap());

        // 复制 ST 空间 (32-159: 128字节 = 32个u32)
        for i in 0..32 {
            let offset = 32 + i * 4;
            result.st_space[i] = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        }

        // 复制 XMM 空间 (160-415: 256字节 = 64个u32)
        for i in 0..64 {
            let offset = 160 + i * 4;
            result.xmm_space[i] = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        }

        result
    }

    /// 从用户态 FpState64 转换回内核 FpState
    ///
    /// 注意：此方法仅用于兼容旧代码，新代码应使用 UserXState::to_kernel_fpstate
    #[allow(dead_code)]
    pub fn to_kernel_fpstate(self) -> FpState {
        let mut result = FpState::new();
        // 使用 legacy_region_mut() 只修改前 512 字节
        let result_bytes = result.legacy_region_mut();

        // 写入控制字段
        result_bytes[0..2].copy_from_slice(&self.cwd.to_le_bytes());
        result_bytes[2..4].copy_from_slice(&self.swd.to_le_bytes());
        result_bytes[4..6].copy_from_slice(&self.twd.to_le_bytes());
        result_bytes[6..8].copy_from_slice(&self.fop.to_le_bytes());
        result_bytes[8..16].copy_from_slice(&self.rip.to_le_bytes());
        result_bytes[16..24].copy_from_slice(&self.rdp.to_le_bytes());
        result_bytes[24..28].copy_from_slice(&self.mxcsr.to_le_bytes());
        result_bytes[28..32].copy_from_slice(&self.mxcsr_mask.to_le_bytes());

        // 复制 ST 空间
        for i in 0..32 {
            let offset = 32 + i * 4;
            result_bytes[offset..offset + 4].copy_from_slice(&self.st_space[i].to_le_bytes());
        }

        // 复制 XMM 空间
        for i in 0..64 {
            let offset = 160 + i * 4;
            result_bytes[offset..offset + 4].copy_from_slice(&self.xmm_space[i].to_le_bytes());
        }

        result
    }
}

impl UserUContext {
    /// 从 TrapFrame 创建 UserUContext
    #[inline(never)]
    pub fn from_trapframe(
        frame: &TrapFrame,
        oldset: &SigSet,
        cr2: u64,
        sig_altstack: &X86SigStack,
    ) -> Self {
        let mut uc_flags = UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS;
        if FpState::is_xsave_enabled() {
            uc_flags |= UC_FP_XSTATE;
        }

        Self {
            uc_flags,
            uc_link: core::ptr::null_mut(),
            uc_stack: sig_altstack.to_user_stack(),
            uc_mcontext: UserSigContext {
                r8: frame.r8,
                r9: frame.r9,
                r10: frame.r10,
                r11: frame.r11,
                r12: frame.r12,
                r13: frame.r13,
                r14: frame.r14,
                r15: frame.r15,
                rdi: frame.rdi,
                rsi: frame.rsi,
                rbp: frame.rbp,
                rbx: frame.rbx,
                rdx: frame.rdx,
                rax: frame.rax,
                rcx: frame.rcx,
                rsp: frame.rsp,
                rip: frame.rip,
                eflags: frame.rflags,
                cs: frame.cs as u16,
                gs: 0, // Linux 不保存 gs/fs 寄存器值
                fs: 0,
                ss: frame.ss as u16,
                err: frame.errcode,
                trapno: 0,
                oldmask: oldset.bits(),
                cr2,
                fpstate: core::ptr::null_mut(), // 稍后设置
                reserved1: [0; 8],
            },
            uc_sigmask: UserSigSet::from_kernel_sigset(oldset),
        }
    }

    /// 将 UserUContext 恢复到 TrapFrame（完全安全的操作）
    pub fn restore_to_trapframe(&self, frame: &mut TrapFrame) {
        frame.r8 = self.uc_mcontext.r8;
        frame.r9 = self.uc_mcontext.r9;
        frame.r10 = self.uc_mcontext.r10;
        frame.r11 = self.uc_mcontext.r11;
        frame.r12 = self.uc_mcontext.r12;
        frame.r13 = self.uc_mcontext.r13;
        frame.r14 = self.uc_mcontext.r14;
        frame.r15 = self.uc_mcontext.r15;
        frame.rdi = self.uc_mcontext.rdi;
        frame.rsi = self.uc_mcontext.rsi;
        frame.rbp = self.uc_mcontext.rbp;
        frame.rbx = self.uc_mcontext.rbx;
        frame.rdx = self.uc_mcontext.rdx;
        frame.rax = self.uc_mcontext.rax;
        frame.rcx = self.uc_mcontext.rcx;
        frame.rsp = self.uc_mcontext.rsp;
        frame.rip = self.uc_mcontext.rip;
        frame.rflags = (frame.rflags & !FIX_EFLAGS) | (self.uc_mcontext.eflags & FIX_EFLAGS);
        // 注意: cs, ss 等段寄存器不恢复，由内核管理
    }

    /// Restore the full XSAVE state from the user fpstate pointed to by sigcontext.
    fn restore_fpstate(&self) -> Result<Option<FpState>, SystemError> {
        if self.uc_mcontext.fpstate.is_null() {
            return Ok(None);
        }

        let fpstate_ptr = self.uc_mcontext.fpstate as *const UserXState;
        let reader = UserBufferReader::new(fpstate_ptr, size_of::<UserXState>(), true)?;
        let mut user_xstate = UserXState::default();
        reader.copy_one_from_user(&mut user_xstate, 0)?;
        user_xstate.to_kernel_fpstate().map(Some)
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

/// 信号处理备用栈的信息（用于 sigaltstack）
#[derive(Debug, Clone, Copy)]
pub struct X86SigStack {
    pub sp: usize,
    pub flags: SigStackFlags,
    pub size: usize,
}

impl X86SigStack {
    pub fn new() -> Self {
        Self {
            sp: 0,
            flags: SigStackFlags::SS_DISABLE,
            size: 0,
        }
    }

    /// 检查给定的栈指针 `sp` 是否在当前备用信号栈的范围内。
    #[inline]
    pub fn on_sig_stack(&self, sp: usize) -> bool {
        !self.flags.contains(SigStackFlags::SS_AUTODISARM) && self.contains_sp(sp)
    }

    /// Pure range check, without considering `SS_AUTODISARM`.
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

impl Default for X86SigStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Linux 兼容的信号栈帧结构
/// x86_64 Linux 布局为 pretcode, ucontext, siginfo，FP/XSAVE 状态跟随在结构之后。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SigFrame {
    /// 指向restorer的地址的指针
    pub ret_code_ptr: *mut c_void,
    /// ucontext_t 结构
    pub ucontext: UserUContext,
    /// siginfo_t 结构
    pub siginfo: PosixSigInfo,
}

impl SigFrame {
    /// 设置 fpstate 指针，指向 sigframe 后方独立的 64 字节对齐 XSAVE 区域。
    pub fn setup_fpstate_pointer(&mut self, fpstate_ptr: *mut UserXState) {
        self.ucontext.uc_mcontext.fpstate =
            unsafe { &mut (*fpstate_ptr).fpstate as *mut UserFpState64 };
    }
}

const _: () = {
    assert!(core::mem::offset_of!(SigFrame, ucontext) == 8);
    assert!(core::mem::offset_of!(SigFrame, siginfo) == 432);
    assert!(core::mem::size_of::<SigFrame>() == 560);
};

struct SignalFrameLocation {
    frame: *mut SigFrame,
    fpstate: *mut UserXState,
}

unsafe fn do_signal(frame: &mut TrapFrame, got_signal: &mut bool) {
    let pcb = ProcessManager::current_pcb();

    let siginfo = pcb.try_siginfo_irqsave(5);

    if unlikely(siginfo.is_none()) {
        pcb.recalc_sigpending();
        return;
    }

    let siginfo_read_guard = siginfo.unwrap();

    // 检查 sigpending 是否为 0（需要同时检查线程级 pending 和进程级 shared_pending）
    let thread_pending = siginfo_read_guard.sig_pending().signal().bits();
    let shared_pending = pcb.sighand().shared_pending_signal().bits();
    if (thread_pending == 0 && shared_pending == 0) || !frame.is_from_user() {
        // 若没有正在等待处理的信号，或者将要返回到的是内核态，则返回
        drop(siginfo_read_guard);
        pcb.recalc_sigpending();
        return;
    }

    let mut sig_number: Signal;
    let mut info: Option<SigInfo>;
    let mut sigaction: Option<Sigaction>;
    let sig_block: SigSet = *siginfo_read_guard.sig_blocked();
    let frame_oldset = if pcb.flags().contains(ProcessFlags::RESTORE_SIG_MASK) {
        *siginfo_read_guard.saved_sigmask()
    } else {
        sig_block
    };
    drop(siginfo_read_guard);

    loop {
        (sig_number, info) = pcb.dequeue_pending_signal(&sig_block);

        // 如果信号非法，则直接返回
        if sig_number == Signal::INVALID {
            return;
        }

        // 对 kernel-only 信号（如 SIGKILL/SIGSTOP）直接使用默认处理，避免任何用户帧构造
        if sig_number.kernel_only() {
            // log::error!(
            //     "do_signal: kernel-only sig={} for pid={:?} -> default handler (no user frame)",
            //     sig_number as i32,
            //     pcb.raw_pid()
            // );
            // 释放锁，按常规路径在本线程上下文执行默认处理
            let _oldset = sig_block;
            drop(pcb);
            CurrentIrqArch::interrupt_enable();
            sig_number.handle_default();
            return;
        }
        let sa = pcb.sighand().handler(sig_number).unwrap();

        match sa.action() {
            SigactionType::SaHandler(action_type) => match action_type {
                SaHandlerType::Error => {
                    error!("Trying to handle a Sigerror on Process:{:?}", pcb.raw_pid());
                    return;
                }
                SaHandlerType::Default => {
                    sigaction = Some(sa);
                }
                SaHandlerType::Ignore => continue,
                SaHandlerType::Customized(_) => {
                    sigaction = Some(sa);
                }
            },
            SigactionType::SaSigaction(_) => todo!(),
        }

        /*
         * Global init gets no signals it doesn't want.
         * Container-init gets no signals it doesn't want from same
         * container.
         *
         * Note that if global/container-init sees a sig_kernel_only()
         * signal here, the signal must have been generated internally
         * or must have come from an ancestor namespace. In either
         * case, the signal cannot be dropped.
         */
        // todo: https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/signal.h?fi=sig_kernel_only#444
        let is_customized_action = sigaction
            .as_ref()
            .is_some_and(|action| action.action().is_customized());
        let drop_for_unkillable = pcb.sighand().flags_contains(SignalFlags::UNKILLABLE)
            && !sig_number.kernel_only()
            && !is_customized_action;
        if drop_for_unkillable {
            continue;
        }

        if sigaction.is_some() {
            break;
        }
    }

    // no sig_struct guard to drop
    pcb.flags().remove(ProcessFlags::RESTORE_SIG_MASK);
    drop(pcb);
    // 做完上面的检查后，开中断
    CurrentIrqArch::interrupt_enable();

    if sigaction.is_none() {
        return;
    }

    let mut sigaction = sigaction.unwrap();
    match sigaction.action() {
        SigactionType::SaHandler(SaHandlerType::Default) => {
            *got_signal = true;
            sig_number.handle_default();
            return;
        }
        SigactionType::SaHandler(SaHandlerType::Customized(_)) => {}
        SigactionType::SaHandler(SaHandlerType::Ignore) => return,
        _ => {
            error!(
                "Unsupported signal action for signal: {}, pid={:?}",
                sig_number as i32,
                ProcessManager::current_pcb().raw_pid()
            );
            return;
        }
    }
    *got_signal = true;

    let oldset = frame_oldset;
    let mut blocked = sig_block | sigaction.mask();
    if !sigaction.flags().contains(SigFlags::SA_NODEFER) {
        blocked.insert(sig_number.into());
    }
    set_current_blocked(&mut blocked);

    // 注意！由于handle_signal里面可能会退出进程，
    // 因此这里需要检查清楚：上面所有的锁、arc指针都被释放了。否则会产生资源泄露的问题！
    let res: Result<i32, SystemError> =
        handle_signal(sig_number, &mut sigaction, &info.unwrap(), &oldset, frame);
    compiler_fence(Ordering::SeqCst);
    if let Err(e) = res {
        let _ = if sig_number == Signal::SIGSEGV {
            crate::ipc::signal::force_kernel_default_signal_to_current(Signal::SIGSEGV)
        } else {
            crate::ipc::signal::force_kernel_signal_to_current(Signal::SIGSEGV)
        };
        if e != SystemError::EFAULT {
            error!(
                "Error occurred when handling signal: {}, pid={:?}, errcode={:?}",
                sig_number as i32,
                ProcessManager::current_pcb().raw_pid(),
                &e
            );
        }
    }
}

fn try_restart_syscall(frame: &mut TrapFrame) {
    defer!({
        // 如果没有信号需要传递，我们只需恢复保存的信号掩码
        restore_saved_sigmask();
    });

    if unsafe { frame.syscall_nr() }.is_none() {
        return;
    }

    let syscall_err = unsafe { frame.syscall_error() };
    if syscall_err.is_none() {
        return;
    }
    let syscall_err = syscall_err.unwrap();

    let mut restart = false;
    match syscall_err {
        SystemError::ERESTARTSYS | SystemError::ERESTARTNOHAND | SystemError::ERESTARTNOINTR => {
            frame.rax = frame.errcode;
            frame.rip -= 2;
            restart = true;
        }
        SystemError::ERESTART_RESTARTBLOCK => {
            frame.rax = SYS_RESTART_SYSCALL as u64;
            frame.rip -= 2;
            restart = true;
        }
        _ => {}
    }
    log::debug!("try restart syscall: {:?}", restart);
}

pub struct X86_64SignalArch;

impl SignalArch for X86_64SignalArch {
    /// 处理信号，并尝试重启系统调用
    ///
    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/signal.c#865
    unsafe fn do_signal_or_restart(frame: &mut TrapFrame) {
        let mut got_signal = false;
        do_signal(frame, &mut got_signal);

        if got_signal {
            return;
        }
        try_restart_syscall(frame);
    }

    fn sys_rt_sigreturn(trap_frame: &mut TrapFrame) -> u64 {
        let frame_ptr = (trap_frame.rsp as usize - size_of::<u64>()) as *mut SigFrame;
        let ucontext_ptr =
            (frame_ptr as usize + core::mem::offset_of!(SigFrame, ucontext)) as *const UserUContext;

        let ucontext = match UserBufferReader::new(ucontext_ptr, size_of::<UserUContext>(), true)
            .and_then(|reader| reader.read_one_from_user::<UserUContext>(0))
        {
            Ok(ucontext) => ucontext,
            Err(err) => {
                error!("sys_rt_sigreturn: failed to read ucontext: {:?}", err);
                let _ = crate::ipc::signal::force_kernel_default_signal_to_current(Signal::SIGSEGV);
                return 0;
            }
        };

        // Align with Linux restore_sigcontext(): after a signal handler returns, any pending
        // restart block should degrade to no-restart state. Subsequent manual calls to
        // restart_syscall should only get EINTR and must not continue consuming the old restart context.
        let _ = ProcessManager::current_pcb().restart_block().take();

        // 1. 恢复信号掩码（从 1024-bit 用户态格式转换到 64-bit 内核格式）
        let mut sigmask = ucontext.uc_sigmask.to_kernel_sigset();
        set_current_blocked(&mut sigmask);

        if let Ok(restored_stack) =
            X86SigStack::from_user_stack(ucontext.uc_stack, trap_frame.rsp as usize)
        {
            let pcb = ProcessManager::current_pcb();
            *pcb.sig_altstack_mut() = restored_stack;
        }

        // 2. 先校验并构造 FP 状态。失败时按 Linux badframe 语义强制 SIGSEGV，
        // 避免把用户篡改的状态交给 FXRSTOR/XRSTOR 触发内核 #GP。
        let kernel_fp = match ucontext.restore_fpstate() {
            Ok(kernel_fp) => kernel_fp,
            Err(err) => {
                error!("sys_rt_sigreturn: failed to restore fpstate: {:?}", err);
                let _ = crate::ipc::signal::force_kernel_default_signal_to_current(Signal::SIGSEGV);
                return 0;
            }
        };

        // 3. 恢复通用寄存器
        ucontext.restore_to_trapframe(trap_frame);

        // 4. 恢复 FP 状态
        if let Some(kernel_fp) = kernel_fp {
            let pcb = ProcessManager::current_pcb();
            let mut archinfo_guard = pcb.arch_info_irqsave();
            *archinfo_guard.fp_state_mut() = Some(kernel_fp);
            archinfo_guard.restore_fp_state();

            // 恢复 cr2
            *archinfo_guard.cr2_mut() = ucontext.uc_mcontext.cr2 as usize;
        }

        // 返回恢复后的 rax 值
        trap_frame.rax
    }
}

/// @brief 真正发送signal，执行自定义的处理函数
///
/// @param sig 信号number
/// @param sigaction 信号响应动作
/// @param info 信号信息
/// @param oldset
/// @param regs 之前的系统调用将要返回的时候，要弹出的栈帧的拷贝
///
/// @return Result<0,SystemError> 若Error, 则返回错误码,否则返回Ok(0)
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/signal.c#787
#[inline(never)]
fn handle_signal(
    sig: Signal,
    sigaction: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    if unsafe { frame.syscall_nr() }.is_some() {
        if let Some(syscall_err) = unsafe { frame.syscall_error() } {
            match syscall_err {
                SystemError::ERESTARTNOHAND | SystemError::ERESTART_RESTARTBLOCK => {
                    frame.rax = SystemError::EINTR.to_posix_errno() as i64 as u64;
                }
                SystemError::ERESTARTSYS => {
                    if !sigaction.flags().contains(SigFlags::SA_RESTART) {
                        frame.rax = SystemError::EINTR.to_posix_errno() as i64 as u64;
                    } else {
                        frame.rax = frame.errcode;
                        frame.rip -= 2;
                    }
                }
                SystemError::ERESTARTNOINTR => {
                    frame.rax = frame.errcode;
                    frame.rip -= 2;
                }
                _ => {}
            }
        }
    }
    // 设置栈帧
    return setup_frame(sig, sigaction, info, oldset, frame);
}

/// 在用户栈上设置信号栈帧（Linux 兼容）
fn setup_frame(
    sig: Signal,
    sigaction: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    trap_frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    // 在设置信号栈帧之前，先处理 rseq
    // 参考 Linux: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kernel/signal.c#211
    if Rseq::on_signal(trap_frame).is_err() {
        return Err(SystemError::EFAULT);
    }

    let ret_code_ptr: *mut c_void;
    let handler_addr: usize;

    match sigaction.action() {
        SigactionType::SaHandler(handler_type) => match handler_type {
            SaHandlerType::Customized(handler) => {
                // 如果handler地址大于等于用户空间末尾，说明它在内核空间，这是非法的。
                if handler >= MMArch::USER_END_VADDR {
                    error!("attempting to execute a signal handler from kernel");
                    let _ = crate::ipc::kill::send_signal_to_pid(
                        ProcessManager::current_pcb().raw_pid(),
                        Signal::SIGSEGV,
                    );
                    return Err(SystemError::EFAULT);
                } else {
                    // 64位程序必须由用户自行指定restorer
                    if sigaction.flags().contains(SigFlags::SA_RESTORER) {
                        ret_code_ptr = sigaction.restorer().unwrap().data() as *mut c_void;
                    } else {
                        error!(
                            "pid-{:?} forgot to set SA_FLAG_RESTORER for signal {:?}",
                            ProcessManager::current_pcb().raw_pid(),
                            sig as i32
                        );
                        let _ = crate::ipc::kill::send_signal_to_pid(
                            ProcessManager::current_pcb().raw_pid(),
                            Signal::SIGSEGV,
                        );
                        return Err(SystemError::EINVAL);
                    }
                    handler_addr = handler.data();
                }
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        SigactionType::SaSigaction(_) => {
            error!("trying to recover from sigaction type instead of handler");
            return Err(SystemError::EINVAL);
        }
    }

    // 分配新的信号栈帧
    let frame_location = get_stack(sigaction, trap_frame, size_of::<SigFrame>());
    let frame_ptr = frame_location.frame;

    // 验证地址位于用户空间
    let mut frame_writer =
        UserBufferWriter::new(frame_ptr, size_of::<SigFrame>(), true).map_err(|_| {
            error!("In setup_frame: access check failed");
            let _ = crate::ipc::kill::send_signal_to_pid(
                ProcessManager::current_pcb().raw_pid(),
                Signal::SIGSEGV,
            );
            SystemError::EFAULT
        })?;
    UserBufferWriter::new(frame_location.fpstate, FPSTATE_FRAME_SIZE, true).map_err(|_| {
        error!("In setup_frame: fpstate access check failed");
        let _ = crate::ipc::kill::send_signal_to_pid(
            ProcessManager::current_pcb().raw_pid(),
            Signal::SIGSEGV,
        );
        SystemError::EFAULT
    })?;

    // 1. 读取 arch 信息并生成用户态数据（避免持锁访问用户内存）
    let pcb = ProcessManager::current_pcb();
    let sig_altstack = *pcb.sig_altstack();
    let mut archinfo_guard = pcb.arch_info_irqsave();
    let cr2 = *archinfo_guard.cr2_mut() as u64;
    let user_ucontext = UserUContext::from_trapframe(trap_frame, oldset, cr2, &sig_altstack);

    // 2. 保存 FP 状态到 PCB，并准备用户态 XSAVE 数据
    archinfo_guard.save_fp_state();
    let user_fpstate =
        (*archinfo_guard.fp_state()).map(|kernel_fp| UserXState::from_kernel_fpstate(&kernel_fp));

    // 根据 Linux 语义，加载干净的 FP 状态到硬件
    // 这样信号处理函数在标准的 FP 环境中执行
    archinfo_guard.clear_fp_state();
    drop(archinfo_guard);

    // 3. 写入用户栈（可能触发缺页，必须在释放锁后进行）
    if let Some(fpstate) = user_fpstate {
        let mut fp_writer =
            UserBufferWriter::new(frame_location.fpstate, size_of::<UserXState>(), true)?;
        fp_writer.copy_one_to_user(&fpstate, 0)?;
        let mut magic_writer = UserBufferWriter::new(
            unsafe { (frame_location.fpstate as *mut u8).add(size_of::<UserXState>()) as *mut u32 },
            FP_XSTATE_MAGIC2_SIZE,
            true,
        )?;
        magic_writer.copy_one_to_user(&FP_XSTATE_MAGIC2, 0)?;
    }

    let mut user_frame = SigFrame {
        ret_code_ptr,
        ucontext: user_ucontext,
        siginfo: info.convert_to_posix_siginfo(),
    };
    // 设置 fpstate 指针指向栈帧内的 fpstate
    user_frame.setup_fpstate_pointer(frame_location.fpstate);

    // 4. Copy sigframe
    frame_writer
        .copy_one_to_user(&user_frame, 0)
        .inspect_err(|_| {
            error!("In setup_frame: failed to copy sigframe");
            let _ = crate::ipc::kill::send_signal_to_pid(
                ProcessManager::current_pcb().raw_pid(),
                Signal::SIGSEGV,
            );
        })?;

    if sig_altstack.flags.contains(SigStackFlags::SS_AUTODISARM) {
        let pcb = ProcessManager::current_pcb();
        pcb.sig_altstack_mut().reset_for_autodisarm();
    }

    // 6. 设置 trap_frame，准备进入信号处理函数
    trap_frame.rdi = sig as u64; // 参数1: 信号编号
    trap_frame.rax = 0; // Linux x86_64: support handlers declared without prototypes
    trap_frame.rsi = (frame_ptr as usize + core::mem::offset_of!(SigFrame, siginfo)) as u64; // arg2: siginfo_t*
    trap_frame.rdx = (frame_ptr as usize + core::mem::offset_of!(SigFrame, ucontext)) as u64; // arg3: ucontext_t*
    trap_frame.rsp = frame_ptr as u64;
    trap_frame.rip = handler_addr as u64;
    trap_frame.cs = (USER_CS.bits() | 0x3) as u64;
    trap_frame.ds = (USER_DS.bits() | 0x3) as u64;
    trap_frame.rflags &= !(X86_EFLAGS_DF | X86_EFLAGS_RF | X86_EFLAGS_TF);

    Ok(0)
}

#[inline(always)]
fn get_stack(sigaction: &mut Sigaction, frame: &TrapFrame, size: usize) -> SignalFrameLocation {
    let pcb = ProcessManager::current_pcb();
    let stack = pcb.sig_altstack();

    let mut rsp: usize;
    let nested_altstack = stack.on_sig_stack(frame.rsp as usize);
    let mut entering_altstack = false;

    // 检查是否使用备用栈
    if sigaction.flags().contains(SigFlags::SA_ONSTACK)
        && !stack.flags.contains(SigStackFlags::SS_DISABLE)
        && !nested_altstack
    {
        rsp = stack.sp + stack.size;
        entering_altstack = true;
    } else {
        // 默认使用用户栈：先跳过 x86_64 ABI red zone。
        rsp = (frame.rsp as usize) - 128;
    }

    let fpstate = (rsp - FPSTATE_FRAME_SIZE) & !(64 - 1);
    rsp = fpstate - size;

    // 16字节对齐，减8是为了保持 x86_64 ABI 的栈对齐约定。
    rsp = (rsp & !(STACK_ALIGN - 1) as usize) - 8;

    if (nested_altstack || entering_altstack) && !stack.contains_sp(rsp) {
        return SignalFrameLocation {
            frame: usize::MAX as *mut SigFrame,
            fpstate: usize::MAX as *mut UserXState,
        };
    }

    SignalFrameLocation {
        frame: rsp as *mut SigFrame,
        fpstate: fpstate as *mut UserXState,
    }
}
