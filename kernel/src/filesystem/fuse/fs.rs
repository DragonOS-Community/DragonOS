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
    process::ProcessManager,
    register_mountable_fs,
    time::PosixTimeSpec,
};

use linkme::distributed_slice;

use super::{
    conn::FuseConn,
    inode::FuseNode,
    private_data::FuseFilePrivateData,
    protocol::{fuse_read_struct, FuseStatfsOut, FUSE_ROOT_ID, FUSE_STATFS},
};

#[derive(Debug)]
pub struct FuseMountData {
    pub rootmode: u32,
    pub user_id: u32,
    pub group_id: u32,
    pub default_permissions: bool,
    pub conn: Arc<FuseConn>,
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
}

impl FuseFS {
    fn parse_mount_options(
        raw: Option<&str>,
    ) -> Result<(i32, u32, u32, u32, bool, bool), SystemError> {
        let mut fd: Option<i32> = None;
        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
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
                    fd = Some(v.parse::<i32>().map_err(|_| SystemError::EINVAL)?);
                }
                "rootmode" => {
                    // Linux expects octal representation.
                    rootmode = Some(u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)?);
                }
                "user_id" => {
                    user_id = Some(v.parse::<u32>().map_err(|_| SystemError::EINVAL)?);
                }
                "group_id" => {
                    group_id = Some(v.parse::<u32>().map_err(|_| SystemError::EINVAL)?);
                }
                "default_permissions" => {
                    default_permissions = v.is_empty() || v != "0";
                }
                "allow_other" => {
                    allow_other = v.is_empty() || v != "0";
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

        Ok((
            fd,
            rootmode,
            user_id,
            group_id,
            default_permissions,
            allow_other,
        ))
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
        if nodeid == FUSE_ROOT_ID {
            return self.root.clone();
        }

        let mut nodes = self.nodes.lock();
        if let Some(w) = nodes.get(&nodeid) {
            if let Some(n) = w.upgrade() {
                n.set_parent_nodeid(parent_nodeid);
                if let Some(md) = cached {
                    n.set_cached_metadata(md);
                }
                return n;
            }
        }

        let n = FuseNode::new(
            Arc::downgrade(self),
            self.conn.clone(),
            nodeid,
            parent_nodeid,
            cached,
        );
        nodes.insert(nodeid, Arc::downgrade(&n));
        n
    }
}

impl MountableFileSystem for FuseFS {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let (fd, rootmode, user_id, group_id, default_permissions, allow_other) =
            Self::parse_mount_options(raw_data)?;

        let file = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        let conn = {
            let pdata = file.private_data.lock();
            match &*pdata {
                crate::filesystem::vfs::FilePrivateData::Fuse(FuseFilePrivateData::Dev(p)) => p
                    .conn
                    .clone()
                    .downcast::<FuseConn>()
                    .map_err(|_| SystemError::EINVAL)?,
                _ => return Err(SystemError::EINVAL),
            }
        };

        conn.configure_mount(user_id, group_id, allow_other);
        conn.mark_mounted()?;

        Ok(Some(Arc::new(FuseMountData {
            rootmode,
            user_id,
            group_id,
            default_permissions,
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
        });

        conn.enqueue_init()?;
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

    fn on_umount(&self) {
        let live_nodes: Vec<Arc<FuseNode>> = {
            let nodes = self.nodes.lock();
            nodes.values().filter_map(|w| w.upgrade()).collect()
        };
        for node in live_nodes {
            node.flush_forget();
        }
        self.conn.on_umount();
    }
}
