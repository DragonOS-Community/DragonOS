//! Implementation of /proc/thread-self/ns/ namespace files
//!
//! This module provides support for namespace files under /proc/thread-self/ns/,
//! which are used by applications to reference namespaces (e.g., for setns syscall).
//!
//! In Linux, these files are typically symlinks that point to strings like "ipc:[4026531839]",
//! where the number is a namespace ID. Opening these files returns a file descriptor
//! that can be used to reference the namespace.

use core::convert::TryFrom;

use alloc::format;
use alloc::string::String;
use system_error::SystemError;

use crate::process::{
    namespace::{nsproxy::NamespaceId, NamespaceOps},
    ProcessManager,
};

use super::ProcfsFilePrivateData;

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

/// Generate namespace ID string in the format "namespace_type:[id]"
///
/// The ID comes from the namespace's ino field, which is allocated when the namespace
/// is created and remains stable throughout its lifetime. This matches Linux's behavior
/// where /proc/.../ns/ files show the namespace's inode number.
fn generate_namespace_id(ns_type: ThreadSelfNsFileType) -> String {
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
            // Cgroup namespace is not yet implemented
            NamespaceId::new(0)
        }
    };

    format!("{}:[{}]", ns_type.name(), ino.data())
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
