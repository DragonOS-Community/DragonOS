//! /proc/[pid]/maps - 进程内存映射信息
//!
//! 返回进程的内存映射信息，格式兼容 Linux procfs

use crate::libs::{casting::DowncastArc, mutex::MutexGuard};
use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read_seq,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::{
        ucontext::{AddressSpace, LockedVMA},
        MemoryManagementArch, VirtAddr, VmFlags,
    },
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/maps 文件的 FileOps 实现
#[derive(Debug)]
pub struct MapsFileOps {
    target: ProcPidTarget,
}

impl MapsFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

/// 将 VmFlags 转换为权限字符串
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
    let s = if vm_flags.contains(VmFlags::VM_MAYSHARE) {
        b's'
    } else {
        b'p'
    };
    [r, w, x, s]
}

/// 格式化设备号、inode 和路径
#[inline(never)]
fn format_dev_inode_and_path(
    file_inode: Option<&Arc<dyn IndexNode>>,
    root_prefix: &str,
) -> (String, String) {
    if let Some(inode) = file_inode {
        let (dev, ino, path) = match inode.metadata() {
            Ok(md) => {
                let dev = format!("{:02x}:{:02x}", (md.dev_id >> 8) & 0xff, md.dev_id & 0xff);
                let ino = md.inode_id.into();
                let mut path = inode
                    .clone()
                    .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
                    .map(|inode| inode.procfs_path())
                    .unwrap_or_else(|| inode.absolute_path())
                    .unwrap_or_default();
                // An unlinked internal inode has a diagnostic dname but no
                // namespace path. Do not apply chroot path stripping to it.
                let diagnostic = path.is_empty() && md.nlinks == 0;
                if diagnostic {
                    if let Ok(name) = inode.dname() {
                        path = format!("/{} (deleted)", name.as_ref());
                    }
                }
                // 尊重进程的 chroot：去掉根目录前缀
                if !diagnostic && !root_prefix.is_empty() && root_prefix != "/" {
                    if let Some(rest) = path.strip_prefix(root_prefix) {
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

/// Renders the mappings from `cursor` on, the way Linux `m_start()` / `m_next()`
/// (`fs/proc/task_mmu.c`) walk the mapping table, and stops once `budget` bytes
/// are out.
///
/// `cursor` is the address the reader stopped at, which is exactly what
/// `m_start()` keeps in `*ppos` (`proc_get_vma()` stores `vma->vm_start`: the
/// address of the mapping the reader has *not* seen yet). It is looked up the
/// way `vma_next()` does, so the mapping this slice starts with is the one
/// covering `cursor`, and a reader never sees a mapping twice. Returns the
/// address the next slice resumes at, or `None` when the table ends here.
///
/// Rendering one slice at a time is what keeps an open fd from buffering a copy
/// of the whole mapping table: Linux `seq_file` likewise holds one buffer, not
/// the record. `budget` bounds the slice and is capped a page at a time by
/// [`proc_read_seq()`], so an fd holds at most one page of the table plus the
/// one line that crossed the bound.
fn render_maps_slice(
    target: &ProcPidTarget,
    vm: Option<&Arc<AddressSpace>>,
    cursor: Option<usize>,
    budget: usize,
    out: &mut Vec<u8>,
) -> Result<Option<usize>, SystemError> {
    // Linux `m_start()` resolves the task for every record it produces, so a
    // target that is gone ends the stream with `-ESRCH` even though the address
    // space itself is pinned by `open()`.
    let target_pcb = target.thread_group_leader().ok_or(SystemError::ESRCH)?;

    // A task without an address space (kernel thread) has no record to render.
    let Some(vm) = vm else {
        return Ok(None);
    };
    // Linux `m_start()`: `mm = priv->mm; if (!mm || !mmget_not_zero(mm)) return
    // NULL;`. The descriptor `open()` took can outlive the memory it described,
    // so the user count is what says whether this fd still has a table to walk;
    // `m_start()`/`m_stop()` hold the same reference across one record. The
    // stream therefore ends where the address space ends, the way it does when
    // the target exits or execs away from it.
    let Some(_mm_user) = vm.try_acquire() else {
        return Ok(None);
    };
    let Some(fs) = target_pcb.try_fs_struct() else {
        return Ok(None);
    };
    let root_prefix = fs.root().absolute_path().unwrap_or_default();

    let as_guard = vm.read_guard_no_reservations();

    // Linux resumes through `find_vma()`: `vma_iter_init(mm, last_addr)` and
    // `vma_next()` hand back the mapping that *covers* the resume address, or
    // the first mapping above it. `find_nearest()` looks the address up the same
    // way, so a mapping that still covers the cursor keeps its place in the
    // stream even if it was moved below the cursor since the previous slice.
    // The walk itself only moves upwards, so it costs O(log n) per slice, not a
    // rescan.
    let resume_addr = VirtAddr::new(cursor.unwrap_or(0));
    let covering = as_guard.mappings.find_nearest(resume_addr);
    let walk_from = covering
        .as_ref()
        .map(|vma| VirtAddr::new(vma.lock().region().start().data() + 1))
        .unwrap_or(resume_addr);

    let mut resume = None;
    for vma in covering
        .into_iter()
        .chain(as_guard.mappings.iter_vmas_starting_at(walk_from))
    {
        if !out.is_empty() && out.len() >= budget {
            resume = Some(vma.lock().region().start().data());
            break;
        }
        append_map_line(&vma, &root_prefix, out);
    }

    if out.is_empty() && cursor.is_none() {
        // 确保文件以换行符结尾
        out.extend_from_slice(b"\n");
    }
    Ok(resume)
}

/// Appends one line of the mapping table, as Linux `show_map_vma()` writes it.
fn append_map_line(vma: &Arc<LockedVMA>, root_prefix: &str, out: &mut Vec<u8>) {
    {
        let g = vma.lock();
        let region = *g.region();
        let vm_flags = *g.vm_flags();

        let perms = perms_from_vm_flags(vm_flags);
        let offset = g
            .backing_page_offset()
            .unwrap_or(0)
            .saturating_mul(MMArch::PAGE_SIZE);

        let (dev_ino, path_tail) = if let Some(f) = g.vm_file() {
            let inode = f.inode();
            format_dev_inode_and_path(Some(&inode), root_prefix)
        } else {
            format_dev_inode_and_path(None, root_prefix)
        };

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
}

impl FileOps for MapsFileOps {
    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        _flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        // Linux `proc_maps_open()` -> `proc_mem_open()`: the target's address
        // space is taken under the exec lock, so this fd is bound to the space
        // it was opened on and an `execve()` between two reads cannot move the
        // stream into a new one. The descriptor is held, not the memory (Linux
        // `mmgrab()` then `mmput()`, "but do not pin its memory"), so the
        // mappings still go away when the target leaves them behind; each slice
        // re-checks the user count for that, as `m_start()` does.
        let task = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        let pinned = {
            let _exec_guard = task.exec_update_read();
            task.basic().user_vm()
        };
        let FilePrivateData::Procfs(pdata) = &mut **data else {
            return Err(SystemError::EINVAL);
        };
        // `None` is a task without an address space (a kernel thread): Linux
        // `m_start()` renders no record for it either.
        pdata.pinned_vm = pinned;
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // The table to stream comes from the address space `open()` took; a
        // continuation read must not resolve it again. A read that arrives
        // without that state did not come through the procfs `open()` hook and
        // has no address space to stream: reporting an empty record for it would
        // claim the target maps nothing.
        let FilePrivateData::Procfs(pdata) = &*data else {
            return Err(SystemError::EINVAL);
        };
        let vm = pdata.pinned_vm.clone();
        // One mapping is rendered per slice, so an fd holds at most one line of
        // the table no matter how many mappings the target has; the cursor keeps
        // a mapping that appears mid-stream from tearing the byte stream.
        proc_read_seq(offset, len, buf, &mut data, |cursor, budget, out| {
            render_maps_slice(&self.target, vm.as_ref(), cursor, budget, out)
        })
    }
}
