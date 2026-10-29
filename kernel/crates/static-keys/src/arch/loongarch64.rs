//! loongarch64 arch-sepcific implementations

use crate::{JumpEntry, JumpLabelType};

/// Length of jump instruction to be replaced
pub const ARCH_JUMP_INS_LENGTH: usize = 4;

const LOONGARCH64_INSN_NOP: u32 = 0x03400000;
const LOONGARCH64_INSN_B: u32 = 0x50000000;
/// New instruction generated according to jump label type and jump entry
#[inline(always)]
pub fn arch_jump_entry_instruction(
    jump_label_type: JumpLabelType,
    jump_entry: &JumpEntry,
) -> [u8; ARCH_JUMP_INS_LENGTH] {
    match jump_label_type {
        // 010100 [IMM]
        // opcode I26[15:0] I26[25:16]
        JumpLabelType::Jmp => {
            // Note that loongarch64 only supports relative address within +/-128MB.
            // In current implementation, this assumption is always hold.
            let relative_addr = (jump_entry.target_addr() - jump_entry.code_addr()) as u32;
            // MASK 25:16 = 0b_0000_0011_1111_1111_0000_0000_0000_0000 = 0x03FF0000
            // MASK 15:0  = 0b_0000_0000_0000_0000_1111_1111_1111_1111 = 0x0000FFFF
            let mut b = LOONGARCH64_INSN_B;
            let relative_addr = relative_addr >> 2;
            b |= ((relative_addr & 0x03FF0000) >> 16) | ((relative_addr & 0x0000FFFF) << 10);
            b.to_ne_bytes()
        }
        JumpLabelType::Nop => LOONGARCH64_INSN_NOP.to_ne_bytes(),
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
