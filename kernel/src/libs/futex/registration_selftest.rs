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
        // Real WAIT values, unlike mismatch/expired prechecks, reach the
        // fallible preparation path. No bucket or timer may become visible.
        for timeout in [
            None,
            Some(PosixTimeSpec {
                tv_sec: 1,
                tv_nsec: 0,
            }),
        ] {
            if Futex::futex_wait(address, FutexFlag::empty(), 0, timeout, u32::MAX)
                != Err(SystemError::ENOMEM)
            {
                return Ok(false);
            }
            let key = Futex::get_futex_key(address, false, FutexAccess::FutexRead)?;
            if FutexData::futex_map().contains_key(&key) {
                return Ok(false);
            }
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
    result.and_then(|passed| Ok(passed && unmapped && prepared_stages()?))
}

/// Cover failures after earlier owners already exist, using the same factories
/// as WAIT. A first-Waker rejection alone cannot prove the later boundaries.
fn prepared_stages() -> Result<bool, SystemError> {
    let (_, waker) = Waiter::try_new_pair()?;
    let object = Arc::try_new(FutexObj {
        waker: waker.clone(),
        key: SpinLock::new(FutexKey {
            ptr: 0,
            word: 0,
            offset: 0,
            key: InnerFutexKey::Private(PrivateKey {
                address_space: None,
                address: 0,
            }),
        }),
        bitset: u32::MAX,
        tid: 0,
    })
    .map_err(|_| SystemError::ENOMEM)?;
    let callback = Box::try_new(WakerTimer {
        waker: waker.clone(),
    })
    .map_err(|_| SystemError::ENOMEM)?;
    let stages = {
        let _deny = denial::Scope::enter(true);
        denial::probe_denied()
            && Box::try_new(WakerTimer { waker }).is_err()
            && Arc::try_new(FutexObj {
                waker: object.waker.clone(),
                key: SpinLock::new(object.key.lock().clone()),
                bitset: u32::MAX,
                tid: 0,
            })
            .is_err()
            && WaitNode::try_new(object).is_err()
            && Timer::try_prepare(callback, crate::time::timer::clock() + 100).is_err()
            && denial::probe_denied()
    };
    Ok(stages && crate::time::timer::heap_denied_activation_selftest()?)
}
