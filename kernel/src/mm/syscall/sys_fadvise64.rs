use crate::arch::MMArch;
use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_FADVISE64};
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::FileType;
use crate::libs::align::page_align_up;
use crate::mm::readahead::{force_page_cache_readahead, MAX_READAHEAD};
use crate::mm::MemoryManagementArch;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::vec::Vec;

#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosixFadviseFlag {
    /// 正常访问，无特殊处理
    Normal = 0,
    /// 随机访问，禁用预读
    Random = 1,
    /// 顺序访问，激进预读
    Sequential = 2,
    /// 即将需要，触发预读
    WillNeed = 3,
    /// 不再需要，可丢弃缓存
    DontNeed = 4,
    /// 数据只访问一次
    NoReuse = 5,
    // 若以后要支持s390x架构，这里需要修改
}

impl PosixFadviseFlag {
    pub fn from_i32(value: i32) -> Result<Self, SystemError> {
        match value {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Random),
            2 => Ok(Self::Sequential),
            3 => Ok(Self::WillNeed),
            4 => Ok(Self::DontNeed),
            5 => Ok(Self::NoReuse),
            _ => Err(SystemError::EINVAL),
        }
    }
}

pub struct SysFadvise64Handle;

impl Syscall for SysFadvise64Handle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let offset = Self::offset(args);
        let len = Self::len(args);
        let advise = Self::advise(args);

        do_fadvise(fd, offset, len, advise)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args))),
            FormattedSyscallParam::new("offset", format!("{:#x}", Self::offset(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("advise", format!("{:#x}", Self::advise(args))),
        ]
    }
}

impl SysFadvise64Handle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn offset(args: &[usize]) -> i64 {
        args[1] as i64
    }

    fn len(args: &[usize]) -> i64 {
        args[2] as i64
    }

    fn advise(args: &[usize]) -> i32 {
        args[3] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_FADVISE64, SysFadvise64Handle);

pub fn do_fadvise(fd: i32, offset: i64, len: i64, advise: i32) -> Result<usize, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let file = pcb
        .fd_table()
        .read()
        .get_file_by_fd_not_raw(fd, FileMode::FMODE_PATH)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    if inode.metadata()?.file_type == FileType::Pipe {
        return Err(SystemError::ESPIPE);
    }

    // 根据POSIX规范，len == 0 表示从offset到文件结尾
    if len < 0 || inode.page_cache().is_none() {
        return Err(SystemError::EINVAL);
    }

    let res = inode.fadvise(&file, offset, len, advise);
    if res != Err(SystemError::ENOSYS) {
        return res;
    };

    // 后续需要考虑DAX和noop_backing_dev_info

    let mut endbyte = offset.saturating_add(len) as usize;
    if len == 0 {
        endbyte = usize::MAX;
    }

    match PosixFadviseFlag::from_i32(advise)? {
        PosixFadviseFlag::Normal => {
            file.set_ra_pages(MAX_READAHEAD);
            file.remove_mode_flags(FileMode::FMODE_RANDOM | FileMode::FMODE_NOREUSE);
        }
        PosixFadviseFlag::Random => {
            file.set_mode_flags(FileMode::FMODE_RANDOM);
        }
        PosixFadviseFlag::Sequential => {
            file.set_ra_pages(MAX_READAHEAD * 2);
            file.remove_mode_flags(FileMode::FMODE_RANDOM);
        }
        PosixFadviseFlag::WillNeed => {
            let start_page = (offset >> MMArch::PAGE_SHIFT) as usize;
            let end_page = (endbyte - 1) >> MMArch::PAGE_SHIFT;
            let mut page_num = end_page - start_page + 1;
            if page_num == 0 {
                page_num = usize::MAX;
            }

            let inode = file.inode();
            let page_cache = inode.page_cache().unwrap();
            let mut ra_state = file.get_ra_state();
            force_page_cache_readahead(&page_cache, &inode, &mut ra_state, start_page, page_num)?;
            file.set_ra_state(ra_state)?;
        }
        PosixFadviseFlag::NoReuse => {
            file.remove_mode_flags(FileMode::FMODE_NOREUSE);
        }
        PosixFadviseFlag::DontNeed => {
            let start_index = page_align_up(offset as usize) >> MMArch::PAGE_SHIFT;
            let mut end_index = endbyte >> MMArch::PAGE_SHIFT;

            // 如果要驱逐的最后一页不是整页，则需要保留
            if (endbyte & MMArch::PAGE_OFFSET_MASK) != MMArch::PAGE_OFFSET_MASK
                && endbyte != inode.metadata()?.size as usize - 1
            {
                if end_index == 0 {
                    return Ok(0);
                }
                end_index -= 1;
            }

            // 若以后实现了per-CPU LRU批处理，需要额外处理（lru_add_drain）

            if end_index >= start_index {
                let page_cache = inode.page_cache().unwrap();
                let mut page_cache_guard = page_cache.lock();
                // 先写回脏页
                page_cache_guard.writeback_range(start_index, end_index)?;
                // 再驱逐干净页
                page_cache_guard.invalidate_range(start_index, end_index);
            }
        }
    }

    Ok(0)
}
