// SPDX-License-Identifier: (Apache-2.0 OR MIT)

#![allow(clippy::unreadable_literal)]
#![cfg(feature = "cranelift")]

extern crate rbpf;
mod common;

use rbpf::{assembler::assemble, helpers};

use crate::common::{TCP_SACK_ASM, TCP_SACK_MATCH, TCP_SACK_NOMATCH};

macro_rules! test_cranelift {
    ($name:ident, $prog:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let prog = assemble($prog).unwrap();
            let mut vm = rbpf::EbpfVmNoData::new(Some(&prog)).unwrap();
            vm.cranelift_compile().unwrap();
            assert_eq!(vm.execute_program_cranelift().unwrap(), $expected);
        }
    };
    ($name:ident, $prog:expr, $mem:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let prog = assemble($prog).unwrap();
            let mem = &mut $mem;
            let mut vm = rbpf::EbpfVmRaw::new(Some(&prog)).unwrap();
            vm.cranelift_compile().unwrap();
            assert_eq!(vm.execute_program_cranelift(mem).unwrap(), $expected);
        }
    };
}

test_cranelift!(
    test_cranelift_add,
    "
    mov32 r0, 0
    mov32 r1, 2
    add32 r0, 1
    add32 r0, r1
    exit
    ",
    0x3
);

test_cranelift!(
    test_cranelift_alu64_arith,
    "
    mov r0, 0
    mov r1, 1
    mov r2, 2
    mov r3, 3
    mov r4, 4
    mov r5, 5
    mov r6, 6
    mov r7, 7
    mov r8, 8
    mov r9, 9
    add r0, 23
    add r0, r7
    sub r0, 13
    sub r0, r1
    mul r0, 7
    mul r0, r3
    div r0, 2
    div r0, r4
    exit
    ",
    0x2a
);

test_cranelift!(
    test_cranelift_alu64_bit,
    "
    mov r0, 0
    mov r1, 1
    mov r2, 2
    mov r3, 3
    mov r4, 4
    mov r5, 5
    mov r6, 6
    mov r7, 7
    mov r8, 8
    or r0, r5
    or r0, 0xa0
    and r0, 0xa3
    mov r9, 0x91
    and r0, r9
    lsh r0, 32
    lsh r0, 22
    lsh r0, r8
    rsh r0, 32
    rsh r0, 19
    rsh r0, r7
    xor r0, 0x03
    xor r0, r2
    exit
    ",
    0x11
);

test_cranelift!(
    test_cranelift_alu_arith,
    "
    mov32 r0, 0
    mov32 r1, 1
    mov32 r2, 2
    mov32 r3, 3
    mov32 r4, 4
    mov32 r5, 5
    mov32 r6, 6
    mov32 r7, 7
    mov32 r8, 8
    mov32 r9, 9
    add32 r0, 23
    add32 r0, r7
    sub32 r0, 13
    sub32 r0, r1
    mul32 r0, 7
    mul32 r0, r3
    div32 r0, 2
    div32 r0, r4
    exit
    ",
    0x2a
);

test_cranelift!(
    test_cranelift_alu_bit,
    "
    mov32 r0, 0
    mov32 r1, 1
    mov32 r2, 2
    mov32 r3, 3
    mov32 r4, 4
    mov32 r5, 5
    mov32 r6, 6
    mov32 r7, 7
    mov32 r8, 8
    or32 r0, r5
    or32 r0, 0xa0
    and32 r0, 0xa3
    mov32 r9, 0x91
    and32 r0, r9
    lsh32 r0, 22
    lsh32 r0, r8
    rsh32 r0, 19
    rsh32 r0, r7
    xor32 r0, 0x03
    xor32 r0, r2
    exit
    ",
    0x11
);

test_cranelift!(
    test_cranelift_arsh32_high_shift,
    "
    mov r0, 8
    lddw r1, 0x100000001
    arsh32 r0, r1
    exit
    ",
    0x4
);

test_cranelift!(
    test_cranelift_arsh,
    "
    mov32 r0, 0xf8
    lsh32 r0, 28
    arsh32 r0, 16
    exit
    ",
    0xffff8000
);

test_cranelift!(
    test_cranelift_arsh64,
    "
    mov32 r0, 1
    lsh r0, 63
    arsh r0, 55
    mov32 r1, 5
    arsh r0, r1
    exit
    ",
    0xfffffffffffffff8
);

test_cranelift!(
    test_cranelift_arsh_reg,
    "
    mov32 r0, 0xf8
    mov32 r1, 16
    lsh32 r0, 28
    arsh32 r0, r1
    exit
    ",
    0xffff8000
);

test_cranelift!(
    test_cranelift_be16,
    "
    ldxh r0, [r1]
    be16 r0
    exit
    ",
    [0x11, 0x22],
    0x1122
);

test_cranelift!(
    test_cranelift_be16_high,
    "
    ldxdw r0, [r1]
    be16 r0
    exit
    ",
    [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
    0x1122
);

test_cranelift!(
    test_cranelift_be32,
    "
    ldxw r0, [r1]
    be32 r0
    exit
    ",
    [0x11, 0x22, 0x33, 0x44],
    0x11223344
);

test_cranelift!(
    test_cranelift_be32_high,
    "
    ldxdw r0, [r1]
    be32 r0
    exit
    ",
    [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
    0x11223344
);

test_cranelift!(
    test_cranelift_be64,
    "
    ldxdw r0, [r1]
    be64 r0
    exit
    ",
    [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
    0x1122334455667788
);

#[test]
fn test_cranelift_call() {
    let prog = assemble(
        "
        mov r1, 1
        mov r2, 2
        mov r3, 3
        mov r4, 4
        mov r5, 5
        call 0
        exit",
    )
    .unwrap();

    let mut vm = rbpf::EbpfVmNoData::new(Some(&prog)).unwrap();
    vm.register_helper(0, helpers::gather_bytes).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift().unwrap(), 0x0102030405);
}

#[test]
#[should_panic(expected = "[CRANELIFT] Error: unknown helper function (id: 0x3f)")]
fn test_cranelift_err_call_unreg() {
    let prog = assemble(
        "
         mov r1, 1
         mov r2, 2
         mov r3, 3
         mov r4, 4
         mov r5, 5
         call 63
         exit
    ",
    )
    .unwrap();
    let mut vm = rbpf::EbpfVmNoData::new(Some(&prog)).unwrap();
    vm.cranelift_compile().unwrap();
}

#[test]
fn test_cranelift_call_memfrob() {
    let prog = assemble(
        "
        mov r6, r1
        add r1, 2
        mov r2, 4
        call 1
        ldxdw r0, [r6]
        be64 r0
        exit",
    )
    .unwrap();

    let mut vm = rbpf::EbpfVmRaw::new(Some(&prog)).unwrap();
    vm.register_helper(1, helpers::memfrob).unwrap();
    let mem = &mut [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    vm.cranelift_compile().unwrap();
    assert_eq!(
        vm.execute_program_cranelift(mem).unwrap(),
        0x102292e2f2c0708
    );
}

test_cranelift!(
    test_cranelift_div32_high_divisor,
    "
    mov r0, 12
    lddw r1, 0x100000004
    div32 r0, r1
    exit
    ",
    0x3
);

test_cranelift!(
    test_cranelift_div32_imm,
    "
    lddw r0, 0x10000000c
    div32 r0, 4
    exit
    ",
    0x3
);

test_cranelift!(
    test_cranelift_div32_reg,
    "
    lddw r0, 0x10000000c
    mov r1, 4
    div32 r0, r1
    exit
    ",
    0x3
);

test_cranelift!(
    test_cranelift_div64_imm,
    "
    mov r0, 0xc
    lsh r0, 32
    div r0, 4
    exit
    ",
    0x300000000
);

test_cranelift!(
    test_cranelift_div64_reg,
    "
    mov r0, 0xc
    lsh r0, 32
    mov r1, 4
    div r0, r1
    exit
    ",
    0x300000000
);

test_cranelift!(
    test_cranelift_early_exit,
    "
    mov r0, 3
    exit
    mov r0, 4
    exit
    ",
    0x3
);

test_cranelift!(
    test_cranelift_div64_by_zero_imm,
    "
    mov32 r0, 1
    div r0, 0
    exit
    ",
    0x0
);

test_cranelift!(
    test_cranelift_div_by_zero_imm,
    "
    mov32 r0, 1
    div32 r0, 0
    exit
    ",
    0x0
);

test_cranelift!(
    test_cranelift_mod64_by_zero_imm,
    "
    mov32 r0, 1
    mod r0, 0
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_mod_by_zero_imm,
    "
    mov32 r0, 1
    mod32 r0, 0
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_div64_by_zero_reg,
    "
    mov32 r0, 1
    mov32 r1, 0
    div r0, r1
    exit
    ",
    0x0
);

test_cranelift!(
    test_cranelift_div_by_zero_reg,
    "
    mov32 r0, 1
    mov32 r1, 0
    div32 r0, r1
    exit
    ",
    0x0
);

test_cranelift!(
    test_cranelift_mod64_by_zero_reg,
    "
    mov32 r0, 1
    mov32 r1, 0
    mod r0, r1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_mod_by_zero_reg,
    "
    mov32 r0, 1
    mov32 r1, 0
    mod32 r0, r1
    exit
    ",
    0x1
);

#[test]
// #[should_panic(expected = "Error: out of bounds memory store (insn #1)")]
#[ignore = "We have stack OOB checks, but we don't yet catch the trap code and convert it into a panic"]
fn test_cranelift_err_stack_out_of_bound() {
    let prog = [
        0x72, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let mut vm = rbpf::EbpfVmNoData::new(Some(&prog)).unwrap();
    vm.cranelift_compile().unwrap();
    vm.execute_program_cranelift().unwrap();
}

test_cranelift!(
    test_cranelift_exit,
    "
    mov r0, 0
    exit
    ",
    0x0
);

test_cranelift!(
    test_cranelift_ja,
    "
    mov r0, 1
    ja +1
    mov r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jeq_imm,
    "
    mov32 r0, 0
    mov32 r1, 0xa
    jeq r1, 0xb, +4
    mov32 r0, 1
    mov32 r1, 0xb
    jeq r1, 0xb, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jeq_reg,
    "
    mov32 r0, 0
    mov32 r1, 0xa
    mov32 r2, 0xb
    jeq r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0xb
    jeq r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jge_imm,
    "
    mov32 r0, 0
    mov32 r1, 0xa
    jge r1, 0xb, +4
    mov32 r0, 1
    mov32 r1, 0xc
    jge r1, 0xb, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jle_imm,
    "
    mov32 r0, 0
    mov32 r1, 5
    jle r1, 4, +1
    jle r1, 6, +1
    exit
    jle r1, 5, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jle_reg,
    "
    mov r0, 0
    mov r1, 5
    mov r2, 4
    mov r3, 6
    jle r1, r2, +2
    jle r1, r1, +1
    exit
    jle r1, r3, +1
    exit
    mov r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jgt_imm,
    "
    mov32 r0, 0
    mov32 r1, 5
    jgt r1, 6, +2
    jgt r1, 5, +1
    jgt r1, 4, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jgt_reg,
    "
    mov r0, 0
    mov r1, 5
    mov r2, 6
    mov r3, 4
    jgt r1, r2, +2
    jgt r1, r1, +1
    jgt r1, r3, +1
    exit
    mov r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jlt_imm,
    "
    mov32 r0, 0
    mov32 r1, 5
    jlt r1, 4, +2
    jlt r1, 5, +1
    jlt r1, 6, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jlt_reg,
    "
    mov r0, 0
    mov r1, 5
    mov r2, 4
    mov r3, 6
    jlt r1, r2, +2
    jlt r1, r1, +1
    jlt r1, r3, +1
    exit
    mov r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jit_bounce,
    "
    mov r0, 1
    mov r6, r0
    mov r7, r6
    mov r8, r7
    mov r9, r8
    mov r0, r9
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jne_reg,
    "
    mov32 r0, 0
    mov32 r1, 0xb
    mov32 r2, 0xb
    jne r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0xa
    jne r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jset_imm,
    "
    mov32 r0, 0
    mov32 r1, 0x7
    jset r1, 0x8, +4
    mov32 r0, 1
    mov32 r1, 0x9
    jset r1, 0x8, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jset_reg,
    "
    mov32 r0, 0
    mov32 r1, 0x7
    mov32 r2, 0x8
    jset r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0x9
    jset r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsge_imm,
    "
    mov32 r0, 0
    mov r1, -2
    jsge r1, -1, +5
    jsge r1, 0, +4
    mov32 r0, 1
    mov r1, -1
    jsge r1, -1, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsge_reg,
    "
    mov32 r0, 0
    mov r1, -2
    mov r2, -1
    mov32 r3, 0
    jsge r1, r2, +5
    jsge r1, r3, +4
    mov32 r0, 1
    mov r1, r2
    jsge r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsle_imm,
    "
    mov32 r0, 0
    mov r1, -2
    jsle r1, -3, +1
    jsle r1, -1, +1
    exit
    mov32 r0, 1
    jsle r1, -2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsle_reg,
    "
    mov32 r0, 0
    mov r1, -1
    mov r2, -2
    mov32 r3, 0
    jsle r1, r2, +1
    jsle r1, r3, +1
    exit
    mov32 r0, 1
    mov r1, r2
    jsle r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsgt_imm,
    "
    mov32 r0, 0
    mov r1, -2
    jsgt r1, -1, +4
    mov32 r0, 1
    mov32 r1, 0
    jsgt r1, -1, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsgt_reg,
    "
    mov32 r0, 0
    mov r1, -2
    mov r2, -1
    jsgt r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0
    jsgt r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jslt_imm,
    "
    mov32 r0, 0
    mov r1, -2
    jslt r1, -3, +2
    jslt r1, -2, +1
    jslt r1, -1, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jslt_reg,
    "
    mov32 r0, 0
    mov r1, -2
    mov r2, -3
    mov r3, -1
    jslt r1, r1, +2
    jslt r1, r2, +1
    jslt r1, r3, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jeq32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0x0
    mov32 r1, 0xa
    jeq32 r1, 0xb, +5
    mov32 r0, 1
    mov r1, 0xb
    or r1, r9
    jeq32 r1, 0xb, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jeq32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0xa
    mov32 r2, 0xb
    jeq32 r1, r2, +5
    mov32 r0, 1
    mov32 r1, 0xb
    or r1, r9
    jeq32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jge32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0xa
    jge32 r1, 0xb, +5
    mov32 r0, 1
    or r1, r9
    mov32 r1, 0xc
    jge32 r1, 0xb, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jge32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0xa
    mov32 r2, 0xb
    jge32 r1, r2, +5
    mov32 r0, 1
    or r1, r9
    mov32 r1, 0xc
    jge32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jgt32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 5
    or r1, r9
    jgt32 r1, 6, +4
    jgt32 r1, 5, +3
    jgt32 r1, 4, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jgt32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov r0, 0
    mov r1, 5
    mov32 r1, 5
    or r1, r9
    mov r2, 6
    mov r3, 4
    jgt32 r1, r2, +4
    jgt32 r1, r1, +3
    jgt32 r1, r3, +1
    exit
    mov r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jle32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 5
    or r1, r9
    jle32 r1, 4, +5
    jle32 r1, 6, +1
    exit
    jle32 r1, 5, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jle32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov r0, 0
    mov r1, 5
    mov r2, 4
    mov r3, 6
    or r1, r9
    jle32 r1, r2, +5
    jle32 r1, r1, +1
    exit
    jle32 r1, r3, +1
    exit
    mov r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jlt32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 5
    or r1, r9
    jlt32 r1, 4, +4
    jlt32 r1, 5, +3
    jlt32 r1, 6, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jlt32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov r0, 0
    mov r1, 5
    mov r2, 4
    mov r3, 6
    or r1, r9
    jlt32 r1, r2, +4
    jlt32 r1, r1, +3
    jlt32 r1, r3, +1
    exit
    mov r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jne32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0xb
    or r1, r9
    jne32 r1, 0xb, +4
    mov32 r0, 1
    mov32 r1, 0xa
    or r1, r9
    jne32 r1, 0xb, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jne32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0xb
    or r1, r9
    mov32 r2, 0xb
    jne32 r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0xa
    or r1, r9
    jne32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jset32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0x7
    or r1, r9
    jset32 r1, 0x8, +4
    mov32 r0, 1
    mov32 r1, 0x9
    jset32 r1, 0x8, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jset32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, 0x7
    or r1, r9
    mov32 r2, 0x8
    jset32 r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0x9
    jset32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsge32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    jsge32 r1, -1, +5
    jsge32 r1, 0, +4
    mov32 r0, 1
    mov r1, -1
    jsge32 r1, -1, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsge32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    mov r2, -1
    mov32 r3, 0
    jsge32 r1, r2, +5
    jsge32 r1, r3, +4
    mov32 r0, 1
    mov r1, r2
    jsge32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsgt32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    jsgt32 r1, -1, +4
    mov32 r0, 1
    mov32 r1, 0
    jsgt32 r1, -1, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsgt32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    mov r2, -1
    jsgt32 r1, r2, +4
    mov32 r0, 1
    mov32 r1, 0
    jsgt32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsle32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    jsle32 r1, -3, +5
    jsle32 r1, -1, +1
    exit
    mov32 r0, 1
    jsle32 r1, -2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jsle32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    mov r2, -3
    mov32 r3, 0
    jsle32 r1, r2, +6
    jsle32 r1, r3, +1
    exit
    mov32 r0, 1
    mov r1, r2
    jsle32 r1, r2, +1
    mov32 r0, 2
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jslt32_imm,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    jslt32 r1, -3, +4
    jslt32 r1, -2, +3
    jslt32 r1, -1, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_jslt32_reg,
    "
    mov r9, 1
    lsh r9, 32
    mov32 r0, 0
    mov32 r1, -2
    or r1, r9
    mov r2, -3
    mov r3, -1
    jslt32 r1, r1, +4
    jslt32 r1, r2, +3
    jslt32 r1, r3, +1
    exit
    mov32 r0, 1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_lddw,
    "
    lddw r0, 0x1122334455667788
    exit
    ",
    0x1122334455667788
);

test_cranelift!(
    test_cranelift_lddw2,
    "
    lddw r0, 0x0000000080000000
    exit
    ",
    0x80000000
);

test_cranelift!(
    test_cranelift_ldxb_all,
    "
    mov r0, r1
    ldxb r9, [r0+0]
    lsh r9, 0
    ldxb r8, [r0+1]
    lsh r8, 4
    ldxb r7, [r0+2]
    lsh r7, 8
    ldxb r6, [r0+3]
    lsh r6, 12
    ldxb r5, [r0+4]
    lsh r5, 16
    ldxb r4, [r0+5]
    lsh r4, 20
    ldxb r3, [r0+6]
    lsh r3, 24
    ldxb r2, [r0+7]
    lsh r2, 28
    ldxb r1, [r0+8]
    lsh r1, 32
    ldxb r0, [r0+9]
    lsh r0, 36
    or r0, r1
    or r0, r2
    or r0, r3
    or r0, r4
    or r0, r5
    or r0, r6
    or r0, r7
    or r0, r8
    or r0, r9
    exit
    ",
    [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09],
    0x9876543210
);

test_cranelift!(
    test_cranelift_ldxb,
    "
    ldxb r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0x11, 0xcc, 0xdd],
    0x11
);

test_cranelift!(
    test_cranelift_ldxdw,
    "
    ldxdw r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0xcc, 0xdd],
    0x8877665544332211
);

test_cranelift!(
    test_cranelift_ldxh_all,
    "
    mov r0, r1
    ldxh r9, [r0+0]
    be16 r9
    lsh r9, 0
    ldxh r8, [r0+2]
    be16 r8
    lsh r8, 4
    ldxh r7, [r0+4]
    be16 r7
    lsh r7, 8
    ldxh r6, [r0+6]
    be16 r6
    lsh r6, 12
    ldxh r5, [r0+8]
    be16 r5
    lsh r5, 16
    ldxh r4, [r0+10]
    be16 r4
    lsh r4, 20
    ldxh r3, [r0+12]
    be16 r3
    lsh r3, 24
    ldxh r2, [r0+14]
    be16 r2
    lsh r2, 28
    ldxh r1, [r0+16]
    be16 r1
    lsh r1, 32
    ldxh r0, [r0+18]
    be16 r0
    lsh r0, 36
    or r0, r1
    or r0, r2
    or r0, r3
    or r0, r4
    or r0, r5
    or r0, r6
    or r0, r7
    or r0, r8
    or r0, r9
    exit
    ",
    [
        0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00, 0x06, 0x00,
        0x07, 0x00, 0x08, 0x00, 0x09
    ],
    0x9876543210
);

test_cranelift!(
    test_cranelift_ldxh_all2,
    "
    mov r0, r1
    ldxh r9, [r0+0]
    be16 r9
    ldxh r8, [r0+2]
    be16 r8
    ldxh r7, [r0+4]
    be16 r7
    ldxh r6, [r0+6]
    be16 r6
    ldxh r5, [r0+8]
    be16 r5
    ldxh r4, [r0+10]
    be16 r4
    ldxh r3, [r0+12]
    be16 r3
    ldxh r2, [r0+14]
    be16 r2
    ldxh r1, [r0+16]
    be16 r1
    ldxh r0, [r0+18]
    be16 r0
    or r0, r1
    or r0, r2
    or r0, r3
    or r0, r4
    or r0, r5
    or r0, r6
    or r0, r7
    or r0, r8
    or r0, r9
    exit
    ",
    [
        0x00, 0x01, 0x00, 0x02, 0x00, 0x04, 0x00, 0x08, 0x00, 0x10, 0x00, 0x20, 0x00, 0x40, 0x00,
        0x80, 0x01, 0x00, 0x02, 0x00
    ],
    0x3ff
);

test_cranelift!(
    test_cranelift_ldxh,
    "
    ldxh r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0x11, 0x22, 0xcc, 0xdd],
    0x2211
);

test_cranelift!(
    test_cranelift_ldxh_same_reg,
    "
    mov r0, r1
    sth [r0], 0x1234
    ldxh r0, [r0]
    exit
    ",
    [0xff, 0xff],
    0x1234
);

test_cranelift!(
    test_cranelift_ldxw_all,
    "
    mov r0, r1
    ldxw r9, [r0+0]
    be32 r9
    ldxw r8, [r0+4]
    be32 r8
    ldxw r7, [r0+8]
    be32 r7
    ldxw r6, [r0+12]
    be32 r6
    ldxw r5, [r0+16]
    be32 r5
    ldxw r4, [r0+20]
    be32 r4
    ldxw r3, [r0+24]
    be32 r3
    ldxw r2, [r0+28]
    be32 r2
    ldxw r1, [r0+32]
    be32 r1
    ldxw r0, [r0+36]
    be32 r0
    or r0, r1
    or r0, r2
    or r0, r3
    or r0, r4
    or r0, r5
    or r0, r6
    or r0, r7
    or r0, r8
    or r0, r9
    exit
    ",
    [
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x08, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x08, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00
    ],
    0x030f0f
);

test_cranelift!(
    test_cranelift_ldxw,
    "
    ldxw r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0x11, 0x22, 0x33, 0x44, 0xcc, 0xdd],
    0x44332211
);

test_cranelift!(
    test_cranelift_le16,
    "
    ldxh r0, [r1]
    le16 r0
    exit
    ",
    [0x22, 0x11],
    0x1122
);

test_cranelift!(
    test_cranelift_le32,
    "
    ldxw r0, [r1]
    le32 r0
    exit
    ",
    [0x44, 0x33, 0x22, 0x11],
    0x11223344
);

test_cranelift!(
    test_cranelift_le64,
    "
    ldxdw r0, [r1]
    le64 r0
    exit
    ",
    [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
    0x1122334455667788
);

test_cranelift!(
    test_cranelift_lsh_reg,
    "
    mov r0, 0x1
    mov r7, 4
    lsh r0, r7
    exit
    ",
    0x10
);

test_cranelift!(
    test_cranelift_mod,
    "
    mov32 r0, 5748
    mod32 r0, 92
    mov32 r1, 13
    mod32 r0, r1
    exit
    ",
    0x5
);

test_cranelift!(
    test_cranelift_mod32,
    "
    lddw r0, 0x100000003
    mod32 r0, 3
    exit
    ",
    0x0
);

test_cranelift!(
    test_cranelift_mod64,
    "
    mov32 r0, -1316649930
    lsh r0, 32
    or r0, 0x100dc5c8
    mov32 r1, 0xdde263e
    lsh r1, 32
    or r1, 0x3cbef7f3
    mod r0, r1
    mod r0, 0x658f1778
    exit
    ",
    0x30ba5a04
);

test_cranelift!(
    test_cranelift_mov,
    "
    mov32 r1, 1
    mov32 r0, r1
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_mul32_imm,
    "
    mov r0, 3
    mul32 r0, 4
    exit
    ",
    0xc
);

test_cranelift!(
    test_cranelift_mul32_reg,
    "
    mov r0, 3
    mov r1, 4
    mul32 r0, r1
    exit
    ",
    0xc
);

test_cranelift!(
    test_cranelift_mul32_reg_overflow,
    "
    mov r0, 0x40000001
    mov r1, 4
    mul32 r0, r1
    exit
    ",
    0x4
);

test_cranelift!(
    test_cranelift_mul64_imm,
    "
    mov r0, 0x40000001
    mul r0, 4
    exit
    ",
    0x100000004
);

test_cranelift!(
    test_cranelift_mul64_reg,
    "
    mov r0, 0x40000001
    mov r1, 4
    mul r0, r1
    exit
    ",
    0x100000004
);

test_cranelift!(
    test_cranelift_mul_loop,
    "
    mov r0, 0x7
    add r1, 0xa
    lsh r1, 0x20
    rsh r1, 0x20
    jeq r1, 0x0, +4
    mov r0, 0x7
    mul r0, 0x7
    add r1, -1
    jne r1, 0x0, -3
    exit
    ",
    0x75db9c97
);

test_cranelift!(
    test_cranelift_neg64,
    "
    mov32 r0, 2
    neg r0
    exit
    ",
    0xfffffffffffffffe
);

test_cranelift!(
    test_cranelift_neg,
    "
    mov32 r0, 2
    neg32 r0
    exit
    ",
    0xfffffffe
);

test_cranelift!(
    test_cranelift_prime,
    "
    mov r1, 67
    mov r0, 0x1
    mov r2, 0x2
    jgt r1, 0x2, +4
    ja +10
    add r2, 0x1
    mov r0, 0x1
    jge r2, r1, +7
    mov r3, r1
    div r3, r2
    mul r3, r2
    mov r4, r1
    sub r4, r3
    mov r0, 0x0
    jne r4, 0x0, -10
    exit
    ",
    1
);

test_cranelift!(
    test_cranelift_rhs32,
    "
    xor r0, r0
    sub r0, 1
    rsh32 r0, 8
    exit
    ",
    0x00ffffff
);

test_cranelift!(
    test_cranelift_rsh_reg,
    "
    mov r0, 0x10
    mov r7, 4
    rsh r0, r7
    exit
    ",
    0x1
);

test_cranelift!(
    test_cranelift_stack,
    "
    mov r1, 51
    stdw [r10-16], 0xab
    stdw [r10-8], 0xcd
    and r1, 1
    lsh r1, 3
    mov r2, r10
    add r2, r1
    ldxdw r0, [r2-16]
    exit
    ",
    0xcd
);

#[test]
fn test_cranelift_stack2() {
    let prog = assemble(
        "
        stb [r10-4], 0x01
        stb [r10-3], 0x02
        stb [r10-2], 0x03
        stb [r10-1], 0x04
        mov r1, r10
        mov r2, 0x4
        sub r1, r2
        call 1
        mov r1, 0
        ldxb r2, [r10-4]
        ldxb r3, [r10-3]
        ldxb r4, [r10-2]
        ldxb r5, [r10-1]
        call 0
        xor r0, 0x2a2a2a2a
        exit",
    )
    .unwrap();

    let mut vm = rbpf::EbpfVmNoData::new(Some(&prog)).unwrap();
    vm.register_helper(0, helpers::gather_bytes).unwrap();
    vm.register_helper(1, helpers::memfrob).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift().unwrap(), 0x01020304);
}

test_cranelift!(
    test_cranelift_stb,
    "
    stb [r1+2], 0x11
    ldxb r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xcc, 0xdd],
    0x11
);

test_cranelift!(
    test_cranelift_stdw,
    "
    stdw [r1+2], 0x44332211
    ldxdw r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xcc, 0xdd],
    0x44332211
);

test_cranelift!(
    test_cranelift_sth,
    "
    sth [r1+2], 0x2211
    ldxh r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xff, 0xcc, 0xdd],
    0x2211
);

#[test]
#[ignore]
fn test_cranelift_string_stack() {
    let prog = assemble(
        "
        mov r1, 0x78636261
        stxw [r10-8], r1
        mov r6, 0x0
        stxb [r10-4], r6
        stxb [r10-12], r6
        mov r1, 0x79636261
        stxw [r10-16], r1
        mov r1, r10
        add r1, -8
        mov r2, r1
        call 0x4
        mov r1, r0
        mov r0, 0x1
        lsh r1, 0x20
        rsh r1, 0x20
        jne r1, 0x0, +11
        mov r1, r10
        add r1, -8
        mov r2, r10
        add r2, -16
        call 0x4
        mov r1, r0
        lsh r1, 0x20
        rsh r1, 0x20
        mov r0, 0x1
        jeq r1, r6, +1
        mov r0, 0x0
        exit",
    )
    .unwrap();

    let mut vm = rbpf::EbpfVmNoData::new(Some(&prog)).unwrap();
    vm.register_helper(4, helpers::strcmp).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift().unwrap(), 0x0);
}

test_cranelift!(
    test_cranelift_stw,
    "
    stw [r1+2], 0x44332211
    ldxw r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xff, 0xff, 0xff, 0xcc, 0xdd],
    0x44332211
);

test_cranelift!(
    test_cranelift_stxb,
    "
    mov32 r2, 0x11
    stxb [r1+2], r2
    ldxb r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xcc, 0xdd],
    0x11
);

test_cranelift!(
    test_cranelift_stxb_all,
    "
    mov r0, 0xf0
    mov r2, 0xf2
    mov r3, 0xf3
    mov r4, 0xf4
    mov r5, 0xf5
    mov r6, 0xf6
    mov r7, 0xf7
    mov r8, 0xf8
    stxb [r1], r0
    stxb [r1+1], r2
    stxb [r1+2], r3
    stxb [r1+3], r4
    stxb [r1+4], r5
    stxb [r1+5], r6
    stxb [r1+6], r7
    stxb [r1+7], r8
    ldxdw r0, [r1]
    be64 r0
    exit
    ",
    [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    0xf0f2f3f4f5f6f7f8
);

test_cranelift!(
    test_cranelift_stxb_all2,
    "
    mov r0, r1
    mov r1, 0xf1
    mov r9, 0xf9
    stxb [r0], r1
    stxb [r0+1], r9
    ldxh r0, [r0]
    be16 r0
    exit
    ",
    [0xff, 0xff],
    0xf1f9
);

test_cranelift!(
    test_cranelift_stxb_chain,
    "
    mov r0, r1
    ldxb r9, [r0+0]
    stxb [r0+1], r9
    ldxb r8, [r0+1]
    stxb [r0+2], r8
    ldxb r7, [r0+2]
    stxb [r0+3], r7
    ldxb r6, [r0+3]
    stxb [r0+4], r6
    ldxb r5, [r0+4]
    stxb [r0+5], r5
    ldxb r4, [r0+5]
    stxb [r0+6], r4
    ldxb r3, [r0+6]
    stxb [r0+7], r3
    ldxb r2, [r0+7]
    stxb [r0+8], r2
    ldxb r1, [r0+8]
    stxb [r0+9], r1
    ldxb r0, [r0+9]
    exit
    ",
    [0x2a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    0x2a
);

test_cranelift!(
    test_cranelift_stxdw,
    "
    mov r2, -2005440939
    lsh r2, 32
    or r2, 0x44332211
    stxdw [r1+2], r2
    ldxdw r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xcc, 0xdd],
    0x8877665544332211
);

test_cranelift!(
    test_cranelift_stxh,
    "
    mov32 r2, 0x2211
    stxh [r1+2], r2
    ldxh r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xff, 0xcc, 0xdd],
    0x2211
);

test_cranelift!(
    test_cranelift_stxw,
    "
    mov32 r2, 0x44332211
    stxw [r1+2], r2
    ldxw r0, [r1+2]
    exit
    ",
    [0xaa, 0xbb, 0xff, 0xff, 0xff, 0xff, 0xcc, 0xdd],
    0x44332211
);

test_cranelift!(
    test_cranelift_subnet,
    "
    mov r2, 0xe
    ldxh r3, [r1+12]
    jne r3, 0x81, +2
    mov r2, 0x12
    ldxh r3, [r1+16]
    and r3, 0xffff
    jne r3, 0x8, +5
    add r1, r2
    mov r0, 0x1
    ldxw r1, [r1+16]
    and r1, 0xffffff
    jeq r1, 0x1a8c0, +1
    mov r0, 0x0
    exit
    ",
    [
        0x00, 0x00, 0xc0, 0x9f, 0xa0, 0x97, 0x00, 0xa0, 0xcc, 0x3b, 0xbf, 0xfa, 0x08, 0x00, 0x45,
        0x10, 0x00, 0x3c, 0x46, 0x3c, 0x40, 0x00, 0x40, 0x06, 0x73, 0x1c, 0xc0, 0xa8, 0x01, 0x02,
        0xc0, 0xa8, 0x01, 0x01, 0x06, 0x0e, 0x00, 0x17, 0x99, 0xc5, 0xa0, 0xec, 0x00, 0x00, 0x00,
        0x00, 0xa0, 0x02, 0x7d, 0x78, 0xe0, 0xa3, 0x00, 0x00, 0x02, 0x04, 0x05, 0xb4, 0x04, 0x02,
        0x08, 0x0a, 0x00, 0x9c, 0x27, 0x24, 0x00, 0x00, 0x00, 0x00, 0x01, 0x03, 0x03, 0x00,
    ],
    0x1
);

const PROG_TCP_PORT_80: [u8; 152] = [
    0x71, 0x12, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x71, 0x13, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x67, 0x03, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x4f, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xb7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x55, 0x03, 0x0c, 0x00, 0x08, 0x00, 0x00, 0x00,
    0x71, 0x12, 0x17, 0x00, 0x00, 0x00, 0x00, 0x00, 0x55, 0x02, 0x0a, 0x00, 0x06, 0x00, 0x00, 0x00,
    0x71, 0x12, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00,
    0x57, 0x02, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x67, 0x02, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
    0x0f, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x69, 0x12, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x15, 0x02, 0x02, 0x00, 0x00, 0x50, 0x00, 0x00, 0x69, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x55, 0x01, 0x01, 0x00, 0x00, 0x50, 0x00, 0x00, 0xb7, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

#[test]
fn test_cranelift_tcp_port80_match() {
    let mem = &mut [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x08, 0x00, 0x45,
        0x00, 0x00, 0x56, 0x00, 0x01, 0x00, 0x00, 0x40, 0x06, 0xf9, 0x4d, 0xc0, 0xa8, 0x00, 0x01,
        0xc0, 0xa8, 0x00, 0x02, 0x27, 0x10, 0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x50, 0x02, 0x20, 0x00, 0xc5, 0x18, 0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
    ];
    let prog = &PROG_TCP_PORT_80;
    let mut vm = rbpf::EbpfVmRaw::new(Some(prog)).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x1);
}

#[test]
fn test_cranelift_tcp_port80_nomatch() {
    let mem = &mut [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x08, 0x00, 0x45,
        0x00, 0x00, 0x56, 0x00, 0x01, 0x00, 0x00, 0x40, 0x06, 0xf9, 0x4d, 0xc0, 0xa8, 0x00, 0x01,
        0xc0, 0xa8, 0x00, 0x02, 0x00, 0x16, 0x27, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x51, 0x02, 0x20, 0x00, 0xc5, 0x18, 0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
    ];
    let prog = &PROG_TCP_PORT_80;
    let mut vm = rbpf::EbpfVmRaw::new(Some(prog)).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x0);
}

#[test]
fn test_cranelift_tcp_port80_nomatch_ethertype() {
    let mem = &mut [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x08, 0x01, 0x45,
        0x00, 0x00, 0x56, 0x00, 0x01, 0x00, 0x00, 0x40, 0x06, 0xf9, 0x4d, 0xc0, 0xa8, 0x00, 0x01,
        0xc0, 0xa8, 0x00, 0x02, 0x27, 0x10, 0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x50, 0x02, 0x20, 0x00, 0xc5, 0x18, 0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
    ];
    let prog = &PROG_TCP_PORT_80;
    let mut vm = rbpf::EbpfVmRaw::new(Some(prog)).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x0);
}

#[test]
fn test_cranelift_tcp_port80_nomatch_proto() {
    let mem = &mut [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x08, 0x00, 0x45,
        0x00, 0x00, 0x56, 0x00, 0x01, 0x00, 0x00, 0x40, 0x11, 0xf9, 0x4d, 0xc0, 0xa8, 0x00, 0x01,
        0xc0, 0xa8, 0x00, 0x02, 0x27, 0x10, 0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x50, 0x02, 0x20, 0x00, 0xc5, 0x18, 0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
    ];
    let prog = &PROG_TCP_PORT_80;
    let mut vm = rbpf::EbpfVmRaw::new(Some(prog)).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x0);
}

#[test]
fn test_cranelift_tcp_sack_match() {
    let mut mem = TCP_SACK_MATCH.to_vec();
    let prog = assemble(TCP_SACK_ASM).unwrap();
    let mut vm = rbpf::EbpfVmRaw::new(Some(&prog)).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(
        vm.execute_program_cranelift(mem.as_mut_slice()).unwrap(),
        0x1
    );
}

#[test]
fn test_cranelift_tcp_sack_nomatch() {
    let mut mem = TCP_SACK_NOMATCH.to_vec();
    let prog = assemble(TCP_SACK_ASM).unwrap();
    let mut vm = rbpf::EbpfVmRaw::new(Some(&prog)).unwrap();
    vm.cranelift_compile().unwrap();
    assert_eq!(
        vm.execute_program_cranelift(mem.as_mut_slice()).unwrap(),
        0x0
    );
}

#[test]
fn test_cranelift_ldabsb() {
    let prog = &[
        0x30, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x33);
}

#[test]
fn test_cranelift_ldabsh() {
    let prog = &[
        0x28, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x4433);
}

#[test]
fn test_cranelift_ldabsw() {
    let prog = &[
        0x20, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x66554433);
}

#[test]
fn test_cranelift_ldabsdw() {
    let prog = &[
        0x38, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(
        vm.execute_program_cranelift(mem).unwrap(),
        0xaa99887766554433
    );
}

#[test]
fn test_cranelift_ldindb() {
    let prog = &[
        0xb7, 0x01, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x50, 0x10, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x88);
}

#[test]
fn test_cranelift_ldindh() {
    let prog = &[
        0xb7, 0x01, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x48, 0x10, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x9988);
}

#[test]
fn test_cranelift_ldindw() {
    let prog = &[
        0xb7, 0x01, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x40, 0x10, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(vm.execute_program_cranelift(mem).unwrap(), 0x88776655);
}

#[test]
fn test_cranelift_ldinddw() {
    let prog = &[
        0xb7, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x58, 0x10, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x00, 0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mem = &mut [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut vm = rbpf::EbpfVmFixedMbuff::new(Some(prog), 0x00, 0x08).unwrap();

    vm.cranelift_compile().unwrap();
    assert_eq!(
        vm.execute_program_cranelift(mem).unwrap(),
        0xccbbaa9988776655
    );
}
