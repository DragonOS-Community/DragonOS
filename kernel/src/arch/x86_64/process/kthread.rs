use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        process::table::{KERNEL_CS, KERNEL_DS},
    },
    process::{
        fork::CloneFlags,
        kthread::{kernel_thread_bootstrap_stage2, KernelThreadCreateInfo, KernelThreadMechanism},
        Pid, ProcessManager,
    },
};

impl KernelThreadMechanism {
    /// 伪造trapframe，创建内核线程
    ///
    /// ## 返回值
    ///
    /// 返回创建的内核线程的pid
    pub fn __inner_create(
        info: &Arc<KernelThreadCreateInfo>,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        // WARNING: If create failed, we must drop the info manually or it will cause memory leak. (refcount will not decrease when create failed)
        let create_info: *const KernelThreadCreateInfo =
            KernelThreadCreateInfo::generate_unsafe_arc_ptr(info.clone());

        let mut frame = TrapFrame::new();
        frame.rbx = create_info as usize as u64;
        frame.ds = KERNEL_DS.bits() as u64;
        frame.es = KERNEL_DS.bits() as u64;
        frame.cs = KERNEL_CS.bits() as u64;
        frame.ss = KERNEL_DS.bits() as u64;

        // 使能中断
        frame.rflags |= 1 << 9;

        frame.rip = kernel_thread_bootstrap_stage1 as usize as u64;

        // fork失败的话，子线程不会执行。否则将导致内存安全问题。
        let pid = ProcessManager::fork(&frame, clone_flags).inspect_err(|_e| {
            unsafe { KernelThreadCreateInfo::parse_unsafe_arc_ptr(create_info) };
        })?;

        ProcessManager::find(pid)
            .unwrap()
            .set_name(info.name().clone());

        return Ok(pid);
    }
}

/// 内核线程引导函数的第一阶段
///
/// 当内核线程开始执行时，会先执行这个函数，这个函数会将伪造的trapframe中的数据弹出，然后跳转到第二阶段
///
/// 跳转之后，指向Box<KernelThreadClosure>的指针将传入到stage2的函数
#[naked]
pub(super) unsafe extern "sysv64" fn kernel_thread_bootstrap_stage1() {
    core::arch::naked_asm!(
        concat!(
            "

            pop r15
            pop r14
            pop r13
            pop r12
            pop r11
            pop r10
            pop r9
            pop r8
            pop rbx
            pop rcx
            pop rdx
            pop rsi
            pop rdi
            pop rbp
            pop rax
            mov ds, ax
            pop rax
            mov es, ax
            pop rax
            add rsp, 0x20
            popfq
            add rsp, 0x10
            mov rdi, rbx
            jmp {stage2_func}
            "
        ),
        stage2_func = sym kernel_thread_bootstrap_stage2,
    )
}
