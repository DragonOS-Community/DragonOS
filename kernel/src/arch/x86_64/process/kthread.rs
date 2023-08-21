use core::arch::asm;

use alloc::{boxed::Box, sync::Arc};

use crate::{
    arch::{
        interrupt::TrapFrame,
        process::table::{KERNEL_CS, KERNEL_DS},
    },
    process::{
        fork::CloneFlags,
        kthread::{
            kernel_thread_bootstrap_stage2, KernelThreadClosure, KernelThreadCreateInfo,
            KernelThreadMechanism,
        },
        Pid, ProcessFlags, ProcessManager,
    },
    syscall::SystemError,
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
        let closure: &mut KernelThreadClosure = Box::leak(info.take_closure().unwrap());

        let mut frame = TrapFrame::new();
        frame.rbx = closure as *mut KernelThreadClosure as u64;
        frame.ds = KERNEL_DS as u64;
        frame.es = KERNEL_DS as u64;
        frame.cs = KERNEL_CS as u64;
        frame.ss = KERNEL_DS as u64;

        // 使能中断
        frame.rflags |= 1 << 9;
        frame.rip = &kernel_thread_bootstrap_stage1 as *const _ as u64;

        return ProcessManager::fork(&mut frame, clone_flags);
    }
}

/// 内核线程引导函数的第一阶段
///
/// 当内核线程开始执行时，会先执行这个函数，这个函数会将伪造的trapframe中的数据弹出，然后跳转到第二阶段
///
/// 跳转之后，指向Box<KernelThreadClosure>的指针将传入到stage2的函数
#[naked]
unsafe extern "sysv64" fn kernel_thread_bootstrap_stage1() {
    asm!(
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
        options(noreturn)
    )
}
