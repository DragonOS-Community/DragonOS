use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    cgroup::{cgroup_accounting_lock, cgroup_root, cpuset, CgroupNode},
    filesystem::vfs::{
        file::{FileFlags, FilePrivateData},
        vcore::generate_inode_id,
        FileSystem, FileType, IndexNode, InodeFlags, InodeMode, Metadata, OpenFileBehavior,
        PollableInode, PostWriteSyncPolicy, SetMetadataMask,
    },
    libs::{mutex::MutexGuard, rwlock::RwLock, rwsem::RwSem, spinlock::SpinLock},
    mm::MemoryManagementArch,
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
    pub(super) event_source: Option<super::event_notify::EventSource>,
    pub(super) event_seen: AtomicU64,
    pub(super) event_registration: AtomicUsize,
    event_admitted: AtomicBool,
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
            event_source: None,
            event_seen: AtomicU64::new(0),
            event_registration: AtomicUsize::new(0),
            event_admitted: AtomicBool::new(false),
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

        let mut retired = Vec::new();
        retired
            .try_reserve(stale.len())
            .map_err(|_| SystemError::ENOMEM)?;
        {
            let mut inner = parent.inner.lock();
            if let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind {
                for name in &stale {
                    if let Some(entry) = children.remove_entry(name) {
                        retired.push(entry);
                    }
                }
            } else {
                return Err(SystemError::ENOTDIR);
            }
        }
        drop(retired);
        Ok(())
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
    ) -> Result<Arc<Self>, SystemError> {
        let cpuset_generation = cpuset::generation(&cgroup);
        let permissions = cgroup.file_permissions_instance(&name, mode as u32);
        let file_generation = permissions.read().generation;
        let source = match ty {
            CgroupCoreFile::MemoryEvents | CgroupCoreFile::MemoryEventsLocal => {
                Some(super::event_notify::EventSource::Memory {
                    account: cgroup
                        .local_memory_generation()
                        .ok_or(SystemError::ENODEV)?
                        .account,
                    local: ty == CgroupCoreFile::MemoryEventsLocal,
                })
            }
            CgroupCoreFile::Events => Some(super::event_notify::EventSource::Core(cgroup.clone())),
            _ => None,
        };
        let seen = source.as_ref().map_or(0, |source| source.sequence());
        let inode = Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            event_source: source,
            event_seen: AtomicU64::new(seen),
            event_registration: AtomicUsize::new(0),
            event_admitted: AtomicBool::new(false),
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
        });
        if inode.event_source.is_some() {
            super::event_notify::INODES.register(&inode, |id| {
                inode.event_registration.store(id, Ordering::Release)
            })?;
        }
        Ok(inode)
    }

    pub(super) fn event_delivery(&self) -> Option<(u64, Arc<Self>, Option<Arc<Self>>)> {
        let _update = crate::cgroup::lock();
        if !self.event_admitted.load(Ordering::Acquire) {
            return None;
        }
        let sequence = self.event_source.as_ref()?.sequence();
        if sequence == self.event_seen.load(Ordering::Acquire) {
            return None;
        }
        let inner = self.inner.lock();
        Some((sequence, self.self_ref.upgrade()?, inner.parent.upgrade()))
    }

    /// A notifier can outlive unmount's mark retirement. Pin the actual
    /// backend if it is still alive, rather than calling fs()'s path invariant
    /// unwrap on a temporarily upgraded, retired canonical inode.
    pub(super) fn event_filesystem(&self) -> Option<Arc<Cgroup2Fs>> {
        self.fs.read().upgrade()
    }

    fn admit_files(dir: &Arc<Self>) {
        let inner = dir.inner.lock();
        if let Cgroup2InodeKind::Dir { children, .. } = &inner.kind {
            for inode in children.values() {
                inode.event_admitted.store(true, Ordering::Release);
            }
        }
    }

    /// Explicit cancellation is necessary even if the notifier temporarily
    /// upgraded a private inode and is waiting for the birth UPDATE guard.
    fn cancel_private_files(dir: &Arc<Self>) {
        let children = {
            let mut inner = dir.inner.lock();
            match &mut inner.kind {
                Cgroup2InodeKind::Dir { children, .. } => core::mem::take(children),
                _ => return,
            }
        };
        for inode in children.values() {
            inode.cancel_event_registration();
        }
        drop(children);
    }

    fn cancel_event_registration(&self) {
        self.event_admitted.store(false, Ordering::Release);
        let id = self.event_registration.swap(0, Ordering::AcqRel);
        super::event_notify::INODES.unregister(id);
    }

    fn add_child(
        parent: &Arc<Cgroup2Inode>,
        name: &str,
        child: Arc<Cgroup2Inode>,
    ) -> Result<(), SystemError> {
        let key = Self::prepare_child_slot(parent, name)?;
        Self::insert_prepared_child(parent, key, child);
        Ok(())
    }

    /// UPDATE excludes cache growth. All backing/key preparation is outside
    /// inode spin locks; final insertion cannot introduce a new failure.
    fn prepare_child_slot(parent: &Arc<Self>, name: &str) -> Result<String, SystemError> {
        let mut key = String::new();
        key.try_reserve_exact(name.len())
            .map_err(|_| SystemError::ENOMEM)?;
        key.push_str(name);
        let (len, capacity) = {
            let inner = parent.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { children, .. } => (children.len(), children.capacity()),
                _ => return Err(SystemError::ENOTDIR),
            }
        };
        let needed = len.checked_add(1).ok_or(SystemError::ENOMEM)?;
        if capacity < needed {
            let mut prepared = HashMap::new();
            prepared
                .try_reserve(needed)
                .map_err(|_| SystemError::ENOMEM)?;
            {
                let mut inner = parent.inner.lock();
                let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind else {
                    unreachable!();
                };
                prepared.extend(children.drain());
                core::mem::swap(children, &mut prepared);
            }
            drop(prepared);
        }
        Ok(key)
    }

    fn insert_prepared_child(parent: &Arc<Self>, key: String, child: Arc<Self>) {
        Self::bind_private_child(parent, &child);
        let retired = {
            let mut inner = parent.inner.lock();
            let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind else {
                unreachable!();
            };
            debug_assert!(children.len() < children.capacity());
            children.insert(key, child)
        };
        drop(retired);
    }

    /// Bind identity before populating children, without publishing/admitting
    /// this inode. New files inherit the actual mounted filesystem identity.
    fn bind_private_child(parent: &Arc<Self>, child: &Arc<Self>) {
        let fs_weak = parent.fs.read().clone();
        child.set_fs(fs_weak);
        child.inner.lock().parent = Arc::downgrade(parent);
    }

    fn add_file_from_spec(
        dir: &Arc<Cgroup2Inode>,
        cgroup: Arc<CgroupNode>,
        spec: CgroupFileSpec,
    ) -> Result<(), SystemError> {
        let file =
            Cgroup2Inode::new_file(spec.name.to_string(), cgroup, spec.ty, spec.init, spec.mode)?;
        if let Err(error) = Self::add_child(dir, spec.name, file.clone()) {
            file.cancel_event_registration();
            return Err(error);
        }
        file.event_admitted.store(
            dir.event_admitted.load(Ordering::Acquire),
            Ordering::Release,
        );
        Ok(())
    }

    fn sync_managed_files_locked(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let cgroup = {
            let inner = dir.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => cgroup.clone(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };
        let desired = files::desired_file_specs(&cgroup);
        let desired_names = files::desired_file_names(&cgroup);
        let count = {
            let inner = dir.inner.lock();
            let Cgroup2InodeKind::Dir { children, .. } = &inner.kind else {
                unreachable!();
            };
            children.len()
        };
        let mut retired = Vec::new();
        retired
            .try_reserve(count)
            .map_err(|_| SystemError::ENOMEM)?;
        {
            let mut inner = dir.inner.lock();
            if let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind {
                for entry in children.drain_filter(|_, child| {
                    let child_inner = child.inner.lock();
                    match &child_inner.kind {
                        Cgroup2InodeKind::File {
                            cgroup,
                            ty,
                            cpuset_generation,
                            file_generation,
                            ..
                        } => {
                            !(desired_names.contains(child_inner.name.as_str())
                                && *file_generation == cgroup.file_generation(&child_inner.name)
                                && (!matches!(ty, CgroupCoreFile::Cpuset(_))
                                    || *cpuset_generation == cpuset::generation(cgroup)))
                        }
                        Cgroup2InodeKind::Dir { .. } => false,
                    }
                }) {
                    retired.push(entry);
                }
            } else {
                return Err(SystemError::ENOTDIR);
            }
        }
        drop(retired);

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

    fn lookup_child(
        parent: &Arc<Cgroup2Inode>,
        name: &str,
    ) -> Result<Arc<Cgroup2Inode>, SystemError> {
        let _update = crate::cgroup::lock();
        Self::lookup_child_locked(parent, name)
    }

    fn lookup_child_locked(parent: &Arc<Self>, name: &str) -> Result<Arc<Self>, SystemError> {
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
        Self::sync_managed_files_locked(parent)?;

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
        Self::bind_private_child(parent, &child);
        let prepared = (|| {
            Self::sync_managed_files_locked(&child)?;
            let key = Self::prepare_child_slot(parent, name)?;
            Self::insert_prepared_child(parent, key, child.clone());
            child.event_admitted.store(true, Ordering::Release);
            Self::admit_files(&child);
            Ok::<(), SystemError>(())
        })();
        if let Err(error) = prepared {
            Self::cancel_private_files(&child);
            return Err(error);
        }

        let inner = parent.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.get(name).cloned().ok_or(SystemError::ENOENT)
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    pub(super) fn populate_core_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let _update = crate::cgroup::lock();
        if let Err(error) = Self::sync_managed_files_locked(dir) {
            Self::cancel_private_files(dir);
            return Err(error);
        }
        dir.event_admitted.store(true, Ordering::Release);
        Self::admit_files(dir);
        Ok(())
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

    fn check_write_len(&self, len: usize) -> Result<(), SystemError> {
        let limit = {
            let inner = self.inner.lock();
            match inner.kind {
                Cgroup2InodeKind::File {
                    ty: CgroupCoreFile::Cpuset(file),
                    ..
                } => cpuset::max_write_len(file),
                Cgroup2InodeKind::File { .. } => crate::arch::MMArch::PAGE_SIZE,
                _ => return Err(SystemError::EISDIR),
            }
        };
        if len > limit {
            return Err(SystemError::E2BIG);
        }
        Ok(())
    }

    fn write_file(
        this: &Arc<Cgroup2Inode>,
        _offset: usize,
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
        // One admission transaction for all nonempty writes. Like kernfs's
        // active reference, liveness precedes authorization and parsing.
        let _update = crate::cgroup::lock();
        files::check_live_file(&cgroup, ty, file_generation)?;

        match ty {
            CgroupCoreFile::Procs | CgroupCoreFile::Threads => {
                super::migration::write_locked(&cgroup, buf, ty == CgroupCoreFile::Procs, open)
            }
            CgroupCoreFile::SubtreeControl => {
                Self::write_subtree_control_locked(this, &cgroup, buf)
            }
            _ => {
                // Delegation admission shares the update transaction with
                // init-namespace remounts which change hierarchy policy.
                if cgroup_root().nsdelegate()
                    && !matches!(
                        ty,
                        CgroupCoreFile::MemoryOomGroup | CgroupCoreFile::MemoryReclaim
                    )
                    && !Arc::ptr_eq(
                        &open.namespace,
                        &crate::process::namespace::cgroup_namespace::INIT_CGROUP_NAMESPACE,
                    )
                    && Arc::ptr_eq(open.namespace.root_cgroup(), &cgroup)
                {
                    return Err(SystemError::EPERM);
                }
                if super::memory::is_memory_write(ty) {
                    let action = super::memory::prepare_write_locked(&cgroup, ty, buf)?;
                    drop(_update);
                    action.execute()?;
                    return Ok(buf.len());
                }
                let new_data = files::write_controller_file_locked(&cgroup, ty, generation, buf)?;
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
        }
    }

    /// Caller holds the update lock and has validated file liveness.
    fn write_subtree_control_locked(
        _this: &Arc<Cgroup2Inode>,
        cgroup: &Arc<CgroupNode>,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        // Like other cgroup files, delegation is authorized by the opener's
        // VFS write permission, not a second global CAP_SYS_ADMIN requirement.
        let was_enabled = cgroup.subtree_controller_enabled("cpuset");
        // The only fallible process snapshot is prepared before committing the
        // controller mask. Allocation failure cannot leave a partial update.
        let tasks = if input
            .split_whitespace()
            .any(|op| matches!(op, "+cpuset" | "-cpuset" | "+cpu" | "-cpu"))
        {
            Some(crate::process::snapshot_all_processes()?)
        } else {
            None
        };
        let control_prepared = files::prepare_subtree_control(cgroup, input)?;
        let _new_data = if let Some(mut prepared) = control_prepared {
            {
                let _cgroup_guard = cgroup_accounting_lock().lock();
                prepared.commit(cgroup);
            }
            prepared.finish(cgroup, tasks.as_deref().unwrap_or(&[]))
        } else {
            let _cgroup_guard = cgroup_accounting_lock().lock();
            files::apply_subtree_control(cgroup, input)?
        };
        let is_enabled = cgroup.subtree_controller_enabled("cpuset");
        if was_enabled != is_enabled {
            cpuset::controller_changed_locked(
                cgroup,
                tasks.expect("cpuset update has a prepared snapshot"),
            );
        }
        // Prepared CPU/memory commits are already complete. Epoch-aware lookup refreshes
        // cached files lazily on every mount; do not report a cache allocation
        // error as a failed controller transaction after changing resources.
        Ok(buf.len())
    }
}

impl PollableInode for Cgroup2Inode {
    fn poll(&self, data: &FilePrivateData) -> Result<usize, SystemError> {
        let flags = match data {
            FilePrivateData::Cgroup(open) => open
                .events
                .as_ref()
                .map_or_else(super::events::default_mask, |events| events.poll()),
            _ => return Err(SystemError::EINVAL),
        };
        Ok(flags.bits() as usize)
    }

    fn add_epitem(
        &self,
        item: Arc<crate::filesystem::epoll::EPollItem>,
        data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        if let FilePrivateData::Cgroup(open) = data {
            if let Some(events) = &open.events {
                events.add(item);
            }
            return Ok(());
        }
        Err(SystemError::EINVAL)
    }

    fn remove_epitem(
        &self,
        item: &Arc<crate::filesystem::epoll::EPollItem>,
        data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        if let FilePrivateData::Cgroup(open) = data {
            if let Some(events) = &open.events {
                return events.remove(item);
            }
            return Ok(());
        }
        Err(SystemError::EINVAL)
    }
}

impl IndexNode for Cgroup2Inode {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
        behavior.data_file_write_checks = false;
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            let current = ProcessManager::current_pcb();
            let events = {
                let (cgroup, ty, generation) = {
                    let inner = self.inner.lock();
                    match &inner.kind {
                        Cgroup2InodeKind::File {
                            cgroup,
                            ty,
                            file_generation,
                            ..
                        } if matches!(
                            ty,
                            CgroupCoreFile::MemoryEvents
                                | CgroupCoreFile::MemoryEventsLocal
                                | CgroupCoreFile::Events
                        ) =>
                        {
                            (Some(cgroup.clone()), *ty, *file_generation)
                        }
                        _ => (None, CgroupCoreFile::Events, 0),
                    }
                };
                if let Some(cgroup) = cgroup {
                    let source = {
                        let _update = crate::cgroup::lock();
                        files::check_live_file(&cgroup, ty, generation)?;
                        self.event_source.clone().ok_or(SystemError::ENODEV)?
                    };
                    Some(super::events::CgroupEventOpen::new(source)?)
                } else {
                    None
                }
            };
            *data = FilePrivateData::Cgroup(super::CgroupOpenState {
                cred: current.cred(),
                namespace: current.nsproxy().cgroup_ns.clone(),
                events,
            });
            Ok(())
        })();
        result.map_err(Into::into)
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        if let FilePrivateData::Cgroup(open) = &*data {
            if let Some(events) = &open.events {
                events.close();
            }
        }
        Ok(())
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            let events = match &*data {
                FilePrivateData::Cgroup(open) => open.events.clone(),
                _ => None,
            };
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
            if let Some(events) = events {
                return events.read(&cgroup, ty, file_generation, offset, len, buf);
            }
            Cgroup2Inode::read_file(&cgroup, ty, generation, file_generation, offset, len, buf)
        })();
        result.map_err(Into::into)
    }

    fn write_primary_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: crate::filesystem::vfs::PrimaryWriteSource<'_, '_>,
        _intent: crate::filesystem::vfs::WriteOperation,
        data: MutexGuard<FilePrivateData>,
        stage: &mut crate::filesystem::vfs::WritePrivilegeStage<'_>,
    ) -> Result<
        crate::filesystem::vfs::PrimaryWriteResult,
        crate::mm::allocation::MemoryAllocationError,
    > {
        self.check_write_len(len)?;
        source.with_exact_buffer(len, |buffer| {
            stage.complete();
            self.write_at(offset, len, buffer, data)
                .map(crate::filesystem::vfs::PrimaryWriteResult::new)
        })
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            self.check_write_len(len)?;
            if buf.len() < len {
                return Err(SystemError::ENOBUFS);
            }
            let open = match &*data {
                FilePrivateData::Cgroup(open) => open.clone(),
                _ => return Err(SystemError::EINVAL),
            };
            drop(data);
            let this = self.self_ref.upgrade().unwrap();
            Cgroup2Inode::write_file(&this, offset, &buf[..len], &open)
        })();
        result.map_err(Into::into)
    }

    fn resize(&self, len: usize) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> =
            match &self.inner.lock().kind {
                Cgroup2InodeKind::File { .. } if len == 0 => Ok(()),
                Cgroup2InodeKind::File { .. } => Err(SystemError::EINVAL),
                Cgroup2InodeKind::Dir { .. } => Err(SystemError::EISDIR),
            };
        result.map_err(Into::into)
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

    fn set_metadata(
        &self,
        metadata: &Metadata,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
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
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let _update = crate::cgroup::lock();
        self.apply_metadata_locked(metadata, mask)
            .map_err(Into::into)
    }

    fn update_metadata_masked(
        &self,
        update: &mut crate::filesystem::vfs::MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, crate::mm::allocation::MemoryAllocationError> {
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
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
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
            let cgroup = {
                let inner = self.inner.lock();
                match &inner.kind {
                    Cgroup2InodeKind::Dir { cgroup, .. } => {
                        if cgroup.child(name).is_some() {
                            return Err(SystemError::EEXIST);
                        }
                        cgroup.clone()
                    }
                    _ => return Err(SystemError::ENOTDIR),
                }
            };

            let cred = ProcessManager::current_pcb().cred();
            let mut prepared_inode = None;
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
                let child = Cgroup2Inode::new_dir(name.to_string(), node.clone());
                Self::bind_private_child(&this, &child);
                let birth = (|| {
                    Self::sync_managed_files_locked(&child)?;
                    let key = Self::prepare_child_slot(&this, name)?;
                    Self::insert_prepared_child(&this, key, child.clone());
                    child.event_admitted.store(true, Ordering::Release);
                    Self::admit_files(&child);
                    Ok::<(), SystemError>(())
                })();
                if let Err(error) = birth {
                    Self::cancel_private_files(&child);
                    return Err(error);
                }
                prepared_inode = Some(child);
                Ok(())
            })?;
            Ok(prepared_inode.expect("successful private directory birth produced an inode"))
        })();
        result
            .map(|inode| inode as Arc<dyn IndexNode>)
            .map_err(Into::into)
    }

    fn rmdir(&self, name: &str) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            if name == "." || name == ".." || name.starts_with("cgroup.") {
                return Err(SystemError::EINVAL);
            }
            let this = self.self_ref.upgrade().unwrap();
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

            let retired = {
                let mut inner = self.inner.lock();
                match &mut inner.kind {
                    Cgroup2InodeKind::Dir { children, .. } => {
                        if children
                            .get(name)
                            .is_some_and(|cached| Arc::ptr_eq(cached, &child))
                        {
                            children.remove_entry(name)
                        } else {
                            None
                        }
                    }
                    _ => return Err(SystemError::ENOTDIR),
                }
            };
            drop(retired);
            Ok(())
        })();
        result.map_err(Into::into)
    }

    fn unlink(
        &self,
        _name: &str,
    ) -> Result<
        crate::filesystem::vfs::LinkRemovalOutcome,
        crate::mm::allocation::MemoryAllocationError,
    > {
        let result: core::result::Result<_, system_error::SystemError> = Err(SystemError::EPERM);
        result.map_err(Into::into)
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
        let _update = crate::cgroup::lock();
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;
        Cgroup2Inode::sync_managed_files_locked(&this)?;
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

impl Drop for Cgroup2Inode {
    fn drop(&mut self) {
        self.cancel_event_registration();
    }
}
