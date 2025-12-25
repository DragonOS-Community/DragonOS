//! Minimal implementation of `/proc/<pid>/maps`.
//!
//! The gVisor syscall tests use `/proc/self/maps` to detect whether x86 vsyscall
//! is enabled. Even if vsyscall is not mapped/enabled, `/proc/self/maps` must
//! exist and be readable.
//!
//! This module keeps the implementation high-cohesion/low-coupling by:
//! - taking a `RawPid` and returning textual maps content
//! - not depending on procfs inode internals beyond `ProcfsFilePrivateData`

use alloc::string::ToString;

use alloc::{format, string::String, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::vfs::IndexNode,
    process::{ProcessManager, RawPid},
};

use crate::mm::{ucontext::LockedVMA, MemoryManagementArch, VmFlags};

use super::ProcfsFilePrivateData;

#[inline(never)]
fn perms_from_vm_flags(vm_flags: VmFlags) -> [u8; 4] {
    let r = if vm_flags.contains(VmFlags::VM_READ) {
        b'r'
    } else {
        b'-'
    };
    let w = if vm_flags.contains(VmFlags::VM_WRITE) {
        b'w'
    } else {
        b'-'
    };
    let x = if vm_flags.contains(VmFlags::VM_EXEC) {
        b'x'
    } else {
        b'-'
    };
    let s = if vm_flags.contains(VmFlags::VM_SHARED) {
        b's'
    } else {
        b'p'
    };
    [r, w, x, s]
}

#[inline(never)]
fn format_dev_inode_and_path(
    file_inode: Option<&dyn IndexNode>,
    root_prefix: &str,
) -> (String, String) {
    if let Some(inode) = file_inode {
        // Best-effort: dev/inode and path are not strictly required by the tests.
        let (dev, ino, path) = match inode.metadata() {
            Ok(md) => {
                let dev = format!("{:02x}:{:02x}", (md.dev_id >> 8) & 0xff, md.dev_id & 0xff);
                let ino = md.inode_id.into();
                let mut path = inode.absolute_path().unwrap_or_default();
                // Respect process chroot: if we can compute the process root's absolute prefix,
                // strip it from the inode's global absolute path so `/proc/<pid>/maps` doesn't
                // leak the pre-chroot path.
                if !root_prefix.is_empty() && root_prefix != "/" {
                    if let Some(rest) = path.strip_prefix(root_prefix) {
                        // Ensure the result is rooted.
                        path = if rest.is_empty() {
                            "/".to_string()
                        } else if rest.starts_with('/') {
                            rest.to_string()
                        } else {
                            format!("/{}", rest)
                        };
                    }
                }
                (dev, ino, path)
            }
            Err(_) => (String::from("00:00"), 0usize, String::new()),
        };
        let mut tail = String::new();
        if !path.is_empty() {
            tail.push(' ');
            tail.push_str(&path);
        }
        return (format!("{} {}", dev, ino), tail);
    }
    (String::from("00:00 0"), String::new())
}

/// Generate `/proc/<pid>/maps` textual content into `pdata`.
///
/// Format (Linux-like):
/// `start-end perms offset dev:inode pathname`
#[inline(never)]
pub fn open_proc_maps(pid: RawPid, pdata: &mut ProcfsFilePrivateData) -> Result<i64, SystemError> {
    let target_pcb = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;

    let vm = target_pcb.basic().user_vm().ok_or(SystemError::EINVAL)?;
    // Compute the target process root prefix for chroot-aware path formatting.
    let root_prefix = target_pcb
        .fs_struct()
        .root()
        .absolute_path()
        .unwrap_or_default();

    let as_guard = vm.read();

    // Collect and sort by address to match Linux ordering.
    let mut vmas: Vec<alloc::sync::Arc<LockedVMA>> =
        as_guard.mappings.iter_vmas().cloned().collect();
    vmas.sort_by_key(|v| v.lock_irqsave().region().start().data());

    let out: &mut Vec<u8> = &mut pdata.data;
    out.clear();

    for vma in vmas {
        let g = vma.lock_irqsave();
        let region = *g.region();
        let vm_flags = *g.vm_flags();

        let perms = perms_from_vm_flags(vm_flags);
        let offset = g
            .backing_page_offset()
            .unwrap_or(0)
            .saturating_mul(MMArch::PAGE_SIZE);

        let (dev_ino, path_tail) = if let Some(f) = g.vm_file() {
            let inode = f.inode();
            format_dev_inode_and_path(Some(inode.as_ref()), &root_prefix)
        } else {
            format_dev_inode_and_path(None, &root_prefix)
        };

        // Linux prints addresses as fixed-width hex; we keep it simple but valid.
        let line = format!(
            "{:016x}-{:016x} {}{}{}{} {:08x} {}{}\n",
            region.start().data(),
            region.end().data(),
            perms[0] as char,
            perms[1] as char,
            perms[2] as char,
            perms[3] as char,
            offset,
            dev_ino,
            path_tail
        );
        out.extend_from_slice(line.as_bytes());
    }

    // Ensure file ends with '\n' even for empty mappings.
    if out.is_empty() {
        out.extend_from_slice(b"\n");
    }

    Ok(out.len() as i64)
}
