use asm_macros::*;
use kdepends::memoffset::offset_of;

use crate::arch::{asm::*, interrupt::TrapFrame};

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/genex.S#55
macro_rules! build_prep_badv_ {
    () => {
        concat!(
            "
        csrrd $t0, {loongarch_csr_badv}
        st.d $t0, $sp, {off_csr_badvaddr}
        "
        )
    };
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/genex.S#60
macro_rules! build_prep_fcsr_ {
    () => {
        concat!(
            "
        movfcsr2gr	$a1, $fcsr0
        "
        )
    };
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/genex.S#64
macro_rules! build_prep_none_ {
    () => {
        ""
    };
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/genex.S#67
macro_rules! build_handler {
    ($exception:expr, $handler:expr, $prep:ident) => {
        paste::paste! {
            /// handle exception的实现请参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/genex.S#69
            #[naked]
            #[no_mangle]
            #[repr(align(8))]
            pub unsafe extern "C" fn [<handle_ $exception _>]() -> ! {
                core::arch::naked_asm!(concat!(
                    backup_t0t1!(),
                    save_all!(),
                    [<build_prep_ $prep _>]!(),
                    "
                    move $a0, $sp
                    la.abs $t0, ", stringify!([<do_ $handler _>]),
                    "
                    jirl $ra, $t0, 0
                    ",
                    restore_all_and_ret!(),
                    "
                    // 以下是为了编译器不报错说变量未使用，才保留的代码
                        /* {off_csr_badvaddr} */
                        /* {loongarch_csr_badv} */
                    "
                ),
                exception_ks0 = const EXCEPTION_KS0,
                exception_ks1 = const EXCEPTION_KS1,
                loongarch_csr_prmd = const LOONGARCH_CSR_PRMD,
                loongarch_csr_crmd = const LOONGARCH_CSR_CRMD,
                loongarch_csr_euen = const LOONGARCH_CSR_EUEN,
                loongarch_csr_ecfg = const LOONGARCH_CSR_ECFG,
                loongarch_csr_estat = const LOONGARCH_CSR_ESTAT,
                loongarch_csr_era = const LOONGARCH_CSR_ERA,
                loongarch_csr_badv = const LOONGARCH_CSR_BADV,
                percpu_base_ks = const PERCPU_BASE_KS,
                thread_mask = const _THREAD_MASK,
                pt_size = const core::mem::size_of::<TrapFrame>(),
                off_r0 = const offset_of!(TrapFrame, r0),
                off_ra = const offset_of!(TrapFrame, ra),
                off_tp = const offset_of!(TrapFrame, tp),
                off_usp = const offset_of!(TrapFrame, usp),
                off_a0 = const offset_of!(TrapFrame, a0),
                off_a1 = const offset_of!(TrapFrame, a1),
                off_a2 = const offset_of!(TrapFrame, a2),
                off_a3 = const offset_of!(TrapFrame, a3),
                off_a4 = const offset_of!(TrapFrame, a4),
                off_a5 = const offset_of!(TrapFrame, a5),
                off_a6 = const offset_of!(TrapFrame, a6),
                off_a7 = const offset_of!(TrapFrame, a7),
                off_t0 = const offset_of!(TrapFrame, t0),
                off_t1 = const offset_of!(TrapFrame, t1),
                off_t2 = const offset_of!(TrapFrame, t2),
                off_t3 = const offset_of!(TrapFrame, t3),
                off_t4 = const offset_of!(TrapFrame, t4),
                off_t5 = const offset_of!(TrapFrame, t5),
                off_t6 = const offset_of!(TrapFrame, t6),
                off_t7 = const offset_of!(TrapFrame, t7),
                off_t8 = const offset_of!(TrapFrame, t8),
                off_r21 = const offset_of!(TrapFrame, r21),
                off_fp = const offset_of!(TrapFrame, fp),
                off_s0 = const offset_of!(TrapFrame, s0),
                off_s1 = const offset_of!(TrapFrame, s1),
                off_s2 = const offset_of!(TrapFrame, s2),
                off_s3 = const offset_of!(TrapFrame, s3),
                off_s4 = const offset_of!(TrapFrame, s4),
                off_s5 = const offset_of!(TrapFrame, s5),
                off_s6 = const offset_of!(TrapFrame, s6),
                off_s7 = const offset_of!(TrapFrame, s7),
                off_s8 = const offset_of!(TrapFrame, s8),
                // off_orig_a0 = const offset_of!(TrapFrame, orig_a0),
                off_csr_era = const offset_of!(TrapFrame, csr_era),
                off_csr_badvaddr = const offset_of!(TrapFrame, csr_badvaddr),
                off_csr_crmd = const offset_of!(TrapFrame, csr_crmd),
                off_csr_prmd = const offset_of!(TrapFrame, csr_prmd),
                off_csr_euen = const offset_of!(TrapFrame, csr_euen),
                off_csr_ecfg = const offset_of!(TrapFrame, csr_ecfg),
                off_csr_estat = const offset_of!(TrapFrame, csr_estat),
                )
            }

        }
    };
}

build_handler!(ade, ade, badv);
build_handler!(ale, ale, badv);
build_handler!(bce, bce, none);
build_handler!(bp, bp, none);
build_handler!(fpe, fpe, fcsr);
build_handler!(fpu, fpu, none);
build_handler!(lsx, lsx, none);
build_handler!(lasx, lasx, none);
build_handler!(lbt, lbt, none);
build_handler!(ri, ri, none);
build_handler!(watch, watch, none);
build_handler!(reserved, reserved, none); /* others */
