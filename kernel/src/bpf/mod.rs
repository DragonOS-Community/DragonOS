mod cgroup_device;
pub mod classic;
pub mod helper;
pub mod map;
pub mod prog;
mod query;
mod sys_bpf;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::FileType;
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_cmd};
use crate::process::ProcessManager;
use core::mem::size_of;
use system_error::SystemError;

type Result<T> = core::result::Result<T, SystemError>;

fn attr_tail_zero(attr: &bpf_attr, used_end: usize) -> Result<()> {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (attr as *const bpf_attr).cast::<u8>(),
            size_of::<bpf_attr>(),
        )
    };
    if bytes[used_end..].iter().any(|byte| *byte != 0) {
        Err(SystemError::EINVAL)
    } else {
        Ok(())
    }
}

/// Evaluate the effective cgroup-v2 device policy for one VFS operation.
/// `access` uses the Linux DEVCG_ACC_* low bits; the device kind occupies the
/// low half of `access_type` and the access bits occupy its high half.
pub(crate) fn check_device_permission(
    kind: FileType,
    dev: DeviceNumber,
    access: u16,
) -> Result<()> {
    let device_type = match kind {
        FileType::BlockDevice => 1u32,
        FileType::CharDevice => 2u32,
        _ => return Ok(()),
    };
    if !ProcessManager::initialized() {
        return Ok(());
    }
    let request = prog::device::DeviceAccess {
        access_type: ((access as u32) << 16) | device_type,
        major: dev.major().data(),
        minor: dev.minor(),
    };
    if ProcessManager::current_pcb()
        .task_cgroup_node()
        .allows_device_access(request)
    {
        Ok(())
    } else {
        Err(SystemError::EPERM)
    }
}

pub fn bpf(cmd: bpf_cmd, attr: &bpf_attr, user_attr: *mut u8) -> Result<usize> {
    let res = match cmd {
        // Map related commands
        bpf_cmd::BPF_MAP_CREATE => map::bpf_map_create(attr),
        bpf_cmd::BPF_MAP_UPDATE_ELEM => map::bpf_map_update_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_ELEM => map::bpf_lookup_elem(attr),
        bpf_cmd::BPF_MAP_GET_NEXT_KEY => map::bpf_map_get_next_key(attr),
        bpf_cmd::BPF_MAP_DELETE_ELEM => map::bpf_map_delete_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_AND_DELETE_ELEM => map::bpf_map_lookup_and_delete_elem(attr),
        bpf_cmd::BPF_MAP_FREEZE => map::bpf_map_freeze(attr),
        // Program related commands
        bpf_cmd::BPF_PROG_LOAD => prog::bpf_prog_load(attr),
        bpf_cmd::BPF_PROG_ATTACH => cgroup_device::attach(attr),
        bpf_cmd::BPF_PROG_DETACH => cgroup_device::detach(attr),
        bpf_cmd::BPF_PROG_GET_FD_BY_ID => prog::get_fd_by_id(attr),
        bpf_cmd::BPF_OBJ_GET_INFO_BY_FD => prog::get_info_by_fd(attr, user_attr),
        bpf_cmd::BPF_PROG_QUERY => query::bpf_prog_query(attr, user_attr),
        _ => Err(SystemError::ENOSYS),
    };
    res
}

/// Initialize the BPF system
pub fn init_bpf_system() {
    helper::init_helper_functions();
}
