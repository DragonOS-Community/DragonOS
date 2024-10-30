#![no_std]
#![no_main]

use aya_ebpf::{macros::kprobe, programs::ProbeContext};
use aya_ebpf::macros::map;
use aya_ebpf::maps::HashMap;
use aya_log_ebpf::info;

#[kprobe]
pub fn syscall_ebpf(ctx: ProbeContext) -> u32 {
    try_syscall_ebpf(ctx).unwrap_or_else(|ret| ret)
}

fn try_syscall_ebpf(ctx: ProbeContext) -> Result<u32, u32> {
    let pt_regs = unsafe {
        &*ctx.regs
    };
    // first arg -> rdi
    // second arg -> rsi
    // third arg -> rdx
    // four arg -> rcx
    let syscall_num  = pt_regs.rsi as usize;
    if syscall_num != 1 {
        unsafe {
            if let Some(v) = SYSCALL_LIST.get(&(syscall_num as u32)){
                let new_v = *v + 1;
                SYSCALL_LIST.insert(&(syscall_num as u32), &new_v,0).unwrap();
            }else {
                SYSCALL_LIST.insert(&(syscall_num as u32), &1,0).unwrap();
            }
        }
        info!(&ctx, "invoke syscall {}", syscall_num);
    }
    Ok(0)
}

#[map] //
static SYSCALL_LIST: HashMap<u32, u32> =
    HashMap::<u32, u32>::with_max_entries(1024, 0);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
