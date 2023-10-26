use core::arch::x86_64::_rdtsc;

use alloc::vec::Vec;

use crate::syscall::{user_access::UserBufferWriter, Syscall, SystemError};

pub fn rand() -> usize {
    return unsafe { (_rdtsc() * _rdtsc() + 998244353_u64 * _rdtsc()) as usize };
}

impl Syscall {
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
                ret.push((rand >> offset * 2) as u8);
                count += 1;
            }
        }

        writer.copy_to_user(&ret, 0)?;
        Ok(len)
    }
}

bitflags! {
    pub struct GRandFlags: u8{
        const GRND_NONBLOCK = 0x0001;
        const GRND_RANDOM = 0x0002;
        const GRND_INSECURE = 0x0004;
    }
}
