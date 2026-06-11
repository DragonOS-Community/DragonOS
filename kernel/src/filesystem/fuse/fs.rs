use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        FileSystem, FileSystemMakerData, FileType, FsInfo, IndexNode, InodeFlags, InodeId,
        InodeMode, Magic, Metadata, MountableFileSystem, SuperBlock, FSMAKER,
    },
    libs::mutex::Mutex,
    mm::{fault::PageFaultMessage, VmFaultReason},
    process::ProcessManager,
    register_mountable_fs,
    time::PosixTimeSpec,
};

use linkme::distributed_slice;

use super::{
    conn::FuseConn,
    inode::FuseNode,
    private_data::FuseFilePrivateData,
    protocol::{fuse_read_struct, FuseStatfsOut, FUSE_ATTR_SUBMOUNT, FUSE_ROOT_ID, FUSE_STATFS},
};

#[derive(Debug)]
pub struct FuseMountData {
    pub rootmode: u32,
    pub user_id: u32,
    pub group_id: u32,
    pub max_read: u32,
    pub allow_other: bool,
    pub default_permissions: bool,
    pub conn: Arc<FuseConn>,
}

struct FuseParsedMountOptions {
    fd: i32,
    rootmode: u32,
    user_id: u32,
    group_id: u32,
    max_read: u32,
    default_permissions: bool,
    allow_other: bool,
}

impl FileSystemMakerData for FuseMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Debug)]
pub struct FuseFS {
    root: Arc<FuseNode>,
    super_block: SuperBlock,
    conn: Arc<FuseConn>,
    nodes: Mutex<BTreeMap<u64, Weak<FuseNode>>>,
    default_permissions: bool,
    is_submount: bool,
}

impl FuseFS {
    fn parse_opt_u32_decimal(v: &str) -> Result<u32, SystemError> {
        v.parse::<u32>().map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_i32_decimal(v: &str) -> Result<i32, SystemError> {
        v.parse::<i32>().map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_u32_octal(v: &str) -> Result<u32, SystemError> {
        u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_bool_switch(v: &str) -> bool {
        v.is_empty() || v != "0"
    }

    fn parse_mount_options(raw: Option<&str>) -> Result<FuseParsedMountOptions, SystemError> {
        let mut fd: Option<i32> = None;
        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
        let mut max_read = u32::MAX;
        let mut default_permissions = false;
        let mut allow_other = false;

        let s = raw.unwrap_or("");
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (part, ""),
            };
            match k {
                "fd" => {
                    fd = Some(Self::parse_opt_i32_decimal(v)?);
                }
                "rootmode" => {
                    // Linux expects octal representation.
                    rootmode = Some(Self::parse_opt_u32_octal(v)?);
                }
                "user_id" => {
                    user_id = Some(Self::parse_opt_u32_decimal(v)?);
                }
                "group_id" => {
                    group_id = Some(Self::parse_opt_u32_decimal(v)?);
                }
                "max_read" => {
                    max_read = Self::parse_opt_u32_decimal(v)?;
                }
                "default_permissions" => {
                    default_permissions = Self::parse_opt_bool_switch(v);
                }
                "allow_other" => {
                    allow_other = Self::parse_opt_bool_switch(v);
                }
                _ => {}
            }
        }

        let fd = fd.ok_or(SystemError::EINVAL)?;
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        let user_id = user_id.unwrap_or(cred.fsuid.data() as u32);
        let group_id = group_id.unwrap_or(cred.fsgid.data() as u32);
        // Default root mode: directory 0755 (with type bit).
        let rootmode = rootmode.unwrap_or(0o040755);

        Ok(FuseParsedMountOptions {
            fd,
            rootmode,
            user_id,
            group_id,
            max_read,
            default_permissions,
            allow_other,
        })
    }

    pub fn root_node(&self) -> Arc<FuseNode> {
        self.root.clone()
    }

    pub fn get_or_create_node(
        self: &Arc<Self>,
        nodeid: u64,
        parent_nodeid: u64,
        cached: Option<Metadata>,
    ) -> Arc<FuseNode> {
        self.get_or_create_node_with_generation(nodeid, parent_nodeid, cached, None)
    }

    pub fn get_or_create_node_with_generation(
        self: &Arc<Self>,
        nodeid: u64,
        parent_nodeid: u64,
        cached: Option<Metadata>,
        generation: Option<u64>,
    ) -> Arc<FuseNode> {
        if nodeid == self.root.nodeid() {
            return self.root.clone();
        }

        let mut nodes = self.nodes.lock();
        if let Some(w) = nodes.get(&nodeid) {
            if let Some(n) = w.upgrade() {
                if let Some(gen) = generation {
                    let old_gen = n.generation();
                    if old_gen != 0 && old_gen != gen {
                        n.mark_stale();
                        n.flush_forget();
                        nodes.remove(&nodeid);
                    } else {
                        n.set_generation(gen);
                        n.set_parent_nodeid(parent_nodeid);
                        if let Some(md) = cached {
                            n.set_cached_metadata(md);
                        }
                        return n;
                    }
                } else {
                    n.set_parent_nodeid(parent_nodeid);
                    if let Some(md) = cached {
                        n.set_cached_metadata(md);
                    }
                    return n;
                }
            }
        }

        let n = FuseNode::new(
            Arc::downgrade(self),
            self.conn.clone(),
            nodeid,
            parent_nodeid,
            cached,
        );
        if let Some(gen) = generation {
            n.set_generation(gen);
        }
        nodes.insert(nodeid, Arc::downgrade(&n));
        n
    }

    /// 为 virtiofs announce-submounts 创建子挂载树（共享同一 FuseConn）。
    pub fn new_submount(
        parent: &Arc<Self>,
        root_nodeid: u64,
        parent_nodeid: u64,
        root_md: Metadata,
    ) -> Arc<Self> {
        let conn = parent.conn.clone();
        let fs = Arc::new_cyclic(|weak| FuseFS {
            root: FuseNode::new(
                weak.clone(),
                conn.clone(),
                root_nodeid,
                parent_nodeid,
                Some(root_md),
            ),
            super_block: parent.super_block.clone(),
            conn,
            nodes: Mutex::new(BTreeMap::new()),
            default_permissions: parent.default_permissions,
            is_submount: true,
        });
        fs.nodes
            .lock()
            .insert(root_nodeid, Arc::downgrade(&fs.root));
        fs
    }
}

/// DragonOS currently mounts announced FUSE submounts eagerly at lookup time.
/// Linux uses dentry automount and only overlays the mountpoint; parent-tree
/// nodes outside that mountpoint remain valid.
///
/// ## Arguments
///
/// - `fuse_node`: FUSE node whose lookup attributes may contain
///   `FUSE_ATTR_SUBMOUNT`.
/// - `mountpoint`: VFS mount inode corresponding to the looked-up node.
///
/// ## Returns
///
/// - `Ok(())`: no submount was needed, the connection does not support
///   submounts, the submount already exists, or a new submount was attached.
/// - `Err(SystemError)`: metadata lookup, path resolution, or mount attachment
///   failed.
pub fn fuse_try_automount_submount(
    fuse_node: &Arc<FuseNode>,
    mountpoint: &Arc<crate::filesystem::vfs::mount::MountFSInode>,
) -> Result<(), SystemError> {
    use crate::filesystem::vfs::mount::{MountFlags, MountPath};

    let attr_flags = fuse_node.lookup_attr_flags();
    if (attr_flags & FUSE_ATTR_SUBMOUNT) == 0 {
        return Ok(());
    }
    if !fuse_node.conn().supports_submounts() {
        return Ok(());
    }

    let md = mountpoint.metadata()?;
    if mountpoint
        .mount_fs()
        .mountpoints()
        .contains_key(&md.inode_id)
    {
        return Ok(());
    }

    let parent_fs = fuse_node.fuse_fs().ok_or(SystemError::ENOENT)?;
    let sub_fs = FuseFS::new_submount(
        &parent_fs,
        fuse_node.nodeid(),
        fuse_node.parent_fuse_nodeid(),
        md,
    );
    let mount_path = Arc::new(MountPath::from(mountpoint.absolute_path()?));
    let submount_flags = mountpoint.mount_fs().mount_flags() | MountFlags::SUBMOUNT;
    let mount_res = mountpoint.mount_subtree_with_state(
        sub_fs.clone(),
        sub_fs.root_node(),
        submount_flags,
        None,
        None,
        Some(mount_path),
    );
    if let Err(e) = mount_res {
        if e != SystemError::EEXIST {
            return Err(e);
        }
    }
    Ok(())
}

impl MountableFileSystem for FuseFS {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let opts = Self::parse_mount_options(raw_data)?;

        let file = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(opts.fd)
            .ok_or(SystemError::EBADF)?;

        let conn = {
            let pdata = file.private_data.lock();
            match &*pdata {
                crate::filesystem::vfs::FilePrivateData::Fuse(FuseFilePrivateData::Dev(p)) => {
                    p.conn_ref()?
                }
                _ => return Err(SystemError::EINVAL),
            }
        };

        Ok(Some(Arc::new(FuseMountData {
            rootmode: opts.rootmode,
            user_id: opts.user_id,
            group_id: opts.group_id,
            max_read: opts.max_read,
            allow_other: opts.allow_other,
            default_permissions: opts.default_permissions,
            conn,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<FuseMountData>())
            .ok_or(SystemError::EINVAL)?;

        let super_block = SuperBlock::new(Magic::FUSE_MAGIC, 4096, 255);

        let root_md = Metadata {
            dev_id: 0,
            inode_id: InodeId::new(FUSE_ROOT_ID as usize),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
            file_type: FileType::Dir,
            mode: InodeMode::from_bits_truncate(mount_data.rootmode),
            flags: InodeFlags::empty(),
            nlinks: 2,
            uid: mount_data.user_id as usize,
            gid: mount_data.group_id as usize,
            raw_dev: crate::driver::base::device::device_number::DeviceNumber::default(),
        };

        let conn = mount_data.conn.clone();
        conn.mark_mounted()?;
        conn.configure_mount(
            mount_data.user_id,
            mount_data.group_id,
            mount_data.allow_other,
            mount_data.max_read,
        );

        let fs = Arc::new_cyclic(|weak_fs| FuseFS {
            root: FuseNode::new(
                weak_fs.clone(),
                conn.clone(),
                FUSE_ROOT_ID,
                FUSE_ROOT_ID,
                Some(root_md),
            ),
            super_block,
            conn: conn.clone(),
            nodes: Mutex::new(BTreeMap::new()),
            default_permissions: mount_data.default_permissions,
            is_submount: false,
        });

        if let Err(e) = conn.enqueue_init() {
            conn.rollback_mount_setup();
            return Err(e);
        }
        Ok(fs)
    }
}

register_mountable_fs!(FuseFS, FUSEFSMAKER, "fuse");

impl FileSystem for FuseFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "fuse"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }

    fn statfs(&self, inode: &Arc<dyn IndexNode>) -> Result<SuperBlock, SystemError> {
        match self.conn.check_allow_current_process() {
            Ok(()) => {}
            Err(SystemError::EACCES) => {
                let mut sb = self.super_block.clone();
                sb.magic = Magic::FUSE_MAGIC;
                return Ok(sb);
            }
            Err(e) => return Err(e),
        }

        let nodeid = inode
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .map(|n| n.nodeid())
            .unwrap_or(FUSE_ROOT_ID);

        let payload = self.conn.request(FUSE_STATFS, nodeid, &[])?;
        let out: FuseStatfsOut = fuse_read_struct(&payload)?;

        let mut sb = self.super_block.clone();
        sb.magic = Magic::FUSE_MAGIC;
        sb.blocks = out.st.blocks;
        sb.bfree = out.st.bfree;
        sb.bavail = out.st.bavail;
        sb.files = out.st.files;
        sb.ffree = out.st.ffree;
        sb.bsize = out.st.bsize as u64;
        sb.namelen = out.st.namelen as u64;
        sb.frsize = out.st.frsize as u64;
        Ok(sb)
    }

    fn permission_policy(&self) -> crate::filesystem::vfs::FsPermissionPolicy {
        if self.default_permissions {
            crate::filesystem::vfs::FsPermissionPolicy::Dac
        } else {
            crate::filesystem::vfs::FsPermissionPolicy::Remote
        }
    }

    fn support_readahead(&self) -> bool {
        false
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        let _ = pfm;
        VmFaultReason::VM_FAULT_SIGBUS
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        let _ = (pfm, start_pgoff, end_pgoff);
        VmFaultReason::VM_FAULT_SIGBUS
    }

    fn on_umount(&self) {
        let live_nodes: Vec<Arc<FuseNode>> = {
            let nodes = self.nodes.lock();
            nodes.values().filter_map(|w| w.upgrade()).collect()
        };
        for node in live_nodes {
            node.flush_forget();
        }
        if !self.is_submount {
            self.conn.on_umount();
        }
    }
}
