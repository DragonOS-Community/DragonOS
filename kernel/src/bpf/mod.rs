pub mod classic;
pub mod helper;
pub mod map;
pub mod prog;
mod query;
mod sys_bpf;
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_cmd};
use system_error::SystemError;

type Result<T> = core::result::Result<T, SystemError>;

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
        bpf_cmd::BPF_PROG_QUERY => query::bpf_prog_query(attr, user_attr),
        _ => Err(SystemError::ENOSYS),
    };
    res
}

/// Initialize the BPF system
pub fn init_bpf_system() {
    helper::init_helper_functions();
}
