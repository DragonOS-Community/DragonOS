///
/// The earliest entry point for the primary CPU.
///
/// 这些代码拷贝、修改自 polyhal (https://github.com/Byte-OS/polyhal.git)
use crate::arch::{cpu::current_cpu_id, init::boot::kernel_main};

const QEMU_DTB_PADDR: usize = 0x100000;

/// The earliest entry point for the primary CPU.
///
/// We can't use bl to jump to higher address, so we use jirl to jump to higher address.
#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!("
    
        ori         $t0, $zero, 0x1     # CSR_DMW1_PLV0
        lu52i.d     $t0, $t0, -2048     # UC, PLV0, 0x8000 xxxx xxxx xxxx
        csrwr       $t0, 0x180          # LOONGARCH_CSR_DMWIN0
        ori         $t0, $zero, 0x11    # CSR_DMW1_MAT | CSR_DMW1_PLV0
        lu52i.d     $t0, $t0, -1792     # CA, PLV0, 0x9000 xxxx xxxx xxxx
        csrwr       $t0, 0x181          # LOONGARCH_CSR_DMWIN1

        # Goto 1 if hart is not 0
        csrrd       $t1, 0x20       # read cpu from csr
        bnez        $t1, 1f

        # Enable PG 
        li.w		$t0, 0xb0		# PLV=0, IE=0, PG=1
        csrwr		$t0, 0x0        # LOONGARCH_CSR_CRMD
        li.w		$t0, 0x00		# PLV=0, PIE=0, PWE=0
        csrwr		$t0, 0x1        # LOONGARCH_CSR_PRMD
        li.w		$t0, 0x00		# FPE=0, SXE=0, ASXE=0, BTE=0
        csrwr		$t0, 0x2        # LOONGARCH_CSR_EUEN

    
        la.global   $sp, {boot_stack}
        li.d        $t0, {boot_stack_size}
        add.d       $sp, $sp, $t0       # setup boot stack
        csrrd       $a0, 0x20           # cpuid
        la.global   $t0, {entry}
        jirl        $zero,$t0,0
    1:
        li.w        $s0, {MBUF0}
        iocsrrd.d   $t0, $s0
        la.global   $t1, {sec_entry}
        bne         $t0, $t1, 1b
        jirl        $zero, $t1, 0
        ",
        boot_stack_size = const size_of_val(&crate::arch::process::BSP_IDLE_STACK_SPACE),
        boot_stack = sym crate::arch::process::BSP_IDLE_STACK_SPACE,
        MBUF0 = const loongArch64::consts::LOONGARCH_CSR_MAIL_BUF0,
        entry = sym rust_tmp_main,
        sec_entry = sym _start_secondary,
    )
}

/// The earliest entry point for the primary CPU.
///
/// We can't use bl to jump to higher address, so we use jirl to jump to higher address.
#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
pub(crate) unsafe extern "C" fn _start_secondary() -> ! {
    core::arch::naked_asm!(
        "
        ori          $t0, $zero, 0x1     # CSR_DMW1_PLV0
        lu52i.d      $t0, $t0, -2048     # UC, PLV0, 0x8000 xxxx xxxx xxxx
        csrwr        $t0, 0x180          # LOONGARCH_CSR_DMWIN0
        ori          $t0, $zero, 0x11    # CSR_DMW1_MAT | CSR_DMW1_PLV0
        lu52i.d      $t0, $t0, -1792     # CA, PLV0, 0x9000 xxxx xxxx xxxx
        csrwr        $t0, 0x181          # LOONGARCH_CSR_DMWIN1

        li.w         $t0, {MBUF1}
        iocsrrd.d    $sp, $t0

        csrrd        $a0, 0x20                  # cpuid
        la.global    $t0, {entry}

        jirl $zero,$t0,0
        ",
        MBUF1 = const loongArch64::consts::LOONGARCH_CSR_MAIL_BUF1,
        entry = sym _rust_secondary_main,
    )
}

/// Rust temporary entry point
///
/// This function will be called after assembly boot stage.
fn rust_tmp_main(hart_id: usize) {
    unsafe { kernel_main(hart_id, QEMU_DTB_PADDR) };
}

/// The entry point for the second core.
pub(crate) extern "C" fn _rust_secondary_main() {
    unsafe { kernel_main(current_cpu_id().data() as usize, QEMU_DTB_PADDR) }
}
