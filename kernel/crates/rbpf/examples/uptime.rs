// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2017 6WIND S.A. <quentin.monnet@6wind.com>

extern crate rbpf;
use rbpf::helpers;

// The main objectives of this example is to show:
//
// * the use of EbpfVmNoData function,
// * and the use of a helper.
//
// The two eBPF programs are independent and are not related to one another.
fn main() {
    let prog1 = &[
        0xb4, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov32 r0, 0
        0xb4, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // mov32 r1, 2
        0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // add32 r0, 1
        0x0c, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // add32 r0, r1
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit and return r0
    ];

    // We use helper `bpf_time_getns()`, which is similar to helper `bpf_ktime_getns()` from Linux
    // kernel. Hence rbpf::helpers module provides the index of this in-kernel helper as a
    // constant, so that we can remain compatible with programs for the kernel. Here we also cast
    // it to a u8 so as to use it directly in program instructions.
    let hkey = helpers::BPF_KTIME_GETNS_IDX as u8;
    let prog2 = &[
        0xb7, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r1, 0
        0xb7, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r1, 0
        0xb7, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r1, 0
        0xb7, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r1, 0
        0xb7, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r1, 0
        0x85, 0x00, 0x00, 0x00, hkey, 0x00, 0x00, 0x00, // call helper <hkey>
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit and return r0
    ];

    // Create a VM: this one takes no data. Load prog1 in it.
    let mut vm = rbpf::EbpfVmNoData::new(Some(prog1)).unwrap();
    // Execute prog1.
    assert_eq!(vm.execute_program().unwrap(), 0x3);

    // As struct EbpfVmNoData does not takes any memory area, its return value is mostly
    // deterministic. So we know prog1 will always return 3. There is an exception: when it uses
    // helpers, the latter may have non-deterministic values, and all calls may not return the same
    // value.
    //
    // In the following example we use a helper to get the elapsed time since boot time: we
    // reimplement uptime in eBPF, in Rust. Because why not.

    vm.set_program(prog2).unwrap();
    vm.register_helper(helpers::BPF_KTIME_GETNS_IDX, helpers::bpf_time_getns)
        .unwrap();

    let time;

    #[cfg(all(not(windows), feature = "std"))]
    {
        vm.jit_compile().unwrap();

        time = unsafe { vm.execute_program_jit().unwrap() };
    }

    #[cfg(any(windows, not(feature = "std")))]
    {
        time = vm.execute_program().unwrap();
    }

    let days = time / 10u64.pow(9) / 60 / 60 / 24;
    let hours = (time / 10u64.pow(9) / 60 / 60) % 24;
    let minutes = (time / 10u64.pow(9) / 60) % 60;
    let seconds = (time / 10u64.pow(9)) % 60;
    let nanosec = time % 10u64.pow(9);

    println!(
        "Uptime: {:#x} ns == {} days {:02}:{:02}:{:02}, {} ns",
        time, days, hours, minutes, seconds, nanosec
    );
}
