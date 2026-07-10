use super::config::OverlayMountData;
use super::entry::OvlLayer;
use super::inode::OvlInode;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::{
    self, FileSystem, FileSystemMakerData, FileType, FsInfo, IndexNode, InodeId,
    MountableFileSystem, SuperBlock,
};
use crate::libs::{casting::DowncastArc, mutex::Mutex};
use crate::process::Cred;
use crate::process::ProcessManager;
use alloc::collections::BTreeMap;
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
        let fs = inode.fs();
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

#[derive(Debug, Default)]
struct OvlInodeCache {
    entries: BTreeMap<OvlInodeCacheKey, Weak<OvlInode>>,
    insertions_since_prune: usize,
}

impl OvlInodeCache {
    fn intern(
        &mut self,
        key: OvlInodeCacheKey,
        create: impl FnOnce() -> Arc<OvlInode>,
    ) -> Arc<OvlInode> {
        if let Some(inode) = self.entries.get(&key).and_then(Weak::upgrade) {
            return inode;
        }

        if self.insertions_since_prune >= INODE_CACHE_PRUNE_INTERVAL {
            self.entries.retain(|_, inode| inode.strong_count() != 0);
            self.insertions_since_prune = 0;
        }

        let inode = create();
        self.entries.insert(key, Arc::downgrade(&inode));
        self.insertions_since_prune += 1;
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
    inode_cache: Mutex<OvlInodeCache>,
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

        let inode = self.inode_cache.lock().intern(key, || {
            let inode = Arc::new(OvlInode::new(
                redirect,
                file_type,
                upper_inode,
                lower_inodes,
            ));
            inode.set_fs(Arc::downgrade(self));
            inode
        });
        Ok(inode)
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

        let root_inode = Arc::new(OvlInode::new(
            String::new(),
            upper_file_type,
            Some(upper_inode),
            lower_roots
                .iter()
                .map(|(_, lower_inode)| lower_inode.clone())
                .collect(),
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
                inode_cache: Mutex::new(OvlInodeCache::default()),
            }
        });
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
