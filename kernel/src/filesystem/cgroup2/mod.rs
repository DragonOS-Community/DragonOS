use alloc::sync::Arc;

use linkme::distributed_slice;
use system_error::SystemError;

use crate::{
    cgroup::{cgroup_root, CgroupNode},
    filesystem::{
        sysfs::sysfs_instance,
        vfs::{mount::MountFlags, FileSystem, FileSystemMakerData, MountableFileSystem, FSMAKER},
    },
    libs::once::Once,
    process::ProcessManager,
    register_mountable_fs,
};

use self::{inode::Cgroup2Inode, mount::Cgroup2Fs};

mod files;
mod inode;
mod migration;
mod mount;
mod permissions;

/// Security identity attached to an open cgroup file description.
#[derive(Clone)]
pub struct CgroupOpenState {
    pub(crate) cred: Arc<crate::process::cred::Cred>,
    pub(crate) namespace: Arc<crate::process::namespace::cgroup_namespace::CgroupNamespace>,
}

impl core::fmt::Debug for CgroupOpenState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CgroupOpenState")
            .field("cred", &self.cred)
            .field("namespace_root", &self.namespace.root_cgroup().id())
            .finish()
    }
}

pub fn cgroup2_check_attach_namespace(
    fs: &dyn FileSystem,
    src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
    ns: &crate::process::namespace::cgroup_namespace::CgroupNamespace,
) -> Result<(), SystemError> {
    if fs
        .as_any_ref()
        .downcast_ref::<Cgroup2Fs>()
        .is_some_and(|fs| fs.nsdelegate())
        && (!ns.root_cgroup().is_ancestor_of(src) || !ns.root_cgroup().is_ancestor_of(dst))
    {
        return Err(SystemError::ENOENT);
    }
    Ok(())
}

pub(super) const CGROUP2_MAX_NAMELEN: usize = 255;
pub(super) const CGROUP2_BLOCK_SIZE: u64 = 512;
pub(super) const AVAILABLE_CONTROLLERS: [&str; 4] = ["cpu", "cpuset", "memory", "pids"];
pub(super) const DOMAIN_CONTROLLERS: [&str; 1] = ["memory"];

pub fn cgroup2_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        result = Some((|| -> Result<(), SystemError> {
            sysfs_instance().ensure_mount_point_path(
                &["fs", "cgroup"],
                crate::filesystem::vfs::InodeMode::from_bits_truncate(0o755),
            )?;

            let root_inode = ProcessManager::current_mntns().root_inode();
            let sys = root_inode.find("sys")?;
            let fs_dir = sys.find("fs")?;
            let cgroup_dir = fs_dir.find("cgroup")?;

            let cgroup_fs = Cgroup2Fs::new(cgroup_root().root());
            cgroup_dir.mount(cgroup_fs, MountFlags::empty())?;

            ::log::info!("Cgroup2 mounted at /sys/fs/cgroup");
            Ok(())
        })());
    });
    result.unwrap_or(Ok(()))
}

pub fn cgroup2_check_attach_permissions(
    fs_root: Arc<dyn crate::filesystem::vfs::IndexNode>,
    src_cgroup: &Arc<CgroupNode>,
    dst_cgroup: &Arc<CgroupNode>,
) -> Result<(), SystemError> {
    Cgroup2Inode::check_attach_permissions(fs_root, src_cgroup, dst_cgroup)
}

pub fn cgroup2_inode_to_node(
    inode: &Arc<dyn crate::filesystem::vfs::IndexNode>,
) -> Result<Arc<CgroupNode>, SystemError> {
    let cgroup_inode = inode
        .as_any_ref()
        .downcast_ref::<Cgroup2Inode>()
        .ok_or(SystemError::EINVAL)?;
    cgroup_inode.cgroup().ok_or(SystemError::ENOTDIR)
}

register_mountable_fs!(Cgroup2Fs, CGROUP2FSMAKER, "cgroup2");
