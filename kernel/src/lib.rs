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
#[cfg(target_arch = "x86_64")]
#[path = "arch/x86_64/mod.rs"]
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
    net::{socket::SocketOptions, Socket},
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

    func();

    return 0;
}

fn func() {
    let binding = NET_DRIVERS.write();

    let device = unsafe {
        (binding.get(&0).unwrap().as_ref() as *const dyn NetDriver
            as *const VirtioNICDriver<PciTransport> as *mut VirtioNICDriver<PciTransport>)
            .as_mut()
            .unwrap()
    };

    let binding = NET_FACES.write();

    let net_face = binding.get(&0).unwrap();

    let net_face = unsafe {
        (net_face.as_ref() as *const crate::net::Interface as *mut crate::net::Interface)
            .as_mut()
            .unwrap()
    };

    drop(binding);

    // Create sockets
    let mut dhcp_socket = dhcpv4::Socket::new();

    // Set a ridiculously short max lease time to show DHCP renews work properly.
    // This will cause the DHCP client to start renewing after 5 seconds, and give up the
    // lease after 10 seconds if renew hasn't succeeded.
    // IMPORTANT: This should be removed in production.
    dhcp_socket.set_max_lease_duration(Some(Duration::from_secs(10)));

    let mut sockets = SocketSet::new(vec![]);
    let dhcp_handle = sockets.add(dhcp_socket);

    const DHCP_TRY_ROUND: u8 = 10;
    for _ in 0..DHCP_TRY_ROUND {
        let timestamp = Instant::from_micros(ktime_get_real_ns());

        let _flag = net_face.inner_iface.poll(timestamp, device, &mut sockets);
        schedule_timeout(1000).ok();
        let event = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).poll();
        // kdebug!("event = {event:?} !!!");

        match event {
            None => {}

            Some(dhcpv4::Event::Configured(config)) => {
                // kdebug!("Find Config!! {config:?}");
                // kdebug!("Find ip address: {}", config.address);
                // kdebug!("iface.ip_addrs={:?}", net_face.inner_iface.ip_addrs());
                set_ipv4_addr(&mut net_face.inner_iface, config.address);
                if let Some(router) = config.router {
                    net_face
                        .inner_iface
                        .routes_mut()
                        .add_default_ipv4_route(router)
                        .unwrap();
                    let cidr = net_face.inner_iface.ip_addrs().first();
                    if cidr.is_some() {
                        let cidr = cidr.unwrap();
                        kinfo!("Successfully allocated ip by Dhcpv4! Ip:{}", cidr,)
                    }
                    break;
                } else {
                    net_face
                        .inner_iface
                        .routes_mut()
                        .remove_default_ipv4_route();
                }
            }

            Some(dhcpv4::Event::Deconfigured) => {
                kdebug!("deconfigured");
                set_ipv4_addr(
                    &mut net_face.inner_iface,
                    Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0),
                );
                net_face
                    .inner_iface
                    .routes_mut()
                    .remove_default_ipv4_route();
            }
        }
    }
}

fn set_ipv4_addr(iface: &mut Interface, cidr: Ipv4Cidr) {
    // kdebug!("set cidr = {cidr:?}");

    iface.update_ip_addrs(|addrs| {
        let dest = addrs.iter_mut().next();
        if let None = dest {
            addrs
                .push(IpCidr::Ipv4(cidr))
                .expect("Push ipCidr failed: full");
        } else {
            let dest = dest.unwrap();
            *dest = IpCidr::Ipv4(cidr);
        }
    });
}
