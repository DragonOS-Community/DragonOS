//! AArch64 arch-sepcific implementations

use crate::{JumpEntry, JumpLabelType};

/// Length of jump instruction to be replaced
pub const ARCH_JUMP_INS_LENGTH: usize = 4;

/// New instruction generated according to jump label type and jump entry
#[inline(always)]
pub fn arch_jump_entry_instruction(
    jump_label_type: JumpLabelType,
    jump_entry: &JumpEntry,
) -> [u8; ARCH_JUMP_INS_LENGTH] {
    match jump_label_type {
        JumpLabelType::Jmp => {
            // Note that aarch64 only supports relative address within +/-128MB.
            // In current implementation, this assumption is always hold.
            let relative_addr = (jump_entry.target_addr() - jump_entry.code_addr()) as u32;
            let [a, b, c, d] = (relative_addr / 4).to_ne_bytes();
            [a, b, c, d | 0b00010100]
        }
        JumpLabelType::Nop => [0x1f, 0x20, 0x03, 0xd5],
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! arch_static_key_init_nop_asm_template {
    () => {
        ::core::concat!(
            r#"
            2:
                nop
            .pushsection "#,
            $crate::os_static_key_sec_name_attr!(),
            r#"
            .balign 8
            .quad 2b - .
            .quad {0} - .
            .quad {1} + {2} - .
            .popsection
            "#
        )
    };
}

// The `0x90,0x90,0x90` are three NOPs, which is to make sure the `jmp {0}` is at least 5 bytes long.
#[doc(hidden)]
#[macro_export]
macro_rules! arch_static_key_init_jmp_asm_template {
    () => {
        ::core::concat!(
            r#"
            2:
                b {0}
            .pushsection "#,
            $crate::os_static_key_sec_name_attr!(),
            r#"
            .balign 8
            .quad 2b - .
            .quad {0} - .
            .quad {1} + {2} - .
            .popsection
            "#
        )
    };
}
