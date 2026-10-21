//! Exercise failed registration through the production WAIT path. The backing
//! mapping is prefaulted before heap denial; no global futex state is reset.
use super::*;
use crate::mm::{
    allocator::page_frame::{PageFrameCount, VirtPageFrame},
    oom::denial,
    syscall::{MapFlags, ProtFlags},
};

pub(super) fn run() -> Result<bool, SystemError> {
    let mm = AddressSpace::current()?;
    let size = 4 * MMArch::PAGE_SIZE;
    let address = mm
        .map_anonymous_wait(
            VirtAddr::new(0),
            size,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            true,
            false,
        )?
        .virt_address();
    let result = (|| {
        let mut writer = UserBufferWriter::new(address.as_ptr::<u32>(), size, true)?;
        // Fault every page before entering the allocator-denied scope.
        for offset in (0..size).step_by(MMArch::PAGE_SIZE) {
            writer.copy_one_to_user(&0u32, offset)?;
        }
        let _scope = denial::Scope::enter(true);
        if !denial::probe_denied() {
            return Ok(false);
        }
        for offset in (0..size).step_by(4) {
            let uaddr = address + offset;
            if Futex::futex_wait(uaddr, FutexFlag::empty(), 1, None, u32::MAX)
                != Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            {
                return Ok(false);
            }
            if Futex::futex_wait(
                uaddr,
                FutexFlag::empty(),
                0,
                Some(PosixTimeSpec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
                u32::MAX,
            ) != Err(SystemError::ETIMEDOUT)
            {
                return Ok(false);
            }
            let key = Futex::get_futex_key(uaddr, false, FutexAccess::FutexRead)?;
            if FutexData::futex_map().contains_key(&key) {
                return Ok(false);
            }
        }
        Ok(denial::probe_denied())
    })();
    let result = result.and_then(|passed| {
        if !passed {
            return Ok(false);
        }
        // Real registered waits must retire their last bucket on timeout, too.
        for offset in (0..128).step_by(4) {
            let uaddr = address + offset;
            if Futex::futex_wait(
                uaddr,
                FutexFlag::empty(),
                0,
                Some(PosixTimeSpec {
                    tv_sec: 0,
                    tv_nsec: 1_000_000,
                }),
                u32::MAX,
            ) != Err(SystemError::ETIMEDOUT)
            {
                return Ok(false);
            }
            let key = Futex::get_futex_key(uaddr, false, FutexAccess::FutexRead)?;
            if FutexData::futex_map().contains_key(&key) {
                return Ok(false);
            }
        }
        Ok(true)
    });
    let unmapped = mm
        .munmap_wait(VirtPageFrame::new(address), PageFrameCount::new(4))
        .is_ok();
    result.map(|passed| passed && unmapped)
}
