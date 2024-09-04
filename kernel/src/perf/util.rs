use crate::include::bindings::linux_bpf::{
    perf_event_attr, perf_event_sample_format, perf_sw_ids, perf_type_id,
};
use crate::syscall::user_access::check_and_clone_cstr;
use alloc::string::String;
use num_traits::FromPrimitive;
use system_error::SystemError;

bitflags! {
    pub struct PerfEventOpenFlags: u32 {
        const PERF_FLAG_FD_NO_GROUP = 1;
        const PERF_FLAG_FD_OUTPUT = 2;
        const PERF_FLAG_PID_CGROUP = 4;
        const PERF_FLAG_FD_CLOEXEC = 8;
    }
}
#[repr(u32)]
#[derive(Debug, Copy, Clone, FromPrimitive)]
pub enum PerfEventIoc {
    Enable = 9216,
    Disable = 9217,
    SetBpf = 1074013192,
}

#[derive(Debug, Clone)]
pub struct PerfProbeArgs {
    pub config: perf_sw_ids,
    pub name: String,
    pub offset: u64,
    pub size: u32,
    pub type_: perf_type_id,
    pub pid: i32,
    pub cpu: i32,
    pub group_fd: i32,
    pub flags: PerfEventOpenFlags,
    pub sample_type: Option<perf_event_sample_format>,
}

impl PerfProbeArgs {
    pub fn try_from(
        attr: &perf_event_attr,
        pid: i32,
        cpu: i32,
        group_fd: i32,
        flags: u32,
    ) -> Result<Self, SystemError> {
        let ty = perf_type_id::from_u32(attr.type_).ok_or(SystemError::EINVAL)?;
        let config = perf_sw_ids::from_u32(attr.config as u32).ok_or(SystemError::EINVAL)?;
        let name = if ty == perf_type_id::PERF_TYPE_MAX {
            let name_ptr = unsafe { attr.__bindgen_anon_3.config1 } as *const u8;
            let name = check_and_clone_cstr(name_ptr, None)?;
            name.into_string().map_err(|_| SystemError::EINVAL)?
        } else {
            String::new()
        };
        let sample_ty = perf_event_sample_format::from_u32(attr.sample_type as u32);
        let args = PerfProbeArgs {
            config,
            name,
            offset: unsafe { attr.__bindgen_anon_4.config2 },
            size: attr.size,
            type_: ty,
            pid,
            cpu,
            group_fd,
            flags: PerfEventOpenFlags::from_bits_truncate(flags),
            sample_type: sample_ty,
        };
        Ok(args)
    }
}
