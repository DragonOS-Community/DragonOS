use crate::{
    include::bindings::bindings::PROC_MAX_FD_NUM,
    libs::nolibc::FdSet,
    mm::VirtAddr,
    syscall::{
        user_access::{copy_from_user, copy_to_user},
        Syscall, SystemError,
    },
    time::TimeSpec,
};

struct FdSetBits {
    lis_in: FdSet,
    lis_out: FdSet,
    lis_ex: FdSet,
    res_in: FdSet,
    res_out: FdSet,
    res_ex: FdSet,
}

impl FdSetBits {
    pub fn new() -> Self {
        Self {
            lis_in: FdSet::new(),
            lis_out: FdSet::new(),
            lis_ex: FdSet::new(),
            res_in: FdSet::new(),
            res_out: FdSet::new(),
            res_ex: FdSet::new(),
        }
    }

    /// @brief 从用户空间拷贝 fd_set 到内核空间
    pub fn get_fd_sets(
        &mut self,
        n: i32,
        inp: *const FdSet,
        outp: *const FdSet,
        exp: *const FdSet,
    ) -> Result<(), SystemError> {
        let vinp = VirtAddr::new(inp as usize);
        let voutp = VirtAddr::new(outp as usize);
        let vexp = VirtAddr::new(exp as usize);

        unsafe {
            if !inp.is_null() {
                copy_from_user(self.lis_in.data(), vinp).map_err(|_| SystemError::EINVAL)?;
            }
            if !outp.is_null() {
                copy_from_user(self.lis_out.data(), voutp).map_err(|_| SystemError::EINVAL)?;
            }
            if !exp.is_null() {
                copy_from_user(self.lis_ex.data(), vexp).map_err(|_| SystemError::EINVAL)?;
            }
        }

        return Ok(());
    }

    /// @brief 从内核空间拷贝 fd_set 到用户空间
    pub fn set_fd_sets(
        &mut self,
        n: i32,
        inp: *const FdSet,
        outp: *const FdSet,
        exp: *const FdSet,
    ) -> Result<(), SystemError> {
        let vinp = VirtAddr::new(inp as usize);
        let voutp = VirtAddr::new(outp as usize);
        let vexp = VirtAddr::new(exp as usize);

        unsafe {
            if !inp.is_null() {
                copy_to_user(vinp, &self.res_in.data())?;
            }
            if !outp.is_null() {
                copy_to_user(voutp, &self.res_out.data())?;
            }
            if !exp.is_null() {
                copy_to_user(vexp, &self.res_ex.data())?;
            }
        }

        return Ok(());
    }
}

impl Syscall {
    /// @ brief 将用户态的 fd_set 复制到内核空间，在调用 do_select 处理完成后，将获取的 fd_set 复制回用户空间
    ///
    /// @param n 最大的文件描述符
    pub fn core_select(
        mut n: i32,
        inp: *const FdSet,
        outp: *const FdSet,
        exp: *const FdSet,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        if n < 0 {
            return Err(SystemError::EINVAL);
        }

        // 判断当前传进的最大文件描述符是否超过位图的上限，超过则修正
        if n > PROC_MAX_FD_NUM as i32 {
            n = PROC_MAX_FD_NUM as i32
        }

        // TODO: 可以根据最大文件描述符数 n 来高效利用空间

        let mut fds = FdSetBits::new();
        // 复制用户的 fd_set 到内核
        fds.get_fd_sets(n, inp, outp, exp)?;
        // 进入 do_select 处理
        Self::do_select(n, fds, end_time)?;
        // 将结果的 fd_set 复制回用户空间
        fds.set_fd_sets(n, inp, outp, exp)?;

        return Ok(0);
    }

    /// @brief 系统调用核心函数
    /// TODO: 增加超时判断逻辑
    /// TODO: 使用等待队列实现
    pub fn do_select(
        mut n: i32,
        fds: FdSetBits,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        // TODO: 如果当前进程已打开的文件描述符表检查目前打开的最大 fd，并修正传入的最大文件描述符数 n
    }
}
