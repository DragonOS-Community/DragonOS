use core::{ffi::c_int, mem};
use std::{
    ffi::{CString, OsStr},
    format, io,
    os::fd::{BorrowedFd, FromRawFd, OwnedFd},
};

use aya_obj::generated::{
    perf_event_attr, perf_event_sample_format::PERF_SAMPLE_RAW,
    perf_sw_ids::PERF_COUNT_SW_BPF_OUTPUT, perf_type_id::PERF_TYPE_SOFTWARE, PERF_FLAG_FD_CLOEXEC,
};
use libc::pid_t;

use crate::sys::{syscall, SysResult, Syscall};

#[allow(clippy::too_many_arguments)]
pub(crate) fn perf_event_open(
    perf_type: u32,
    config: u64,
    pid: pid_t,
    cpu: c_int,
    sample_period: u64,
    sample_frequency: Option<u64>,
    wakeup: bool,
    inherit: bool,
    flags: u32,
) -> SysResult<OwnedFd> {
    let mut attr = unsafe { mem::zeroed::<perf_event_attr>() };

    attr.config = config;
    attr.size = mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = perf_type;
    attr.sample_type = PERF_SAMPLE_RAW as u64;
    attr.set_inherit(if inherit { 1 } else { 0 });
    attr.__bindgen_anon_2.wakeup_events = u32::from(wakeup);

    if let Some(frequency) = sample_frequency {
        attr.set_freq(1);
        attr.__bindgen_anon_1.sample_freq = frequency;
    } else {
        attr.__bindgen_anon_1.sample_period = sample_period;
    }

    perf_event_sys(attr, pid, cpu, flags)
}
pub(crate) fn perf_event_open_probe(
    ty: u32,
    ret_bit: Option<u32>,
    name: &OsStr,
    offset: u64,
    pid: Option<pid_t>,
) -> SysResult<OwnedFd> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut attr = unsafe { mem::zeroed::<perf_event_attr>() };

    if let Some(ret_bit) = ret_bit {
        attr.config = 1 << ret_bit;
    }

    let c_name = CString::new(name.as_bytes()).unwrap();

    attr.size = mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = ty;
    attr.__bindgen_anon_3.config1 = c_name.as_ptr() as u64;
    attr.__bindgen_anon_4.config2 = offset;

    let cpu = if pid.is_some() { -1 } else { 0 };
    let pid = pid.unwrap_or(-1);

    perf_event_sys(attr, pid, cpu, PERF_FLAG_FD_CLOEXEC)
}

pub(crate) fn perf_event_open_bpf(cpu: c_int) -> SysResult<OwnedFd> {
    perf_event_open(
        PERF_TYPE_SOFTWARE as u32,
        PERF_COUNT_SW_BPF_OUTPUT as u64,
        -1,
        cpu,
        1,
        None,
        true,
        false,
        PERF_FLAG_FD_CLOEXEC,
    )
}
fn perf_event_sys(attr: perf_event_attr, pid: pid_t, cpu: i32, flags: u32) -> SysResult<OwnedFd> {
    let fd = syscall(Syscall::PerfEventOpen {
        attr,
        pid,
        cpu,
        group: -1,
        flags,
    })?;

    let fd = fd.try_into().map_err(|_| {
        (
            fd,
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("perf_event_open: invalid fd returned: {fd}"),
            ),
        )
    })?;

    // SAFETY: perf_event_open returns a new file descriptor on success.
    unsafe { Ok(OwnedFd::from_raw_fd(fd)) }
}

pub(crate) fn perf_event_ioctl(fd: BorrowedFd<'_>, request: c_int, arg: c_int) -> SysResult<i64> {
    let call = Syscall::PerfEventIoctl { fd, request, arg };
    return syscall(call);
}
