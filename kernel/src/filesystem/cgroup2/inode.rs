use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    cgroup::{cgroup_accounting_lock, cgroup_root, cpuset, CgroupNode},
    filesystem::vfs::{
        file::{FileFlags, FilePrivateData},
        vcore::generate_inode_id,
        FileSystem, FileType, IndexNode, InodeFlags, InodeMode, Metadata, OpenFileBehavior,
        PostWriteSyncPolicy, SetMetadataMask,
    },
    libs::{mutex::MutexGuard, rwlock::RwLock, rwsem::RwSem, spinlock::SpinLock},
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
    permissions: Arc<RwLock<crate::cgroup::core::CgroupFilePermissions>>,
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
        cpuset_generation: u64,
        file_generation: u64,
        data: Vec<u8>,
    },
}

impl Cgroup2Inode {
    pub(super) fn new_dir(name: String, cgroup: Arc<CgroupNode>) -> Arc<Self> {
        let permissions = cgroup.file_permissions_instance("", 0o755);
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            inner: SpinLock::new(Cgroup2InodeInner {
                parent: Weak::new(),
                permissions,
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

    pub(super) fn check_attach_permissions(
        _fs_root: Arc<dyn IndexNode>,
        src_cgroup: &Arc<CgroupNode>,
        dst_cgroup: &Arc<CgroupNode>,
    ) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();
        let cred = current.cred();

        super::permissions::check_procs_write(dst_cgroup, &cred)?;
        super::permissions::check_common_ancestor(src_cgroup, dst_cgroup, &cred)
    }

    fn new_file(
        name: String,
        cgroup: Arc<CgroupNode>,
        ty: CgroupCoreFile,
        init: &[u8],
        mode: u16,
    ) -> Arc<Self> {
        let cpuset_generation = cpuset::generation(&cgroup);
        let permissions = cgroup.file_permissions_instance(&name, mode as u32);
        let file_generation = permissions.read().generation;
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            inner: SpinLock::new(Cgroup2InodeInner {
                parent: Weak::new(),
                permissions,
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
                    cpuset_generation,
                    file_generation,
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
                        Cgroup2InodeKind::File {
                            cgroup,
                            ty,
                            cpuset_generation,
                            file_generation,
                            ..
                        } => {
                            desired_names.contains(child_inner.name.as_str())
                                && *file_generation == cgroup.file_generation(&child_inner.name)
                                && (!matches!(ty, CgroupCoreFile::Cpuset(_))
                                    || *cpuset_generation == cpuset::generation(cgroup))
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
        cgroup: &Arc<CgroupNode>,
        ty: CgroupCoreFile,
        generation: u64,
        file_generation: u64,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let bytes = files::read_file(cgroup, ty, generation, file_generation)?;

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

    /// Caller holds UPDATE_LOCK. Merge only requested attributes into this
    /// inode's pinned instance, including descriptors of removed files.
    fn apply_metadata_locked(
        &self,
        requested: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        // Validate before touching either canonical permissions or timestamps.
        let uid = if mask.contains(SetMetadataMask::UID) {
            Some(u32::try_from(requested.uid).map_err(|_| SystemError::EINVAL)?)
        } else {
            None
        };
        let gid = if mask.contains(SetMetadataMask::GID) {
            Some(u32::try_from(requested.gid).map_err(|_| SystemError::EINVAL)?)
        } else {
            None
        };
        let mut inner = self.inner.lock();
        {
            let mut attrs = inner.permissions.write();
            if let Some(uid) = uid {
                attrs.uid = uid;
            }
            if let Some(gid) = gid {
                attrs.gid = gid;
            }
            if mask.contains(SetMetadataMask::MODE) {
                attrs.mode = requested.mode.bits();
            }
        }
        // Identity, size and generation are not setattr fields. Preserve any
        // timestamp not requested, including a concurrent atime-only update.
        crate::filesystem::vfs::merge_metadata_masked(&mut inner.metadata, requested, mask);
        Ok(())
    }

    fn write_file(
        this: &Arc<Cgroup2Inode>,
        offset: usize,
        buf: &[u8],
        open: &super::CgroupOpenState,
    ) -> Result<usize, SystemError> {
        let (cgroup, ty, generation, file_generation) = {
            let inner = this.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::File {
                    cgroup,
                    ty,
                    cpuset_generation,
                    file_generation,
                    ..
                } => (cgroup.clone(), *ty, *cpuset_generation, *file_generation),
                _ => return Err(SystemError::EISDIR),
            }
        };

        if buf.is_empty() {
            return Ok(0);
        }
        if offset != 0
            && matches!(
                ty,
                CgroupCoreFile::Procs | CgroupCoreFile::Threads | CgroupCoreFile::SubtreeControl
            )
        {
            return Err(SystemError::EINVAL);
        }

        match ty {
            CgroupCoreFile::Procs | CgroupCoreFile::Threads => {
                super::migration::write(&cgroup, buf, ty == CgroupCoreFile::Procs, open)
            }
            CgroupCoreFile::SubtreeControl => Self::write_subtree_control(this, &cgroup, buf),
            _ => {
                // Serialize delegation admission with the controller update
                // and init-namespace remounts which change hierarchy policy.
                let _update = crate::cgroup::lock();
                if cgroup_root().nsdelegate()
                    && !Arc::ptr_eq(
                        &open.namespace,
                        &crate::process::namespace::cgroup_namespace::INIT_CGROUP_NAMESPACE,
                    )
                    && Arc::ptr_eq(open.namespace.root_cgroup(), &cgroup)
                {
                    return Err(SystemError::EPERM);
                }
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let new_data = files::write_controller_file_locked(
                    &cgroup,
                    ty,
                    generation,
                    file_generation,
                    buf,
                )?;
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
        }
    }

    fn write_subtree_control(
        this: &Arc<Cgroup2Inode>,
        cgroup: &Arc<CgroupNode>,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        // Like other cgroup files, delegation is authorized by the opener's
        // VFS write permission, not a second global CAP_SYS_ADMIN requirement.
        let dir = this
            .inner
            .lock()
            .parent
            .upgrade()
            .ok_or(SystemError::ENOENT)?;
        let _cpuset_guard = cpuset::lock();
        let was_enabled = cgroup.subtree_control().iter().any(|name| name == "cpuset");
        // The only fallible process snapshot is prepared before committing the
        // controller mask. Allocation failure cannot leave a partial update.
        let tasks = if input
            .split_whitespace()
            .any(|op| op == "+cpuset" || op == "-cpuset")
        {
            Some(crate::process::snapshot_all_processes()?)
        } else {
            None
        };
        let new_data = {
            let _cgroup_guard = cgroup_accounting_lock().lock();
            if !cgroup_root().is_online(cgroup) {
                return Err(SystemError::ENODEV);
            }
            files::apply_subtree_control(cgroup, input)?
        };
        let is_enabled = cgroup.subtree_control().iter().any(|name| name == "cpuset");
        if was_enabled != is_enabled {
            cpuset::controller_changed_locked(
                cgroup,
                tasks.expect("cpuset update has a prepared snapshot"),
            );
        }
        Self::replace_file_data(this, &new_data)?;
        Self::sync_cached_child_controller_files(&dir)?;
        Ok(buf.len())
    }
}

impl IndexNode for Cgroup2Inode {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();
        *data = FilePrivateData::Cgroup(super::CgroupOpenState {
            cred: current.cred(),
            namespace: current.nsproxy().cgroup_ns.clone(),
        });
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
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        // Controller reads may acquire a sleeping mutex. Never retain the
        // inode spinlock across controller calls (writes use the reverse order).
        let (cgroup, ty, generation, file_generation) = {
            let inner = self.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::File {
                    cgroup,
                    ty,
                    cpuset_generation,
                    file_generation,
                    ..
                } => (cgroup.clone(), *ty, *cpuset_generation, *file_generation),
                _ => return Err(SystemError::EISDIR),
            }
        };
        Cgroup2Inode::read_file(&cgroup, ty, generation, file_generation, offset, len, buf)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let open = match &*data {
            FilePrivateData::Cgroup(open) => open.clone(),
            _ => return Err(SystemError::EINVAL),
        };
        drop(data);
        let n = core::cmp::min(len, buf.len());
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::write_file(&this, offset, &buf[..n], &open)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        match &self.inner.lock().kind {
            Cgroup2InodeKind::File { .. } if len == 0 => Ok(()),
            Cgroup2InodeKind::File { .. } => Err(SystemError::EINVAL),
            Cgroup2InodeKind::Dir { .. } => Err(SystemError::EISDIR),
        }
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let (mut metadata, permissions) = {
            let inner = self.inner.lock();
            (inner.metadata.clone(), inner.permissions.clone())
        };
        let attrs = permissions.read();
        metadata.uid = attrs.uid as usize;
        metadata.gid = attrs.gid as usize;
        metadata.mode = InodeMode::from_bits_truncate(attrs.mode as _);
        Ok(metadata)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.set_metadata_masked(
            metadata,
            SetMetadataMask::MODE
                | SetMetadataMask::UID
                | SetMetadataMask::GID
                | SetMetadataMask::ATIME
                | SetMetadataMask::MTIME
                | SetMetadataMask::CTIME,
        )
    }

    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        let _update = crate::cgroup::lock();
        self.apply_metadata_locked(metadata, mask)
    }

    fn update_metadata_masked(
        &self,
        update: &mut crate::filesystem::vfs::MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, SystemError> {
        let _update = crate::cgroup::lock();
        // The VFS callback authorizes and computes changes from the same
        // canonical state that will be committed, across all mount views.
        // Do not hold the inode spinlock while invoking that callback.
        let current = self.metadata()?;
        let (requested, mask) = update(&current)?;
        self.apply_metadata_locked(&requested, mask)?;
        Ok(mask)
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
        mode: InodeMode,
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

        let cred = ProcessManager::current_pcb().cred();
        let child_cgroup =
            cgroup_root().create_child_exclusive_with_init(&cgroup, name, |node| {
                // Parent metadata and child publication share UPDATE_LOCK.
                let owner = crate::cgroup::core::CgroupFilePermissions::for_creation(
                    cgroup.file_permissions("", 0o755),
                    cred.fsuid.data() as u32,
                    cred.fsgid.data() as u32,
                    mode.bits(),
                    true,
                );
                node.set_file_permissions("", owner);
                for spec in files::desired_file_specs(node) {
                    node.set_file_permissions(
                        spec.name,
                        crate::cgroup::core::CgroupFilePermissions::for_creation(
                            owner,
                            cred.fsuid.data() as u32,
                            cred.fsgid.data() as u32,
                            spec.mode as u32,
                            false,
                        ),
                    );
                }
            })?;
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

        let child_cgroup = {
            let inner = child.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => cgroup.clone(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };
        let parent_cgroup = this.cgroup().ok_or(SystemError::ENOTDIR)?;
        // Do not hold an inode spin lock while waiting for the cgroup
        // structure mutex. The core transaction checks tasks, reserved fork
        // charges and online state together with removal.
        cgroup_root().remove_child(&parent_cgroup, name, &child_cgroup)?;

        let mut inner = self.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                if children
                    .get(name)
                    .is_some_and(|cached| Arc::ptr_eq(cached, &child))
                {
                    children.remove(name);
                }
                Ok(())
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn unlink(
        &self,
        _name: &str,
    ) -> Result<crate::filesystem::vfs::LinkRemovalOutcome, SystemError> {
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
