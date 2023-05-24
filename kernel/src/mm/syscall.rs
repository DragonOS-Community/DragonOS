use crate::{
    include::bindings::bindings::mm_stat_t,
    syscall::{Syscall, SystemError},
};
bitflags! {
    /// Memory protection flags
    pub struct ProtFlags: u64 {
        const PROT_NONE = 0x0;
        const PROT_READ = 0x1;
        const PROT_WRITE = 0x2;
        const PROT_EXEC = 0x4;
    }

    /// Memory mapping flags
    pub struct MapFlags: u64 {
        const MAP_NONE = 0x0;
        /// share changes
        const MAP_SHARED = 0x1;
        /// changes are private
        const MAP_PRIVATE = 0x2;
        /// Interpret addr exactly
        const MAP_FIXED = 0x10;
        /// don't use a file
        const MAP_ANONYMOUS = 0x20;
        // linux-6.1-rc5/include/uapi/asm-generic/mman.h#7
        /// stack-like segment
        const MAP_GROWSDOWN = 0x100;
        /// ETXTBSY
        const MAP_DENYWRITE = 0x800;
        /// Mark it as an executable
        const MAP_EXECUTABLE = 0x1000;
        /// Pages are locked
        const MAP_LOCKED = 0x2000;
        /// don't check for reservations
        const MAP_NORESERVE = 0x4000;
        /// populate (prefault) pagetables
        const MAP_POPULATE = 0x8000;
        /// do not block on IO
        const MAP_NONBLOCK = 0x10000;
        /// give out an address that is best suited for process/thread stacks
        const MAP_STACK = 0x20000;
        /// create a huge page mapping
        const MAP_HUGETLB = 0x40000;
        /// perform synchronous page faults for the mapping
        const MAP_SYNC = 0x80000;
        /// MAP_FIXED which doesn't unmap underlying mapping
        const MAP_FIXED_NOREPLACE = 0x100000;

        /// For anonymous mmap, memory could be uninitialized
        const MAP_UNINITIALIZED = 0x4000000;

    }
}
extern "C" {
    fn sys_do_brk(new_addr: usize) -> usize;
    fn sys_do_sbrk(incr: isize) -> usize;
    fn sys_do_mstat(dst: *mut mm_stat_t, from_user: bool) -> usize;
}
impl Syscall {
    pub fn brk(new_addr: usize) -> Result<usize, SystemError> {
        let ret = unsafe { sys_do_brk(new_addr) };
        if (ret as isize) < 0 {
            return Err(
                SystemError::from_posix_errno(-(ret as isize) as i32).expect("brk: Invalid errno")
            );
        }
        return Ok(ret);
    }

    pub fn sbrk(incr: isize) -> Result<usize, SystemError> {
        let ret = unsafe { sys_do_sbrk(incr) };
        if (ret as isize) < 0 {
            return Err(
                SystemError::from_posix_errno(-(ret as isize) as i32).expect("sbrk: Invalid errno")
            );
        }
        return Ok(ret);
    }

    /// 获取内存统计信息
    ///
    /// TODO: 该函数不是符合POSIX标准的，在将来需要删除！
    pub fn mstat(dst: *mut mm_stat_t, from_user: bool) -> Result<usize, SystemError> {
        let ret = unsafe { sys_do_mstat(dst, from_user) };
        if (ret as isize) < 0 {
            return Err(SystemError::from_posix_errno(-(ret as isize) as i32)
                .expect("mstat: Invalid errno"));
        }
        return Ok(ret);
    }
}
