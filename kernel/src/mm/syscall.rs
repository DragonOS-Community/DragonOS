use crate::{
    include::bindings::bindings::mm_stat_t,
    syscall::{Syscall, SystemError},
};

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
