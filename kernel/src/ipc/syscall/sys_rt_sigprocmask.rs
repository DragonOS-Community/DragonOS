use crate::alloc::vec::Vec;
use crate::{
    arch::ipc::signal::{SigSet, Signal},
    arch::syscall::nr::SYS_RT_SIGPROCMASK,
    ipc::signal::{set_sigprocmask, SigHow},
    mm::VirtAddr,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};
use core::mem::size_of;
use syscall_table_macros::declare_syscall;
use system_error::SystemError; // 添加 Vec

pub struct SysRtSigprocmaskHandle;

/// # SYS_SIGPROCMASK系统调用函数，用于设置或查询当前进程的信号屏蔽字
///
/// ## 参数
///
/// - `how`: 指示如何修改信号屏蔽字
/// - `nset`: 新的信号屏蔽字
/// - `oset`: 旧的信号屏蔽字的指针，由于可以是NULL，所以用Option包装
/// - `sigsetsize`: 信号集的大小
///
/// ## 返回值
///
/// 成功：0
/// 失败：错误码
///
/// ## 说明
/// 根据 https://man7.org/linux/man-pages/man2/sigprocmask.2.html ，传进来的oldset和newset都是指针类型，这里选择传入usize然后转换为u64的指针类型
pub(super) fn do_kernel_rt_sigprocmask(
    how: i32,
    newset: usize,
    oldset: usize,
    sigsetsize: usize,
) -> Result<usize, SystemError> {
    // 对应oset传进来一个NULL的情况
    let oset = if oldset == 0 { None } else { Some(oldset) };
    let nset = if newset == 0 { None } else { Some(newset) };

    if sigsetsize != size_of::<SigSet>() {
        return Err(SystemError::EFAULT);
    }

    let sighow = SigHow::try_from(how)?;

    let mut new_set = SigSet::default();
    if let Some(nset) = nset {
        let reader = UserBufferReader::new(
            VirtAddr::new(nset).as_ptr::<u64>(),
            core::mem::size_of::<u64>(),
            true,
        )?;

        let nset = reader.read_one_from_user::<u64>(0)?;
        new_set = SigSet::from_bits_truncate(*nset);
        // debug!("Get Newset: {}", &new_set.bits());
        let to_remove: SigSet =
            <Signal as Into<SigSet>>::into(Signal::SIGKILL) | Signal::SIGSTOP.into();
        new_set.remove(to_remove);
    }

    let oldset_to_return = set_sigprocmask(sighow, new_set)?;
    if let Some(oldset) = oset {
        // debug!("Get Oldset to return: {}", &oldset_to_return.bits());
        let mut writer = UserBufferWriter::new(
            VirtAddr::new(oldset).as_ptr::<u64>(),
            core::mem::size_of::<u64>(),
            true,
        )?;
        writer.copy_one_to_user::<u64>(&oldset_to_return.bits(), 0)?;
    }

    Ok(0)
}

impl SysRtSigprocmaskHandle {
    #[inline(always)]
    fn how(args: &[usize]) -> i32 {
        // 第一个参数是 how
        args[0] as i32
    }

    #[inline(always)]
    fn nset(args: &[usize]) -> usize {
        // 第二个参数是新信号集的指针
        args[1]
    }

    #[inline(always)]
    fn oset(args: &[usize]) -> usize {
        // 第三个参数是旧信号集的指针
        args[2]
    }

    #[inline(always)]
    fn sigsetsize(args: &[usize]) -> usize {
        // 第四个参数是 sigset_t 的大小
        args[3]
    }
}

impl Syscall for SysRtSigprocmaskHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let how = Self::how(args);
        let nset_ptr = Self::nset(args);
        let oset_ptr = Self::oset(args);
        let sigsetsize = Self::sigsetsize(args);

        vec![
            FormattedSyscallParam::new("how", format!("{:#x}", how)),
            FormattedSyscallParam::new("nset", format!("{:#x}", nset_ptr)),
            FormattedSyscallParam::new("oset", format!("{:#x}", oset_ptr)),
            FormattedSyscallParam::new("sigsetsize", format!("{}", sigsetsize)),
        ]
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let how = Self::how(args);
        let nset = Self::nset(args);
        let oset = Self::oset(args);
        let sigsetsize = Self::sigsetsize(args);

        do_kernel_rt_sigprocmask(how, nset, oset, sigsetsize)
    }
}

declare_syscall!(SYS_RT_SIGPROCMASK, SysRtSigprocmaskHandle);
