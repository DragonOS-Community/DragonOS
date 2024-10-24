// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2017 6WIND S.A. <quentin.monnet@6wind.com>

#[macro_use]
extern crate json;

extern crate elf;
use std::path::PathBuf;

extern crate rbpf;
use rbpf::disassembler;

// Turn a program into a JSON string.
//
// Relies on `json` crate.
//
// You may copy this function and adapt it according to your needs. For instance, you may want to:
//
// * Remove the "desc" (description) attributes from the output.
// * Print integers as integers, and not as strings containing their hexadecimal representation
//   (just replace the relevant `format!()` calls by the commented values.
fn to_json(prog: &[u8]) -> String {
    // This call returns a high-level representation of the instructions, with the two parts of
    // `LD_DW_IMM` instructions merged, and name and descriptions of the instructions.
    // If you prefer to use a lower-level representation, use `ebpf::to_insn_vec()` function
    // instead.
    let insns = disassembler::to_insn_vec(prog);
    let mut json_insns = vec![];
    for insn in insns {
        json_insns.push(object!(
            "opc"  => format!("{:#x}", insn.opc), // => insn.opc,
            "dst"  => format!("{:#x}", insn.dst), // => insn.dst,
            "src"  => format!("{:#x}", insn.src), // => insn.src,
            "off"  => format!("{:#x}", insn.off), // => insn.off,
            // Warning: for imm we use a i64 instead of a i32 (to have correct values for
            // `lddw` operation. If we print a number in the JSON this is not a problem, the
            // internal i64 has the same value with extended sign on 32 most significant bytes.
            // If we print the hexadecimal value as a string however, we want to cast as a i32
            // to prevent all other instructions to print spurious `ffffffff` prefix if the
            // number is negative. When values takes more than 32 bits with `lddw`, the cast
            // has no effect and the complete value is printed anyway.
            "imm"  => format!("{:#x}", insn.imm as i32), // => insn.imm,
            "desc" => insn.desc
        ));
    }
    json::stringify_pretty(
        object!(
        "size"  => json_insns.len(),
        "insns" => json_insns
        ),
        4,
    )
}

// Load a program from an object file, and prints it to standard output as a JSON string.
fn main() {
    // Let's reuse this file from `load_elf/example`.
    let filename = "examples/load_elf__block_a_port.elf";

    let path = PathBuf::from(filename);
    let file = match elf::File::open_path(path) {
        Ok(f) => f,
        Err(e) => panic!("Error: {:?}", e),
    };

    let text_scn = match file.get_section(".classifier") {
        Some(s) => s,
        None => panic!("Failed to look up .classifier section"),
    };

    let prog = &text_scn.data;

    println!("{}", to_json(prog));
}
