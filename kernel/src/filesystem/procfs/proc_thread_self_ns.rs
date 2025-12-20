//! Implementation of /proc/thread-self/ns/ namespace files
//!
//! This module provides support for namespace files under /proc/thread-self/ns/,
//! which are used by applications to reference namespaces (e.g., for setns syscall).
//!
//! In Linux, these files are typically symlinks that point to strings like "ipc:[4026531839]",
//! where the number is a namespace ID. Opening these files returns a file descriptor
//! that can be used to reference the namespace.
//!
//! Reference: https://man7.org/linux/man-pages/man7/namespaces.7.html

use core::convert::TryFrom;

use alloc::format;
use alloc::string::String;
use system_error::SystemError;

use crate::process::{
    fork::CloneFlags,
    namespace::{nsproxy::NamespaceId, NamespaceOps},
    ProcessManager, RawPid,
};

use super::ProcfsFilePrivateData;

// ============================================================================
// Namespace ioctl commands (Linux nsfs.h)
// ============================================================================

/// Namespace ioctl command definitions.
///
/// These commands are used with ioctl() on namespace file descriptors
/// obtained by opening /proc/[pid]/ns/* files.
///
/// The magic number 0xb7 is defined in Linux's include/uapi/linux/nsfs.h
/// Command format: _IO(0xb7, N) = (0xb7 << 8) | N
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum NsIoctlCmd {
    /// Get a file descriptor for the owning user namespace.
    /// Returns a new fd referring to the user namespace that owns
    /// the namespace referred to by the fd on which ioctl() is performed.
    GetUserns,

    /// Get a file descriptor for the parent namespace.
    /// Only valid for hierarchical namespace types (PID, user).
    /// Returns EINVAL for non-hierarchical namespaces.
    GetParent,

    /// Get the namespace type.
    /// Returns the CLONE_NEW* flag value that identifies the namespace type.
    GetNstype,

    /// Get the owner UID of a user namespace.
    /// Only valid for user namespaces.
    /// Returns EINVAL for other namespace types.
    GetOwnerUid,
}

impl From<NsIoctlCmd> for u32 {
    fn from(cmd: NsIoctlCmd) -> Self {
        match cmd {
            NsIoctlCmd::GetUserns => 0xb701,
            NsIoctlCmd::GetParent => 0xb702,
            NsIoctlCmd::GetNstype => 0xb703,
            NsIoctlCmd::GetOwnerUid => 0xb704,
        }
    }
}

impl TryFrom<u32> for NsIoctlCmd {
    type Error = SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0xb701 => Ok(NsIoctlCmd::GetUserns),
            0xb702 => Ok(NsIoctlCmd::GetParent),
            0xb703 => Ok(NsIoctlCmd::GetNstype),
            0xb704 => Ok(NsIoctlCmd::GetOwnerUid),
            _ => Err(SystemError::ENOTTY),
        }
    }
}

// ============================================================================
// Namespace file types
// ============================================================================

/// Namespace file types for /proc/thread-self/ns/
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadSelfNsFileType {
    Ipc,
    Uts,
    Mnt,
    Net,
    Pid,
    PidForChildren,
    Time,
    TimeForChildren,
    User,
    Cgroup,
}

impl ThreadSelfNsFileType {
    pub const ALL_NAME: [&'static str; 10] = [
        Self::Ipc.name(),
        Self::Uts.name(),
        Self::Mnt.name(),
        Self::Net.name(),
        Self::Pid.name(),
        Self::PidForChildren.name(),
        Self::Time.name(),
        Self::TimeForChildren.name(),
        Self::User.name(),
        Self::Cgroup.name(),
    ];

    /// Get the namespace type name (e.g., "ipc", "uts")
    pub const fn name(&self) -> &'static str {
        match self {
            ThreadSelfNsFileType::Ipc => "ipc",
            ThreadSelfNsFileType::Uts => "uts",
            ThreadSelfNsFileType::Mnt => "mnt",
            ThreadSelfNsFileType::Net => "net",
            ThreadSelfNsFileType::Pid => "pid",
            ThreadSelfNsFileType::PidForChildren => "pid_for_children",
            ThreadSelfNsFileType::Time => "time",
            ThreadSelfNsFileType::TimeForChildren => "time_for_children",
            ThreadSelfNsFileType::User => "user",
            ThreadSelfNsFileType::Cgroup => "cgroup",
        }
    }

    /// Get the CLONE_NEW* flag corresponding to this namespace type.
    ///
    /// This is used by NS_GET_NSTYPE ioctl to return the namespace type.
    pub const fn clone_flag(&self) -> CloneFlags {
        match self {
            ThreadSelfNsFileType::Ipc => CloneFlags::CLONE_NEWIPC,
            ThreadSelfNsFileType::Uts => CloneFlags::CLONE_NEWUTS,
            ThreadSelfNsFileType::Mnt => CloneFlags::CLONE_NEWNS,
            ThreadSelfNsFileType::Net => CloneFlags::CLONE_NEWNET,
            ThreadSelfNsFileType::Pid | ThreadSelfNsFileType::PidForChildren => {
                CloneFlags::CLONE_NEWPID
            }
            ThreadSelfNsFileType::Time | ThreadSelfNsFileType::TimeForChildren => {
                CloneFlags::CLONE_NEWTIME
            }
            ThreadSelfNsFileType::User => CloneFlags::CLONE_NEWUSER,
            ThreadSelfNsFileType::Cgroup => CloneFlags::CLONE_NEWCGROUP,
        }
    }

    /// Check if this namespace type supports NS_GET_PARENT ioctl.
    ///
    /// Only hierarchical namespace types (PID, user) support get_parent.
    /// Cgroup, network, mount, IPC, UTS namespaces are not hierarchical
    /// in the sense that NS_GET_PARENT would return a parent.
    pub const fn supports_get_parent(&self) -> bool {
        matches!(
            self,
            ThreadSelfNsFileType::Pid
                | ThreadSelfNsFileType::PidForChildren
                | ThreadSelfNsFileType::User
        )
    }
}

impl TryFrom<&str> for ThreadSelfNsFileType {
    type Error = SystemError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "ipc" => Ok(ThreadSelfNsFileType::Ipc),
            "uts" => Ok(ThreadSelfNsFileType::Uts),
            "mnt" => Ok(ThreadSelfNsFileType::Mnt),
            "net" => Ok(ThreadSelfNsFileType::Net),
            "pid" => Ok(ThreadSelfNsFileType::Pid),
            "pid_for_children" => Ok(ThreadSelfNsFileType::PidForChildren),
            "time" => Ok(ThreadSelfNsFileType::Time),
            "time_for_children" => Ok(ThreadSelfNsFileType::TimeForChildren),
            "user" => Ok(ThreadSelfNsFileType::User),
            "cgroup" => Ok(ThreadSelfNsFileType::Cgroup),
            _ => Err(SystemError::ENOENT),
        }
    }
}

/// Get current thread's namespace "inode number" (nsid) for a given type.
///
/// This value is stable for the lifetime of the namespace and is used both
/// as the numeric component of `/proc/thread-self/ns/*` readlink output and
/// as the `st_ino` reported by `stat(2)` on these entries.
#[inline(never)]
pub fn current_thread_self_ns_ino(ns_type: ThreadSelfNsFileType) -> usize {
    let pcb = ProcessManager::current_pcb();
    let nsproxy = pcb.nsproxy();

    // Get namespace inode number based on type
    let ino = match ns_type {
        ThreadSelfNsFileType::Ipc => {
            let ns = nsproxy.ipc_ns.clone();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::Uts => {
            let ns = nsproxy.uts_ns.clone();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::Mnt => {
            let ns = nsproxy.mnt_ns.clone();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::Net => {
            let ns = nsproxy.net_ns.clone();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::Pid => {
            // For current process PID namespace, we use the active PID namespace
            // Note: In Linux, /proc/thread-self/ns/pid refers to the thread's own PID namespace
            let ns = pcb.active_pid_ns();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::PidForChildren => {
            let ns = nsproxy.pid_ns_for_children.clone();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::Time | ThreadSelfNsFileType::TimeForChildren => {
            // Time namespace is not yet implemented, return a placeholder
            // In Linux, this would be the time namespace ID
            NamespaceId::new(0)
        }
        ThreadSelfNsFileType::User => {
            // User namespace is stored in cred, not nsproxy
            let cred = pcb.cred();
            let ns = cred.user_ns.clone();
            ns.ns_common().nsid
        }
        ThreadSelfNsFileType::Cgroup => {
            // Cgroup namespace
            let ns = nsproxy.cgroup_ns.clone();
            ns.ns_common().nsid
        }
    };

    ino.data()
}

/// Generate namespace ID string in the format "namespace_type:[id]"
///
/// The ID is derived from `current_thread_self_ns_ino` and remains stable
/// throughout the namespace's lifetime.
fn generate_namespace_id(ns_type: ThreadSelfNsFileType) -> String {
    let ino = current_thread_self_ns_ino(ns_type);

    format!("{}:[{}]", ns_type.name(), ino)
}

/// Open a namespace file (for symlink, this just returns the target length)
#[inline(never)]
pub fn open_thread_self_ns_file(
    ns_type: ThreadSelfNsFileType,
    _pdata: &mut ProcfsFilePrivateData,
) -> Result<i64, SystemError> {
    let target = generate_namespace_id(ns_type);
    Ok(target.len() as i64)
}

/// Read a namespace symlink target
#[inline(never)]
pub fn read_thread_self_ns_link(
    ns_type: ThreadSelfNsFileType,
    buf: &mut [u8],
    offset: usize,
) -> Result<usize, SystemError> {
    let target = generate_namespace_id(ns_type);
    let target_bytes = target.as_bytes();

    if offset >= target_bytes.len() {
        return Ok(0);
    }

    let len = buf.len().min(target_bytes.len() - offset);
    buf[..len].copy_from_slice(&target_bytes[offset..offset + len]);
    Ok(len)
}

// ============================================================================
// PID-specific namespace file functions (for /proc/<pid>/ns/)
// ============================================================================

/// Get namespace "inode number" (nsid) for a specific PID.
///
/// Similar to `current_thread_self_ns_ino` but for a specific process.
/// Returns ESRCH if the process doesn't exist.
#[inline(never)]
pub fn pid_ns_ino(pid: RawPid, ns_type: ThreadSelfNsFileType) -> Result<usize, SystemError> {
    let pcb = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;
    let nsproxy = pcb.nsproxy();

    let ino = match ns_type {
        ThreadSelfNsFileType::Ipc => nsproxy.ipc_ns.ns_common().nsid,
        ThreadSelfNsFileType::Uts => nsproxy.uts_ns.ns_common().nsid,
        ThreadSelfNsFileType::Mnt => nsproxy.mnt_ns.ns_common().nsid,
        ThreadSelfNsFileType::Net => nsproxy.net_ns.ns_common().nsid,
        ThreadSelfNsFileType::Pid => pcb.active_pid_ns().ns_common().nsid,
        ThreadSelfNsFileType::PidForChildren => nsproxy.pid_ns_for_children.ns_common().nsid,
        ThreadSelfNsFileType::Time | ThreadSelfNsFileType::TimeForChildren => {
            // Time namespace is not yet implemented
            NamespaceId::new(0)
        }
        ThreadSelfNsFileType::User => pcb.cred().user_ns.ns_common().nsid,
        ThreadSelfNsFileType::Cgroup => nsproxy.cgroup_ns.ns_common().nsid,
    };

    Ok(ino.data())
}

/// Generate namespace ID string for a specific PID in the format "namespace_type:[id]"
fn generate_pid_namespace_id(
    pid: RawPid,
    ns_type: ThreadSelfNsFileType,
) -> Result<String, SystemError> {
    let ino = pid_ns_ino(pid, ns_type)?;
    Ok(format!("{}:[{}]", ns_type.name(), ino))
}

/// Open a namespace file for a specific PID (returns the target length)
#[inline(never)]
pub fn open_pid_ns_file(
    pid: RawPid,
    ns_type: ThreadSelfNsFileType,
    _pdata: &mut ProcfsFilePrivateData,
) -> Result<i64, SystemError> {
    let target = generate_pid_namespace_id(pid, ns_type)?;
    Ok(target.len() as i64)
}

/// Read a namespace symlink target for a specific PID
#[inline(never)]
pub fn read_pid_ns_link(
    pid: RawPid,
    ns_type: ThreadSelfNsFileType,
    buf: &mut [u8],
    offset: usize,
) -> Result<usize, SystemError> {
    let target = generate_pid_namespace_id(pid, ns_type)?;
    let target_bytes = target.as_bytes();

    if offset >= target_bytes.len() {
        return Ok(0);
    }

    let len = buf.len().min(target_bytes.len() - offset);
    buf[..len].copy_from_slice(&target_bytes[offset..offset + len]);
    Ok(len)
}

// ============================================================================
// Namespace file ioctl handling
// ============================================================================

/// Handle ioctl operations for /proc/[pid]/ns/* files.
///
/// This function processes namespace-specific ioctl commands as defined
/// in Linux's nsfs.h. Only namespace files under /proc should call this.
///
/// # Arguments
/// * `ns_type` - The type of namespace file being operated on
/// * `cmd` - The ioctl command (NS_GET_NSTYPE, NS_GET_USERNS, etc.)
/// * `_data` - Additional data for the ioctl (unused for current commands)
///
/// # Returns
/// * `Ok(usize)` - Command-specific return value
/// * `Err(SystemError)` - Error if command fails or is unsupported
///
/// # Supported Commands
/// * `NS_GET_NSTYPE` - Returns the CLONE_NEW* flag for this namespace type
/// * `NS_GET_USERNS` - Returns fd to owning user namespace (not yet implemented)
/// * `NS_GET_PARENT` - Returns fd to parent namespace (only for hierarchical ns)
#[inline(never)]
pub fn ns_file_ioctl(
    ns_type: ThreadSelfNsFileType,
    cmd: u32,
    _data: usize,
) -> Result<usize, SystemError> {
    let cmd = NsIoctlCmd::try_from(cmd)?;

    match cmd {
        NsIoctlCmd::GetNstype => {
            // Return the namespace type as CLONE_NEW* flag value
            Ok(ns_type.clone_flag().bits() as usize)
        }

        NsIoctlCmd::GetUserns => {
            // TODO: Return fd to owning user namespace
            // This requires:
            // 1. Getting the user namespace that owns this namespace
            // 2. Creating a new file descriptor pointing to that user namespace
            // 3. Installing the fd in the current process's fd table
            //
            // For now, return ENOSYS as this is not yet fully implemented.
            // This is acceptable for most container runtimes which primarily
            // need NS_GET_NSTYPE.
            Err(SystemError::ENOSYS)
        }

        NsIoctlCmd::GetParent => {
            // Only hierarchical namespace types support get_parent
            if !ns_type.supports_get_parent() {
                return Err(SystemError::EINVAL);
            }

            // TODO: Return fd to parent namespace
            // This requires similar work as NS_GET_USERNS:
            // 1. Getting the parent namespace
            // 2. Creating a new file descriptor
            // 3. Installing the fd
            //
            // For now, return ENOSYS
            Err(SystemError::ENOSYS)
        }

        NsIoctlCmd::GetOwnerUid => {
            // Only valid for user namespaces
            if ns_type != ThreadSelfNsFileType::User {
                return Err(SystemError::EINVAL);
            }

            // TODO: Return the owner UID of the user namespace
            // This requires writing the UID to the user-provided buffer at _data
            Err(SystemError::ENOSYS)
        }
    }
}
