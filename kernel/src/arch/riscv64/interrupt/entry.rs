use crate::arch::{
    asm::csr::{CSR_SCAUSE, CSR_SEPC, CSR_SSCRATCH, CSR_SSTATUS, CSR_STVAL, SR_SPP},
    cpu::LocalContext,
    interrupt::TrapFrame,
};
use asm_macros::{restore_from_x6_to_x31, save_from_x6_to_x31};
use kdepends::memoffset::offset_of;

/// Riscv64中断处理入口
#[naked]
#[no_mangle]
#[repr(align(4))]
pub unsafe extern "C" fn handle_exception() -> ! {
    core::arch::naked_asm!(
        concat!("
        /*
	        * If coming from userspace, preserve the user thread pointer and load
	        * the kernel thread pointer.  If we came from the kernel, the scratch
	        * register will contain 0, and we should continue on the current TP.
        */
            
            csrrw tp, {csr_scratch}, tp
            bnez tp, _save_context

            /* 从内核态进入中断 */
            j {_restore_kernel_tpsp}
        "),
        csr_scratch = const CSR_SSCRATCH,
        _restore_kernel_tpsp = sym _restore_kernel_tpsp
    )
}

#[naked]
#[no_mangle]
unsafe extern "C" fn _restore_kernel_tpsp() -> ! {
    core::arch::naked_asm!(
        concat!("
            // 这次是从内核态进入中断
            // 从sscratch寄存器加载当前cpu的上下文
            csrr tp, {csr_scratch}

            // 把当前的sp寄存器的值保存到当前cpu的上下文的kernel_sp字段
            sd sp, {lc_off_kernel_sp}(tp)

            j {_save_context}
        "),
        csr_scratch = const CSR_SSCRATCH,
        lc_off_kernel_sp = const offset_of!(LocalContext, kernel_sp),
        _save_context = sym _save_context
    )
}

#[naked]
#[no_mangle]
unsafe extern "C" fn _save_context() -> ! {
    core::arch::naked_asm!(
        concat!("


            // 保存当前cpu的上下文

            // 保存用户sp
            sd sp, {lc_off_user_sp}(tp)
            // 加载内核sp
            ld sp, {lc_off_kernel_sp}(tp)

            addi sp, sp, -{trap_frame_size_on_stack}
            sd x1, {off_ra}(sp)
            sd x3, {off_gp}(sp)
            sd x5, {off_t0}(sp)
        ",
        save_from_x6_to_x31!(),
        "
        /*
	        * Disable user-mode memory access as it should only be set in the
	        * actual user copy routines.
	        *
	        * Disable the FPU/Vector to detect illegal usage of floating point
	        * or vector in kernel space.
        */

        li t0, {sr_sum_and_fsvs}
        
        ld s0, {lc_off_user_sp}(tp)
        csrrc s1, {csr_status}, t0
        csrr s2, {csr_epc}
        csrr s3, {csr_tval}
        csrr s4, {csr_cause}
        csrr s5, {csr_scratch}
        sd s0, {off_sp}(sp)
        sd s1, {off_status}(sp)
        sd s2, {off_epc}(sp)
        sd s3, {off_badaddr}(sp)
        sd s4, {off_cause}(sp)
        sd s5, {off_tp}(sp)

        /*
	    * Set the scratch register to 0, so that if a recursive exception
	    * occurs, the exception vector knows it came from the kernel
	    */

        csrw {csr_scratch}, x0

        /* Load the global pointer */
        // linux 加载了global pointer,但是我们暂时没有用到

        // .option push
        // .option norelax
        //     la gp, __global_pointer$
        // .option pop

        mv a0, sp
        la ra, ret_from_exception

        tail riscv64_do_irq
        "
    ),

        lc_off_user_sp = const offset_of!(LocalContext, user_sp),
        lc_off_kernel_sp = const offset_of!(LocalContext, kernel_sp),
        trap_frame_size_on_stack = const TrapFrame::SIZE_ON_STACK,
        off_ra = const offset_of!(TrapFrame, ra),
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
        off_sp = const offset_of!(TrapFrame, sp),
        off_status = const offset_of!(TrapFrame, status),
        off_badaddr = const offset_of!(TrapFrame, badaddr),
        off_cause = const offset_of!(TrapFrame, cause),
        off_tp = const offset_of!(TrapFrame, tp),
        off_epc = const offset_of!(TrapFrame, epc),
        sr_sum_and_fsvs = const (0), // 暂时在内核中不禁用FPU和Vector，以及不禁用用户内存访问
        // sr_sum_and_fsvs = const (SR_FS_VS | SR_SUM),
        csr_status = const CSR_SSTATUS,
        csr_epc = const CSR_SEPC,
        csr_tval = const CSR_STVAL,
        csr_cause = const CSR_SCAUSE,
        csr_scratch = const CSR_SSCRATCH
    )
}

#[naked]
#[no_mangle]
pub unsafe extern "C" fn ret_from_exception() -> ! {
    core::arch::naked_asm!(
        concat!("
            ld s0, {off_status}(sp)
            andi s0, s0, {sr_spp}
            
            bnez s0, 3f

            // Save unwound kernel stack pointer in thread_info
            addi s0, sp, {trap_frame_size_on_stack}
            sd s0, {lc_off_kernel_sp}(tp)

            /*
	        * Save TP into the scratch register , so we can find the kernel data
	        * structures again.
	        */
            csrw {csr_scratch}, tp
        3:

            ld a0, {off_status}(sp)

            ld a2, {off_epc}(sp)
            sc.d x0, a2, {off_epc}(sp)

            csrw {csr_status}, a0
            csrw {csr_epc}, a2

            ld x1, {off_ra}(sp)
            ld x3, {off_gp}(sp)
            ld x4, {off_tp}(sp)
            ld x5, {off_t0}(sp)

        ",
        restore_from_x6_to_x31!(),
        "
            ld x2, {off_sp}(sp)

            sret
        "
        ),
        off_status = const offset_of!(TrapFrame, status),
        sr_spp = const SR_SPP,
        trap_frame_size_on_stack = const TrapFrame::SIZE_ON_STACK,
        lc_off_kernel_sp = const offset_of!(LocalContext, kernel_sp),
        csr_scratch = const CSR_SSCRATCH,
        csr_status = const CSR_SSTATUS,
        csr_epc = const CSR_SEPC,
        off_ra = const offset_of!(TrapFrame, ra),
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
        off_sp = const offset_of!(TrapFrame, sp),
        off_tp = const offset_of!(TrapFrame, tp),
        off_epc = const offset_of!(TrapFrame, epc)
    )
}
