use alloc::{sync::Arc, vec};
use core::fmt::Write;

use linkme::distributed_slice;
use system_error::SystemError;

use crate::{
    driver::virtio::virtio_fs::{virtio_fs_find_instance, VirtioFsInstance},
    filesystem::vfs::{
        file::File, mount::MountFS, FilePrivateData, FileSystem, FileSystemMakerData, FsInfo,
        IndexNode, MountableFileSystem, SuperBlock, FSMAKER,
    },
    mm::{
        fault::{FaultFlags, PageFaultMessage},
        MemoryManagementArch, VirtRegion, VmFaultReason, VmFlags,
    },
    process::ProcessManager,
    register_mountable_fs,
};

use super::super::{
    conn::FuseConn,
    fs::{FuseFS, FuseMountData},
    private_data::FuseFilePrivateData,
    protocol::FuseOutHeader,
};
use super::{
    bridge::start_bridge, dax::DaxMountMode, VIRTIOFS_MAX_REQUEST_SIZE, VIRTIOFS_RSP_BUF_SIZE,
};

#[derive(Debug)]
struct VirtioFsMountData {
    rootmode: u32,
    user_id: u32,
    group_id: u32,
    allow_other: bool,
    default_permissions: bool,
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
    conn: Arc<FuseConn>,
    instance: Arc<VirtioFsInstance>,
    session_id: u64,
}

impl VirtioFsFs {
    unsafe fn dax_fault(&self, pfm: &mut PageFaultMessage) -> Option<VmFaultReason> {
        let vma = pfm.vma();
        let guard = vma.lock();
        let vm_flags = *guard.vm_flags();
        let file = guard.vm_file()?;
        drop(guard);
        let node = {
            let private = file.private_data.lock();
            let FilePrivateData::Fuse(FuseFilePrivateData::File(private)) = &*private else {
                return Some(VmFaultReason::VM_FAULT_SIGBUS);
            };
            private.node.clone()
        };
        if !node.dax_active() {
            return None;
        }

        let page_index = match pfm.backing_pgoff() {
            Some(index) => index,
            None => return Some(VmFaultReason::VM_FAULT_SIGBUS),
        };
        let file_offset = match page_index.checked_mul(crate::arch::MMArch::PAGE_SIZE) {
            Some(offset) => offset,
            None => return Some(VmFaultReason::VM_FAULT_SIGBUS),
        };
        let _layout = node.dax_layout_read();
        let file_size = match node.dax_file_size() {
            Ok(size) => size,
            Err(_) => return Some(VmFaultReason::VM_FAULT_SIGBUS),
        };
        if file_offset >= file_size {
            return Some(VmFaultReason::VM_FAULT_SIGBUS);
        }

        let write = pfm.flags().contains(FaultFlags::FAULT_FLAG_WRITE);
        let shared = vm_flags.contains(VmFlags::VM_SHARED);
        let access = match node.dax_access(file_offset as u64, write && shared) {
            Ok(access) => access,
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                pfm.set_retry_wait(if node.dax_host_invalidation_blocked() {
                    node.dax_host_invalidation_retry_wait()
                } else {
                    node.conn().dax_fault_retry_wait()
                });
                return Some(VmFaultReason::VM_FAULT_RETRY);
            }
            Err(_) => return Some(VmFaultReason::VM_FAULT_SIGBUS),
        };
        let _host_access = match node.dax_try_host_access() {
            Ok(guard) => guard,
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                drop(access);
                drop(_layout);
                pfm.set_retry_wait(node.dax_host_invalidation_retry_wait());
                return Some(VmFaultReason::VM_FAULT_RETRY);
            }
            Err(_) => return Some(VmFaultReason::VM_FAULT_SIGBUS),
        };
        let in_mapping = file_offset & (super::dax::DAX_RANGE_SIZE - 1);
        let paddr = match access.checked_paddr(in_mapping, crate::arch::MMArch::PAGE_SIZE) {
            Ok(paddr) => paddr,
            Err(_) => return Some(VmFaultReason::VM_FAULT_SIGBUS),
        };
        let old_pfn = pfm.external_pfn();
        let result = if write && !shared {
            let mut source = vec![0; crate::arch::MMArch::PAGE_SIZE];
            if access.copy_to(in_mapping, &mut source).is_err() {
                return Some(VmFaultReason::VM_FAULT_SIGBUS);
            }
            pfm.cow_external_page(old_pfn, &source)
        } else if let Some(old_pfn) = old_pfn {
            if old_pfn != paddr {
                VmFaultReason::VM_FAULT_SIGBUS
            } else if write {
                pfm.upgrade_external_pfn(paddr)
            } else {
                VmFaultReason::VM_FAULT_COMPLETED
            }
        } else {
            pfm.map_external_pfn(paddr, write && shared)
        };
        if result.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            if write && shared {
                node.note_mmap_write();
            }
            if node.dax_note_pte_published().is_err() {
                return Some(VmFaultReason::VM_FAULT_SIGBUS);
            }
        }
        Some(result)
    }

    fn parse_opt_u32_decimal(v: &str) -> Result<u32, SystemError> {
        v.parse::<u32>().map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_u32_octal(v: &str) -> Result<u32, SystemError> {
        u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_bool_switch(v: &str) -> bool {
        v.is_empty() || v != "0"
    }

    fn parse_dax_mode(v: Option<&str>) -> Result<DaxMountMode, SystemError> {
        let Some(v) = v else {
            return Ok(DaxMountMode::Always);
        };

        match v {
            "always" => Ok(DaxMountMode::Always),
            "never" => Ok(DaxMountMode::Never),
            "inode" => Ok(DaxMountMode::Inode),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn parse_mount_options(
        raw: Option<&str>,
    ) -> Result<(u32, u32, u32, bool, bool, DaxMountMode), SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
        let mut default_permissions = true;
        let mut allow_other = true;
        let mut dax_mode = DaxMountMode::InodeDefault;

        for part in raw.unwrap_or("").split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, value) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (part, None),
            };
            let v = value.unwrap_or("");

            match k {
                "rootmode" => rootmode = Some(Self::parse_opt_u32_octal(v)?),
                "user_id" => user_id = Some(Self::parse_opt_u32_decimal(v)?),
                "group_id" => group_id = Some(Self::parse_opt_u32_decimal(v)?),
                "default_permissions" => default_permissions = Self::parse_opt_bool_switch(v),
                "allow_other" => allow_other = Self::parse_opt_bool_switch(v),
                "dax" => dax_mode = Self::parse_dax_mode(value)?,
                _ => return Err(SystemError::EINVAL),
            }
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

    fn proc_show_mount_options(
        &self,
        _mount: &MountFS,
        out: &mut dyn Write,
    ) -> Result<(), SystemError> {
        if let Some(option) = self.conn.dax_mode().proc_option() {
            out.write_str(option).map_err(|_| SystemError::EINVAL)?;
        }
        Ok(())
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.dax_fault(pfm).unwrap_or_else(|| self.inner.fault(pfm))
    }

    fn fault_before_map_pages(&self) -> bool {
        self.inner.fault_before_map_pages()
    }

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.dax_fault(pfm)
            .unwrap_or_else(|| self.inner.page_mkwrite(pfm))
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
        let cache_window = instance.cache_window();
        if dax_mode == DaxMountMode::Always
            && cache_window
                .as_ref()
                .is_none_or(|window| window.len() < super::dax::DAX_RANGE_SIZE)
        {
            return Err(SystemError::EINVAL);
        }
        let conn = FuseConn::new_for_virtiofs_with_dax_window(
            VIRTIOFS_MAX_REQUEST_SIZE,
            VIRTIOFS_RSP_BUF_SIZE,
            cache_window,
            dax_mode,
        )?;
        if dax_mode == DaxMountMode::Always && !conn.dax_enabled() {
            return Err(SystemError::EINVAL);
        }

        Ok(Some(Arc::new(VirtioFsMountData {
            rootmode,
            user_id,
            group_id,
            allow_other,
            default_permissions,
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
            conn: md.conn.clone(),
            instance: md.instance.clone(),
            session_id,
        }))
    }
}

register_mountable_fs!(VirtioFsFs, VIRTIOFSMAKER, "virtiofs");

#[cfg(test)]
mod tests {
    use system_error::SystemError;

    use super::{DaxMountMode, VirtioFsFs};

    #[test]
    fn dax_option_parser_matches_linux_modes() {
        assert_eq!(VirtioFsFs::parse_dax_mode(None), Ok(DaxMountMode::Always));
        assert_eq!(
            VirtioFsFs::parse_dax_mode(Some("always")),
            Ok(DaxMountMode::Always)
        );
        assert_eq!(
            VirtioFsFs::parse_dax_mode(Some("never")),
            Ok(DaxMountMode::Never)
        );
        assert_eq!(
            VirtioFsFs::parse_dax_mode(Some("inode")),
            Ok(DaxMountMode::Inode)
        );
        assert_eq!(
            VirtioFsFs::parse_dax_mode(Some("")),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            VirtioFsFs::parse_dax_mode(Some("on")),
            Err(SystemError::EINVAL)
        );
    }

    #[test]
    fn proc_option_distinguishes_default_and_explicit_inode_modes() {
        assert_eq!(DaxMountMode::InodeDefault.proc_option(), None);
        assert_eq!(DaxMountMode::Always.proc_option(), Some("dax=always"));
        assert_eq!(DaxMountMode::Never.proc_option(), Some("dax=never"));
        assert_eq!(DaxMountMode::Inode.proc_option(), Some("dax=inode"));
    }
}
