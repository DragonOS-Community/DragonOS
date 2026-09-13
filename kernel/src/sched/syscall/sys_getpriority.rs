use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_GETPRIORITY},
    sched::prio::PrioUtil,
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::util::resolve_prio_targets;

struct SysGetpriority;

/// Linux `SYSCALL_DEFINE2(getpriority)` (`kernel/sys.c`).
///
/// There is no permission check: any task may read any other task's nice value.
/// The result is the `RLIMIT_NICE` encoding `MAX_NICE - nice + 1` rather than
/// the nice value itself, so that it stays positive; libc's `getpriority()`
/// wrapper recovers the nice value by subtracting it from 20. That indirection
/// is why the wrapper can legitimately return `-1` for nice `-1` and leave
/// callers no way to tell it apart from an error except by clearing `errno`
/// first.
impl Syscall for SysGetpriority {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = args[0] as i32;
        let who = args[1] as i32;

        // Linux takes the maximum of the encoded values over the selected set,
        // which in this inverted encoding is the *lowest* nice value any member
        // holds.
        let mut highest = None;
        for target in resolve_prio_targets(which, who)? {
            let nice = PrioUtil::prio_to_nice(target.sched_info().static_prio());
            let encoded = PrioUtil::nice_to_rlimit(nice);
            highest = Some(highest.map_or(encoded, |previous: i64| previous.max(encoded)));
        }

        // `resolve_prio_targets()` never hands back an empty set, so this only
        // stands in for Linux's `retval = -ESRCH` initializer.
        highest
            .map(|encoded| encoded as usize)
            .ok_or(SystemError::ESRCH)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("who", (args[1] as i32).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPRIORITY, SysGetpriority);
