use crate::filesystem::vfs::MAX_PATHLEN;
use crate::include::bindings::linux_bpf::{
    perf_event_attr, perf_event_header, perf_event_sample_format, perf_sw_ids, perf_type_id,
};
use crate::syscall::user_access::vfs_check_and_clone_cstr;
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

/// The `PerfEventIoc` enum is used to define the ioctl commands for perf events.
///
/// See https://elixir.bootlin.com/linux/v6.1/source/include/uapi/linux/perf_event.h#L544
#[repr(u32)]
#[derive(Debug, Copy, Clone, FromPrimitive)]
pub enum PerfEventIoc {
    /// Equivalent to [crate::include::bindings::linux_bpf::AYA_PERF_EVENT_IOC_ENABLE].
    Enable = 9216,
    /// Equivalent to [crate::include::bindings::linux_bpf::AYA_PERF_EVENT_IOC_DISABLE].
    Disable = 9217,
    /// Equivalent to Linux `PERF_EVENT_IOC_RESET` (`_IO('$', 3)`).
    Reset = 9219,
    /// Equivalent to [crate::include::bindings::linux_bpf::AYA_PERF_EVENT_IOC_SET_BPF].
    SetBpf = 1074013192,
}

#[derive(Debug, Clone)]
#[allow(unused)]
/// `perf_event_open` syscall arguments.
pub struct PerfProbeArgs {
    pub config: PerfProbeConfig,
    pub name: String,
    pub offset: u64,
    pub size: u32,
    /// Raw `perf_event_attr.type` value. Dynamic PMU types intentionally live
    /// outside `perf_type_id`, whose `PERF_TYPE_MAX` member is not ABI.
    pub type_: u32,
    pub pid: i32,
    pub cpu: i32,
    pub group_fd: i32,
    pub flags: PerfEventOpenFlags,
    pub sample_type: Option<perf_event_sample_format>,
    /// Requested layout for read(2) on the perf event fd.
    pub read_format: u64,
    /// `perf_event_attr.disabled`: whether the event starts disabled (review R11a).
    pub disabled: bool,
    pub inherit: bool,
    pub enable_on_exec: bool,
    pub remove_on_exec: bool,
}

/// DragonOS currently has no general PMU type allocator. Keep the two
/// software probe PMUs in the dynamic range and expose these exact values via
/// event_source sysfs.
pub const PERF_TYPE_KPROBE: u32 = perf_type_id::PERF_TYPE_MAX as u32;
pub const PERF_TYPE_UPROBE: u32 = PERF_TYPE_KPROBE + 1;

/// Linux 6.6 bounds perf kprobe symbol names with `KSYM_NAME_LEN`.
const KSYM_NAME_LEN: usize = 512;

fn copy_probe_name(user: *const u8, max_len: usize) -> Result<String, SystemError> {
    let name = vfs_check_and_clone_cstr(user, Some(max_len)).map_err(|error| {
        if error == SystemError::ENAMETOOLONG {
            // perf_{k,u}probe_init() exposes strndup_user() exhaustion as
            // E2BIG, rather than the pathname-oriented ENAMETOOLONG.
            SystemError::E2BIG
        } else {
            error
        }
    })?;
    name.into_string().map_err(|_| SystemError::EINVAL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfProbeConfig {
    PerfSwIds(perf_sw_ids),
    Raw(u64),
}

impl PerfProbeArgs {
    pub fn try_from(
        attr: &perf_event_attr,
        pid: i32,
        cpu: i32,
        group_fd: i32,
        flags: usize,
    ) -> Result<Self, SystemError> {
        if attr.__reserved_1() != 0 || attr.__reserved_2 != 0 || attr.__reserved_3 != 0 {
            return Err(SystemError::EINVAL);
        }
        const KNOWN_READ_FORMAT_BITS: u64 = (1 << 5) - 1;
        if attr.read_format & !KNOWN_READ_FORMAT_BITS != 0 {
            return Err(SystemError::EINVAL);
        }
        const KNOWN_SAMPLE_TYPE_BITS: u64 = perf_event_sample_format::PERF_SAMPLE_MAX as u64 - 1;
        if attr.sample_type & !KNOWN_SAMPLE_TYPE_BITS != 0 {
            return Err(SystemError::EINVAL);
        }
        let ty = attr.type_;
        let config = match perf_type_id::from_u32(ty) {
            Some(perf_type_id::PERF_TYPE_SOFTWARE) => {
                let sw_id = perf_sw_ids::from_u32(attr.config as u32).ok_or(SystemError::EINVAL)?;
                PerfProbeConfig::PerfSwIds(sw_id)
            }
            _ => PerfProbeConfig::Raw(attr.config),
        };
        let name_ptr = unsafe { attr.__bindgen_anon_3.config1 } as *const u8;
        let name = match ty {
            PERF_TYPE_KPROBE => copy_probe_name(name_ptr, KSYM_NAME_LEN)?,
            PERF_TYPE_UPROBE => copy_probe_name(name_ptr, MAX_PATHLEN)?,
            _ => String::new(),
        };
        let sample_ty = perf_event_sample_format::from_u32(attr.sample_type as u32);
        let raw_flags = u32::try_from(flags).map_err(|_| SystemError::EINVAL)?;
        let args = PerfProbeArgs {
            config,
            name,
            offset: unsafe { attr.__bindgen_anon_4.config2 },
            size: attr.size,
            type_: ty,
            pid,
            cpu,
            group_fd,
            flags: PerfEventOpenFlags::from_bits(raw_flags).ok_or(SystemError::EINVAL)?,
            sample_type: sample_ty,
            read_format: attr.read_format,
            disabled: attr.disabled() != 0,
            inherit: attr.inherit() != 0,
            enable_on_exec: attr.enable_on_exec() != 0,
            remove_on_exec: attr.remove_on_exec() != 0,
        };
        Ok(args)
    }
}

/// The event type in our particular use case will be `PERF_RECORD_SAMPLE` or `PERF_RECORD_LOST`.
/// `PERF_RECORD_SAMPLE` indicating that there is an actual sample after this header.
/// And `PERF_RECORD_LOST` indicating that there is a record lost header following the perf event header.
#[repr(C)]
#[derive(Debug)]
pub struct LostSamples {
    pub header: perf_event_header,
    pub id: u64,
    pub count: u64,
}

impl LostSamples {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct SampleHeader {
    pub header: perf_event_header,
    pub size: u32,
}

impl SampleHeader {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct PerfSample<'a> {
    pub s_hdr: SampleHeader,
    pub value: &'a [u8],
}

impl PerfSample<'_> {
    pub fn calculate_size(value_size: usize) -> usize {
        size_of::<SampleHeader>() + value_size
    }
}
