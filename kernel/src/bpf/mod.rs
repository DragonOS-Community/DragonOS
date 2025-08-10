pub mod helper;
pub mod map;
pub mod prog;
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_cmd};
use crate::syscall::user_access::UserBufferReader;
use crate::syscall::Syscall;
use log::error;
use num_traits::FromPrimitive;
use system_error::SystemError;

type Result<T> = core::result::Result<T, SystemError>;

impl Syscall {
    pub fn sys_bpf(cmd: u32, attr: *mut u8, size: u32) -> Result<usize> {
        let buf = UserBufferReader::new(attr, size as usize, true)?;
        let attr = buf.read_one_from_user::<bpf_attr>(0)?;
        let cmd = bpf_cmd::from_u32(cmd).ok_or(SystemError::EINVAL)?;
        bpf(cmd, attr)
    }
}

pub fn bpf(cmd: bpf_cmd, attr: &bpf_attr) -> Result<usize> {
    let res = match cmd {
        // Map related commands
        bpf_cmd::BPF_MAP_CREATE => map::bpf_map_create(attr),
        bpf_cmd::BPF_MAP_UPDATE_ELEM => map::bpf_map_update_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_ELEM => map::bpf_lookup_elem(attr),
        bpf_cmd::BPF_MAP_GET_NEXT_KEY => map::bpf_map_get_next_key(attr),
        bpf_cmd::BPF_MAP_DELETE_ELEM => map::bpf_map_delete_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_AND_DELETE_ELEM => map::bpf_map_lookup_and_delete_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_BATCH => map::bpf_map_lookup_batch(attr),
        bpf_cmd::BPF_MAP_FREEZE => map::bpf_map_freeze(attr),
        // Program related commands
        bpf_cmd::BPF_PROG_LOAD => prog::bpf_prog_load(attr),
        // Object creation commands
        bpf_cmd::BPF_BTF_LOAD | bpf_cmd::BPF_LINK_CREATE | bpf_cmd::BPF_OBJ_GET_INFO_BY_FD => {
            error!("bpf cmd {:?} not implemented", cmd);
            return Err(SystemError::ENOSYS);
        }
        ty => {
            unimplemented!("bpf cmd {:?} not implemented", ty)
        }
    };
    res
}

/// Initialize the BPF system
pub fn init_bpf_system() {
    helper::init_helper_functions();
}
