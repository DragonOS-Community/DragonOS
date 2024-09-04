use crate::include::bindings::linux_bpf::{bpf_attr, bpf_map_type};
use alloc::string::{String, ToString};
use core::ffi::CStr;
use num_traits::FromPrimitive;
use system_error::SystemError;

#[derive(Debug, Clone)]
pub struct BpfMapMeta {
    pub map_type: bpf_map_type,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub map_flags: u32,
    pub map_name: String,
}

impl TryFrom<&bpf_attr> for BpfMapMeta {
    type Error = SystemError;
    fn try_from(value: &bpf_attr) -> Result<Self, Self::Error> {
        let u = unsafe { &value.__bindgen_anon_1 };
        let map_name_slice = unsafe {
            core::slice::from_raw_parts(u.map_name.as_ptr() as *const u8, u.map_name.len())
        };
        let map_name = CStr::from_bytes_until_nul(map_name_slice)
            .map_err(|_| SystemError::EINVAL)?
            .to_str()
            .map_err(|_| SystemError::EINVAL)?
            .to_string();
        let map_type = bpf_map_type::from_u32(u.map_type).ok_or(SystemError::EINVAL)?;
        Ok(BpfMapMeta {
            map_type,
            key_size: u.key_size,
            value_size: u.value_size,
            max_entries: u.max_entries,
            map_flags: u.map_flags,
            map_name,
        })
    }
}

#[derive(Debug)]
pub struct BpfMapUpdateArg {
    pub map_fd: u32,
    pub key: u64,
    pub value: u64,
    pub flags: u64,
}

impl From<&bpf_attr> for BpfMapUpdateArg {
    fn from(value: &bpf_attr) -> Self {
        unsafe {
            let u = &value.__bindgen_anon_2;
            BpfMapUpdateArg {
                map_fd: u.map_fd,
                key: u.key,
                value: u.__bindgen_anon_1.value,
                flags: u.flags,
            }
        }
    }
}
#[derive(Debug)]
pub struct BpfMapGetNextKeyArg {
    pub map_fd: u32,
    pub key: Option<u64>,
    pub next_key: u64,
}

impl From<&bpf_attr> for BpfMapGetNextKeyArg {
    fn from(value: &bpf_attr) -> Self {
        unsafe {
            let u = &value.__bindgen_anon_2;
            BpfMapGetNextKeyArg {
                map_fd: u.map_fd,
                key: if u.key != 0 { Some(u.key) } else { None },
                next_key: u.__bindgen_anon_1.next_key,
            }
        }
    }
}

#[inline]
/// Round up `x` to the nearest multiple of `align`.
pub fn round_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}
