use alloc::sync::Arc;

use linkme::distributed_slice;
use system_error::SystemError;

use crate::{
    driver::virtio::virtio_fs::{virtio_fs_find_instance, VirtioFsInstance},
    filesystem::vfs::{
        file::File, FileSystem, FileSystemMakerData, FsInfo, IndexNode, MountableFileSystem,
        SuperBlock, FSMAKER,
    },
    mm::{fault::PageFaultMessage, VirtRegion, VmFaultReason, VmFlags},
    process::ProcessManager,
    register_mountable_fs,
};

use super::super::{
    conn::FuseConn,
    fs::{FuseFS, FuseMountData},
    protocol::FuseOutHeader,
};
use super::{bridge::start_bridge, VIRTIOFS_MAX_REQUEST_SIZE, VIRTIOFS_RSP_BUF_SIZE};

#[derive(Debug)]
struct VirtioFsMountData {
    rootmode: u32,
    user_id: u32,
    group_id: u32,
    allow_other: bool,
    default_permissions: bool,
    dax_mode: VirtioFsDaxMode,
    conn: Arc<FuseConn>,
    instance: Arc<VirtioFsInstance>,
}

impl FileSystemMakerData for VirtioFsMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Debug)]
struct VirtioFsFs {
    inner: Arc<dyn FileSystem>,
    instance: Arc<VirtioFsInstance>,
    session_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VirtioFsDaxMode {
    Never,
    Always,
    Inode,
}

impl VirtioFsFs {
    fn parse_opt_u32_decimal(v: &str) -> Result<u32, SystemError> {
        v.parse::<u32>().map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_u32_octal(v: &str) -> Result<u32, SystemError> {
        u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_bool_switch(v: &str) -> bool {
        v.is_empty() || v != "0"
    }

    fn parse_dax_mode(v: &str) -> Result<VirtioFsDaxMode, SystemError> {
        if v.is_empty() {
            return Ok(VirtioFsDaxMode::Always);
        }

        match v {
            "always" => Ok(VirtioFsDaxMode::Always),
            "never" => Ok(VirtioFsDaxMode::Never),
            "inode" => Ok(VirtioFsDaxMode::Inode),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn parse_mount_options(
        raw: Option<&str>,
    ) -> Result<(u32, u32, u32, bool, bool, VirtioFsDaxMode), SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
        let mut default_permissions = true;
        let mut allow_other = true;
        let mut dax_mode = VirtioFsDaxMode::Never;

        for part in raw.unwrap_or("").split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (part, ""),
            };

            match k {
                "rootmode" => rootmode = Some(Self::parse_opt_u32_octal(v)?),
                "user_id" => user_id = Some(Self::parse_opt_u32_decimal(v)?),
                "group_id" => group_id = Some(Self::parse_opt_u32_decimal(v)?),
                "default_permissions" => default_permissions = Self::parse_opt_bool_switch(v),
                "allow_other" => allow_other = Self::parse_opt_bool_switch(v),
                "dax" => dax_mode = Self::parse_dax_mode(v)?,
                _ => return Err(SystemError::EINVAL),
            }
        }

        if dax_mode != VirtioFsDaxMode::Never {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        Ok((
            rootmode.unwrap_or(0o040755),
            user_id.unwrap_or(cred.fsuid.data() as u32),
            group_id.unwrap_or(cred.fsgid.data() as u32),
            default_permissions,
            allow_other,
            dax_mode,
        ))
    }
}

impl FileSystem for VirtioFsFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.inner.root_inode()
    }

    fn info(&self) -> FsInfo {
        self.inner.info()
    }

    fn support_readahead(&self) -> bool {
        self.inner.support_readahead()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "virtiofs"
    }

    fn super_block(&self) -> SuperBlock {
        self.inner.super_block()
    }

    fn statfs(&self, inode: &Arc<dyn IndexNode>) -> Result<SuperBlock, SystemError> {
        self.inner.statfs(inode)
    }

    fn permission_policy(&self) -> crate::filesystem::vfs::FsPermissionPolicy {
        self.inner.permission_policy()
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner.fault(pfm)
    }

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner.page_mkwrite(pfm)
    }

    fn mprotect(&self, old_vm_flags: VmFlags, new_vm_flags: VmFlags) -> Result<(), SystemError> {
        self.inner.mprotect(old_vm_flags, new_vm_flags)
    }

    fn vma_close(&self, file: &Arc<File>, region: VirtRegion, vm_flags: VmFlags) {
        self.inner.vma_close(file, region, vm_flags)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        self.inner.map_pages(pfm, start_pgoff, end_pgoff)
    }

    fn on_umount(&self) {
        self.inner.on_umount();
        self.instance.wait_session_released(self.session_id);
    }
}

impl MountableFileSystem for VirtioFsFs {
    fn make_mount_data(
        raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        if source.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let (rootmode, user_id, group_id, default_permissions, allow_other, dax_mode) =
            Self::parse_mount_options(raw_data)?;
        let instance = virtio_fs_find_instance(source).ok_or(SystemError::ENODEV)?;
        let conn = FuseConn::new_for_virtiofs_with_dax(
            VIRTIOFS_MAX_REQUEST_SIZE,
            VIRTIOFS_RSP_BUF_SIZE,
            instance.cache_window_len(),
        );

        Ok(Some(Arc::new(VirtioFsMountData {
            rootmode,
            user_id,
            group_id,
            allow_other,
            default_permissions,
            dax_mode,
            conn,
            instance,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let md = data
            .and_then(|d| d.as_any().downcast_ref::<VirtioFsMountData>())
            .ok_or(SystemError::EINVAL)?;

        let fuse_mount_data = FuseMountData {
            rootmode: md.rootmode,
            user_id: md.user_id,
            group_id: md.group_id,
            max_read: VIRTIOFS_RSP_BUF_SIZE
                .saturating_sub(core::mem::size_of::<FuseOutHeader>())
                .min(u32::MAX as usize) as u32,
            allow_other: md.allow_other,
            default_permissions: md.default_permissions,
            conn: md.conn.clone(),
        };

        if md.dax_mode != VirtioFsDaxMode::Never {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let inner = <FuseFS as MountableFileSystem>::make_fs(Some(
            &fuse_mount_data as &dyn FileSystemMakerData,
        ))?;

        let session_id = match start_bridge(md.instance.clone(), md.conn.clone()) {
            Ok(id) => id,
            Err(e) => {
                inner.on_umount();
                return Err(e);
            }
        };

        Ok(Arc::new(VirtioFsFs {
            inner,
            instance: md.instance.clone(),
            session_id,
        }))
    }
}

register_mountable_fs!(VirtioFsFs, VIRTIOFSMAKER, "virtiofs");
