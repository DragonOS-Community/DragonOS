use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{mm::LockedFrameAllocator, rand::rand},
    libs::rand::GRandFlags,
    mm::allocator::page_frame::FrameAllocator,
};

use super::{user_access::UserBufferWriter, Syscall};

#[repr(C)]

/// 系统信息
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/sysinfo.h#8
#[derive(Debug, Default, Copy, Clone)]
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
    pub fn sysinfo(info: *mut SysInfo) -> Result<usize, SystemError> {
        let mut writer = UserBufferWriter::new(info, core::mem::size_of::<SysInfo>(), true)?;
        let mut sysinfo = SysInfo::default();

        let mem = unsafe { LockedFrameAllocator.usage() };
        sysinfo.uptime = 0;
        sysinfo.loads = [0; 3];
        sysinfo.totalram = mem.total().bytes() as u64;
        sysinfo.freeram = mem.free().bytes() as u64;
        sysinfo.sharedram = 0;
        sysinfo.bufferram = 0;
        sysinfo.totalswap = 0;
        sysinfo.freeswap = 0;
        sysinfo.procs = 0;
        sysinfo.pad = 0;
        sysinfo.totalhigh = 0;
        sysinfo.freehigh = 0;
        sysinfo.mem_unit = 0;

        writer.copy_one_to_user(&sysinfo, 0)?;

        return Ok(0);
    }

    pub fn umask(_mask: u32) -> Result<usize, SystemError> {
        kwarn!("SYS_UMASK has not yet been implemented\n");
        return Ok(0o777);
    }

    /// ## 将随机字节填入buf
    ///
    /// ### 该系统调用与linux不一致，因为目前没有其他随机源
    pub fn get_random(buf: *mut u8, len: usize, flags: GRandFlags) -> Result<usize, SystemError> {
        if flags.bits() == (GRandFlags::GRND_INSECURE.bits() | GRandFlags::GRND_RANDOM.bits()) {
            return Err(SystemError::EINVAL);
        }

        let mut writer = UserBufferWriter::new(buf, len, true)?;

        let mut ret = Vec::new();
        let mut count = 0;
        while count < len {
            let rand = rand();
            for offset in 0..4 {
                ret.push((rand >> (offset * 2)) as u8);
                count += 1;
            }
        }

        writer.copy_to_user(&ret, 0)?;
        Ok(len)
    }
}
