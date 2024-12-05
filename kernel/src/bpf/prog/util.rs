use crate::include::bindings::linux_bpf::{bpf_attach_type, bpf_attr, bpf_prog_type};
use crate::syscall::user_access::{check_and_clone_cstr, UserBufferReader};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ffi::CStr;
use core::fmt::Debug;
use num_traits::FromPrimitive;
use system_error::SystemError;

bitflags::bitflags! {

    pub struct VerifierLogLevel: u32 {
        /// Sets no verifier logging.
        const DISABLE = 0;
        /// Enables debug verifier logging.
        const DEBUG = 1;
        /// Enables verbose verifier logging.
        const VERBOSE = 2 | Self::DEBUG.bits();
        /// Enables verifier stats.
        const STATS = 4;
    }
}

#[derive(Debug)]
pub struct BpfProgVerifierInfo {
    /// This attribute specifies the level/detail of the log output. Valid values are.
    pub log_level: VerifierLogLevel,
    /// This attributes indicates the size of the memory region in bytes
    /// indicated by `log_buf` which can safely be written to by the kernel.
    pub _log_buf_size: u32,
    /// This attributes can be set to a pointer to a memory region
    /// allocated/reservedby the loader process where the verifier log will
    /// be written to.
    /// The detail of the log is set by log_level. The verifier log
    /// is often the only indication in addition to the error code of
    /// why the syscall command failed to load the program.
    ///
    /// The log is also written to on success. If the kernel runs out of
    /// space in the buffer while loading, the loading process will fail
    /// and the command will return with an error code of -ENOSPC. So it
    /// is important to correctly size the buffer when enabling logging.
    pub _log_buf_ptr: usize,
}

impl From<&bpf_attr> for BpfProgVerifierInfo {
    fn from(attr: &bpf_attr) -> Self {
        unsafe {
            let u = &attr.__bindgen_anon_3;
            Self {
                log_level: VerifierLogLevel::from_bits_truncate(u.log_level),
                _log_buf_size: u.log_size,
                _log_buf_ptr: u.log_buf as usize,
            }
        }
    }
}

pub struct BpfProgMeta {
    pub prog_flags: u32,
    pub prog_type: bpf_prog_type,
    pub expected_attach_type: bpf_attach_type,
    pub insns: Vec<u8>,
    pub license: String,
    pub kern_version: u32,
    pub name: String,
}

impl Debug for BpfProgMeta {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BpfProgMeta")
            .field("prog_flags", &self.prog_flags)
            .field("prog_type", &self.prog_type)
            .field("expected_attach_type", &self.expected_attach_type)
            .field("insns_len", &(self.insns.len() / 8))
            .field("license", &self.license)
            .field("kern_version", &self.kern_version)
            .field("name", &self.name)
            .finish()
    }
}

impl TryFrom<&bpf_attr> for BpfProgMeta {
    type Error = SystemError;
    fn try_from(attr: &bpf_attr) -> Result<Self, Self::Error> {
        let u = unsafe { &attr.__bindgen_anon_3 };
        let prog_type = bpf_prog_type::from_u32(u.prog_type).ok_or(SystemError::EINVAL)?;
        let expected_attach_type =
            bpf_attach_type::from_u32(u.expected_attach_type).ok_or(SystemError::EINVAL)?;
        unsafe {
            let insns_buf =
                UserBufferReader::new(u.insns as *mut u8, u.insn_cnt as usize * 8, true)?;
            let insns = insns_buf.read_from_user::<u8>(0)?.to_vec();
            let name_slice =
                core::slice::from_raw_parts(u.prog_name.as_ptr() as *const u8, u.prog_name.len());
            let prog_name = CStr::from_bytes_until_nul(name_slice)
                .map_err(|_| SystemError::EINVAL)?
                .to_str()
                .map_err(|_| SystemError::EINVAL)?
                .to_string();
            let license = check_and_clone_cstr(u.license as *const u8, None)?;
            Ok(Self {
                prog_flags: u.prog_flags,
                prog_type,
                expected_attach_type,
                insns,
                license: license.into_string().map_err(|_| SystemError::EINVAL)?,
                kern_version: u.kern_version,
                name: prog_name,
            })
        }
    }
}
