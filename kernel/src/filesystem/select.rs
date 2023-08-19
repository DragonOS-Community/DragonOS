use crate::{
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
            copy_from_user(self.lis_in.data(), vinp).map_err(|_| SystemError::EINVAL)?;
            copy_from_user(self.lis_out.data(), voutp).map_err(|_| SystemError::EINVAL)?;
            copy_from_user(self.lis_ex.data(), vexp).map_err(|_| SystemError::EINVAL)?;
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
            copy_to_user(vinp, &self.res_in.data())?;
            copy_to_user(voutp, &self.res_out.data())?;
            copy_to_user(vexp, &self.res_ex.data())?;
        }

        return Ok(());
    }
}

impl Syscall {
    /// @ brief 将用户态的 fd_set 复制到内核空间，在调用 do_select 处理完成后，将获取的 fd_set 复制回用户空间
    ///
    /// @param n 最大的文件描述符
    pub fn core_select(
        n: i32,
        inp: *const FdSet,
        outp: *const FdSet,
        exp: *const FdSet,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        if n < 0 {
            return Err(SystemError::EINVAL);
        }

        // TODO: 进程的 max_fd 可能会变化
        // TODO: 可以根据 n 来高效利用空间

        let mut fds = FdSetBits::new();
        // 复制用户的 fd_set 到内核
        fds.get_fd_sets(n, inp, outp, exp)?;
        // 进入 do_select 处理
        Self::do_select(n, fds, end_time);
        // 将结果的 fd_set 复制回用户空间
        fds.set_fd_sets(n, inp, outp, exp)?;

        return Ok(0);
    }

    /// @brief 系统调用核心函数
    pub fn do_select(
        n: i32,
        fds: FdSetBits,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
    }
}
