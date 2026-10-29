//! riscv64 arch-sepcific implementations

use crate::{JumpEntry, JumpLabelType};

/// Length of jump instruction to be replaced
pub const ARCH_JUMP_INS_LENGTH: usize = 4;

const RISCV_INSN_JAL: u32 = 0x0000006f;

/// New instruction generated according to jump label type and jump entry
#[inline(always)]
pub fn arch_jump_entry_instruction(
    jump_label_type: JumpLabelType,
    jump_entry: &JumpEntry,
) -> [u8; ARCH_JUMP_INS_LENGTH] {
    match jump_label_type {
        // [offset          ] [rd] [opcode ]
        // [20|10:1|11|19:12] [rd] [1101111]
        JumpLabelType::Jmp => {
            // Note that riscv64 only supports relative address within +/-512k.
            // In current implementation, this assumption is always hold.
            let relative_addr = (jump_entry.target_addr() - jump_entry.code_addr()) as u32;
            let mut jal = RISCV_INSN_JAL;
            // MASK 19:12 = 0b_0000_0000_0000_1111_1111_0000_0000_0000 = 0x000FF000
            // MASK 11    = 0b_0000_0000_0000_0000_0000_1000_0000_0000 = 0x00000800
            // MASK 10:1  = 0b_0000_0000_0000_0000_0000_0111_1111_1110 = 0x000007FE
            // MASK 20    = 0b_0000_0000_0001_0000_0000_0000_0000_0000 = 0x00100000
            jal |= ((relative_addr & 0x000FF000) << 0)
                | ((relative_addr & 0x00000800) << 9)
                | ((relative_addr & 0x000007FE) << 20)
                | ((relative_addr & 0x00100000) << 11);
            jal.to_ne_bytes()
        }
        // RISCV_INSN_NOP 0x00000013
        JumpLabelType::Nop => [0x13, 0x00, 0x00, 0x00],
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! arch_static_key_init_nop_asm_template {
    () => {
        ::core::concat!(
            r#"
            .option push
            .option norelax
            .option norvc
            2:
                nop
            .option pop
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
            .option push
            .option norelax
            .option norvc
            2:
                jal zero, {0}
            .option pop
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
