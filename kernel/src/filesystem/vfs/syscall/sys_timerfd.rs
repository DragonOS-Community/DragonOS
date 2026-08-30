use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::mem::size_of;

use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_TIMERFD_CREATE, SYS_TIMERFD_GETTIME, SYS_TIMERFD_SETTIME},
    },
    filesystem::{
        timerfd::{TimerFdCreateFlags, TimerFdInode, TimerFdSettimeFlags},
        vfs::file::{File, FileFlags, FilePrivateData},
    },
    libs::casting::DowncastArc,
    process::{
        cred::{capable, CAPFlags},
        posix_timer::PosixItimerspec,
        ProcessManager,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::syscall::PosixClockID,
};

fn valid_clock(clock_id: PosixClockID) -> bool {
    matches!(
        clock_id,
        PosixClockID::Realtime
            | PosixClockID::Monotonic
            | PosixClockID::Boottime
            | PosixClockID::RealtimeAlarm
            | PosixClockID::BoottimeAlarm
    )
}

fn check_alarm_capability(clock_id: PosixClockID) -> Result<(), SystemError> {
    if matches!(
        clock_id,
        PosixClockID::RealtimeAlarm | PosixClockID::BoottimeAlarm
    ) && !capable(CAPFlags::CAP_WAKE_ALARM)
    {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

fn timerfd_fget(fd: i32) -> Result<(Arc<File>, Arc<TimerFdInode>), SystemError> {
    let table = ProcessManager::current_pcb().fd_table();
    let file = table.read().get_file_by_fd(fd).ok_or(SystemError::EBADF)?;
    let inode = file
        .inode()
        .downcast_arc::<TimerFdInode>()
        .ok_or(SystemError::EINVAL)?;
    Ok((file, inode))
}

pub struct SysTimerFdCreate;

impl Syscall for SysTimerFdCreate {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let clock_id = PosixClockID::try_from(args[0] as i32)?;
        if !valid_clock(clock_id) {
            return Err(SystemError::EINVAL);
        }
        let flags = TimerFdCreateFlags::from_bits(args[1] as u32).ok_or(SystemError::EINVAL)?;
        check_alarm_capability(clock_id)?;

        let mut file_flags = FileFlags::O_RDWR;
        if flags.contains(TimerFdCreateFlags::TFD_NONBLOCK) {
            file_flags.insert(FileFlags::O_NONBLOCK);
        }
        let inode = TimerFdInode::new(clock_id);
        let file =
            File::new_with_private_data(inode, file_flags, FilePrivateData::TimerFd(file_flags))?;
        let cloexec = flags.contains(TimerFdCreateFlags::TFD_CLOEXEC);
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(file, None, cloexec)
            .map(|fd| fd as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("clockid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_TIMERFD_CREATE, SysTimerFdCreate);

pub struct SysTimerFdSettime;

impl Syscall for SysTimerFdSettime {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let new_ptr = args[2] as *const PosixItimerspec;
        if new_ptr.is_null() {
            return Err(SystemError::EFAULT);
        }
        let reader = UserBufferReader::new(new_ptr, size_of::<PosixItimerspec>(), true)?;
        let value = reader.read_one_from_user::<PosixItimerspec>(0)?;
        TimerFdInode::validate_spec(&value)?;
        let flags = TimerFdSettimeFlags::from_bits(args[1] as u32).ok_or(SystemError::EINVAL)?;

        let (_file, inode) = timerfd_fget(args[0] as i32)?;
        if inode.is_alarm() {
            check_alarm_capability(PosixClockID::RealtimeAlarm)?;
        }
        let old = inode.settime(flags, value)?;

        let old_ptr = args[3] as *mut PosixItimerspec;
        if !old_ptr.is_null() {
            let mut writer = UserBufferWriter::new(old_ptr, size_of::<PosixItimerspec>(), true)?;
            writer.copy_one_to_user(&old, 0)?;
        }
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("new_value", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("old_value", format!("{:#x}", args[3])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_TIMERFD_SETTIME, SysTimerFdSettime);

pub struct SysTimerFdGettime;

impl Syscall for SysTimerFdGettime {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let (_file, inode) = timerfd_fget(args[0] as i32)?;
        let value = inode.gettime()?;
        let value_ptr = args[1] as *mut PosixItimerspec;
        if value_ptr.is_null() {
            return Err(SystemError::EFAULT);
        }
        let mut writer = UserBufferWriter::new(value_ptr, size_of::<PosixItimerspec>(), true)?;
        writer.copy_one_to_user(&value, 0)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("curr_value", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_TIMERFD_GETTIME, SysTimerFdGettime);
