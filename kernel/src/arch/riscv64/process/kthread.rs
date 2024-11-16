use core::arch::asm;

use crate::{
    arch::{asm::csr::CSR_SSTATUS, interrupt::TrapFrame},
    process::{
        fork::CloneFlags,
        kthread::{kernel_thread_bootstrap_stage2, KernelThreadCreateInfo, KernelThreadMechanism},
        Pid, ProcessManager,
    },
};
use alloc::sync::Arc;
use asm_macros::restore_from_x6_to_x31;
use kdepends::memoffset::offset_of;
use riscv::register::sstatus::SPP;
use system_error::SystemError;

impl KernelThreadMechanism {
    /// 伪造trapframe，创建内核线程
    ///
    /// ## 返回值
    ///
    /// 返回创建的内核线程的pid
    #[inline(never)]
    pub fn __inner_create(
        info: &Arc<KernelThreadCreateInfo>,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        // WARNING: If create failed, we must drop the info manually or it will cause memory leak. (refcount will not decrease when create failed)
        let create_info: *const KernelThreadCreateInfo =
            KernelThreadCreateInfo::generate_unsafe_arc_ptr(info.clone());

        let mut frame = TrapFrame::new();
        frame.a2 = create_info as usize;

        // 使能中断
        frame.status.update_sie(true);
        frame.status.update_spp(SPP::Supervisor);
        frame.status.update_sum(true);

        frame.ra = kernel_thread_bootstrap_stage1 as usize;

        // fork失败的话，子线程不会执行。否则将导致内存安全问题。
        let pid = ProcessManager::fork(&frame, clone_flags).map_err(|e| {
            unsafe { KernelThreadCreateInfo::parse_unsafe_arc_ptr(create_info) };
            e
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
// #[naked]
// pub(super) unsafe extern "C" fn kernel_thread_bootstrap_stage1() {
//     todo!()
// }
#[naked]
pub(super) unsafe extern "C" fn kernel_thread_bootstrap_stage1() {
    // 这个函数要是naked的，只是因为现在还没有实现，而naked func不能打`unimplemented!()`
    // 所以先写成了普通函数
    core::arch::naked_asm!(concat!(
        "
            ld x3, {off_gp}(sp)
            ld x5, {off_t0}(sp)

        ",
        restore_from_x6_to_x31!(),

        "
            ld a0, {off_status}(sp)
            csrw {csr_status}, a0
            mv a0, a2
            j {stage2_func}
        "
    ),
        csr_status = const CSR_SSTATUS,
        off_status = const offset_of!(TrapFrame, status),
        off_gp = const offset_of!(TrapFrame, gp),
        off_t0 = const offset_of!(TrapFrame, t0),
        off_t1 = const offset_of!(TrapFrame, t1),
        off_t2 = const offset_of!(TrapFrame, t2),
        off_s0 = const offset_of!(TrapFrame, s0),
        off_s1 = const offset_of!(TrapFrame, s1),
        off_a0 = const offset_of!(TrapFrame, a0),
        off_a1 = const offset_of!(TrapFrame, a1),
        off_a2 = const offset_of!(TrapFrame, a2),
        off_a3 = const offset_of!(TrapFrame, a3),
        off_a4 = const offset_of!(TrapFrame, a4),
        off_a5 = const offset_of!(TrapFrame, a5),
        off_a6 = const offset_of!(TrapFrame, a6),
        off_a7 = const offset_of!(TrapFrame, a7),
        off_s2 = const offset_of!(TrapFrame, s2),
        off_s3 = const offset_of!(TrapFrame, s3),
        off_s4 = const offset_of!(TrapFrame, s4),
        off_s5 = const offset_of!(TrapFrame, s5),
        off_s6 = const offset_of!(TrapFrame, s6),
        off_s7 = const offset_of!(TrapFrame, s7),
        off_s8 = const offset_of!(TrapFrame, s8),
        off_s9 = const offset_of!(TrapFrame, s9),
        off_s10 = const offset_of!(TrapFrame, s10),
        off_s11 = const offset_of!(TrapFrame, s11),
        off_t3 = const offset_of!(TrapFrame, t3),
        off_t4 = const offset_of!(TrapFrame, t4),
        off_t5 = const offset_of!(TrapFrame, t5),
        off_t6 = const offset_of!(TrapFrame, t6),
        stage2_func = sym jump_to_stage2
    );
}

fn jump_to_stage2(ptr: *const KernelThreadCreateInfo) {
    unsafe { kernel_thread_bootstrap_stage2(ptr) };
}
