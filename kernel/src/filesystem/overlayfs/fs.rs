use super::config::OverlayMountData;
use super::entry::OvlLayer;
use super::inode::{DirState, OvlInode};
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::mount::{MountFS, MountFSInode};
use crate::filesystem::vfs::{
    self, vcore::generate_inode_id, FileSystem, FileSystemMakerData, FileType, FsInfo, IndexNode,
    InodeId, MountableFileSystem, SuperBlock,
};
use crate::libs::{casting::DowncastArc, mutex::Mutex};
use crate::process::Cred;
use crate::process::ProcessManager;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use system_error::SystemError;

const MAX_MOUNT_ANCESTOR_DEPTH: usize = vfs::MAX_PATHLEN;
const INODE_CACHE_PRUNE_INTERVAL: usize = 256;
type LowerRoot = (String, Arc<dyn IndexNode>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RealInodeIdentity {
    filesystem: usize,
    dev_id: usize,
    inode_id: InodeId,
}

impl RealInodeIdentity {
    fn from_inode(inode: &Arc<dyn IndexNode>) -> Result<Self, SystemError> {
        let fs = OverlayFS::canonical_backing_fs(inode);
        let metadata = inode.metadata()?;
        Ok(Self {
            filesystem: Arc::as_ptr(&fs) as *const () as usize,
            dev_id: metadata.dev_id,
            inode_id: metadata.inode_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OvlInodeOrigin {
    Lower {
        lower: Vec<RealInodeIdentity>,
        upper: Option<RealInodeIdentity>,
    },
    Upper(RealInodeIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OvlInodeCacheKey {
    redirect: String,
    origin: OvlInodeOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DirIdentity {
    Lower(Vec<RealInodeIdentity>),
    Upper(RealInodeIdentity),
}

#[derive(Debug, Default)]
struct DirStateCache {
    entries: BTreeMap<DirIdentity, Weak<DirState>>,
    insertions_since_prune: usize,
}

impl DirStateCache {
    fn intern(&mut self, key: DirIdentity) -> Arc<DirState> {
        if let Some(state) = self.entries.get(&key).and_then(Weak::upgrade) {
            return state;
        }
        if self.insertions_since_prune >= INODE_CACHE_PRUNE_INTERVAL {
            self.entries.retain(|_, state| state.strong_count() != 0);
            self.insertions_since_prune = 0;
        }
        let state = Arc::new(DirState::default());
        self.entries.insert(key, Arc::downgrade(&state));
        self.insertions_since_prune += 1;
        state
    }
}

#[derive(Debug, Default)]
struct OvlInodeCache {
    entries: BTreeMap<OvlInodeCacheKey, Weak<OvlInode>>,
    /// Mount-global bounded retention for repeated short-lived lookups.
    recent_lookups: VecDeque<Arc<OvlInode>>,
    /// Bounded strong cache for non-samefs directories whose fallback inode
    /// number is only meaningful for the OvlInode cache lifetime.
    recent_fallback_dirs: VecDeque<Arc<OvlInode>>,
    insertions_since_prune: usize,
}

const FALLBACK_DIR_CACHE_SIZE: usize = 256;
const LOOKUP_RETENTION_SIZE: usize = 1024;
const LOCK_STRIPE_COUNT: usize = 64;

impl OvlInodeCache {
    fn intern(
        &mut self,
        key: OvlInodeCacheKey,
        retain_fallback_dir: bool,
        create: impl FnOnce() -> Arc<OvlInode>,
    ) -> Arc<OvlInode> {
        let inode = if let Some(inode) = self.entries.get(&key).and_then(Weak::upgrade) {
            inode
        } else {
            if self.insertions_since_prune >= INODE_CACHE_PRUNE_INTERVAL {
                self.entries.retain(|_, inode| inode.strong_count() != 0);
                self.insertions_since_prune = 0;
            }

            let inode = create();
            self.entries.insert(key, Arc::downgrade(&inode));
            self.insertions_since_prune += 1;
            inode
        };

        self.recent_lookups
            .retain(|cached| !Arc::ptr_eq(cached, &inode));
        self.recent_lookups.push_back(inode.clone());
        if self.recent_lookups.len() > LOOKUP_RETENTION_SIZE {
            self.recent_lookups.pop_front();
        }

        if retain_fallback_dir {
            self.recent_fallback_dirs
                .retain(|cached| !Arc::ptr_eq(cached, &inode));
            self.recent_fallback_dirs.push_back(inode.clone());
            if self.recent_fallback_dirs.len() > FALLBACK_DIR_CACHE_SIZE {
                self.recent_fallback_dirs.pop_front();
            }
        }
        inode
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct OvlSuperBlock {
    super_block: SuperBlock,
    pseudo_dev: DeviceNumber, // virtual device number
    is_lower: bool,
}

#[derive(Debug)]
pub(super) struct OverlayFS {
    #[allow(dead_code)]
    pub(super) numlayer: usize,
    #[allow(dead_code)]
    pub(super) numfs: u32,
    #[allow(dead_code)]
    pub(super) numdatalayer: usize,
    pub(super) layers: Vec<OvlLayer>, // layer 0 is read-write, subsequent layers are read-only
    pub(super) workdir: Arc<dyn IndexNode>,
    pub(super) root_inode: Arc<OvlInode>,
    pub(super) super_block: SuperBlock,
    pub(super) mutation_lock: Mutex<()>,
    pub(super) backing_cred: Arc<Cred>,
    pub(super) samefs: bool,
    inode_cache: Mutex<OvlInodeCache>,
    dir_state_cache: Mutex<DirStateCache>,
    copy_up_locks: Vec<Mutex<()>>,
    content_locks: Vec<Mutex<()>>,
}

impl FileSystem for OverlayFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> vfs::FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "overlayfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }
}

impl OverlayFS {
    pub(super) fn ovl_upper_mnt(&self) -> Arc<OvlInode> {
        self.layers[0].mnt.clone()
    }

    pub(super) fn intern_inode(
        self: &Arc<Self>,
        redirect: String,
        file_type: FileType,
        upper_inode: Option<Arc<dyn IndexNode>>,
        lower_inodes: Vec<Arc<dyn IndexNode>>,
    ) -> Result<Arc<OvlInode>, SystemError> {
        let origin = if lower_inodes.is_empty() {
            OvlInodeOrigin::Upper(RealInodeIdentity::from_inode(
                upper_inode.as_ref().ok_or(SystemError::ENOENT)?,
            )?)
        } else {
            OvlInodeOrigin::Lower {
                lower: lower_inodes
                    .iter()
                    .map(RealInodeIdentity::from_inode)
                    .collect::<Result<Vec<_>, _>>()?,
                upper: upper_inode
                    .as_ref()
                    .map(RealInodeIdentity::from_inode)
                    .transpose()?,
            }
        };
        let key = OvlInodeCacheKey {
            redirect: redirect.clone(),
            origin,
        };

        let retain_fallback_dir = !self.samefs && file_type == FileType::Dir;
        let inode = self
            .inode_cache
            .lock()
            .intern(key, retain_fallback_dir, || {
                let fallback_id = retain_fallback_dir.then(generate_inode_id);
                let inode = Arc::new(OvlInode::new(
                    redirect,
                    file_type,
                    upper_inode,
                    lower_inodes,
                    fallback_id,
                ));
                inode.set_fs(Arc::downgrade(self));
                inode
            });
        inode.load_origin_once()?;
        Ok(inode)
    }

    pub(super) fn intern_dir_state(&self, inode: &OvlInode) -> Result<Arc<DirState>, SystemError> {
        let identity = if inode.lower_inodes.is_empty() {
            DirIdentity::Upper(RealInodeIdentity::from_inode(
                inode
                    .upper_inode
                    .lock()
                    .as_ref()
                    .ok_or(SystemError::ENOENT)?,
            )?)
        } else {
            DirIdentity::Lower(
                inode
                    .lower_inodes
                    .iter()
                    .map(RealInodeIdentity::from_inode)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        Ok(self.dir_state_cache.lock().intern(identity))
    }

    pub(super) fn copy_up_lock(&self, redirect: &str) -> &Mutex<()> {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in redirect.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        &self.copy_up_locks[hash as usize % self.copy_up_locks.len()]
    }

    pub(super) fn content_lock(
        &self,
        inode: &Arc<dyn IndexNode>,
    ) -> Result<&Mutex<()>, SystemError> {
        let identity = RealInodeIdentity::from_inode(inode)?;
        let mut hash = identity.filesystem as u64;
        hash ^= (identity.dev_id as u64).rotate_left(21);
        hash ^= (identity.inode_id.data() as u64).rotate_left(42);
        Ok(&self.content_locks[hash as usize % self.content_locks.len()])
    }

    fn canonical_backing_fs(inode: &Arc<dyn IndexNode>) -> Arc<dyn FileSystem> {
        let mut fs = inode.fs();
        while let Some(mount_fs) = fs.clone().downcast_arc::<MountFS>() {
            fs = mount_fs.inner_filesystem();
        }
        fs
    }

    pub(super) fn backing_fsid(&self, inode: &Arc<dyn IndexNode>) -> Result<u32, SystemError> {
        let target_fs = Self::canonical_backing_fs(inode);
        // Origin records describe a lower object. Prefer a lower layer when the
        // upper and lower directories happen to share the same filesystem.
        for layer in self.layers.iter().skip(1).chain(self.layers.iter().take(1)) {
            let real = if layer.index == 0 {
                layer.mnt.upper_inode.lock().clone()
            } else {
                layer.mnt.lower_inodes.first().cloned()
            };
            let Some(real) = real else {
                continue;
            };
            if Arc::ptr_eq(&target_fs, &Self::canonical_backing_fs(&real)) {
                return Ok(layer.fsid);
            }
        }
        Err(SystemError::EXDEV)
    }

    pub(super) fn backing_fsid_matches_device(
        &self,
        fsid: u32,
        dev_id: usize,
    ) -> Result<bool, SystemError> {
        let Some(layer) = self.layers.iter().find(|layer| layer.fsid == fsid) else {
            return Ok(false);
        };
        let Some(lower) = layer.mnt.lower_inodes.first() else {
            return Ok(false);
        };
        Ok(lower.metadata()?.dev_id == dev_id)
    }

    fn same_mount_inode(
        left: &Arc<dyn IndexNode>,
        right: &Arc<dyn IndexNode>,
    ) -> Result<bool, SystemError> {
        let left = Self::canonical_inode(left.clone());
        let right = Self::canonical_inode(right.clone());
        if !Arc::ptr_eq(&left.fs(), &right.fs()) {
            return Ok(false);
        }

        Ok(left.metadata()?.inode_id == right.metadata()?.inode_id)
    }

    fn is_mount_ancestor(
        ancestor: &Arc<dyn IndexNode>,
        node: &Arc<dyn IndexNode>,
    ) -> Result<bool, SystemError> {
        let ancestor = Self::canonical_inode(ancestor.clone());
        let node = Self::canonical_inode(node.clone());
        if !Arc::ptr_eq(&ancestor.fs(), &node.fs()) {
            return Ok(false);
        }

        let mut current = node;
        for _ in 0..MAX_MOUNT_ANCESTOR_DEPTH {
            if Self::same_mount_inode(&ancestor, &current)? {
                return Ok(true);
            }

            let parent = Self::canonical_inode(current.parent().map_err(|_| SystemError::EINVAL)?);
            if Self::same_mount_inode(&parent, &current)? {
                return Ok(false);
            }
            if !Arc::ptr_eq(&ancestor.fs(), &parent.fs()) {
                return Ok(false);
            }
            current = parent;
        }

        Err(SystemError::ELOOP)
    }

    fn canonical_inode(mut inode: Arc<dyn IndexNode>) -> Arc<dyn IndexNode> {
        while let Some(mount_inode) = inode.clone().downcast_arc::<MountFSInode>() {
            inode = mount_inode.underlying_inode();
        }
        inode
    }

    fn layers_overlap_either_direction(
        left: &Arc<dyn IndexNode>,
        right: &Arc<dyn IndexNode>,
    ) -> Result<bool, SystemError> {
        Ok(Self::same_mount_inode(left, right)?
            || Self::is_mount_ancestor(left, right)?
            || Self::is_mount_ancestor(right, left)?)
    }
}

impl MountableFileSystem for OverlayFS {
    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<OverlayMountData>())
            .ok_or(SystemError::EINVAL)?;
        let root_inode = ProcessManager::current_mntns().root_inode();
        let upper_inode = root_inode
            .lookup(&mount_data.upper_dir)
            .map_err(|_| SystemError::EINVAL)?;
        let upper_file_type = upper_inode.metadata()?.file_type;
        if upper_file_type != FileType::Dir {
            return Err(SystemError::EINVAL);
        }
        let upper_layer = OvlLayer {
            mnt: Arc::new(OvlInode::new(
                mount_data.upper_dir.clone(),
                upper_file_type,
                Some(upper_inode.clone()),
                Vec::new(),
                None,
            )),
            index: 0,
            fsid: 0,
        };

        let lower_roots: Result<Vec<LowerRoot>, SystemError> = mount_data
            .lower_dirs
            .iter()
            .map(|dir| {
                let lower_inode = ProcessManager::current_mntns()
                    .root_inode()
                    .lookup(dir)
                    .map_err(|_| SystemError::EINVAL)?;
                if lower_inode.metadata()?.file_type != FileType::Dir {
                    return Err(SystemError::EINVAL);
                }
                Ok((dir.clone(), lower_inode))
            })
            .collect();

        let lower_roots = lower_roots?;

        let lower_layers: Result<Vec<OvlLayer>, SystemError> = lower_roots
            .iter()
            .enumerate()
            .map(|(i, (dir, lower_inode))| {
                let lower_file_type = lower_inode.metadata()?.file_type;
                Ok(OvlLayer {
                    mnt: Arc::new(OvlInode::new(
                        dir.clone(),
                        lower_file_type,
                        None,
                        vec![lower_inode.clone()],
                        None,
                    )),
                    index: (i + 1) as u32,
                    fsid: (i + 1) as u32,
                })
            })
            .collect();

        let lower_layers = lower_layers?;

        let workdir_inode = root_inode
            .lookup(&mount_data.work_dir)
            .map_err(|_| SystemError::EINVAL)?;
        if workdir_inode.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::EINVAL);
        }
        if !Arc::ptr_eq(&upper_inode.fs(), &workdir_inode.fs())
            || Self::layers_overlap_either_direction(&upper_inode, &workdir_inode)?
        {
            return Err(SystemError::EINVAL);
        }
        for (i, (_, lower_inode)) in lower_roots.iter().enumerate() {
            if Self::layer_is_same_or_descendant_of(lower_inode, &upper_inode)?
                || Self::layer_is_same_or_descendant_of(lower_inode, &workdir_inode)?
            {
                return Err(SystemError::ELOOP);
            }

            for (_, other_lower_inode) in lower_roots.iter().skip(i + 1) {
                if Self::layers_overlap_either_direction(lower_inode, other_lower_inode)? {
                    return Err(SystemError::ELOOP);
                }
            }
        }

        if lower_roots.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let mut layers = Vec::new();
        layers.push(upper_layer);
        layers.extend(lower_layers);

        let upper_backing_fs = Self::canonical_backing_fs(&upper_inode);
        let samefs = lower_roots
            .iter()
            .all(|(_, lower)| Arc::ptr_eq(&upper_backing_fs, &Self::canonical_backing_fs(lower)));
        let root_inode = Arc::new(OvlInode::new(
            String::new(),
            upper_file_type,
            Some(upper_inode),
            lower_roots
                .iter()
                .map(|(_, lower_inode)| lower_inode.clone())
                .collect(),
            (!samefs).then(generate_inode_id),
        ));

        let super_block = SuperBlock::new(vfs::Magic::OVERLAYFS_MAGIC, 4096, 255);
        let backing_cred = ProcessManager::current_pcb().cred();
        let fs = Arc::new_cyclic(|weak_fs| {
            for layer in &layers {
                layer.mnt.set_fs(weak_fs.clone());
            }
            root_inode.set_fs(weak_fs.clone());

            OverlayFS {
                numlayer: layers.len(),
                numfs: 1,
                numdatalayer: lower_roots.len(),
                layers,
                workdir: workdir_inode,
                root_inode,
                super_block: super_block.clone(),
                mutation_lock: Mutex::new(()),
                backing_cred,
                samefs,
                inode_cache: Mutex::new(OvlInodeCache::default()),
                dir_state_cache: Mutex::new(DirStateCache::default()),
                copy_up_locks: (0..LOCK_STRIPE_COUNT).map(|_| Mutex::new(())).collect(),
                content_locks: (0..LOCK_STRIPE_COUNT).map(|_| Mutex::new(())).collect(),
            }
        });
        fs.root_inode.load_origin_once()?;
        Ok(fs)
    }

    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mount_data = OverlayMountData::from_raw(raw_data).map_err(|e| {
            log::error!("Failed to create overlay mount data: {:?}", e);
            e
        })?;
        Ok(Some(Arc::new(mount_data)))
    }
}

impl OverlayFS {
    fn layer_is_same_or_descendant_of(
        layer: &Arc<dyn IndexNode>,
        ancestor: &Arc<dyn IndexNode>,
    ) -> Result<bool, SystemError> {
        Ok(Self::same_mount_inode(layer, ancestor)? || Self::is_mount_ancestor(ancestor, layer)?)
    }
}
