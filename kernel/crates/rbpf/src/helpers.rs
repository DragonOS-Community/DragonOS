// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2015 Big Switch Networks, Inc
//      (Algorithms for uBPF helpers, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust, other helpers)

//! This module implements some built-in helpers that can be called from within an eBPF program.
//!
//! These helpers may originate from several places:
//!
//! * Some of them mimic the helpers available in the Linux kernel.
//! * Some of them were proposed as example helpers in uBPF and they were adapted here.
//! * Other helpers may be specific to rbpf.
//!
//! The prototype for helpers is always the same: five `u64` as arguments, and a `u64` as a return
//! value. Hence some helpers have unused arguments, or return a 0 value in all cases, in order to
//! respect this convention.

// Helpers associated to kernel helpers
// See also linux/include/uapi/linux/bpf.h in Linux kernel sources.

// bpf_ktime_getns()

/// Index of helper `bpf_ktime_getns()`, equivalent to `bpf_time_getns()`, in Linux kernel, see
/// <https://git.kernel.org/cgit/linux/kernel/git/torvalds/linux.git/tree/include/uapi/linux/bpf.h>.
pub const BPF_KTIME_GETNS_IDX: u32 = 5;

/// Get monotonic time (since boot time) in nanoseconds. All arguments are unused.
///
/// # Examples
///
/// ```
/// use rbpf::helpers;
///
/// let t = helpers::bpf_time_getns(0, 0, 0, 0, 0);
/// let d =  t / 10u64.pow(9)  / 60   / 60  / 24;
/// let h = (t / 10u64.pow(9)  / 60   / 60) % 24;
/// let m = (t / 10u64.pow(9)  / 60 ) % 60;
/// let s = (t / 10u64.pow(9)) % 60;
/// let ns = t % 10u64.pow(9);
/// println!("Uptime: {:#x} == {} days {}:{}:{}, {} ns", t, d, h, m, s, ns);
/// ```
#[allow(dead_code)]
#[allow(unused_variables)]
#[allow(deprecated)]
#[cfg(feature = "std")]
pub fn bpf_time_getns(unused1: u64, unused2: u64, unused3: u64, unused4: u64, unused5: u64) -> u64 {
    time::precise_time_ns()
}

// bpf_trace_printk()

/// Index of helper `bpf_trace_printk()`, equivalent to `bpf_trace_printf()`, in Linux kernel, see
/// <https://git.kernel.org/cgit/linux/kernel/git/torvalds/linux.git/tree/include/uapi/linux/bpf.h>.
pub const BPF_TRACE_PRINTK_IDX: u32 = 6;

/// Prints its **last three** arguments to standard output. The **first two** arguments are
/// **unused**. Returns the number of bytes written.
///
/// By ignoring the first two arguments, it creates a helper that will have a behavior similar to
/// the one of the equivalent helper `bpf_trace_printk()` from Linux kernel.
///
/// # Examples
///
/// ```
/// use rbpf::helpers;
///
/// let res = helpers::bpf_trace_printf(0, 0, 1, 15, 32);
/// assert_eq!(res as usize, "bpf_trace_printf: 0x1, 0xf, 0x20\n".len());
/// ```
///
/// This will print `bpf_trace_printf: 0x1, 0xf, 0x20`.
///
/// The eBPF code needed to perform the call in this example would be nearly identical to the code
/// obtained by compiling the following code from C to eBPF with clang:
///
/// ```c
/// #include <linux/bpf.h>
/// #include "path/to/linux/samples/bpf/bpf_helpers.h"
///
/// int main(struct __sk_buff *skb)
/// {
///     // Only %d %u %x %ld %lu %lx %lld %llu %llx %p %s conversion specifiers allowed.
///     // See <https://git.kernel.org/cgit/linux/kernel/git/torvalds/linux.git/tree/kernel/trace/bpf_trace.c>.
///     char *fmt = "bpf_trace_printk %llx, %llx, %llx\n";
///     return bpf_trace_printk(fmt, sizeof(fmt), 1, 15, 32);
/// }
/// ```
///
/// This would equally print the three numbers in `/sys/kernel/debug/tracing` file each time the
/// program is run.
#[allow(dead_code)]
#[allow(unused_variables)]
#[cfg(feature = "std")]
pub fn bpf_trace_printf(unused1: u64, unused2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    println!("bpf_trace_printf: {arg3:#x}, {arg4:#x}, {arg5:#x}");
    let size_arg = |x| {
        if x == 0 {
            1
        } else {
            (x as f64).log(16.0).floor() as u64 + 1
        }
    };
    "bpf_trace_printf: 0x, 0x, 0x\n".len() as u64 + size_arg(arg3) + size_arg(arg4) + size_arg(arg5)
}

// Helpers coming from uBPF <https://github.com/iovisor/ubpf/blob/master/vm/test.c>

/// The idea is to assemble five bytes into a single `u64`. For compatibility with the helpers API,
/// each argument must be a `u64`.
///
/// # Examples
///
/// ```
/// use rbpf::helpers;
///
/// let gathered = helpers::gather_bytes(0x11, 0x22, 0x33, 0x44, 0x55);
/// assert_eq!(gathered, 0x1122334455);
/// ```
pub fn gather_bytes(arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    arg1.wrapping_shl(32)
        | arg2.wrapping_shl(24)
        | arg3.wrapping_shl(16)
        | arg4.wrapping_shl(8)
        | arg5
}

/// Same as `void *memfrob(void *s, size_t n);` in `string.h` in C. See the GNU manual page (in
/// section 3) for `memfrob`. The memory is directly modified, and the helper returns 0 in all
/// cases. Arguments 3 to 5 are unused.
///
/// # Examples
///
/// ```
/// use rbpf::helpers;
///
/// let val: u64 = 0x112233;
/// let val_ptr = &val as *const u64;
///
/// helpers::memfrob(val_ptr as u64, 8, 0, 0, 0);
/// assert_eq!(val, 0x2a2a2a2a2a3b0819);
/// helpers::memfrob(val_ptr as u64, 8, 0, 0, 0);
/// assert_eq!(val, 0x112233);
/// ```
#[allow(unused_variables)]
pub fn memfrob(ptr: u64, len: u64, unused3: u64, unused4: u64, unused5: u64) -> u64 {
    for i in 0..len {
        unsafe {
            let mut p = (ptr + i) as *mut u8;
            *p ^= 0b101010;
        }
    }
    0
}

// TODO: Try again when asm!() is available in stable Rust.
// #![feature(asm)]
// #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
// #[allow(unused_variables)]
// pub fn memfrob (ptr: u64, len: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
//     unsafe {
//         asm!(
//                 "mov $0xf0, %rax"
//             ::: "mov $0xf1, %rcx"
//             ::: "mov $0xf2, %rdx"
//             ::: "mov $0xf3, %rsi"
//             ::: "mov $0xf4, %rdi"
//             ::: "mov $0xf5, %r8"
//             ::: "mov $0xf6, %r9"
//             ::: "mov $0xf7, %r10"
//             ::: "mov $0xf8, %r11"
//         );
//     }
//     0
// }

/// Compute and return the square root of argument 1, cast as a float. Arguments 2 to 5 are
/// unused.
///
/// # Examples
///
/// ```
/// use rbpf::helpers;
///
/// let x = helpers::sqrti(9, 0, 0, 0, 0);
/// assert_eq!(x, 3);
/// ```
#[allow(dead_code)]
#[allow(unused_variables)]
#[cfg(feature = "std")] // sqrt is only available when using `std`
pub fn sqrti(arg1: u64, unused2: u64, unused3: u64, unused4: u64, unused5: u64) -> u64 {
    (arg1 as f64).sqrt() as u64
}

/// C-like `strcmp`, return 0 if the strings are equal, and a non-null value otherwise.
///
/// # Examples
///
/// ```
/// use rbpf::helpers;
///
/// let foo = "This is a string.\0".as_ptr() as u64;
/// let bar = "This is another sting.\0".as_ptr() as u64;
///
/// assert!(helpers::strcmp(foo, foo, 0, 0, 0) == 0);
/// assert!(helpers::strcmp(foo, bar, 0, 0, 0) != 0);
/// ```
#[allow(dead_code)]
#[allow(unused_variables)]
pub fn strcmp(arg1: u64, arg2: u64, arg3: u64, unused4: u64, unused5: u64) -> u64 {
    // C-like strcmp, maybe shorter than converting the bytes to string and comparing?
    if arg1 == 0 || arg2 == 0 {
        return u64::MAX;
    }
    let mut a = arg1;
    let mut b = arg2;
    unsafe {
        let mut a_val = *(a as *const u8);
        let mut b_val = *(b as *const u8);
        while a_val == b_val && a_val != 0 && b_val != 0 {
            a += 1;
            b += 1;
            a_val = *(a as *const u8);
            b_val = *(b as *const u8);
        }
        if a_val >= b_val {
            (a_val - b_val) as u64
        } else {
            (b_val - a_val) as u64
        }
    }
}

// Some additional helpers

/// Returns a random u64 value comprised between `min` and `max` values (inclusive). Arguments 3 to
/// 5 are unused.
///
/// Relies on `rand()` function from libc, so `libc::srand()` should be called once before this
/// helper is used.
///
/// # Examples
///
/// ```
/// extern crate libc;
/// extern crate rbpf;
/// extern crate time;
///
/// unsafe {
///     libc::srand(time::precise_time_ns() as u32)
/// }
///
/// let n = rbpf::helpers::rand(3, 6, 0, 0, 0);
/// assert!(3 <= n && n <= 6);
/// ```
#[allow(dead_code)]
#[allow(unused_variables)]
#[cfg(feature = "std")]
pub fn rand(min: u64, max: u64, unused3: u64, unused4: u64, unused5: u64) -> u64 {
    let mut n = unsafe { (libc::rand() as u64).wrapping_shl(32) + libc::rand() as u64 };
    if min < max {
        n = n % (max + 1 - min) + min;
    };
    n
}
/// Prints the helper functions name and it's index.
#[cfg(feature = "std")]
pub fn show_helper() {
    for (index, name) in BPF_FUNC_MAPPER.iter().enumerate() {
        println!("{}:{}", index, name);
    }
}

/// See https://github.com/torvalds/linux/blob/master/include/uapi/linux/bpf.h
pub const BPF_FUNC_MAPPER: &[&str] = &[
    "unspec",
    "map_lookup_elem",
    "map_update_elem",
    "map_delete_elem",
    "probe_read",
    "ktime_get_ns",
    "trace_printk",
    "get_prandom_u32",
    "get_smp_processor_id",
    "skb_store_bytes",
    "l3_csum_replace",
    "l4_csum_replace",
    "tail_call",
    "clone_redirect",
    "get_current_pid_tgid",
    "get_current_uid_gid",
    "get_current_comm",
    "get_cgroup_classid",
    "skb_vlan_push",
    "skb_vlan_pop",
    "skb_get_tunnel_key",
    "skb_set_tunnel_key",
    "perf_event_read",
    "redirect",
    "get_route_realm",
    "perf_event_output",
    "skb_load_bytes",
    "get_stackid",
    "csum_diff",
    "skb_get_tunnel_opt",
    "skb_set_tunnel_opt",
    "skb_change_proto",
    "skb_change_type",
    "skb_under_cgroup",
    "get_hash_recalc",
    "get_current_task",
    "probe_write_user",
    "current_task_under_cgroup",
    "skb_change_tail",
    "skb_pull_data",
    "csum_update",
    "set_hash_invalid",
    "get_numa_node_id",
    "skb_change_head",
    "xdp_adjust_head",
    "probe_read_str",
    "get_socket_cookie",
    "get_socket_uid",
    "set_hash",
    "setsockopt",
    "skb_adjust_room",
    "redirect_map",
    "sk_redirect_map",
    "sock_map_update",
    "xdp_adjust_meta",
    "perf_event_read_value",
    "perf_prog_read_value",
    "getsockopt",
    "override_return",
    "sock_ops_cb_flags_set",
    "msg_redirect_map",
    "msg_apply_bytes",
    "msg_cork_bytes",
    "msg_pull_data",
    "bind",
    "xdp_adjust_tail",
    "skb_get_xfrm_state",
    "get_stack",
    "skb_load_bytes_relative",
    "fib_lookup",
    "sock_hash_update",
    "msg_redirect_hash",
    "sk_redirect_hash",
    "lwt_push_encap",
    "lwt_seg6_store_bytes",
    "lwt_seg6_adjust_srh",
    "lwt_seg6_action",
    "rc_repeat",
    "rc_keydown",
    "skb_cgroup_id",
    "get_current_cgroup_id",
    "get_local_storage",
    "sk_select_reuseport",
    "skb_ancestor_cgroup_id",
    "sk_lookup_tcp",
    "sk_lookup_udp",
    "sk_release",
    "map_push_elem",
    "map_pop_elem",
    "map_peek_elem",
    "msg_push_data",
    "msg_pop_data",
    "rc_pointer_rel",
    "spin_lock",
    "spin_unlock",
    "sk_fullsock",
    "tcp_sock",
    "skb_ecn_set_ce",
    "get_listener_sock",
    "skc_lookup_tcp",
    "tcp_check_syncookie",
    "sysctl_get_name",
    "sysctl_get_current_value",
    "sysctl_get_new_value",
    "sysctl_set_new_value",
    "strtol",
    "strtoul",
    "sk_storage_get",
    "sk_storage_delete",
    "send_signal",
    "tcp_gen_syncookie",
    "skb_output",
    "probe_read_user",
    "probe_read_kernel",
    "probe_read_user_str",
    "probe_read_kernel_str",
    "tcp_send_ack",
    "send_signal_thread",
    "jiffies64",
    "read_branch_records",
    "get_ns_current_pid_tgid",
    "xdp_output",
    "get_netns_cookie",
    "get_current_ancestor_cgroup_id",
    "sk_assign",
    "ktime_get_boot_ns",
    "seq_printf",
    "seq_write",
    "sk_cgroup_id",
    "sk_ancestor_cgroup_id",
    "ringbuf_output",
    "ringbuf_reserve",
    "ringbuf_submit",
    "ringbuf_discard",
    "ringbuf_query",
    "csum_level",
    "skc_to_tcp6_sock",
    "skc_to_tcp_sock",
    "skc_to_tcp_timewait_sock",
    "skc_to_tcp_request_sock",
    "skc_to_udp6_sock",
    "get_task_stack",
    "load_hdr_opt",
    "store_hdr_opt",
    "reserve_hdr_opt",
    "inode_storage_get",
    "inode_storage_delete",
    "d_path",
    "copy_from_user",
    "snprintf_btf",
    "seq_printf_btf",
    "skb_cgroup_classid",
    "redirect_neigh",
    "per_cpu_ptr",
    "this_cpu_ptr",
    "redirect_peer",
    "task_storage_get",
    "task_storage_delete",
    "get_current_task_btf",
    "bprm_opts_set",
    "ktime_get_coarse_ns",
    "ima_inode_hash",
    "sock_from_file",
    "check_mtu",
    "for_each_map_elem",
    "snprintf",
    "sys_bpf",
    "btf_find_by_name_kind",
    "sys_close",
    "timer_init",
    "timer_set_callback",
    "timer_start",
    "timer_cancel",
    "get_func_ip",
    "get_attach_cookie",
    "task_pt_regs",
    "get_branch_snapshot",
    "trace_vprintk",
    "skc_to_unix_sock",
    "kallsyms_lookup_name",
    "find_vma",
    "loop",
    "strncmp",
    "get_func_arg",
    "get_func_ret",
    "get_func_arg_cnt",
    "get_retval",
    "set_retval",
    "xdp_get_buff_len",
    "xdp_load_bytes",
    "xdp_store_bytes",
    "copy_from_user_task",
    "skb_set_tstamp",
    "ima_file_hash",
    "kptr_xchg",
    "map_lookup_percpu_elem",
    "skc_to_mptcp_sock",
    "dynptr_from_mem",
    "ringbuf_reserve_dynptr",
    "ringbuf_submit_dynptr",
    "ringbuf_discard_dynptr",
    "dynptr_read",
    "dynptr_write",
    "dynptr_data",
    "tcp_raw_gen_syncookie_ipv4",
    "tcp_raw_gen_syncookie_ipv6",
    "tcp_raw_check_syncookie_ipv4",
    "tcp_raw_check_syncookie_ipv6",
    "ktime_get_tai_ns",
    "user_ringbuf_drain",
    "cgrp_storage_get",
    "cgrp_storage_delete",
];
