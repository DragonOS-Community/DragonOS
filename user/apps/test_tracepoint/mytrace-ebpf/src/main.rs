#![no_std]
#![no_main]

use aya_ebpf::{
    helpers::bpf_probe_read_user_str_bytes,
    macros::{map, tracepoint},
    maps::PerCpuArray,
    programs::TracePointContext,
};
use aya_log_ebpf::info;

const MAX_PATH: usize = 4096;

#[repr(C)]
pub struct Buf {
    pub buf: [u8; MAX_PATH],
}

#[map]
pub static mut BUF: PerCpuArray<Buf> = PerCpuArray::with_max_entries(1, 0); //

#[tracepoint]
pub fn mytrace(ctx: TracePointContext) -> u32 {
    match try_mytrace(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_mytrace(ctx: TracePointContext) -> Result<u32, u32> {
    // info!(&ctx, "tracepoint sys_enter_openat called");
    match try_aya_tracepoint_echo_open(&ctx) {
        Ok(_) => Ok(0),
        Err(e) => {
            info!(&ctx, "tracepoint sys_enter_openat called, error: {}", e);
            Err(e as u32)
        }
    }
}

fn try_aya_tracepoint_echo_open(ctx: &TracePointContext) -> Result<u32, i64> {
    // Load the pointer to the filename. The offset value can be found running:
    // sudo cat /sys/kernel/debug/tracing/events/syscalls/sys_enter_open/format
    const FILENAME_OFFSET: usize = 12;

    if let Ok(filename_addr) = unsafe { ctx.read_at::<u64>(FILENAME_OFFSET) } {
        // get the map-backed buffer that we're going to use as storage for the filename
        let buf = unsafe {
            let ptr = BUF.get_ptr_mut(0).ok_or(0)?; //

            &mut *ptr
        };

        // read the filename
        let filename = unsafe {
            core::str::from_utf8_unchecked(bpf_probe_read_user_str_bytes(
                filename_addr as *const u8,
                &mut buf.buf,
            )?)
        };

        if filename.len() < MAX_PATH {
            // log the filename
            info!(
                ctx,
                "Kernel tracepoint sys_enter_openat called,  filename :{}", filename
            );
        }
    }
    Ok(0)
}

// This function assumes that the maximum length of a file's path can be of 16 bytes. This is meant
// to be read as an example, only. Refer to the accompanying `tracepoints.md` for its inclusion in the
// code.
fn try_aya_tracepoint_echo_open_small_file_path(ctx: &TracePointContext) -> Result<u32, i64> {
    const MAX_SMALL_PATH: usize = 16;
    let mut buf: [u8; MAX_SMALL_PATH] = [0; MAX_SMALL_PATH];

    // Load the pointer to the filename. The offset value can be found running:
    // sudo cat /sys/kernel/debug/tracing/events/syscalls/sys_enter_open/format
    const FILENAME_OFFSET: usize = 12;
    if let Ok(filename_addr) = unsafe { ctx.read_at::<u64>(FILENAME_OFFSET) } {
        // read the filename
        let filename = unsafe {
            // Get an UTF-8 String from an array of bytes
            core::str::from_utf8_unchecked(
                // Use the address of the kernel's string  //

                // to copy its contents into the array named 'buf'
                match bpf_probe_read_user_str_bytes(filename_addr as *const u8, &mut buf) {
                    Ok(_) => &buf,
                    Err(e) => {
                        info!(
                            ctx,
                            "tracepoint sys_enter_openat called buf_probe failed {}", e
                        );
                        return Err(e);
                    }
                },
            )
        };
        info!(
            ctx,
            "tracepoint sys_enter_openat called, filename  {}", filename
        );
    }
    Ok(0)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[link_section = "license"]
#[no_mangle]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
