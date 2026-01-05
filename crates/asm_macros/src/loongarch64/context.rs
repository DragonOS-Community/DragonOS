/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#48
#[macro_export]
macro_rules! backup_t0t1 {
    () => {
        concat!(
            "
            csrwr $t0, {exception_ks0}
            csrwr $t1, {exception_ks1}
        "
        )
    };
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#53
#[macro_export]
macro_rules! reload_t0t1 {
    () => {
        concat!(
            "
            csrrd $t0, {exception_ks0}
            csrrd $t1, {exception_ks1}
        "
        )
    };
}

/// get_saved_sp returns the SP for the current CPU by looking in the
/// kernelsp array for it. It stores the current sp in t0 and loads the
/// new value in sp.
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h?r=&mo=2487&fi=119#88
#[macro_export]
macro_rules! get_saved_sp {
    () => {
        concat!(
            "
            la.abs $t1, KERNEL_SP
            csrrd $t0, {percpu_base_ks}
            add.d $t1, $t1, $t0

            move $t0, $sp
            ld.d $sp, $t1, 0
        "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h?r=&mo=2487&fi=119#101
#[macro_export]
macro_rules! set_saved_sp {
    ($stackp:tt, $temp:tt) => {
        concat!(
            "la.pcrel ",
            stringify!($temp),
            ", KERNEL_SP\n",
            "add.d ",
            stringify!($temp),
            ", ",
            stringify!($temp),
            ", $r21\n",
            "st.d ",
            stringify!($stackp),
            ", ",
            stringify!($temp),
            ", 0\n"
        )
    };
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#109
#[macro_export]
macro_rules! save_some {
    () => {
        concat!(
            "
            csrrd $t1, {loongarch_csr_prmd}
            andi	$t1, $t1, 0x3
            move $t0, $sp
            beqz $t1, 3f

        ",
            get_saved_sp!(),
        "
        3:
            addi.d $sp, $sp, -{pt_size}
        

            st.d $t0, $sp, {off_usp} // 等价于 cfi_st	t0, PT_R3, docfi=0

            st.d $zero, $sp, {off_r0}
            csrrd $t0, {loongarch_csr_prmd}
            st.d $t0, $sp, {off_csr_prmd}
            csrrd $t0, {loongarch_csr_crmd}
            st.d $t0, $sp, {off_csr_crmd}
            csrrd $t0, {loongarch_csr_euen}
            st.d $t0, $sp, {off_csr_euen}
            csrrd $t0, {loongarch_csr_ecfg}
            st.d $t0, $sp, {off_csr_ecfg}
            csrrd $t0, {loongarch_csr_estat}
            st.d $t0, $sp, {off_csr_estat}

            st.d $ra, $sp, {off_ra}
            st.d $a0, $sp, {off_a0}
            st.d $a1, $sp, {off_a1}
            st.d $a2, $sp, {off_a2}
            st.d $a3, $sp, {off_a3}
            st.d $a4, $sp, {off_a4}
            st.d $a5, $sp, {off_a5}
            st.d $a6, $sp, {off_a6}
            st.d $a7, $sp, {off_a7}

            csrrd $ra, {loongarch_csr_era}
            st.d $ra, $sp, {off_csr_era}

            st.d $tp, $sp, {off_tp}
            st.d $fp, $sp, {off_fp}

            /* Set thread_info if we're coming from user mode */
            csrrd $t0, {loongarch_csr_prmd}
            andi $t0, $t0, 0x3
            beqz $t0, 9f

            li.d $tp, ~{thread_mask}
            and $tp, $tp, $sp
            st.d $r21, $sp, {off_r21}
            csrrd $r21, {percpu_base_ks}
            9:
        "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#197
#[macro_export]
macro_rules! restore_some {
    () => {
        concat!(
            "
            ld.d $a0, $sp, {off_csr_prmd}
            // extract pplv bit
            andi $a0, $a0, 0x3
            beqz $a0, 4f
            ld.d $r21, $sp, {off_r21}

            4:
            ld.d $a0, $sp, {off_csr_era}
            csrwr $a0, {loongarch_csr_era}
            ld.d $a0, $sp, {off_csr_prmd}
            csrwr $a0, {loongarch_csr_prmd}
            ld.d $ra, $sp, {off_ra}
            ld.d $a0, $sp, {off_a0}
            ld.d $a1, $sp, {off_a1}
            ld.d $a2, $sp, {off_a2}
            ld.d $a3, $sp, {off_a3}
            ld.d $a4, $sp, {off_a4}
            ld.d $a5, $sp, {off_a5}
            ld.d $a6, $sp, {off_a6}
            ld.d $a7, $sp, {off_a7}
            ld.d $tp, $sp, {off_tp}
            ld.d $fp, $sp, {off_fp}
            "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#58
#[macro_export]
macro_rules! save_temp {
    () => {
        concat!(
            reload_t0t1!(),
            "
            st.d $t0, $sp, {off_t0}
            st.d $t1, $sp, {off_t1}
            st.d $t2, $sp, {off_t2}
            st.d $t3, $sp, {off_t3}
            st.d $t4, $sp, {off_t4}
            st.d $t5, $sp, {off_t5}
            st.d $t6, $sp, {off_t6}
            st.d $t7, $sp, {off_t7}
            st.d $t8, $sp, {off_t8}
            "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#173
#[macro_export]
macro_rules! restore_temp {
    () => {
        concat!(
            reload_t0t1!(),
            "
            ld.d $t0, $sp, {off_t0}
            ld.d $t1, $sp, {off_t1}
            ld.d $t2, $sp, {off_t2}
            ld.d $t3, $sp, {off_t3}
            ld.d $t4, $sp, {off_t4}
            ld.d $t5, $sp, {off_t5}
            ld.d $t6, $sp, {off_t6}
            ld.d $t7, $sp, {off_t7}
            ld.d $t8, $sp, {off_t8}
            "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#71
#[macro_export]
macro_rules! save_static {
    () => {
        concat!(
            "
            st.d $s0, $sp, {off_s0}
            st.d $s1, $sp, {off_s1}
            st.d $s2, $sp, {off_s2}
            st.d $s3, $sp, {off_s3}
            st.d $s4, $sp, {off_s4}
            st.d $s5, $sp, {off_s5}
            st.d $s6, $sp, {off_s6}
            st.d $s7, $sp, {off_s7}
            st.d $s8, $sp, {off_s8}
            "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#185
#[macro_export]
macro_rules! restore_static {
    () => {
        concat!(
            "
            ld.d $s0, $sp, {off_s0}
            ld.d $s1, $sp, {off_s1}
            ld.d $s2, $sp, {off_s2}
            ld.d $s3, $sp, {off_s3}
            ld.d $s4, $sp, {off_s4}
            ld.d $s5, $sp, {off_s5}
            ld.d $s6, $sp, {off_s6}
            ld.d $s7, $sp, {off_s7}
            ld.d $s8, $sp, {off_s8}
            "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#167
#[macro_export]
macro_rules! save_all {
    () => {
        concat!(save_some!(), "\n", save_temp!(), "\n", save_static!(),)
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#220
#[macro_export]
macro_rules! restore_sp_and_ret {
    () => {
        concat!(
            "
            ld.d $sp, $sp, {off_usp}
            ertn
            "
        )
    };
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/asm/stackframe.h#225
#[macro_export]
macro_rules! restore_all_and_ret {
    () => {
        concat!(
            restore_static!(),
            restore_temp!(),
            restore_some!(),
            restore_sp_and_ret!()
        )
    };
}
