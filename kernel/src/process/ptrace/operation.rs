#[cfg(target_arch = "x86_64")]
use super::UserRegsStruct;
use super::{
    ptrace_session_matches, ptracer_of_locked, syscall_retval_is_error, validate_ptrace_options,
    PendingDebugRecord, PendingDebugSignal, PtraceEvent, PtraceFreezeToken, PtraceOptions,
    PtraceRequest, PtraceState, PtraceSyscallInfo, PtraceSyscallInfoOp, AUDIT_ARCH_X86_64,
    PTRACE_EVENTMSG_SYSCALL_ENTRY, PTRACE_EVENTMSG_SYSCALL_EXIT, PTRACE_SYSGOOD_BIT,
};
use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::{SigSet, Signal},
        MMArch,
    },
    ipc::signal_types::SigInfo,
    mm::{remote_access::RemoteAccess, MemoryManagementArch},
    process::{
        KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager, PTRACE_RELATION_LOCK,
    },
};
use alloc::sync::Arc;
use core::mem::size_of;
use system_error::SystemError;

/// Single-step bit (BS) in x86 DR6. When do_debug's error_code (=DR6) has this bit set, it indicates a single-step trap.
pub const X86_DR_BS: u64 = 1 << 14;
/// Hardware breakpoint hit bits (B0-B3) in x86 DR6.
pub const X86_DR_B_MASK: u64 = 0x0f;
/// x86 EFLAGS Trap Flag (single-step) bit.
pub const X86_EFLAGS_TF: u64 = 0x100;
/// x86 EFLAGS Resume Flag (resume flag) bit.
pub const X86_EFLAGS_RF: u64 = 1 << 16;
/// x86 DR6 reserved bits. ptrace exposes the positive-polarity virtual_dr6; this mask is applied when converting to/from the hardware DR6.
pub(crate) const DR6_RESERVED: u64 = 0xffff_0ff0;
/// x86_64 DR7 reserved-bit mask (including the GD bit)
/// Must be cleared before loading into hardware: setting a reserved bit is undefined behavior, and setting GD triggers #DB when the kernel accesses the debug registers.
pub(crate) const DR_CONTROL_RESERVED: u64 = 0xffff_ffff_0000_fc00;

/// Validate the configuration/address combination of a hardware breakpoint slot
#[cfg(target_arch = "x86_64")]
pub(super) fn validate_dr_slot(nibble: u64, addr: u64) -> Result<(), SystemError> {
    let rw = nibble & 0b11;
    let len_bits = (nibble >> 2) & 0b11;
    if rw == 0b10 {
        return Err(SystemError::EINVAL);
    }
    // Execution breakpoints only support a 1-byte length.
    if rw == 0b00 && len_bits != 0 {
        return Err(SystemError::EINVAL);
    }
    let len = match len_bits {
        0b00 => 1u64,
        0b01 => 2,
        0b10 => 8,
        _ => 4,
    };
    let user_end = MMArch::USER_END_VADDR.data() as u64;
    if addr >= user_end {
        return Err(SystemError::EINVAL);
    }
    // The breakpoint address must be aligned to its length.
    if addr & (len - 1) != 0 {
        return Err(SystemError::EINVAL);
    }
    // The end of the breakpoint range must not cross the top of the user address space.
    let end = addr.checked_add(len - 1).ok_or(SystemError::EINVAL)?;
    if end >= user_end {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
impl UserRegsStruct {
    /// Construct from a TrapFrame (GETREGS path).
    /// Note that orig_ax takes TrapFrame.errcode (the syscall number during a syscall).
    pub fn from_trap_frame(frame: &TrapFrame) -> Self {
        Self {
            r15: frame.r15,
            r14: frame.r14,
            r13: frame.r13,
            r12: frame.r12,
            bp: frame.rbp,
            bx: frame.rbx,
            r11: frame.r11,
            r10: frame.r10,
            r9: frame.r9,
            r8: frame.r8,
            ax: frame.rax,
            cx: frame.rcx,
            dx: frame.rdx,
            si: frame.rsi,
            di: frame.rdi,
            orig_ax: frame.errcode,
            ip: frame.rip,
            cs: frame.cs,
            flags: frame.rflags,
            sp: frame.rsp,
            ss: frame.ss,
            fs_base: 0,
            gs_base: 0,
            ds: frame.ds,
            es: frame.es,
            fs: 0,
            gs: 0,
        }
    }

    /// Write back to a TrapFrame (SETREGS path).
    /// Safety checks:
    /// - cs/ss must be RPL=3 and non-zero (prevents ring-0 injection)
    /// - rflags only allows the FLAG_MASK bits through, preserving the frame's non-masked bits (prevents clearing IF and hanging user mode)
    pub fn write_to_trap_frame(&self, frame: &mut TrapFrame) -> Result<(), SystemError> {
        // Segment selector validation
        // cs/ss: RPL=3, non-zero (SEGMENT_RPL_MASK=0x3, USER_RPL=3).
        if (self.cs & 0x3) != 3 || self.cs == 0 {
            return Err(SystemError::EIO);
        }
        if (self.ss & 0x3) != 3 || self.ss == 0 {
            return Err(SystemError::EIO);
        }
        // rflags: preserve the frame's non-masked bits, only allow the FLAG_MASK bits through.
        // FLAG_MASK = FLAG_MASK_32 | NT = CF|PF|AF|ZF|SF|TF|DF|RF|AC|NT = 0x00054DD5.
        const FLAG_MASK: u64 = 0x0005_4DD5;
        let new_rflags = (frame.rflags & !FLAG_MASK) | (self.flags & FLAG_MASK);

        frame.r15 = self.r15;
        frame.r14 = self.r14;
        frame.r13 = self.r13;
        frame.r12 = self.r12;
        frame.rbp = self.bp;
        frame.rbx = self.bx;
        frame.r11 = self.r11;
        frame.r10 = self.r10;
        frame.r9 = self.r9;
        frame.r8 = self.r8;
        frame.rax = self.ax;
        frame.rcx = self.cx;
        frame.rdx = self.dx;
        frame.rsi = self.si;
        frame.rdi = self.di;
        frame.errcode = self.orig_ax;
        frame.rip = self.ip;
        frame.cs = self.cs;
        frame.rflags = new_rflags;
        frame.rsp = self.sp;
        frame.ss = self.ss;
        frame.ds = self.ds;
        frame.es = self.es;
        Ok(())
    }
}

/// Linear proof that one tracer owns a stable generation of a ptrace-stop.
///
/// Creation commits the relation/session/stop checks together with the freeze,
/// then waits for the tracee's switch-out tail. Drop only releases the exact
/// token it owns, so a stale request cannot unfreeze a later stop or session.
pub(crate) struct PtraceRequestGuard {
    tracee: Arc<ProcessControlBlock>,
    tracer: Arc<ProcessControlBlock>,
    token: PtraceFreezeToken,
}

impl PtraceRequestGuard {
    pub(crate) fn begin(
        tracee: Arc<ProcessControlBlock>,
        tracer: Arc<ProcessControlBlock>,
    ) -> Result<Self, SystemError> {
        let token = {
            // DragonOS's tasklist-like transaction. Keep this nesting aligned
            // with ptrace_stop(): relation -> sighand -> siginfo -> ptrace state.
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let owner = ptracer_of_locked(&tracee).ok_or(SystemError::ESRCH)?;
            if !Arc::ptr_eq(&owner, &tracer) {
                return Err(SystemError::ESRCH);
            }
            let session_generation = tracee.ptrace_session_generation();

            let sighand = tracee.sighand();
            let sighand_guard = sighand.inner_read();
            let siginfo_guard = tracee.sig_info_irqsave();
            let fatal = siginfo_guard
                .sig_pending()
                .signal()
                .contains(Signal::SIGKILL.into())
                || sighand_guard
                    .shared_pending
                    .signal()
                    .contains(Signal::SIGKILL.into());
            if fatal {
                return Err(SystemError::ESRCH);
            }

            let mut state = tracee.ptrace.state.lock_irqsave();
            if !tracee.sched_info().state().is_stopped() {
                return Err(SystemError::ESRCH);
            }
            state.install_freeze_owner(session_generation)?
        };

        // Construct the provisional guard immediately after the freeze commit.
        // Any failure below therefore compare-releases the token via Drop.
        let guard = Self {
            tracee,
            tracer,
            token,
        };

        // `running`, unlike DragonOS's task_cpu-style `on_cpu`, becomes false
        // only after switch_finish_hook has stopped using the kernel stack and
        // architecture context.
        guard.tracee.sched_info().wait_until_not_running();

        let still_valid = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            guard.tracee.ptrace_session_generation() == guard.token.session_generation
                && ptracer_of_locked(&guard.tracee)
                    .map(|owner| Arc::ptr_eq(&owner, &guard.tracer))
                    .unwrap_or(false)
                && {
                    let state = guard.tracee.ptrace.state.lock_irqsave();
                    state.freeze_owner_matches(guard.token)
                        && guard.tracee.sched_info().state().is_stopped()
                        && !guard.tracee.sched_info().is_running()
                }
        };
        if !still_valid {
            return Err(SystemError::ESRCH);
        }

        Ok(guard)
    }

    pub(crate) fn detach(self, signal: Option<Signal>) -> Result<isize, SystemError> {
        self.tracee
            .ptrace_detach_guarded(&self.tracer, self.token, signal)
    }

    pub(crate) fn listen(self) -> Result<isize, SystemError> {
        self.tracee.ptrace_listen_guarded(self.token)
    }

    pub(crate) fn resume(
        self,
        request: PtraceRequest,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        self.tracee
            .ptrace_resume_guarded(self.token, request, signal)
    }

    pub(crate) fn set_options(&self, options: PtraceOptions) -> Result<(), SystemError> {
        self.tracee.set_ptrace_options(options)
    }

    pub(crate) fn event_message(&self) -> usize {
        self.tracee.ptrace_get_event_message()
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn get_regs(&self) -> Result<UserRegsStruct, SystemError> {
        self.tracee.tracee_user_regs()
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn set_regs(&self, regs: &UserRegsStruct) -> Result<(), SystemError> {
        self.tracee.write_tracee_user_regs(regs)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn peek_user(&self, offset: usize) -> Result<usize, SystemError> {
        self.tracee.ptrace_peek_user(offset)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn poke_user(&self, offset: usize, value: usize) -> Result<(), SystemError> {
        self.tracee.ptrace_poke_user(offset, value)
    }

    pub(crate) fn get_siginfo(&self) -> Result<SigInfo, SystemError> {
        self.tracee.ptrace_get_siginfo()
    }

    pub(crate) fn set_siginfo(&self, info: SigInfo) -> Result<(), SystemError> {
        self.tracee.ptrace_set_siginfo(info)
    }

    pub(crate) fn get_sigmask(&self) -> SigSet {
        self.tracee.ptrace_get_sigmask()
    }

    pub(crate) fn set_sigmask(&self, mask: SigSet) {
        self.tracee.ptrace_set_sigmask(mask);
    }

    pub(crate) fn peek_data(&self, addr: usize) -> Result<usize, SystemError> {
        self.tracee.ptrace_peek_data(addr)
    }

    pub(crate) fn poke_data(&self, addr: usize, value: usize) -> Result<(), SystemError> {
        self.tracee.ptrace_poke_data(addr, value)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn syscall_info(&self) -> Result<PtraceSyscallInfo, SystemError> {
        self.tracee.ptrace_get_syscall_info()
    }
}

impl Drop for PtraceRequestGuard {
    fn drop(&mut self) {
        let release = self
            .tracee
            .ptrace
            .state
            .lock_irqsave()
            .release_freeze_owner(self.token);
        release.apply(&self.tracee);
    }
}

impl ProcessControlBlock {
    /// Set the ptrace options (PTRACE_SETOPTIONS).
    fn set_ptrace_options(&self, options: PtraceOptions) -> Result<(), SystemError> {
        validate_ptrace_options(options)?;
        let mut ps = self.ptrace.state.lock_irqsave();
        ps.options = options;
        Ok(())
    }

    /// Read the most recent event message (PTRACE_GETEVENTMSG).
    fn ptrace_get_event_message(&self) -> usize {
        self.ptrace.state.lock_irqsave().stop_event_message()
    }

    /// Syscall stack accessor (ptrace needs to read the trap frame on the syscall stack).
    fn syscall_stack(&self) -> crate::libs::rwlock::RwLockReadGuard<'_, KernelStack> {
        self.syscall_stack.read()
    }

    /// Check whether the current rsp is within the syscall stack range.
    /// Used to dynamically determine which stack the TrapFrame is on in ptrace_stop.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn current_stop_frame_on_syscall_stack(&self) -> bool {
        let current_rsp = x86::current::registers::rsp() as usize;
        let syscall_stack = self.syscall_stack();
        let start = syscall_stack.start_address().data();
        let end = syscall_stack.stack_max_address().data();
        (start..end).contains(&current_rsp)
    }

    /// Compute the TrapFrame pointer on the kernel stack.
    fn trap_frame_ptr_on_kernel_stack(stack: &KernelStack) -> *mut TrapFrame {
        let ptr = stack.stack_max_address().data() - size_of::<TrapFrame>();
        ptr as *mut TrapFrame
    }

    /// Compute the TrapFrame pointer on the syscall stack.
    /// Note: init_syscall_stack sets GS:0x0 = stack_max - 8 (reserving 8 bytes),
    /// so the TrapFrame is actually at stack_max - 8 - sizeof(TrapFrame).
    #[cfg(target_arch = "x86_64")]
    fn trap_frame_ptr_on_syscall_stack(stack: &KernelStack) -> *mut TrapFrame {
        let ptr = stack.stack_max_address().data() - 8 - size_of::<TrapFrame>();
        ptr as *mut TrapFrame
    }

    /// Compute the TrapFrame pointer for the given stack choice.
    pub(super) fn trap_frame_ptr_for(&self, on_syscall_stack: bool) -> *mut TrapFrame {
        if on_syscall_stack {
            #[cfg(target_arch = "x86_64")]
            {
                let s = self.syscall_stack();
                Self::trap_frame_ptr_on_syscall_stack(&s)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                let s = self.kernel_stack();
                Self::trap_frame_ptr_on_kernel_stack(&s)
            }
        } else {
            let s = self.kernel_stack();
            Self::trap_frame_ptr_on_kernel_stack(&s)
        }
    }

    /// Re-verify under the ptrace_state lock that the tracee is still in a ptrace-stop (frame stable).
    /// A fatal signal may wake the tracee between check_attach and here; once the frame is stale,
    /// the caller should give up writing and return ESRCH.
    pub(super) fn trap_frame_stable_locked(&self, ps: &PtraceState) -> bool {
        ps.is_traced_stop() && self.sched_info().state().is_stopped()
    }

    /// Wait for the tracee to actually be scheduled out
    fn wait_tracee_descheduled(&self) {
        while self.sched_info().is_running() && self.sched_info().state().is_stopped() {
            core::hint::spin_loop();
        }
    }

    /// Read the tracee's user registers (PTRACE_GETREGS).
    #[cfg(target_arch = "x86_64")]
    fn tracee_user_regs(&self) -> Result<UserRegsStruct, SystemError> {
        loop {
            self.wait_tracee_descheduled();
            let ps = self.ptrace.state.lock_irqsave();
            if !Self::trap_frame_stable_locked(self, &ps) {
                return Err(SystemError::ESRCH);
            }
            if self.sched_info().is_running() {
                // After waiting, the tracee was woken up again and stopped once more in the pre-schedule window; retry.
                continue;
            }
            // SAFETY: The re-verification passed; the tracee is still in a ptrace-stop and the TrapFrame is stable.
            let frame = unsafe { &*self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            let mut regs = UserRegsStruct::from_trap_frame(frame);
            // fs/gs base are not in the TrapFrame: read them from the ArchPCBInfo authoritative storage (the latest values were read back at switch-out).
            {
                let arch = self.arch_info_irqsave();
                regs.fs_base = arch.fsbase() as u64;
                regs.gs_base = arch.gsbase() as u64;
            }
            if ps.forced_trap_flag {
                regs.flags &= !X86_EFLAGS_TF;
            }
            return Ok(regs);
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn write_tracee_user_regs(&self, regs: &UserRegsStruct) -> Result<(), SystemError> {
        loop {
            self.wait_tracee_descheduled();
            let mut ps = self.ptrace.state.lock_irqsave();
            if !Self::trap_frame_stable_locked(self, &ps) {
                return Err(SystemError::ESRCH);
            }
            if self.sched_info().is_running() {
                continue;
            }
            // fs/gs base validation
            let user_end = MMArch::USER_END_VADDR.data() as u64;
            if regs.fs_base >= user_end || regs.gs_base >= user_end {
                return Err(SystemError::EIO);
            }
            // SAFETY: The tracee is still in a ptrace-stop and the TrapFrame is stable.
            let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            regs.write_to_trap_frame(frame)?;
            // Write fs/gs base into the ArchPCBInfo authoritative storage; only touches memory, not hardware,
            {
                let mut arch = self.arch_info_irqsave();
                arch.set_fsbase(regs.fs_base as usize);
                arch.set_gsbase(regs.gs_base as usize);
            }
            if frame.rflags & X86_EFLAGS_TF != 0 {
                ps.forced_trap_flag = false;
            } else if ps.forced_trap_flag {
                frame.rflags |= X86_EFLAGS_TF;
            }
            return Ok(());
        }
    }
    #[cfg(target_arch = "x86_64")]
    fn ptrace_peek_user(&self, offset: usize) -> Result<usize, SystemError> {
        // 8-byte alignment check
        if offset & (size_of::<u64>() - 1) != 0 {
            return Err(SystemError::EIO);
        }
        const SIZEOF_USER: usize = 928; // sizeof(struct user) x86_64
        const DR_OFFSET: usize = 848; // offsetof(struct user, u_debugreg[0])
        const GP_REGS_SIZE: usize = size_of::<UserRegsStruct>(); // 216
        if offset
            .checked_add(size_of::<u64>())
            .is_none_or(|end| end > SIZEOF_USER)
        {
            return Err(SystemError::EIO);
        }
        // General-purpose register area: offset 0..GP_REGS_SIZE
        if offset < GP_REGS_SIZE {
            let regs = self.tracee_user_regs()?;
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    &regs as *const UserRegsStruct as *const u8,
                    GP_REGS_SIZE,
                )
            };
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[offset..offset + 8]);
            return Ok(u64::from_ne_bytes(buf) as usize);
        }
        // Debug register area: offset DR_OFFSET..=DR_OFFSET+56 (DR0-DR7, 8 slots)
        if (DR_OFFSET..DR_OFFSET + 8 * 8).contains(&offset) {
            let idx = (offset - DR_OFFSET) / 8;
            let mut val = self.ptrace.state.lock_irqsave().debug_regs[idx];
            // DR6 (slot6) stores the positive-polarity virtual_dr6; flip it back to the hardware polarity when returning.
            if idx == 6 {
                val ^= DR6_RESERVED;
            }
            return Ok(val as usize);
        }
        // Gap area (padding fields between GP_REGS_SIZE and DR_OFFSET): silently return 0.
        Ok(0)
    }

    /// PTRACE_POKEUSER: write one word at a byte offset.
    /// Requires 8-byte alignment and validation via putreg.
    #[cfg(target_arch = "x86_64")]
    fn ptrace_poke_user(&self, offset: usize, value: usize) -> Result<(), SystemError> {
        const SIZEOF_USER: usize = 928;
        const DR_OFFSET: usize = 848;
        const GP_REGS_SIZE: usize = size_of::<UserRegsStruct>();
        if offset & (size_of::<u64>() - 1) != 0 {
            return Err(SystemError::EIO);
        }
        if offset
            .checked_add(size_of::<u64>())
            .is_none_or(|end| end > SIZEOF_USER)
        {
            return Err(SystemError::EIO);
        }
        let val = value as u64;
        // General-purpose register area: write back to the trap frame after putreg validation.
        if offset < GP_REGS_SIZE {
            let mut regs = self.tracee_user_regs()?;
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    &mut regs as *mut UserRegsStruct as *mut u8,
                    GP_REGS_SIZE,
                )
            };
            bytes[offset..offset + 8].copy_from_slice(&val.to_ne_bytes());
            self.write_tracee_user_regs(&regs)?;
            return Ok(());
        }
        // Debug register area.
        if (DR_OFFSET..DR_OFFSET + 8 * 8).contains(&offset) {
            let idx = (offset - DR_OFFSET) / 8;
            // DR4/DR5 do not exist; refuse the write.
            if idx == 4 || idx == 5 {
                return Err(SystemError::EIO);
            }

            let has_dr = {
                let mut ps = self.ptrace.state.lock_irqsave();
                match idx {
                    // DR0-3: address registers. Out-of-range values are forbidden
                    0..=3 => {
                        if val >= MMArch::USER_END_VADDR.data() as u64 {
                            return Err(SystemError::EINVAL);
                        }
                        let dr7 = ps.debug_regs[7] & !DR_CONTROL_RESERVED;
                        if ((dr7 >> (idx * 2)) & 3) != 0 {
                            validate_dr_slot((dr7 >> (16 + idx * 4)) & 0xf, val)?;
                        }
                        ps.debug_regs[idx] = val;
                    }
                    // DR6 (slot6) stores the positive-polarity virtual_dr6; flip when writing.
                    6 => {
                        ps.debug_regs[6] = val ^ DR6_RESERVED;
                    }
                    // DR7: control register. In the same critical section, "validate everything first, then commit
                    _ => {
                        let v = val & !DR_CONTROL_RESERVED;
                        for i in 0..4usize {
                            // A slot that is not enabled and whose address was never written has no
                            // combination to validate, so skip it; a slot whose address was written (a
                            // combination state exists) is validated regardless of whether it is enabled,
                            // guaranteeing the combination state is always valid under any write order.
                            if ((v >> (i * 2)) & 3) == 0 && ps.debug_regs[i] == 0 {
                                continue;
                            }
                            validate_dr_slot((v >> (16 + i * 4)) & 0xf, ps.debug_regs[i])?;
                        }
                        ps.debug_regs[7] = val;
                    }
                }
                // Maintain the hardware-breakpoint fast-path flag: any non-zero address register (DR0-3) or control register (DR7) counts as having configuration, and context switch loads/clears accordingly.
                ps.debug_regs[0..4].iter().any(|&v| v != 0) || ps.debug_regs[7] != 0
            };
            if has_dr {
                self.flags().insert(ProcessFlags::HW_DEBUG_REGS);
            } else {
                self.flags().remove(ProcessFlags::HW_DEBUG_REGS);
            }
            return Ok(());
        }
        // The gap area refuses writes.
        Err(SystemError::EIO)
    }

    /// Clear the hardware breakpoint configuration after a successful exec
    pub fn flush_ptrace_hw_debug_regs(&self) {
        {
            let mut ps = self.ptrace.state.lock_irqsave();
            ps.debug_regs = [0; 8];
            ps.pending_debug = None;
        }
        self.flags()
            .remove(ProcessFlags::HW_DEBUG_REGS | ProcessFlags::PENDING_DEBUG);
        #[cfg(target_arch = "x86_64")]
        if ProcessManager::current_pcb().raw_pid() == self.raw_pid() {
            crate::arch::x86_64::process::clear_current_debug_regs(self);
        }
    }

    /// Record #DB causes without allocating or delivering a signal from the
    /// exception entry. `owned_bits` belong to the ptrace session identified
    /// by `owner_generation`; `unowned_bits` are user-originated TF/ICEBP
    /// causes and remain deliverable without a tracer.
    pub(crate) fn record_pending_debug(
        &self,
        owned_bits: u64,
        unowned_bits: u64,
        icebp: bool,
        addr: usize,
        owner_generation: u64,
    ) {
        let mut ps = self.ptrace.state.lock_irqsave();
        match ps.pending_debug.as_mut() {
            Some(pending) if pending.owner_generation == owner_generation => {
                pending.owned_bits |= owned_bits;
                pending.unowned_bits |= unowned_bits;
                pending.icebp |= icebp;
                if pending.addr == 0 {
                    pending.addr = addr;
                }
            }
            _ => {
                ps.pending_debug = Some(PendingDebugRecord {
                    owned_bits,
                    unowned_bits,
                    icebp,
                    addr,
                    owner_generation,
                });
            }
        }
        drop(ps);
        self.flags().insert(ProcessFlags::PENDING_DEBUG);
    }

    /// Consume pending #DB causes at return-to-user. Causes owned by a stale
    /// ptrace session are discarded; user-originated causes remain signals.
    pub(crate) fn take_pending_debug_signal(&self) -> Option<PendingDebugSignal> {
        let pending = self.ptrace.state.lock_irqsave().pending_debug.take()?;
        let owner_is_current =
            pending.owned_bits != 0 && ptrace_session_matches(self, pending.owner_generation);
        let owned_bits = if owner_is_current {
            pending.owned_bits
        } else {
            0
        };
        let bits = owned_bits | pending.unowned_bits;
        (bits != 0 || pending.icebp).then_some(PendingDebugSignal {
            bits,
            icebp: pending.icebp,
            addr: pending.addr,
        })
    }

    /// Commit the virtual DR6 value for a user #DB and snapshot whether the
    /// single-step reason was armed by ptrace in the same state transaction.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn record_ptrace_debug_status(&self, report_bits: u64) -> bool {
        let mut state = self.ptrace.state.lock_irqsave();
        state.debug_regs[6] = report_bits;
        state.forced_trap_flag
    }

    /// Preserve user-breakpoint causes observed while handling a kernel #DB.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn merge_ptrace_debug_status(&self, breakpoint_bits: u64) {
        self.ptrace.state.lock_irqsave().debug_regs[6] |= breakpoint_bits;
    }

    /// Snapshot the saved debug registers for context-switch restore.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn ptrace_debug_regs_snapshot(&self) -> [u64; 8] {
        self.ptrace.state.lock_irqsave().debug_regs
    }

    /// PTRACE_GETSIGINFO: read last_siginfo.
    fn ptrace_get_siginfo(&self) -> Result<SigInfo, SystemError> {
        let ps = self.ptrace.state.lock_irqsave();
        ps.stop_siginfo().ok_or(SystemError::EINVAL)
    }

    /// PTRACE_SETSIGINFO: write last_siginfo.
    fn ptrace_set_siginfo(&self, info: SigInfo) -> Result<(), SystemError> {
        let mut ps = self.ptrace.state.lock_irqsave();
        let slot = ps.stop_siginfo_mut().ok_or(SystemError::EINVAL)?;
        *slot = info;
        Ok(())
    }

    /// PTRACE_GETSIGMASK: read the current blocked mask.
    /// Returns a SigSet (DragonOS-v GenericSigSet, u64).
    fn ptrace_get_sigmask(&self) -> SigSet {
        let g = self.sig_info_irqsave();
        *g.sig_blocked()
    }

    /// PTRACE_SETSIGMASK: set the blocked mask (SIGKILL/SIGSTOP cannot be blocked).
    fn ptrace_set_sigmask(&self, mut new_set: SigSet) {
        new_set.remove(SigSet::SIGKILL);
        new_set.remove(SigSet::SIGSTOP);
        let mut g = self.sig_info_mut();
        *g.sig_block_mut() = new_set;
    }

    // PEEKDATA / POKEDATA -- read/write the tracee's user memory via the MM layer's unified remote-access API

    /// PTRACE_PEEKDATA/PEEKTEXT: read one word from the tracee's user space.
    /// Correctly handles page-crossing words (an 8-byte word spans two pages when addr is at the end of a page).
    fn ptrace_peek_data(&self, addr: usize) -> Result<usize, SystemError> {
        // Wrap-around prevention check
        let last = addr
            .checked_add(size_of::<usize>() - 1)
            .ok_or(SystemError::EIO)?;
        if last >= MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EIO);
        }
        let _mm_guard = self.active_vm().ok_or(SystemError::ESRCH)?;
        let target_vm = _mm_guard.vm().clone();
        let mut bytes = [0u8; size_of::<usize>()];
        // Copy the whole word under a single AddressSpace read lock (force=true: ptrace override semantics)
        let n = target_vm.access_remote_vm(addr, RemoteAccess::Read(&mut bytes), true)?;
        if n != size_of::<usize>() {
            return Err(SystemError::EIO);
        }
        Ok(usize::from_ne_bytes(bytes))
    }

    /// PTRACE_POKEDATA/POKETEXT: write one word to the tracee's user space.
    fn ptrace_poke_data(&self, addr: usize, value: usize) -> Result<(), SystemError> {
        let last = addr
            .checked_add(size_of::<usize>() - 1)
            .ok_or(SystemError::EIO)?;
        if last >= MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EIO);
        }
        let _mm_guard = self.active_vm().ok_or(SystemError::ESRCH)?;
        let target_vm = _mm_guard.vm().clone();
        let bytes = value.to_ne_bytes();
        let n = target_vm.access_remote_vm(addr, RemoteAccess::Write(&bytes), true)?;
        if n != size_of::<usize>() {
            return Err(SystemError::EIO);
        }
        Ok(())
    }

    /// PTRACE_GET_SYSCALL_INFO.
    /// Determines op from the mutable siginfo and event message published together with the stop
    /// generation, aligning with Linux 6.6's semantics of directly reading last_siginfo/ptrace_message.
    /// The op decision and frame read are completed in the same ptrace_state critical section, and the
    /// tracee is first re-verified to still be in a ptrace-stop (returning ESRCH on failure), so the two
    /// never come from snapshots assembled at different times; the user-space copy is done by the caller outside the lock.
    #[cfg(target_arch = "x86_64")]
    fn ptrace_get_syscall_info(&self) -> Result<PtraceSyscallInfo, SystemError> {
        let ps = self.ptrace.state.lock_irqsave();
        if !Self::trap_frame_stable_locked(self, &ps) {
            return Err(SystemError::ESRCH);
        }
        let code = ps
            .stop_siginfo()
            .map(|info| info.sig_code().as_i32())
            .unwrap_or(0);
        let msg = ps.stop_event_message();
        let op = match (code, msg) {
            (c, PTRACE_EVENTMSG_SYSCALL_ENTRY)
                if (c & 0xff) == (Signal::SIGTRAP as i32 | PTRACE_SYSGOOD_BIT as i32) =>
            {
                PtraceSyscallInfoOp::Entry
            }
            (c, PTRACE_EVENTMSG_SYSCALL_EXIT)
                if (c & 0xff) == (Signal::SIGTRAP as i32 | PTRACE_SYSGOOD_BIT as i32) =>
            {
                PtraceSyscallInfoOp::Exit
            }
            _ if (code >> 8) == PtraceEvent::Seccomp as i32 => PtraceSyscallInfoOp::Seccomp,
            _ => PtraceSyscallInfoOp::None,
        };
        // On a Seccomp stop, ret_data = SECCOMP_RET_DATA
        let ret_data = if op == PtraceSyscallInfoOp::Seccomp {
            msg as u32
        } else {
            0
        };
        // Read the trap frame to fill in ip/sp/nr/args.
        // SAFETY: The re-verification passed; the tracee is still in a ptrace-stop and the TrapFrame is stable.
        let frame = unsafe { &*self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
        // AUDIT_ARCH_X86_64 (the only supported architecture on x86_64; switch to arch-provided when multi-arch).
        let arch: u32 = AUDIT_ARCH_X86_64;
        let mut info = PtraceSyscallInfo::new(arch, frame.rip, frame.rsp);
        match op {
            PtraceSyscallInfoOp::Entry | PtraceSyscallInfoOp::Seccomp => {
                let nr = frame.errcode;
                let args = [
                    frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
                ];
                info = info.with_entry(nr, args);
                if op == PtraceSyscallInfoOp::Seccomp {
                    info = info.with_seccomp(nr, args, ret_data);
                }
            }
            PtraceSyscallInfoOp::Exit => {
                let rval = frame.rax as i64;
                let is_error = syscall_retval_is_error(rval);
                info = info.with_exit(rval, is_error);
            }
            PtraceSyscallInfoOp::None => {}
        }
        Ok(info)
    }
}
