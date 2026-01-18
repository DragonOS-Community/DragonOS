use crate::arch::interrupt::TrapFrame;
use crate::arch::mm::LockedFrameAllocator;
use crate::arch::syscall::nr::SYS_SYSINFO;
use crate::mm::allocator::page_frame::FrameAllocator;
use crate::mm::allocator::slab::slab_usage;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use crate::time::uptime_secs;
use alloc::vec::Vec;
use system_error::SystemError;

/// 系统信息结构体
/// 这里的Posix的意思指的是对齐Linux的面向用户态的结构体。避免内核态把他混淆使用。
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/sysinfo.h#8
#[derive(Debug, Default, Copy, Clone)]
#[repr(C)]
struct PosixSysInfo {
    uptime: u64,
    loads: [u64; 3],
    totalram: u64,
    freeram: u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap: u64,
    procs: u16,
    pad: u16,
    totalhigh: u64,
    freehigh: u64,
    mem_unit: u32,
}

/// System call handler for the `sysinfo` syscall
///
/// This handler implements the `Syscall` trait to provide system information.
pub struct SysInfoHandle;

impl SysInfoHandle {
    /// Extracts the info pointer from syscall arguments
    fn info(args: &[usize]) -> *mut PosixSysInfo {
        args[0] as *mut PosixSysInfo
    }
}

impl Syscall for SysInfoHandle {
    /// Returns the number of arguments expected by the `sysinfo` syscall
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the `sysinfo` system call
    ///
    /// Returns system information including uptime, memory statistics, and process count.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Pointer to SysInfo struct (*mut SysInfo)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(0)` - Success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let info = Self::info(args);
        do_sysinfo(info)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "info",
            format!("{:#x}", Self::info(args) as usize),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYSINFO, SysInfoHandle);

/// Internal implementation of the sysinfo operation
///
/// # Arguments
/// * `info` - Pointer to SysInfo struct to fill
///
/// # Returns
/// * `Ok(0)` - Success
/// * `Err(SystemError)` - Error code if operation fails
fn do_sysinfo(info: *mut PosixSysInfo) -> Result<usize, SystemError> {
    let mut writer = UserBufferWriter::new(info, core::mem::size_of::<PosixSysInfo>(), true)?;
    let mut sysinfo = PosixSysInfo::default();

    let mem = unsafe { LockedFrameAllocator.usage() };
    let slab_usage = unsafe { slab_usage() };

    sysinfo.uptime = uptime_secs();
    sysinfo.loads = [0; 3];
    sysinfo.totalram = mem.total().bytes() as u64;
    sysinfo.freeram = mem.free().bytes() as u64 + slab_usage.free();
    sysinfo.sharedram = 0;
    sysinfo.bufferram = 0;
    sysinfo.totalswap = 0;
    sysinfo.freeswap = 0;
    sysinfo.procs = ProcessManager::current_pidns().pid_allocated() as u16;
    sysinfo.pad = 0;
    sysinfo.totalhigh = 0;
    sysinfo.freehigh = 0;
    sysinfo.mem_unit = 1;

    writer.buffer_protected(0)?.write_one(0, &sysinfo)?;

    Ok(0)
}
