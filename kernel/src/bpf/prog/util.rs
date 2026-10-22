use crate::include::bindings::linux_bpf::{bpf_attach_type, bpf_attr, bpf_prog_type};
use crate::process::cred::{capable, CAPFlags};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::String;
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
    pub license: Vec<u8>,
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
        // Bound and authorize the request before copying any user-controlled
        // program or license data. The device interpreter has a deliberately
        // smaller bound than Linux's general-purpose verifier.
        let is_device = prog_type == bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE;
        if is_device
            && (!(capable(CAPFlags::CAP_BPF) || capable(CAPFlags::CAP_SYS_ADMIN))
                || !(capable(CAPFlags::CAP_NET_ADMIN) || capable(CAPFlags::CAP_SYS_ADMIN)))
        {
            return Err(SystemError::EPERM);
        }
        let max_insns = if is_device { 8192 } else { 1_000_000 };
        if u.insn_cnt == 0 || u.insn_cnt > max_insns {
            return Err(SystemError::E2BIG);
        }
        let insns_len = (u.insn_cnt as usize)
            .checked_mul(8)
            .ok_or(SystemError::E2BIG)?;
        unsafe {
            let insns_buf = UserBufferReader::new(u.insns as *mut u8, insns_len, true)?;
            let mut insns = Vec::new();
            insns
                .try_reserve_exact(insns_len)
                .map_err(|_| SystemError::ENOMEM)?;
            insns.resize(insns_len, 0);
            insns_buf.copy_from_user_protected(&mut insns, 0)?;
            let name_slice =
                core::slice::from_raw_parts(u.prog_name.as_ptr() as *const u8, u.prog_name.len());
            let prog_name = copy_program_name(name_slice)?;
            let license = copy_license(u.license as *const u8)?;
            Ok(Self {
                prog_flags: u.prog_flags,
                prog_type,
                expected_attach_type,
                insns,
                license,
                kern_version: u.kern_version,
                name: prog_name,
            })
        }
    }
}

/// Linux bpf_obj_name_cpy: a bounded NUL-terminated ASCII identifier.
fn copy_program_name(bytes: &[u8]) -> Result<String, SystemError> {
    let bytes = CStr::from_bytes_until_nul(bytes)
        .map_err(|_| SystemError::EINVAL)?
        .to_bytes();
    if bytes
        .iter()
        .any(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'.')
    {
        return Err(SystemError::EINVAL);
    }
    let mut name = String::new();
    name.try_reserve_exact(bytes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    // The character check above already establishes UTF-8 validity.
    name.push_str(core::str::from_utf8(bytes).map_err(|_| SystemError::EINVAL)?);
    Ok(name)
}

/// Linux reads at most 127 license bytes and supplies the terminating NUL.
/// License data is a byte string, rather than an additional UTF-8 ABI rule.
fn copy_license(pointer: *const u8) -> Result<Vec<u8>, SystemError> {
    if pointer.is_null() {
        return Err(SystemError::EFAULT);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(127)
        .map_err(|_| SystemError::ENOMEM)?;
    for offset in 0..127 {
        let address = (pointer as usize)
            .checked_add(offset)
            .ok_or(SystemError::EFAULT)?;
        let mut byte = [0];
        UserBufferReader::new(address as *const u8, 1, true)?
            .copy_from_user_protected(&mut byte, 0)?;
        if byte[0] == 0 {
            break;
        }
        bytes.push(byte[0]);
    }
    Ok(bytes)
}

#[cfg(feature = "mm_selftest")]
pub(super) fn name_birth_selftest() -> bool {
    let _denied = crate::mm::oom::denial::Scope::enter(true);
    matches!(copy_program_name(b"nonempty\0"), Err(SystemError::ENOMEM))
        && matches!(
            copy_license(c"GPL".as_ptr().cast()),
            Err(SystemError::ENOMEM)
        )
}
