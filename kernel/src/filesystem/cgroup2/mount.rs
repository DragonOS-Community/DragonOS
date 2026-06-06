use alloc::{string::String, sync::Arc};
use core::any::Any;

use system_error::SystemError;

use crate::{
    cgroup::{cgroup_root, CgroupNode},
    filesystem::vfs::{
        FileSystem, FileSystemMakerData, FsInfo, Magic, MountableFileSystem, SuperBlock,
    },
    process::ProcessManager,
};

use super::{inode::Cgroup2Inode, CGROUP2_BLOCK_SIZE, CGROUP2_MAX_NAMELEN};

#[derive(Debug)]
pub(super) struct Cgroup2Fs {
    root_inode: Arc<Cgroup2Inode>,
    nsdelegate: bool,
}

#[derive(Debug)]
struct Cgroup2MountData {
    root_cgroup: Arc<CgroupNode>,
    nsdelegate: bool,
}

impl FileSystemMakerData for Cgroup2MountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Cgroup2Fs {
    pub(super) fn new(root_cg: Arc<CgroupNode>, nsdelegate: bool) -> Arc<Self> {
        let root_inode = Cgroup2Inode::new_dir(String::new(), root_cg);

        let fs = Arc::new(Self {
            root_inode: root_inode.clone(),
            nsdelegate,
        });
        root_inode.set_fs(Arc::downgrade(&fs));

        Cgroup2Inode::populate_core_files(&root_inode)
            .expect("cgroup2: populate root files failed");
        fs
    }

    pub(super) fn nsdelegate(&self) -> bool {
        self.nsdelegate
    }
}

impl FileSystem for Cgroup2Fs {
    fn root_inode(&self) -> Arc<dyn crate::filesystem::vfs::IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: CGROUP2_MAX_NAMELEN,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "cgroup2"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::CGROUP2_SUPER_MAGIC,
            CGROUP2_BLOCK_SIZE,
            CGROUP2_MAX_NAMELEN as u64,
        )
    }
}

impl MountableFileSystem for Cgroup2Fs {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mut nsdelegate = false;
        if let Some(opts) = raw_data {
            for raw in opts.split(',') {
                let token = raw.trim();
                if token.is_empty() {
                    continue;
                }
                match token {
                    "nsdelegate" => nsdelegate = true,
                    "nsdelegate=0" => nsdelegate = false,
                    "nsdelegate=1" => nsdelegate = true,
                    _ => return Err(SystemError::EINVAL),
                }
            }
        }

        let root_cgroup = ProcessManager::current_pcb()
            .nsproxy()
            .cgroup_ns
            .root_cgroup()
            .clone();
        Ok(Some(Arc::new(Cgroup2MountData {
            root_cgroup,
            nsdelegate,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data.and_then(|d| d.as_any().downcast_ref::<Cgroup2MountData>());
        let root_cgroup = mount_data
            .map(|d| d.root_cgroup.clone())
            .unwrap_or_else(|| cgroup_root().root());
        let nsdelegate = mount_data.map(|d| d.nsdelegate).unwrap_or(false);
        Ok(Cgroup2Fs::new(root_cgroup, nsdelegate))
    }
}
