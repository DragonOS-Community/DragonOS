use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    cgroup::{
        cgroup_accounting_lock, cgroup_common_ancestor, cgroup_migrate_vet_dst_with_src,
        cgroup_root, CgroupNode,
    },
    filesystem::vfs::{
        file::{FileFlags, FilePrivateData},
        permission::PermissionMask,
        vcore::generate_inode_id,
        FileSystem, FileType, IndexNode, InodeFlags, InodeMode, Metadata,
    },
    libs::{mutex::MutexGuard, rwsem::RwSem, spinlock::SpinLock},
    process::ProcessManager,
    time::PosixTimeSpec,
};

use super::{
    files::{self, CgroupCoreFile, CgroupFileSpec},
    mount::Cgroup2Fs,
    CGROUP2_BLOCK_SIZE,
};

#[derive(Debug)]
pub(super) struct Cgroup2Inode {
    self_ref: Weak<Cgroup2Inode>,
    fs: RwSem<Weak<Cgroup2Fs>>,
    inner: SpinLock<Cgroup2InodeInner>,
}

#[derive(Debug)]
struct Cgroup2InodeInner {
    parent: Weak<Cgroup2Inode>,
    metadata: Metadata,
    name: String,
    kind: Cgroup2InodeKind,
}

#[derive(Debug)]
enum Cgroup2InodeKind {
    Dir {
        cgroup: Arc<CgroupNode>,
        children: HashMap<String, Arc<Cgroup2Inode>>,
    },
    File {
        cgroup: Arc<CgroupNode>,
        ty: CgroupCoreFile,
        data: Vec<u8>,
    },
}

impl Cgroup2Inode {
    pub(super) fn new_dir(name: String, cgroup: Arc<CgroupNode>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            inner: SpinLock::new(Cgroup2InodeInner {
                parent: Weak::new(),
                metadata: Metadata {
                    size: 0,
                    mode: InodeMode::from_bits_truncate(0o755),
                    uid: 0,
                    gid: 0,
                    blk_size: CGROUP2_BLOCK_SIZE as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    file_type: FileType::Dir,
                    nlinks: 2,
                    raw_dev: Default::default(),
                    flags: InodeFlags::empty(),
                },
                name,
                kind: Cgroup2InodeKind::Dir {
                    cgroup,
                    children: HashMap::new(),
                },
            }),
        })
    }

    pub(super) fn set_fs(&self, fs: Weak<Cgroup2Fs>) {
        *self.fs.write() = fs;
    }

    pub(super) fn cgroup(&self) -> Option<Arc<CgroupNode>> {
        let inner = self.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { cgroup, .. } => Some(cgroup.clone()),
            _ => None,
        }
    }

    fn fs_root_cgroup(fs_root: &Arc<dyn IndexNode>) -> Result<Arc<CgroupNode>, SystemError> {
        let root_inode = fs_root
            .as_any_ref()
            .downcast_ref::<Cgroup2Inode>()
            .ok_or(SystemError::EINVAL)?;
        root_inode.cgroup().ok_or(SystemError::EINVAL)
    }

    fn prune_stale_dir_cache(parent: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let (parent_cgroup, entries) = {
            let inner = parent.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, children } => {
                    let entries = children
                        .iter()
                        .map(|(name, inode)| (name.clone(), inode.clone()))
                        .collect::<Vec<_>>();
                    (cgroup.clone(), entries)
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let mut stale = Vec::new();
        for (name, inode) in entries {
            let Some(cached_cgroup) = inode.cgroup() else {
                continue;
            };
            match parent_cgroup.child(&name) {
                Some(real) if Arc::ptr_eq(&real, &cached_cgroup) => {}
                _ => stale.push(name),
            }
        }

        if stale.is_empty() {
            return Ok(());
        }

        let mut inner = parent.inner.lock();
        if let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind {
            for name in stale {
                children.remove(&name);
            }
            return Ok(());
        }

        Err(SystemError::ENOTDIR)
    }

    fn find_cgroup_dir_from_fs_root(
        fs_root: Arc<dyn IndexNode>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let view_root = Self::fs_root_cgroup(&fs_root)?;
        if !view_root.is_ancestor_of(cgroup) {
            return Err(SystemError::ENOENT);
        }
        let rel = crate::cgroup::cgroup_path_relative_to_node(cgroup, &view_root);
        if rel == "/" {
            return Ok(fs_root);
        }

        let mut cur = fs_root;
        for comp in rel.trim_start_matches('/').split('/') {
            if comp.is_empty() {
                continue;
            }
            if comp == ".." {
                return Err(SystemError::ENOENT);
            }
            cur = cur.find(comp)?;
        }
        Ok(cur)
    }

    fn cgroup_procs_inode_from_fs_root(
        fs_root: Arc<dyn IndexNode>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let dir = Self::find_cgroup_dir_from_fs_root(fs_root, cgroup)?;
        dir.find("cgroup.procs")
    }

    pub(super) fn check_attach_permissions(
        fs_root: Arc<dyn IndexNode>,
        src_cgroup: &Arc<CgroupNode>,
        dst_cgroup: &Arc<CgroupNode>,
    ) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();
        let cred = current.cred();

        let dst_procs = Self::cgroup_procs_inode_from_fs_root(fs_root.clone(), dst_cgroup)?;
        let dst_md = dst_procs.metadata()?;
        cred.inode_permission(&dst_md, PermissionMask::MAY_WRITE.bits())
            .map_err(|_| SystemError::EACCES)?;

        let common = cgroup_common_ancestor(src_cgroup, dst_cgroup);
        let common_procs = Self::cgroup_procs_inode_from_fs_root(fs_root, &common)?;
        let common_md = common_procs.metadata()?;
        cred.inode_permission(&common_md, PermissionMask::MAY_WRITE.bits())
            .map_err(|_| SystemError::EACCES)?;

        Ok(())
    }

    fn new_file(
        name: String,
        cgroup: Arc<CgroupNode>,
        ty: CgroupCoreFile,
        init: &[u8],
        mode: u16,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            inner: SpinLock::new(Cgroup2InodeInner {
                parent: Weak::new(),
                metadata: Metadata {
                    size: init.len() as i64,
                    mode: InodeMode::from_bits_truncate(mode as u32),
                    uid: 0,
                    gid: 0,
                    blk_size: CGROUP2_BLOCK_SIZE as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    file_type: FileType::File,
                    nlinks: 1,
                    raw_dev: Default::default(),
                    flags: InodeFlags::empty(),
                },
                name,
                kind: Cgroup2InodeKind::File {
                    cgroup,
                    ty,
                    data: init.to_vec(),
                },
            }),
        })
    }

    fn add_child(
        parent: &Arc<Cgroup2Inode>,
        name: &str,
        child: Arc<Cgroup2Inode>,
    ) -> Result<(), SystemError> {
        let fs_weak = parent.fs.read().clone();
        child.set_fs(fs_weak);
        child.inner.lock().parent = Arc::downgrade(parent);

        let mut inner = parent.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.insert(name.to_string(), child);
                Ok(())
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn add_file_from_spec(
        dir: &Arc<Cgroup2Inode>,
        cgroup: Arc<CgroupNode>,
        spec: CgroupFileSpec,
    ) -> Result<(), SystemError> {
        let file =
            Cgroup2Inode::new_file(spec.name.to_string(), cgroup, spec.ty, spec.init, spec.mode);
        Self::add_child(dir, spec.name, file)
    }

    fn sync_managed_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let (cgroup, desired, desired_names) = {
            let inner = dir.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => (
                    cgroup.clone(),
                    files::desired_file_specs(cgroup),
                    files::desired_file_names(cgroup),
                ),
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        {
            let mut inner = dir.inner.lock();
            if let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind {
                children.retain(|_, child| {
                    let child_inner = child.inner.lock();
                    match &child_inner.kind {
                        Cgroup2InodeKind::File { .. } => {
                            desired_names.contains(child_inner.name.as_str())
                        }
                        Cgroup2InodeKind::Dir { .. } => true,
                    }
                });
            } else {
                return Err(SystemError::ENOTDIR);
            }
        }

        for spec in desired {
            let exists = {
                let inner = dir.inner.lock();
                match &inner.kind {
                    Cgroup2InodeKind::Dir { children, .. } => children.contains_key(spec.name),
                    _ => return Err(SystemError::ENOTDIR),
                }
            };
            if !exists {
                Self::add_file_from_spec(dir, cgroup.clone(), spec)?;
            }
        }

        Ok(())
    }

    fn sync_cached_child_controller_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let cached_dirs = {
            let inner = dir.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { children, .. } => children
                    .values()
                    .filter(|child| {
                        let child_inner = child.inner.lock();
                        matches!(&child_inner.kind, Cgroup2InodeKind::Dir { .. })
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        for child in cached_dirs {
            Self::sync_managed_files(&child)?;
        }
        Ok(())
    }

    fn lookup_child(
        parent: &Arc<Cgroup2Inode>,
        name: &str,
    ) -> Result<Arc<Cgroup2Inode>, SystemError> {
        if name == "." {
            return Ok(parent.clone());
        }
        if name == ".." {
            return Ok(parent
                .inner
                .lock()
                .parent
                .upgrade()
                .unwrap_or_else(|| parent.clone()));
        }

        Self::prune_stale_dir_cache(parent)?;
        Self::sync_managed_files(parent)?;

        {
            let inner = parent.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { children, .. } => {
                    if let Some(inode) = children.get(name).cloned() {
                        return Ok(inode);
                    }
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        }

        let parent_cgroup = {
            let inner = parent.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => cgroup.clone(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let child_cgroup = parent_cgroup.child(name).ok_or(SystemError::ENOENT)?;
        let child = Cgroup2Inode::new_dir(name.to_string(), child_cgroup);
        Cgroup2Inode::add_child(parent, name, child.clone())?;
        Cgroup2Inode::populate_core_files(&child)?;

        let inner = parent.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.get(name).cloned().ok_or(SystemError::ENOENT)
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    pub(super) fn populate_core_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        Self::sync_managed_files(dir)
    }

    fn read_file(
        inner: &Cgroup2InodeInner,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let bytes = match &inner.kind {
            Cgroup2InodeKind::File { cgroup, ty, .. } => files::read_file(cgroup, *ty),
            _ => return Err(SystemError::EISDIR),
        };

        let start = core::cmp::min(offset, bytes.len());
        let end = core::cmp::min(offset + len, bytes.len());
        let n = end.saturating_sub(start);
        if n > buf.len() {
            return Err(SystemError::ENOBUFS);
        }
        buf[..n].copy_from_slice(&bytes[start..end]);
        Ok(n)
    }

    fn replace_file_data(this: &Arc<Cgroup2Inode>, new_data: &[u8]) -> Result<(), SystemError> {
        let mut inner = this.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::File { data, .. } => {
                data.clear();
                data.extend_from_slice(new_data);
                inner.metadata.size = data.len() as i64;
                Ok(())
            }
            _ => Err(SystemError::EISDIR),
        }
    }

    fn write_file(
        this: &Arc<Cgroup2Inode>,
        offset: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let (cgroup, ty) = {
            let inner = this.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::File { cgroup, ty, .. } => (cgroup.clone(), *ty),
                _ => return Err(SystemError::EISDIR),
            }
        };

        if offset != 0 {
            return Err(SystemError::EINVAL);
        }

        match ty {
            CgroupCoreFile::Procs => Self::write_procs(this, &cgroup, buf),
            CgroupCoreFile::SubtreeControl => Self::write_subtree_control(this, &cgroup, buf),
            _ => {
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let new_data = files::write_controller_file(&cgroup, ty, input)?;
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
        }
    }

    fn write_procs(
        this: &Arc<Cgroup2Inode>,
        cgroup: &Arc<CgroupNode>,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        let pid_str = input.trim();
        let current = ProcessManager::current_pcb();
        let task = if pid_str == "0" {
            current.clone()
        } else {
            let pid_num = pid_str.parse::<usize>().map_err(|_| SystemError::EINVAL)?;
            ProcessManager::find_task_by_vpid(crate::process::RawPid::new(pid_num))
                .ok_or(SystemError::ESRCH)?
        };
        let src = task.task_cgroup_node();
        let fs_nsdelegate = this
            .fs()
            .as_any_ref()
            .downcast_ref::<Cgroup2Fs>()
            .map(|fs| fs.nsdelegate())
            .unwrap_or(false);
        if fs_nsdelegate {
            let ns_root = current.nsproxy().cgroup_ns.root_cgroup().clone();
            if !ns_root.is_ancestor_of(cgroup) {
                return Err(SystemError::ENOENT);
            }
            if !ns_root.is_ancestor_of(&src) {
                return Err(SystemError::ENOENT);
            }
        }
        if Arc::ptr_eq(&src, cgroup) {
            return Ok(buf.len());
        }
        Self::check_attach_permissions(this.fs().root_inode(), &src, cgroup)?;
        let leader = {
            let ti = task.threads_read_irqsave();
            ti.group_leader().unwrap_or_else(|| task.clone())
        };
        let others = leader.threads_read_irqsave().group_tasks_clone();

        let mut to_move = Vec::new();
        if !leader.is_exited() {
            to_move.push(leader.clone());
        }
        for weak in others {
            if let Some(t) = weak.upgrade() {
                if !t.is_exited() {
                    to_move.push(t);
                }
            }
        }
        if to_move.is_empty() {
            return Err(SystemError::ESRCH);
        }
        let moved_tasks = to_move.len();

        let _cgroup_guard = cgroup_accounting_lock().lock();
        cgroup_migrate_vet_dst_with_src(&src, cgroup, moved_tasks)?;

        for t in to_move {
            t.set_task_cgroup_node(cgroup.clone());
        }
        Ok(buf.len())
    }

    fn write_subtree_control(
        this: &Arc<Cgroup2Inode>,
        cgroup: &Arc<CgroupNode>,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        if !ProcessManager::current_pcb().cred().has_cap_sys_admin() {
            return Err(SystemError::EPERM);
        }
        let dir = this
            .inner
            .lock()
            .parent
            .upgrade()
            .ok_or(SystemError::ENOENT)?;
        let _cgroup_guard = cgroup_accounting_lock().lock();
        let new_data = files::apply_subtree_control(cgroup, input)?;
        Self::replace_file_data(this, &new_data)?;
        Self::sync_cached_child_controller_files(&dir)?;
        Ok(buf.len())
    }
}

impl IndexNode for Cgroup2Inode {
    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let inner = self.inner.lock();
        Cgroup2Inode::read_file(&inner, offset, len, buf)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let n = core::cmp::min(len, buf.len());
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::write_file(&this, offset, &buf[..n])
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        match &self.inner.lock().kind {
            Cgroup2InodeKind::File { .. } if len == 0 => Ok(()),
            Cgroup2InodeKind::File { .. } => Err(SystemError::EINVAL),
            Cgroup2InodeKind::Dir { .. } => Err(SystemError::EISDIR),
        }
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.inner.lock().metadata.clone())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.inner.lock().metadata = metadata.clone();
        Ok(())
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inner.metadata, now, relatime);
        Ok(())
    }

    fn create(
        &self,
        name: &str,
        file_type: FileType,
        _mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if file_type != FileType::Dir {
            return Err(SystemError::EINVAL);
        }

        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\n')
        {
            return Err(SystemError::EINVAL);
        }

        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;

        let cgroup = {
            let inner = self.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, children } => {
                    if children.contains_key(name) || cgroup.child(name).is_some() {
                        return Err(SystemError::EEXIST);
                    }
                    cgroup.clone()
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let child_cgroup = cgroup_root().create_child(&cgroup, name)?;
        let child = Cgroup2Inode::new_dir(name.to_string(), child_cgroup);
        Cgroup2Inode::add_child(&this, name, child.clone())?;
        Cgroup2Inode::populate_core_files(&child)?;
        Ok(child)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        if name == "." || name == ".." || name.starts_with("cgroup.") {
            return Err(SystemError::EINVAL);
        }
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;
        let child = Cgroup2Inode::lookup_child(&this, name)?;

        let _cgroup = {
            let inner = child.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => {
                    if cgroup.has_children() {
                        return Err(SystemError::ENOTEMPTY);
                    }
                    if cgroup.has_tasks() {
                        return Err(SystemError::EBUSY);
                    }
                    cgroup.clone()
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        {
            let parent_cgroup = this.inner.lock();
            if let Cgroup2InodeKind::Dir { cgroup: p, .. } = &parent_cgroup.kind {
                cgroup_root().remove_child(p, name)?;
            }
        }

        let mut inner = self.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.remove(name);
                Ok(())
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // cgroup core files are always present and managed by kernel.
        Err(SystemError::EPERM)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let this = self.self_ref.upgrade().unwrap();
        let inode = Cgroup2Inode::lookup_child(&this, name)?;
        Ok(inode)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.read().upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;
        Cgroup2Inode::sync_managed_files(&this)?;
        let inner = self.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { cgroup, children } => {
                let mut names = vec![".".to_string(), "..".to_string()];
                for child in cgroup.children_names() {
                    if !names.iter().any(|n| n == &child) {
                        names.push(child);
                    }
                }
                names.extend(children.keys().cloned());
                names.sort();
                names.dedup();
                Ok(names)
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn dname(&self) -> Result<crate::filesystem::vfs::utils::DName, SystemError> {
        Ok(self.inner.lock().name.clone().into())
    }
}
