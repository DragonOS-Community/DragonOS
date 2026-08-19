//! x86 arch-sepcific implementations

use crate::{JumpEntry, JumpLabelType};

/// Length of jump instruction to be replaced
pub const ARCH_JUMP_INS_LENGTH: usize = 5;

/// New instruction generated according to jump label type and jump entry
#[inline(always)]
pub fn arch_jump_entry_instruction(
    jump_label_type: JumpLabelType,
    jump_entry: &JumpEntry,
) -> [u8; ARCH_JUMP_INS_LENGTH] {
    match jump_label_type {
        JumpLabelType::Jmp => {
            let relative_addr =
                (jump_entry.target_addr() - (jump_entry.code_addr() + ARCH_JUMP_INS_LENGTH)) as u32;
            let [a, b, c, d] = relative_addr.to_ne_bytes();
            [0xe9, a, b, c, d]
        }
        JumpLabelType::Nop => [0x3e, 0x8d, 0x74, 0x26, 0x00],
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! arch_static_key_init_nop_asm_template {
    () => {
        ::core::concat!(
            r#"
            2:
            .byte 0x3e,0x8d,0x74,0x26,0x00
            .pushsection "#,
            $crate::os_static_key_sec_name_attr!(),
            r#"
            .balign 4
            .long 2b - .
            .long {0} - .
            .long {1} + {2} - .
            .popsection
            "#
        )
    };
}

// Here we do not use `jmp {0}` because it may be compiled into a 3-byte jmp instead of 5 byte.
// See https://stackoverflow.com/q/74771372/10005095
#[doc(hidden)]
#[macro_export]
macro_rules! arch_static_key_init_jmp_asm_template {
    () => {
        ::core::concat!(
            r#"
            2:
            .byte 0xe9
            .long ({0} - 4) - .
            .pushsection "#,
            $crate::os_static_key_sec_name_attr!(),
            r#"
            .balign 4
            .long 2b - .
            .long {0} - .
            .long {1} + {2} - .
            .popsection
            "#
        )
    };
}
