// Copyright Microsoft Corporation
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

// Path: examples/rbpf_plugin.rs
use std::io::Read;

// Helper function used by https://github.com/Alan-Jowett/bpf_conformance/blob/main/tests/call_unwind_fail.data
fn _unwind(a: u64, _b: u64, _c: u64, _d: u64, _e: u64) -> u64 {
    a
}

// This is a plugin for the bpf_conformance test suite (https://github.com/Alan-Jowett/bpf_conformance)
// It accepts a single argument, the memory contents to pass to the VM.
// It reads the program from stdin.
fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    #[allow(unused_mut)] // In no_std the jit variable isn't mutated.
    let mut jit: bool = false;
    let mut cranelift: bool = false;
    let mut program_text = String::new();
    let mut memory_text = String::new();

    args.remove(0);

    // Memory is always the first argument.
    if !args.is_empty() {
        memory_text.clone_from(&args[0]);
        // Strip whitespace
        memory_text.retain(|c| !c.is_whitespace());
        args.remove(0);
    }

    // Process the rest of the arguments.
    while !args.is_empty() {
        match args[0].as_str() {
            "--help" => {
                println!("Usage: rbpf_plugin [memory] < program");
                return;
            }
            "--jit" => {
                #[cfg(any(windows, not(feature = "std")))]
                {
                    println!("JIT not supported");
                    return;
                }
                #[cfg(all(not(windows), feature = "std"))]
                {
                    jit = true;
                }
            }
            "--cranelift" => {
                cranelift = true;

                #[cfg(not(feature = "cranelift"))]
                {
                    let _ = cranelift;
                    println!("Cranelift is not enabled");
                    return;
                }
            }
            "--program" => {
                if args.len() < 2 {
                    println!("Missing argument to --program");
                    return;
                }
                args.remove(0);
                if !args.is_empty() {
                    program_text.clone_from(&args[0]);
                    args.remove(0);
                }
            }
            _ => panic!("Unknown argument {}", args[0]),
        }
        args.remove(0);
    }

    if program_text.is_empty() {
        // Read program text from stdin
        std::io::stdin().read_to_string(&mut program_text).unwrap();
    }

    // Strip whitespace
    program_text.retain(|c| !c.is_whitespace());

    // Convert program from hex to bytecode
    let bytecode = hex::decode(program_text).unwrap();

    // Convert memory from hex to bytes
    let mut memory: Vec<u8> = hex::decode(memory_text).unwrap();

    // Create rbpf vm
    let mut vm = rbpf::EbpfVmRaw::new(Some(&bytecode)).unwrap();

    // Register the helper function used by call_unwind_fail.data test.
    vm.register_helper(5, _unwind).unwrap();

    let result: u64;
    if jit {
        #[cfg(any(windows, not(feature = "std")))]
        {
            println!("JIT not supported");
            return;
        }
        #[cfg(all(not(windows), feature = "std"))]
        {
            unsafe {
                vm.jit_compile().unwrap();
                result = vm.execute_program_jit(&mut memory).unwrap();
            }
        }
    } else if cranelift {
        #[cfg(not(feature = "cranelift"))]
        {
            println!("Cranelift is not enabled");
            return;
        }
        #[cfg(feature = "cranelift")]
        {
            vm.cranelift_compile().unwrap();
            result = vm.execute_program_cranelift(&mut memory).unwrap();
        }
    } else {
        result = vm.execute_program(&mut memory).unwrap();
    }
    println!("{result:x}");
}
