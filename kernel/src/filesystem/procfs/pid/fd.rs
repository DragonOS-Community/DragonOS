//! /proc/[pid]/fd - 进程文件描述符目录
//!
//! 这个目录包含了进程打开的所有文件描述符的符号链接

use crate::filesystem::{
    procfs::{
        pid::ProcPidTarget,
        template::{Builder, DirOps, ProcDir, ProcDirBuilder, ProcSymBuilder, SymOps},
    },
    vfs::{permission::PermissionMask, IndexNode, InodeMode, Metadata, SpecialNodeData},
};
use crate::libs::casting::DowncastArc;
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/fd 目录的 DirOps 实现
#[derive(Debug)]
pub struct FdDirOps {
    target: ProcPidTarget,
}

impl FdDirOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { target }, InodeMode::from_bits_truncate(0o500)) // dr-x------
            .parent(parent)
            .volatile() // fd 是易失的，因为它们与特定进程关联
            .build()
            .unwrap()
    }

    fn get_process(&self) -> Option<Arc<crate::process::ProcessControlBlock>> {
        // Linux resolves the directory's `get_proc_task(inode)`, so a hidden
        // tid shows the files table of *that thread*: a thread that took a
        // private one with `close_range(CLOSE_RANGE_UNSHARE)` must not read the
        // leader's. For `/proc/<tgid>` the two agree, because the tgid resolves
        // to the leader.
        self.target.task()
    }
}

impl DirOps for FdDirOps {
    fn check_dac_permission(
        &self,
        metadata: &Metadata,
        mask: PermissionMask,
    ) -> Result<(), SystemError> {
        let current = crate::process::ProcessManager::current_pcb();
        let generic =
            crate::process::ProcessManager::current_cred().inode_permission(metadata, mask.bits());
        if generic.is_ok() {
            return generic;
        }
        // Linux proc_fd_permission(): introspection remains possible after
        // setuid(), without granting another thread group a DAC bypass.
        if self
            .get_process()
            .is_some_and(|target| target.raw_tgid() == current.raw_tgid())
        {
            return Ok(());
        }
        generic
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析文件描述符编号
        let fd = name.parse::<i32>().map_err(|_| SystemError::ENOENT)?;

        // 获取进程引用
        let process = self.get_process().ok_or(SystemError::ENOENT)?;

        // 检查文件描述符是否存在，并立即释放fd_table锁
        {
            // A process can become a zombie after ProcPidTarget resolved it.
            // exit_files() has already removed its descriptor table in that
            // state, so /proc/<pid>/fd must report a disappearing entry rather
            // than calling ProcessControlBlock::fd_table(), which unwraps.
            let fd_table = process
                .basic()
                .try_fd_table()
                .clone()
                .ok_or(SystemError::ENOENT)?;
            let fd_table_guard = fd_table.read();

            if fd_table_guard.get_file_by_fd(fd).is_none() {
                return Err(SystemError::ENOENT);
            }
        } // fd_table_guard在这里被释放

        // 创建或获取缓存的符号链接
        let mut cached_children = dir.cached_children().write();

        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建新的符号链接（传递 PID 和 fd）
        let inode = FdSymOps::new_inode(self.target.clone(), fd, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());

        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        refresh_fd_children(&self.target, dir, |fd, parent| {
            FdSymOps::new_inode(self.target.clone(), fd, parent)
        });
    }

    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        child.special_node().is_some()
    }
}

/// Refresh fd/fdinfo names without discarding live slot inodes. Like Linux's
/// proc_fill_cache, an inode names a stable PID object and an fd number, not
/// the open file description currently installed there. Reads still resolve
/// the live table, so close/reuse never pins an obsolete file or link target.
pub(super) fn refresh_fd_children<Ops: DirOps + 'static>(
    target: &ProcPidTarget,
    dir: &ProcDir<Ops>,
    create: impl Fn(i32, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
) {
    let fds: Vec<i32> = target
        .task()
        .and_then(|task| task.basic().try_fd_table().clone())
        .map(|table| table.read().iter().map(|(fd, _)| fd).collect())
        .unwrap_or_default();
    // FileDescriptorIterator visits installed slots in increasing order.
    // Release the table lock before taking the cache lock or constructing nodes.
    let mut children = dir.cached_children().write();
    children.retain(|name, _| {
        // Lookup may have cached a parseable alias ("03", "+3"). Only
        // canonical decimal names belong in the directory listing.
        if !name.as_bytes().first().is_some_and(u8::is_ascii_digit)
            || (name.len() > 1 && name.starts_with('0'))
        {
            return false;
        }
        name.parse::<i32>()
            .is_ok_and(|fd| fds.binary_search(&fd).is_ok())
    });
    for fd in fds {
        children
            .entry(fd.to_string())
            .or_insert_with(|| create(fd, dir.self_ref_weak().clone()));
    }
}

/// /proc/[pid]/fd/[fd] 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct FdSymOps {
    target: ProcPidTarget,
    fd: i32,
}

impl FdSymOps {
    fn checked_process(&self) -> Result<Arc<crate::process::ProcessControlBlock>, SystemError> {
        // Linux proc_fd_access_allowed() returns false for a vanished task;
        // get_link/readlink report EACCES before attempting to resolve its fd.
        let target = self.target.task().ok_or(SystemError::EACCES)?;
        if !crate::process::ProcessManager::current_pcb().has_permission_to_trace(
            &target,
            crate::process::ptrace::PtraceAccessCreds::FsCreds,
            &crate::process::ProcessManager::current_cred(),
        ) {
            return Err(SystemError::EACCES);
        }
        Ok(target)
    }

    pub fn new_inode(
        target: ProcPidTarget,
        fd: i32,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self { target, fd }, InodeMode::from_bits_truncate(0o700)) // lrwx------
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }
}

impl SymOps for FdSymOps {
    fn check_symlink_access(&self) -> Result<(), SystemError> {
        self.checked_process().map(|_| ())
    }

    fn is_magic_link(&self) -> bool {
        true
    }

    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // `proc_fd_link()` walks `get_proc_task(inode)->files`, so the link is
        // resolved against the thread this node names, not the group leader.
        let process = self.checked_process()?;

        // 先获取文件对象的 clone，然后立即释放 fd_table 锁
        // 避免在持有锁时调用可能获取其他锁的方法（如 absolute_path）
        let file = {
            let fd_table = process
                .basic()
                .try_fd_table()
                .clone()
                .ok_or(SystemError::ENOENT)?;
            let fd_table_guard = fd_table.read();

            fd_table_guard
                .get_file_by_fd(self.fd)
                .ok_or(SystemError::ENOENT)?
        }; // fd_table 锁在这里被释放

        let inode = file.path_inode();
        if let Some(target) = inode.proc_fd_link_target() {
            let copy_len = target.len().min(buf.len());
            buf[..copy_len].copy_from_slice(&target[..copy_len]);
            return Ok(copy_len);
        }
        // proc_fd_link resolves the target file, but d_path uses the reader's
        // fs root. Anonymous targets above do not require a pathname view.
        let reader_root = crate::process::ProcessManager::current_pcb()
            .try_fs_struct()
            .ok_or(SystemError::ENOENT)?
            .root()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let path = if let Some(mount_inode) = inode
            .clone()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>(
        ) {
            // A mounted file is not anonymous when rendering fails. Preserve
            // allocation/topology errors instead of inventing a link target.
            mount_inode.procfs_path(&reader_root)?
        } else if let Ok(path) = inode.absolute_path() {
            path
        } else {
            let inode_id = inode.metadata()?.inode_id;
            format!("anon_inode:[{}]", inode_id)
        };

        // 复制路径到缓冲区
        let path_bytes = path.as_bytes();
        let copy_len = path_bytes.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&path_bytes[..copy_len]);

        Ok(copy_len)
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        let process = self.target.task()?;

        // 获取文件对象
        let file = {
            let fd_table = process.basic().try_fd_table().clone()?;
            let fd_table_guard = fd_table.read();
            fd_table_guard.get_file_by_fd(self.fd)?
        };

        // Keep the open file description alive until the path walker can
        // retain the target inode. An inode Arc alone does not prevent a
        // zero-link ext4 O_TMPFILE from entering final reclaim.
        Some(SpecialNodeData::FileReference(file))
    }
}
