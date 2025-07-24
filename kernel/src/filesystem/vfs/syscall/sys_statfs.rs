use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_STATFS;
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::syscall::open_utils;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::syscall::PosixStatfs;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::ROOT_INODE;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::check_and_clone_cstr;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysStatfsHandle;

impl Syscall for SysStatfsHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let user_statfs = Self::statfs(args);
        let mut writer = UserBufferWriter::new(user_statfs, size_of::<PosixStatfs>(), true)?;
        let fd = open_utils::do_open(
            path,
            FileMode::O_RDONLY.bits(),
            ModeType::empty().bits(),
            true,
        )?;
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let pcb = ProcessManager::current_pcb();
        let (_inode_begin, remain_path) = user_path_at(&pcb, fd as i32, &path)?;
        let inode = ROOT_INODE().lookup_follow_symlink(&remain_path, MAX_PATHLEN)?;
        let statfs = PosixStatfs::from(inode.fs().super_block());
        writer.copy_one_to_user(&statfs, 0)?;
        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("statfs", format!("{:#x}", Self::statfs(args) as usize)),
        ]
    }
}

impl SysStatfsHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
    fn statfs(args: &[usize]) -> *mut PosixStatfs {
        args[1] as *mut PosixStatfs
    }
}

syscall_table_macros::declare_syscall!(SYS_STATFS, SysStatfsHandle);
