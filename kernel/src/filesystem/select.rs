use alloc::vec::Vec;

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::PollStatus,
    include::bindings::bindings::PROC_MAX_FD_NUM,
    mm::{verify_area, VirtAddr},
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

const FD_SET_SIZE: usize = 1024;
const FD_SET_IDX_MASK: usize = 8 * core::mem::size_of::<u64>();
const FD_SET_BIT_MASK: usize = FD_SET_IDX_MASK - 1;
const FD_SET_LONGS: usize = (FD_SET_SIZE + FD_SET_BIT_MASK) / FD_SET_IDX_MASK;

/// @brief select() 使用的文件描述符集合
#[repr(C)]
pub struct FdSet([u64; FD_SET_LONGS]);

impl FdSet {
    pub fn new() -> Self {
        Self([0; FD_SET_LONGS])
    }

    /// @brief 清除 fd_set 中指定的 fd 位
    pub fn clear_fd(&mut self, fd: i32) {
        if fd >= 0 {
            let fd = fd as usize;
            let index = fd / FD_SET_IDX_MASK;
            if let Some(x) = self.0.get_mut(index) {
                *x &= !(1 << (fd & FD_SET_BIT_MASK));
            }
        }
    }

    /// @brief 设置 fd_set 中指定的 fd 位
    pub fn set_fd(&mut self, fd: i32) {
        if fd >= 0 {
            let fd = fd as usize;
            let index = fd / FD_SET_IDX_MASK;
            if let Some(x) = self.0.get_mut(index) {
                *x |= 1 << (fd & FD_SET_BIT_MASK);
            }
        }
    }

    /// @brief 判断 fd_set 中是否存在指定的 fd 位
    pub fn contains_fd(&self, fd: i32) -> bool {
        if fd >= 0 {
            let fd = fd as usize;
            let index = fd / FD_SET_IDX_MASK;
            if let Some(x) = self.0.get(index) {
                return (*x & (1 << (fd & FD_SET_BIT_MASK))) != 0;
            }
        }
        return false;
    }

    /// @brief 清空 fd_set 中所有的 fd
    pub fn zero_fds(&mut self) {
        for x in self.0.iter_mut() {
            *x = 0;
        }
    }
}

#[repr(C)]
/// @ brief poll() 使用的文件描述符结构体
pub struct PollFd {
    fd: i32,
    events: PollStatus,
    revents: PollStatus,
}

impl PollFd {
    pub fn new(fd: i32, events: PollStatus) -> Self {
        Self {
            fd,
            events,
            revents: PollStatus::empty(),
        }
    }

    fn set_revents(&mut self, mask: PollStatus) {
        self.revents = mask;
    }
}

// TODO: 使用更加节省空间的设计
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
        copy_fd_set(inp, &mut self.lis_in as *mut FdSet, CopyOp::FromUser)
            .map_err(|_| SystemError::EINVAL)?;
        copy_fd_set(outp, &mut self.lis_out as *mut FdSet, CopyOp::FromUser)
            .map_err(|_| SystemError::EINVAL)?;
        copy_fd_set(exp, &mut self.lis_ex as *mut FdSet, CopyOp::FromUser)
            .map_err(|_| SystemError::EINVAL)?;
        return Ok(());
    }

    /// @brief 从内核空间拷贝 fd_set 到用户空间
    pub fn set_fd_sets(
        &self,
        n: i32,
        inp: *mut FdSet,
        outp: *mut FdSet,
        exp: *mut FdSet,
    ) -> Result<(), SystemError> {
        copy_fd_set(&self.res_in as *const FdSet, inp, CopyOp::ToUser)?;
        copy_fd_set(&self.res_out as *const FdSet, outp, CopyOp::ToUser)?;
        copy_fd_set(&self.res_ex as *const FdSet, exp, CopyOp::ToUser)?;
        return Ok(());
    }
}

/// @brief 在内核空间使用 vector 来表示 pollfd 数组
struct PollList(Vec<PollFd>);

impl PollList {
    pub fn new() -> Self {
        Self(vec![])
    }

    /// @brief 从用户空间拷贝 pollfd 数组到内核空间
    pub fn get_pollfds(&mut self, ufds: *const PollFd, nfds: usize) -> Result<(), SystemError> {
        // 更新 poll_list 的容量和长度
        self.0
            .resize_with(nfds, || PollFd::new(-1, PollStatus::empty()));
        copy_pollfds(ufds, self.0.as_mut_ptr(), nfds, CopyOp::FromUser)?;
        return Ok(());
    }

    /// @brief 从内核空间拷贝 pollfd 数组到用户空间
    pub fn set_pollfds(&self, ufds: *mut PollFd, nfds: usize) -> Result<(), SystemError> {
        copy_pollfds(self.0.as_ptr(), ufds, nfds, CopyOp::ToUser)?;
        return Ok(());
    }
}

enum CopyOp {
    FromUser,
    ToUser,
}

/// @brief 在内核和用户空间之间进行 fd_set 的复制
fn copy_fd_set(src: *const FdSet, dst: *mut FdSet, op: CopyOp) -> Result<(), SystemError> {
    let usr_addr = match op {
        CopyOp::FromUser => src,
        CopyOp::ToUser => dst,
    };

    let vaddr = VirtAddr::new(usr_addr as usize);
    verify_area(vaddr, core::mem::size_of::<FdSet>())?;
    if !src.is_null() {
        unsafe {
            core::ptr::copy(src, dst, 1);
        }
    }
    return Ok(());
}

/// @brief 在内核和用户空间之间进行 pollfd 的复制
fn copy_pollfds(
    src: *const PollFd,
    dst: *mut PollFd,
    count: usize,
    op: CopyOp,
) -> Result<(), SystemError> {
    let usr_addr = match op {
        CopyOp::FromUser => src,
        CopyOp::ToUser => dst,
    };

    let vaddr = VirtAddr::new(usr_addr as usize);
    verify_area(vaddr, count * core::mem::size_of::<PollFd>())?;
    if !src.is_null() {
        unsafe {
            core::ptr::copy(src, dst, count);
        }
    }
    return Ok(());
}

impl Syscall {
    // @brief 系统调用 select 的入口函数
    ///
    /// @param n 最大的文件描述符+1
    ///
    // TODO: 将时间复制到内核，增加超时机制
    pub fn select(
        n: i32,
        read_fds: *mut FdSet,
        write_fds: *mut FdSet,
        except_fds: *mut FdSet,
        end_time: TimeSpec,
    ) -> Result<usize, SystemError> {
        return Self::core_select(n, read_fds, write_fds, except_fds, None);
    }

    /// @ brief 将用户态的 fd_set 复制到内核空间，在调用 do_select 处理完成后，将获取的 fd_set 复制回用户空间
    ///
    /// @param n 最大的文件描述符+1
    ///
    /// TODO: 增加超时判断逻辑
    fn core_select(
        mut n: i32,
        read_fds: *mut FdSet,
        write_fds: *mut FdSet,
        except_fds: *mut FdSet,
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
        // 拷贝参数的 fd_set 到内核空间
        fds.get_fd_sets(n, read_fds, write_fds, except_fds)?;
        // 进入 do_select 处理
        let retval = Self::do_select(n, &mut fds, end_time)?;
        // 将结果的 fd_set 拷贝回用户空间
        fds.set_fd_sets(n, read_fds, write_fds, except_fds)?;

        return Ok(retval);
    }

    /// @brief select 系统调用核心函数
    ///
    /// TODO: 增加超时判断逻辑
    /// TODO: 使用等待队列实现
    fn do_select(
        mut n: i32,
        fds: &mut FdSetBits,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        // TODO: 如果当前进程已打开的文件描述符表检查目前打开的最大 fd，并修正传入的最大文件描述符数 n

        let mut retval = 0;
        loop {
            let mut fd = 0; // 文件描述符
            for i in 0..FD_SET_LONGS {
                if fd >= n {
                    break;
                }

                // 先以 64 位宽进行扫描，加快速度
                let in_bits = fds.lis_in.0[i];
                let out_bits = fds.lis_out.0[i];
                let ex_bits = fds.lis_ex.0[i];
                let all_bits = in_bits | out_bits | ex_bits;
                if all_bits == 0 {
                    fd += FD_SET_BIT_MASK as i32;
                    continue;
                }

                // 现在逐位扫描判断
                for j in 0..FD_SET_IDX_MASK {
                    if fd >= n {
                        break;
                    }

                    // 跳过无需监听的描述符
                    let bit = 1 << j;
                    if (bit & all_bits) == 0 {
                        fd += 1;
                        continue;
                    }

                    let mut mask: PollStatus = PollStatus::empty();
                    let cur = current_pcb();
                    if let Some(file) = cur.get_file_ref_by_fd(fd) {
                        mask = match file.inode().poll() {
                            Ok(status) => status,
                            Err(_) => PollStatus::empty(),
                        }
                    }

                    if mask.contains(PollStatus::READ) && fds.lis_in.contains_fd(fd) {
                        fds.res_in.set_fd(fd);
                        retval += 1;
                    }
                    if mask.contains(PollStatus::WRITE) && fds.lis_out.contains_fd(fd) {
                        fds.res_out.set_fd(fd);
                        retval += 1;
                    }
                    if mask.contains(PollStatus::ERROR) && fds.lis_ex.contains_fd(fd) {
                        fds.res_ex.set_fd(fd);
                        retval += 1;
                    }

                    fd += 1;
                }
            }
            if retval != 0 {
                break;
            }
        }
        return Ok(retval);
    }

    /// @ brief 将用户态的 pollfd 数组复制到内核空间，在调用 do_poll 处理完成后，将获取的 pollfd 数组复制回用户空间
    ///
    /// @parme nfds pollfd 数组长度
    ///
    /// TODO: 增加超时判断逻辑
    /// TODO: 使用等待队列实现
    fn core_poll(
        ufds: *mut PollFd,
        nfds: usize,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        // TODO: 判断 nfds 是否超过进程可以使用的文件描述符上限

        // 创建 poll_list 在内核空间存储 pollfd
        let mut poll_list = PollList::new();
        // 复制参数的 pollfd 数组到内核空间
        poll_list.get_pollfds(ufds, nfds)?;
        // 进入 do_poll 处理
        let retval = Self::do_poll(&mut poll_list, end_time)?;
        // 将结果的 pollfd 数组复制回用户空间
        poll_list.set_pollfds(ufds, nfds)?;

        return Ok(retval);
    }

    ///@brief poll 系统调用核心函数
    ///
    /// TODO: 增加超时判断逻辑
    /// TODO: 使用等待队列实现
    fn do_poll(poll_list: &mut PollList, end_time: Option<TimeSpec>) -> Result<usize, SystemError> {
        Ok(0)
    }
}
