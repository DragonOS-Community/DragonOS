#![no_std] // <1>
#![no_main] // <1>
#![feature(const_mut_refs)]
#![feature(core_intrinsics)] // <2>
#![feature(alloc_error_handler)]
#![feature(panic_info_message)]
#![feature(drain_filter)] // 允许Vec的drain_filter特性
#![feature(c_void_variant)]
use core::arch::x86_64::_rdtsc;
// used in kernel/src/exception/softirq.rs
#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
use core::panic::PanicInfo;

/// 导出x86_64架构相关的代码，命名为arch模块
#[macro_use]
mod arch;
#[macro_use]
mod libs;
#[macro_use]
mod include;
mod driver; // 如果driver依赖了libs，应该在libs后面导出
mod exception;
mod filesystem;
mod io;
mod ipc;
mod mm;
mod net;
mod process;
mod sched;
mod smp;
mod syscall;
mod time;

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate lazy_static;
extern crate num;
#[macro_use]
extern crate num_derive;
extern crate smoltcp;
extern crate thingbuf;

use driver::NET_DRIVERS;
#[cfg(target_arch = "x86_64")]
extern crate x86;

use mm::allocator::KernelAllocator;
use smoltcp::{
    iface::{Interface, SocketSet},
    time::{Duration, Instant},
    wire::{IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr},
};

// <3>
use crate::{
    arch::asm::current::current_pcb,
    driver::{
        net::{virtio_net::VirtioNICDriver, NetDriver},
        virtio::transport_pci::PciTransport,
    },
    filesystem::vfs::ROOT_INODE,
    include::bindings::bindings::{process_do_exit, BLACK, GREEN},
    net::{socket::SocketOptions, Socket, net_core::net_init},
    time::{sleep::us_sleep, timekeep::ktime_get_real_ns, timer::schedule_timeout, TimeSpec},
};

// 声明全局的slab分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator {};

/// 全局的panic处理函数
#[panic_handler]
#[no_mangle]
pub fn panic(info: &PanicInfo) -> ! {
    kerror!("Kernel Panic Occurred.");

    match info.location() {
        Some(loc) => {
            println!(
                "Location:\n\tFile: {}\n\tLine: {}, Column: {}",
                loc.file(),
                loc.line(),
                loc.column()
            );
        }
        None => {
            println!("No location info");
        }
    }

    match info.message() {
        Some(msg) => {
            println!("Message:\n\t{}", msg);
        }
        None => {
            println!("No panic message.");
        }
    }

    println!("Current PCB:\n\t{:?}", current_pcb());
    unsafe {
        process_do_exit(u64::MAX);
    };
    loop {}
}

use net::NET_FACES;
// use smoltcp::
use smoltcp::socket::dhcpv4;

/// 该函数用作测试，在process.c的initial_kernel_thread()中调用了此函数
#[no_mangle]
pub extern "C" fn __rust_demo_func() -> i32 {
    printk_color!(GREEN, BLACK, "__rust_demo_func()\n");
    net_init();
    return 0;
}

