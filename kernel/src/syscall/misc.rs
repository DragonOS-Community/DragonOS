use crate::{
    arch::mm::LockedFrameAllocator,
    mm::allocator::{page_frame::FrameAllocator, slab::slab_usage},
    process::ProcessManager,
    time::clocksource::HZ,
    time::timer::clock,
};
use system_error::SystemError;

use super::{user_access::UserBufferWriter, Syscall};

/// 系统信息
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/sysinfo.h#8
#[derive(Debug, Default, Copy, Clone)]
#[repr(C)]
pub struct SysInfo {
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
    // 这后面还有一小段，但是我们不需要
}

impl Syscall {
    /// ## 将系统信息写入info指向的用户 vma 中的结构体
    pub fn sysinfo(info: *mut SysInfo) -> Result<usize, SystemError> {
        // 在内核上创建一个 SysInfo 结构体，然后创建一个简单验证目标用户态缓冲区是否在用户地址空间内的 UserBufferWriter。
        let mut sysinfo = SysInfo::default();
        let mut writer = UserBufferWriter::new(info, core::mem::size_of::<SysInfo>(), true)?;

        // 填充 SysInfo 结构体
        let mem = unsafe { LockedFrameAllocator.usage() };
        let slab_usage = unsafe { slab_usage() };
        sysinfo.uptime = clock() / HZ;
        sysinfo.loads = [0; 3];
        sysinfo.totalram = mem.total().bytes() as u64;
        sysinfo.freeram = mem.free().bytes() as u64 + slab_usage.free();
        sysinfo.sharedram = 0;
        sysinfo.bufferram = 0;
        sysinfo.totalswap = 0;
        sysinfo.freeswap = 0;
        sysinfo.procs = ProcessManager::get_all_processes().len() as u16;
        sysinfo.pad = 0;
        sysinfo.totalhigh = 0;
        sysinfo.freehigh = 0;
        sysinfo.mem_unit = 1;

        // 将 SysInfo 结构体复制到用户缓冲区
        writer.copy_one_to_user(&sysinfo, 0)?;

        return Ok(0);
    }
}
