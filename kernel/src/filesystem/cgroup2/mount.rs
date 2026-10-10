use alloc::{string::String, sync::Arc};
use core::{any::Any, fmt::Write};

use system_error::SystemError;

use crate::{
    cgroup::{cgroup_root, CgroupNode},
    filesystem::vfs::{
        mount::{MountFS, MountFlags},
        FileSystem, FileSystemMakerData, FsCreationContext, FsInfo, FsReconfigureRequest, Magic,
        MountableFileSystem, SuperBlock,
    },
    process::{namespace::cgroup_namespace::INIT_CGROUP_NAMESPACE, ProcessManager},
};

use super::{inode::Cgroup2Inode, CGROUP2_BLOCK_SIZE, CGROUP2_MAX_NAMELEN};

#[derive(Debug)]
pub(super) struct Cgroup2Fs {
    root_inode: Arc<Cgroup2Inode>,
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
    pub(super) fn new(root_cg: Arc<CgroupNode>) -> Arc<Self> {
        let root_inode = Cgroup2Inode::new_dir(String::new(), root_cg);

        let fs = Arc::new(Self {
            root_inode: root_inode.clone(),
        });
        root_inode.set_fs(Arc::downgrade(&fs));

        Cgroup2Inode::populate_core_files(&root_inode)
            .expect("cgroup2: populate root files failed");
        fs
    }

    pub(super) fn nsdelegate(&self) -> bool {
        cgroup_root().nsdelegate()
    }
}

fn parse_options(raw_data: Option<&str>) -> Result<bool, SystemError> {
    let mut nsdelegate = false;
    for token in raw_data.unwrap_or("").split(',').map(str::trim) {
        match token {
            "" => {}
            "nsdelegate" => nsdelegate = true,
            _ => return Err(SystemError::EINVAL),
        }
    }
    Ok(nsdelegate)
}

/// Linux apply_cgroup_root_flags(): a successful mount/remount from the
/// initial namespace replaces the policy; a child namespace cannot change it.
fn apply_options(nsdelegate: bool) {
    let namespace = ProcessManager::current_pcb().nsproxy().cgroup_ns.clone();
    if Arc::ptr_eq(&namespace, &INIT_CGROUP_NAMESPACE) {
        let _update = crate::cgroup::lock();
        cgroup_root().set_nsdelegate(nsdelegate);
    }
}

impl FileSystem for Cgroup2Fs {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

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

    fn reconfigure(&self, request: FsReconfigureRequest<'_>) -> Result<MountFlags, SystemError> {
        let nsdelegate = parse_options(request.raw_data)?;
        apply_options(nsdelegate);
        Ok(request.sb_flags & request.sb_flags_mask)
    }

    fn validate_reconfigure_parameter(
        &self,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), SystemError> {
        if key == "nsdelegate" && value.is_none() {
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn proc_show_mount_options(
        &self,
        _mount: &MountFS,
        out: &mut dyn Write,
    ) -> Result<(), SystemError> {
        if self.nsdelegate() {
            out.write_str("nsdelegate")
                .map_err(|_| SystemError::EINVAL)?;
        }
        Ok(())
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
    const SUPPORTS_FSCONFIG_LEGACY_OPTIONS: bool = true;

    fn make_mount_data(
        raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        Self::make_mount_data_in_context(raw_data, source, &FsCreationContext::current())
    }

    fn make_mount_data_in_context(
        raw_data: Option<&str>,
        _source: &str,
        context: &FsCreationContext,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let nsdelegate = parse_options(raw_data)?;

        let root_cgroup = context.cgroup_ns.root_cgroup().clone();
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
        let fs = Cgroup2Fs::new(root_cgroup);
        apply_options(nsdelegate);
        Ok(fs)
    }
}
