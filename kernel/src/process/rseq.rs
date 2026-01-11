//! Restartable Sequences (rseq) 机制实现
//!
//! rseq 是一套用户态"可重启临界区"协议，允许用户态代码在进入一个短临界区时
//! 使用 per-CPU 数据结构进行高效操作。内核保证在返回用户态前对临界区进行修正。
//!
//! # 设计原则
//!
//! - **高内聚**: 所有 rseq 相关逻辑都封装在此模块中
//! - **低耦合**: 通过 trait 和清晰的接口与其他模块交互
//! - **类型安全**: 使用 newtype 模式封装用户空间地址和偏移量
//! - **错误处理**: 使用 Result 类型和专门的错误枚举
//!
//! 参考: Linux 6.6.21 kernel/rseq.c

use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    arch::{cpu::current_cpu_id, ipc::signal::Signal, MMArch},
    ipc::kill::send_signal_to_pcb,
    mm::{MemoryManagementArch, VirtAddr},
    process::{ProcessControlBlock, ProcessFlags, ProcessManager},
    syscall::user_access::{copy_from_user_protected, copy_to_user_protected},
};

// ============================================================================
// 常量定义
// ============================================================================

/// The original rseq structure size (including padding) is 32 bytes.
pub const ORIG_RSEQ_SIZE: u32 = 32;

/// rseq 结构的对齐要求
pub const RSEQ_ALIGN: u32 = 32;

/// CPU ID 未初始化状态
pub const RSEQ_CPU_ID_UNINITIALIZED: i32 = -1;

/// CPU ID 注册失败状态
#[allow(dead_code)]
pub const RSEQ_CPU_ID_REGISTRATION_FAILED: i32 = -2;

// ============================================================================
// 标志位定义
// ============================================================================

bitflags! {
    /// sys_rseq flags 参数
    pub struct RseqFlags: i32 {
        /// 反注册 rseq
        const UNREGISTER = 1 << 0;
    }

    /// rseq_cs 的 flags 字段（已废弃，但需要检测）
    pub struct RseqCsFlags: u32 {
        /// 抢占时不重启
        const NO_RESTART_ON_PREEMPT = 1 << 0;
        /// 信号递送时不重启
        const NO_RESTART_ON_SIGNAL = 1 << 1;
        /// 迁移时不重启
        const NO_RESTART_ON_MIGRATE = 1 << 2;
    }

    /// rseq 事件掩码（用于记录需要重启的事件）
    pub struct RseqEventMask: u32 {
        /// 抢占事件
        const PREEMPT = 1 << 0;
        /// 信号事件
        const SIGNAL = 1 << 1;
        /// 迁移事件
        const MIGRATE = 1 << 2;
    }
}

// ============================================================================
// 用户态 ABI 结构定义（与 Linux uapi 兼容）
// ============================================================================

/// 用户态 struct rseq 结构体的字段偏移量
mod rseq_offsets {
    pub const CPU_ID_START: usize = 0;
    pub const CPU_ID: usize = 4;
    pub const RSEQ_CS: usize = 8;
    pub const FLAGS: usize = 16;
    pub const NODE_ID: usize = 20;
    pub const MM_CID: usize = 24;
}

/// 用户态 struct rseq_cs（临界区描述符）
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy, Default)]
pub struct RseqCs {
    /// 版本号，必须为 0
    pub version: u32,
    /// 标志位
    pub flags: u32,
    /// 临界区起始 IP
    pub start_ip: u64,
    /// 从 start_ip 开始的偏移量，表示临界区结束位置
    pub post_commit_offset: u64,
    /// 中止时跳转的地址
    pub abort_ip: u64,
}

impl RseqCs {
    /// 检查 IP 是否在此临界区内
    #[inline]
    pub fn contains_ip(&self, ip: u64) -> bool {
        // ip >= start_ip && ip < start_ip + post_commit_offset
        // 等价于：ip - start_ip < post_commit_offset（使用 wrapping_sub 避免溢出）
        ip.wrapping_sub(self.start_ip) < self.post_commit_offset
    }

    /// 验证临界区描述符的合法性
    pub fn validate(&self, user_end: usize) -> Result<(), RseqError> {
        // 版本必须为 0
        if self.version > 0 {
            return Err(RseqError::InvalidVersion);
        }

        // 地址必须在用户空间
        if self.start_ip as usize >= user_end
            || self.start_ip.saturating_add(self.post_commit_offset) as usize >= user_end
            || self.abort_ip as usize >= user_end
        {
            return Err(RseqError::InvalidAddress);
        }

        // 检查溢出
        if self.start_ip.checked_add(self.post_commit_offset).is_none() {
            return Err(RseqError::Overflow);
        }

        // 确保 abort_ip 不在临界区内
        let abort_offset = self.abort_ip.wrapping_sub(self.start_ip);
        if abort_offset < self.post_commit_offset {
            return Err(RseqError::AbortIpInCriticalSection);
        }

        Ok(())
    }
}

// ============================================================================
// rseq 错误类型
// ============================================================================

/// rseq 操作可能产生的错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RseqError {
    /// 无效的版本号
    InvalidVersion,
    /// 无效的地址
    InvalidAddress,
    /// 地址计算溢出
    Overflow,
    /// abort_ip 在临界区内
    AbortIpInCriticalSection,
    /// 签名不匹配
    SignatureMismatch,
    /// 无效的标志位
    InvalidFlags,
    /// 用户内存访问失败
    UserAccessFault,
    /// 已经注册
    AlreadyRegistered,
    /// 未注册
    NotRegistered,
    /// 参数不匹配
    ParameterMismatch,
}

impl From<RseqError> for SystemError {
    fn from(err: RseqError) -> Self {
        match err {
            RseqError::InvalidVersion
            | RseqError::InvalidAddress
            | RseqError::Overflow
            | RseqError::AbortIpInCriticalSection
            | RseqError::InvalidFlags
            | RseqError::ParameterMismatch => SystemError::EINVAL,
            RseqError::SignatureMismatch => SystemError::EPERM,
            RseqError::UserAccessFault => SystemError::EFAULT,
            RseqError::AlreadyRegistered => SystemError::EBUSY,
            RseqError::NotRegistered => SystemError::EINVAL,
        }
    }
}

// ============================================================================
// 用户空间内存访问辅助
// ============================================================================

/// 用户空间 rseq 结构的访问器
///
/// 封装对用户态 rseq 结构的读写操作，确保类型安全和错误处理
struct UserRseqAccess {
    base: VirtAddr,
}

impl UserRseqAccess {
    fn new(base: VirtAddr) -> Self {
        Self { base }
    }

    /// 读取 u32 值
    unsafe fn read_u32(&self, offset: usize) -> Result<u32, RseqError> {
        let mut bytes = [0u8; 4];
        copy_from_user_protected(&mut bytes, self.base + offset)
            .map_err(|_| RseqError::UserAccessFault)?;
        Ok(u32::from_ne_bytes(bytes))
    }

    /// 读取 u64 值
    unsafe fn read_u64(&self, offset: usize) -> Result<u64, RseqError> {
        let mut bytes = [0u8; 8];
        copy_from_user_protected(&mut bytes, self.base + offset)
            .map_err(|_| RseqError::UserAccessFault)?;
        Ok(u64::from_ne_bytes(bytes))
    }

    /// 写入 u32 值
    unsafe fn write_u32(&self, offset: usize, value: u32) -> Result<(), RseqError> {
        copy_to_user_protected(self.base + offset, &value.to_ne_bytes())
            .map_err(|_| RseqError::UserAccessFault)?;
        Ok(())
    }

    /// 写入 u64 值
    unsafe fn write_u64(&self, offset: usize, value: u64) -> Result<(), RseqError> {
        copy_to_user_protected(self.base + offset, &value.to_ne_bytes())
            .map_err(|_| RseqError::UserAccessFault)?;
        Ok(())
    }

    /// 读取 rseq_cs 指针并获取描述符
    unsafe fn read_rseq_cs(&self, sig: u32, user_end: usize) -> Result<Option<RseqCs>, RseqError> {
        let rseq_cs_ptr = self.read_u64(rseq_offsets::RSEQ_CS)?;

        // 如果为 0，表示不在临界区
        if rseq_cs_ptr == 0 {
            return Ok(None);
        }

        // 验证指针在用户空间
        if rseq_cs_ptr as usize >= user_end {
            return Err(RseqError::InvalidAddress);
        }

        // 读取 rseq_cs 结构
        let cs_access = UserRseqAccess::new(VirtAddr::new(rseq_cs_ptr as usize));
        let rseq_cs = RseqCs {
            version: cs_access.read_u32(0)?,
            flags: cs_access.read_u32(4)?,
            start_ip: cs_access.read_u64(8)?,
            post_commit_offset: cs_access.read_u64(16)?,
            abort_ip: cs_access.read_u64(24)?,
        };

        // 验证描述符
        rseq_cs.validate(user_end)?;

        // 验证签名
        if rseq_cs.abort_ip < 4 {
            return Err(RseqError::InvalidAddress);
        }
        let sig_addr = VirtAddr::new((rseq_cs.abort_ip - 4) as usize);
        let mut sig_bytes = [0u8; 4];
        copy_from_user_protected(&mut sig_bytes, sig_addr)
            .map_err(|_| RseqError::UserAccessFault)?;
        let read_sig = u32::from_ne_bytes(sig_bytes);

        if read_sig != sig {
            log::warn!(
                "rseq: signature mismatch! expected 0x{:x}, got 0x{:x} (pid={})",
                sig,
                read_sig,
                ProcessManager::current_pid().data()
            );
            return Err(RseqError::SignatureMismatch);
        }

        Ok(Some(rseq_cs))
    }

    /// 清除 rseq_cs 指针
    unsafe fn clear_rseq_cs(&self) -> Result<(), RseqError> {
        self.write_u64(rseq_offsets::RSEQ_CS, 0)
    }

    /// 更新 CPU 和节点 ID
    unsafe fn update_cpu_node_id(
        &self,
        cpu_id: u32,
        node_id: u32,
        mm_cid: u32,
    ) -> Result<(), RseqError> {
        self.write_u32(rseq_offsets::CPU_ID_START, cpu_id)?;
        self.write_u32(rseq_offsets::CPU_ID, cpu_id)?;
        self.write_u32(rseq_offsets::NODE_ID, node_id)?;
        self.write_u32(rseq_offsets::MM_CID, mm_cid)?;
        Ok(())
    }

    /// 重置为未初始化状态
    unsafe fn reset(&self) -> Result<(), RseqError> {
        self.write_u32(rseq_offsets::CPU_ID_START, 0)?;
        self.write_u32(rseq_offsets::CPU_ID, RSEQ_CPU_ID_UNINITIALIZED as u32)?;
        self.write_u32(rseq_offsets::NODE_ID, 0)?;
        self.write_u32(rseq_offsets::MM_CID, 0)?;
        Ok(())
    }

    /// 读取用户态 flags
    unsafe fn read_flags(&self) -> Result<u32, RseqError> {
        self.read_u32(rseq_offsets::FLAGS)
    }
}

// ============================================================================
// 每线程的 rseq 注册状态
// ============================================================================

/// 每线程的 rseq 注册状态
///
/// 这个结构体存储在 PCB 中，记录线程的 rseq 注册信息。
/// 设计为不可变配置 + 原子事件掩码的组合，以减少锁竞争。
#[derive(Debug)]
pub struct RseqState {
    /// 注册信息（不可变部分，通过替换整体来修改）
    registration: Option<RseqRegistration>,
    /// 事件掩码（原子操作，无需锁）
    event_mask: AtomicU32,
}

/// rseq 注册信息
#[derive(Debug, Clone, Copy)]
struct RseqRegistration {
    /// 用户态 struct rseq 的地址
    ptr: VirtAddr,
    /// 注册的 rseq 结构长度
    len: u32,
    /// 注册的签名值
    sig: u32,
}

impl Default for RseqState {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for RseqState {
    fn clone(&self) -> Self {
        Self {
            registration: self.registration,
            event_mask: AtomicU32::new(self.event_mask.load(Ordering::Relaxed)),
        }
    }
}

impl RseqState {
    /// 创建新的未注册状态
    pub const fn new() -> Self {
        Self {
            registration: None,
            event_mask: AtomicU32::new(0),
        }
    }

    /// 检查是否已注册
    #[inline]
    pub fn is_registered(&self) -> bool {
        self.registration.is_some()
    }

    /// 获取注册信息
    #[inline]
    fn registration(&self) -> Option<&RseqRegistration> {
        self.registration.as_ref()
    }

    /// 获取当前 rseq_cs（从用户内存读取）
    /// 用于 rseq_syscall_check：检查是否在 rseq 临界区内发起了系统调用
    ///
    /// # Safety
    ///
    /// 调用者必须确保用户内存有效
    pub unsafe fn get_rseq_cs(&self) -> Option<(RseqCs, u32)> {
        let reg = self.registration.as_ref()?;
        let access = UserRseqAccess::new(reg.ptr);
        let user_end = MMArch::USER_END_VADDR.data();

        // 读取 rseq_cs，忽略签名验证（因为在 syscall 路径中我们已经注册过）
        match access.read_rseq_cs(reg.sig, user_end) {
            Ok(Some(cs)) => Some((cs, reg.sig)),
            _ => None,
        }
    }

    /// 设置事件掩码（原子操作）
    #[inline]
    pub fn set_event(&self, event: RseqEventMask) {
        self.event_mask.fetch_or(event.bits(), Ordering::SeqCst);
    }

    /// 获取并清除事件掩码（原子操作）
    #[inline]
    pub fn fetch_clear_event_mask(&self) -> RseqEventMask {
        let bits = self.event_mask.swap(0, Ordering::SeqCst);
        RseqEventMask::from_bits_truncate(bits)
    }
}

// ============================================================================
// rseq 操作 trait
// ============================================================================

/// TrapFrame 需要实现的 rseq 相关操作
pub trait RseqTrapFrame {
    /// 获取用户态返回地址（instruction pointer）
    fn rseq_ip(&self) -> usize;

    /// 设置用户态返回地址
    fn set_rseq_ip(&mut self, ip: usize);
}

// ============================================================================
// 核心业务逻辑
// ============================================================================

/// rseq 子系统
///
/// 提供 rseq 机制的核心操作
pub struct Rseq;

impl Rseq {
    /// 执行 sys_rseq 系统调用
    pub fn syscall(
        rseq_ptr: VirtAddr,
        rseq_len: u32,
        flags: i32,
        sig: u32,
    ) -> Result<usize, SystemError> {
        let flags = RseqFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        let pcb = ProcessManager::current_pcb();

        if flags.contains(RseqFlags::UNREGISTER) {
            if flags != RseqFlags::UNREGISTER {
                return Err(SystemError::EINVAL);
            }
            Self::do_unregister(&pcb, rseq_ptr, rseq_len, sig)
        } else if !flags.is_empty() {
            Err(SystemError::EINVAL)
        } else {
            Self::do_register(&pcb, rseq_ptr, rseq_len, sig)
        }
    }

    /// 执行注册
    fn do_register(
        pcb: &ProcessControlBlock,
        rseq_ptr: VirtAddr,
        rseq_len: u32,
        sig: u32,
    ) -> Result<usize, SystemError> {
        let mut rseq_state = pcb.rseq_state_mut();

        // 检查是否已注册
        if let Some(reg) = rseq_state.registration() {
            if reg.ptr != rseq_ptr || reg.len != rseq_len {
                return Err(SystemError::EINVAL);
            }
            if reg.sig != sig {
                return Err(SystemError::EPERM);
            }
            return Err(SystemError::EBUSY);
        }

        // 验证参数
        if rseq_len < ORIG_RSEQ_SIZE {
            return Err(SystemError::EINVAL);
        }

        // 验证对齐
        // Linux 6.6: 如果 rseq_len == ORIG_RSEQ_SIZE，需要对齐到 ORIG_RSEQ_SIZE；
        // 否则需要对齐到 __alignof__(struct rseq)，即 RSEQ_ALIGN (32 字节)
        let required_align = if rseq_len == ORIG_RSEQ_SIZE {
            ORIG_RSEQ_SIZE as usize
        } else {
            RSEQ_ALIGN as usize
        };

        if !rseq_ptr.check_aligned(required_align) {
            return Err(SystemError::EINVAL);
        }

        // 验证用户地址
        let user_end = MMArch::USER_END_VADDR;
        if rseq_ptr.data() >= user_end.data()
            || rseq_ptr.data() + rseq_len as usize > user_end.data()
        {
            return Err(SystemError::EFAULT);
        }

        // 执行注册
        rseq_state.registration = Some(RseqRegistration {
            ptr: rseq_ptr,
            len: rseq_len,
            sig,
        });
        rseq_state.event_mask.store(0, Ordering::SeqCst);
        drop(rseq_state);

        // 设置 NEED_RSEQ 标志
        pcb.flags().insert(ProcessFlags::NEED_RSEQ);

        Ok(0)
    }

    /// 执行反注册
    fn do_unregister(
        pcb: &ProcessControlBlock,
        rseq_ptr: VirtAddr,
        rseq_len: u32,
        sig: u32,
    ) -> Result<usize, SystemError> {
        let rseq_state = pcb.rseq_state_mut();

        let reg = rseq_state.registration().ok_or(SystemError::EINVAL)?;

        // 验证参数匹配
        if reg.ptr != rseq_ptr {
            return Err(SystemError::EINVAL);
        }
        if reg.len != rseq_len {
            return Err(SystemError::EINVAL);
        }
        if reg.sig != sig {
            return Err(SystemError::EPERM);
        }

        let ptr = reg.ptr;
        drop(rseq_state);

        // 重置用户态结构
        let access = UserRseqAccess::new(ptr);
        unsafe { access.reset() }.map_err(|_| SystemError::EFAULT)?;

        // 清除注册
        pcb.rseq_state_mut().registration = None;

        Ok(0)
    }

    /// 处理 notify-resume
    ///
    /// 在返回用户态前调用，执行 IP 修正和 cpu_id 更新
    pub fn handle_notify_resume<F: RseqTrapFrame>(frame: Option<&mut F>) -> Result<(), ()> {
        let pcb = ProcessManager::current_pcb();

        // 如果进程正在退出，直接返回
        if pcb.flags().contains(ProcessFlags::EXITING) {
            return Ok(());
        }

        let (ptr, sig) = {
            let rseq_state = pcb.rseq_state();
            match rseq_state.registration() {
                Some(reg) => (reg.ptr, reg.sig),
                None => {
                    pcb.flags().remove(ProcessFlags::NEED_RSEQ);
                    return Ok(());
                }
            }
        };

        let access = UserRseqAccess::new(ptr);
        let user_end = MMArch::USER_END_VADDR.data();

        // 如果有 frame，执行 IP 修正
        if let Some(frame) = frame {
            if let Err(e) = Self::ip_fixup(frame, &access, sig, user_end, &pcb) {
                log::error!("rseq ip_fixup failed: {:?}", e);
                return Err(());
            }
        }

        // 更新 cpu_id 等字段
        let cpu_id = current_cpu_id().data() as u32;
        if let Err(e) = unsafe { access.update_cpu_node_id(cpu_id, 0, 0) } {
            log::error!("rseq update_cpu_node_id failed: {:?}", e);
            return Err(());
        }

        pcb.flags().remove(ProcessFlags::NEED_RSEQ);
        Ok(())
    }

    /// 执行 IP 修正
    fn ip_fixup<F: RseqTrapFrame>(
        frame: &mut F,
        access: &UserRseqAccess,
        sig: u32,
        user_end: usize,
        pcb: &ProcessControlBlock,
    ) -> Result<(), RseqError> {
        let current_ip = frame.rseq_ip() as u64;

        // 获取 rseq_cs 描述符
        let rseq_cs = match unsafe { access.read_rseq_cs(sig, user_end) }? {
            Some(cs) => cs,
            None => return Ok(()),
        };

        // 检查是否在临界区内
        if !rseq_cs.contains_ip(current_ip) {
            // 不在临界区，lazy clear
            unsafe { access.clear_rseq_cs() }?;
            return Ok(());
        }

        // 在临界区内，检查是否需要重启
        let event_mask = pcb.rseq_state().fetch_clear_event_mask();
        let cs_flags = RseqCsFlags::from_bits_truncate(rseq_cs.flags);

        if !Self::need_restart(access, cs_flags, event_mask)? {
            return Ok(());
        }

        // 需要重启：清除 rseq_cs，修改 IP 为 abort_ip
        unsafe { access.clear_rseq_cs() }?;
        frame.set_rseq_ip(rseq_cs.abort_ip as usize);

        Ok(())
    }

    /// 判断是否需要重启
    fn need_restart(
        access: &UserRseqAccess,
        cs_flags: RseqCsFlags,
        event_mask: RseqEventMask,
    ) -> Result<bool, RseqError> {
        // 检查用户态 flags
        let user_flags = unsafe { access.read_flags() }?;
        let user_rseq_flags = RseqCsFlags::from_bits_truncate(user_flags);

        Self::warn_flags("rseq", user_rseq_flags)?;
        Self::warn_flags("rseq_cs", cs_flags)?;

        Ok(!event_mask.is_empty())
    }

    /// 检查并警告已弃用的 flags
    fn warn_flags(name: &str, flags: RseqCsFlags) -> Result<(), RseqError> {
        if flags.is_empty() {
            return Ok(());
        }

        let no_restart = RseqCsFlags::NO_RESTART_ON_PREEMPT
            | RseqCsFlags::NO_RESTART_ON_SIGNAL
            | RseqCsFlags::NO_RESTART_ON_MIGRATE;

        if flags.intersects(no_restart) {
            log::warn!(
                "rseq: deprecated flags ({:?}) in {} ABI structure",
                flags & no_restart,
                name
            );
        }

        let unknown = flags & !no_restart;
        if !unknown.is_empty() {
            log::warn!(
                "rseq: unknown flags ({:?}) in {} ABI structure",
                unknown,
                name
            );
        }

        // Linux 6.6: 只要有任何 flags（无论是已弃用的还是未知的）就返回错误
        // 参见 kernel/rseq.c 中的 rseq_warn_flags 和 rseq_need_restart
        Err(RseqError::InvalidFlags)
    }

    /// 在抢占/调度切换时调用
    #[inline]
    pub fn on_preempt(pcb: &ProcessControlBlock) {
        if pcb.rseq_state().is_registered() {
            pcb.rseq_state().set_event(RseqEventMask::PREEMPT);
            pcb.flags().insert(ProcessFlags::NEED_RSEQ);
        }
    }

    /// 在信号递送时调用
    pub fn on_signal<F: RseqTrapFrame>(frame: &mut F) {
        let pcb = ProcessManager::current_pcb();
        if pcb.rseq_state().is_registered() {
            pcb.rseq_state().set_event(RseqEventMask::SIGNAL);
            if Self::handle_notify_resume(Some(frame)).is_err() {
                let _ = send_signal_to_pcb(pcb.clone(), Signal::SIGSEGV);
            }
        }
    }

    /// 在 CPU 迁移时调用
    #[inline]
    #[allow(dead_code)]
    pub fn on_migrate(pcb: &ProcessControlBlock) {
        if pcb.rseq_state().is_registered() {
            pcb.rseq_state().set_event(RseqEventMask::MIGRATE);
            pcb.flags().insert(ProcessFlags::NEED_RSEQ);
        }
    }

    /// 系统调用退出时的 rseq 检查
    /// **注意**: Linux 的 rseq_syscall 仅在 CONFIG_DEBUG_RSEQ 启用时编译，
    /// 用于调试目的，检测在 rseq 临界区内发起系统调用的违规行为。
    ///
    /// 在生产环境中，此函数应为空操作。rseq 的正确性依赖于：
    /// 此函数目前为空操作，与 Linux 生产内核行为一致。
    ///
    /// # Safety
    ///
    /// 调用者必须保证 frame 指向有效的 TrapFrame
    #[inline]
    pub unsafe fn rseq_syscall_check<F: RseqTrapFrame>(_frame: &F) {
        // 生产环境：空操作，与 Linux 生产内核一致
        // 若需启用调试检查，应编译时启用 DEBUG_RSEQ 特性标志
    }
}

// ============================================================================
// 进程生命周期钩子
// ============================================================================

/// fork 时处理 rseq 状态
pub fn rseq_fork(child: &ProcessControlBlock, clone_vm: bool) {
    if clone_vm {
        // 线程共享地址空间，子线程需要重新注册
        child.rseq_state_mut().registration = None;
    } else {
        // 进程 fork，继承父进程的 rseq 状态
        // 先获取父进程的注册信息副本，然后释放读锁
        let parent_reg = {
            let pcb = ProcessManager::current_pcb();
            let parent_state = pcb.rseq_state();
            parent_state.registration().copied()
        };
        if let Some(reg) = parent_reg {
            child.rseq_state_mut().registration = Some(reg);
        }
    }
}

/// exec 时清除 rseq 状态
pub fn rseq_execve(pcb: &ProcessControlBlock) {
    pcb.rseq_state_mut().registration = None;
    pcb.rseq_state_mut().event_mask.store(0, Ordering::SeqCst);
}
