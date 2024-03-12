use crate::arch::{
    asm::csr::{
        CSR_SCAUSE, CSR_SEPC, CSR_SSCRATCH, CSR_SSTATUS, CSR_STVAL, SR_FS_VS, SR_SPP, SR_SUM,
    },
    cpu::LocalContext,
    interrupt::TrapFrame,
};
use core::arch::asm;
use kdepends::memoffset::offset_of;

/// 保存x6-x31寄存器
macro_rules! save_from_x6_to_x31 {
    () => {
        concat!(
            "
            sd x6, {off_t1}(sp)
            sd x7, {off_t2}(sp)
            sd x8, {off_s0}(sp)
            sd x9, {off_s1}(sp)
            sd x10, {off_a0}(sp)
            sd x11, {off_a1}(sp)
            sd x12, {off_a2}(sp)
            sd x13, {off_a3}(sp)
            sd x14, {off_a4}(sp)
            sd x15, {off_a5}(sp)
            sd x16, {off_a6}(sp)
            sd x17, {off_a7}(sp)
            sd x18, {off_s2}(sp)
            sd x19, {off_s3}(sp)
            sd x20, {off_s4}(sp)
            sd x21, {off_s5}(sp)
            sd x22, {off_s6}(sp)
            sd x23, {off_s7}(sp)
            sd x24, {off_s8}(sp)
            sd x25, {off_s9}(sp)
            sd x26, {off_s10}(sp)
            sd x27, {off_s11}(sp)
            sd x28, {off_t3}(sp)
            sd x29, {off_t4}(sp)
            sd x30, {off_t5}(sp)
            sd x31, {off_t6}(sp)

        "
        )
    };
}

macro_rules! restore_from_x6_to_x31 {
    () => {
        concat!("
        
            ld x6, {off_t1}(sp)
            ld x7, {off_t2}(sp)
            ld x8, {off_s0}(sp)
            ld x9, {off_s1}(sp)
            ld x10, {off_a0}(sp)
            ld x11, {off_a1}(sp)
            ld x12, {off_a2}(sp)
            ld x13, {off_a3}(sp)
            ld x14, {off_a4}(sp)
            ld x15, {off_a5}(sp)
            ld x16, {off_a6}(sp)
            ld x17, {off_a7}(sp)
            ld x18, {off_s2}(sp)
            ld x19, {off_s3}(sp)
            ld x20, {off_s4}(sp)
            ld x21, {off_s5}(sp)
            ld x22, {off_s6}(sp)
            ld x23, {off_s7}(sp)
            ld x24, {off_s8}(sp)
            ld x25, {off_s9}(sp)
            ld x26, {off_s10}(sp)
            ld x27, {off_s11}(sp)
            ld x28, {off_t3}(sp)
            ld x29, {off_t4}(sp)
            ld x30, {off_t5}(sp)
            ld x31, {off_t6}(sp)
        ")
    };
}

/// Riscv64中断处理入口
#[naked]
#[no_mangle]
#[repr(align(4))]
pub unsafe extern "C" fn handle_exception() -> ! {
    asm!(
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
        _restore_kernel_tpsp = sym _restore_kernel_tpsp,
        options(noreturn),
    )
}

#[naked]
#[no_mangle]
unsafe extern "C" fn _restore_kernel_tpsp() -> ! {
    asm!(
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
        _save_context = sym _save_context,

        options(noreturn),
    )
}

#[naked]
#[no_mangle]
unsafe extern "C" fn _save_context() -> ! {
    asm!(
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
        sr_sum_and_fsvs = const (SR_FS_VS | SR_SUM),
        csr_status = const CSR_SSTATUS,
        csr_epc = const CSR_SEPC,
        csr_tval = const CSR_STVAL,
        csr_cause = const CSR_SCAUSE,
        csr_scratch = const CSR_SSCRATCH,
        options(noreturn),
    )
}

#[naked]
#[no_mangle]
unsafe extern "C" fn ret_from_exception() -> ! {
    asm!(
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
        off_epc = const offset_of!(TrapFrame, epc),

        options(noreturn),
    )
}
