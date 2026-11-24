use alloc::{string::String, string::ToString};

use system_error::SystemError;

use crate::{
    debug::sysfs::debugfs_kobj,
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData},
        vfs::{InodeMode, PollStatus},
    },
};

#[derive(Debug)]
enum MmReportCallback {
    Snapshot(fn() -> Result<String, SystemError>),
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    PageMigrationCommand,
}

impl KernFSCallback for MmReportCallback {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = match self {
            // A report is bound at registration so this shared frame does
            // not reserve stack space for unrelated report generators.
            Self::Snapshot(build) => build()?,
            #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
            Self::PageMigrationCommand => String::new(),
        };
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(report));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let report: &String = match data.file_private_data() {
            Some(KernFilePrivateData::DebugTextSnapshot(report)) => report,
            _ => return Err(SystemError::EINVAL),
        };
        let bytes = report.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = buf.len().min(bytes.len() - offset);
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        #[allow(unused_mut, unused_variables)] mut data: KernCallbackData,
        #[allow(unused_variables)] buf: &[u8],
        #[allow(unused_variables)] offset: usize,
    ) -> Result<usize, SystemError> {
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        if matches!(self, Self::PageMigrationCommand) {
            if offset != 0 {
                return Err(SystemError::EINVAL);
            }
            let report = crate::mm::page::migration::selftest::command(buf)?;
            data.file_private_data_mut()
                .replace(KernFilePrivateData::DebugTextSnapshot(report));
            return Ok(buf.len());
        }
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        if matches!(self, Self::PageMigrationCommand) {
            return Ok(PollStatus::READ | PollStatus::WRITE);
        }
        Ok(PollStatus::READ)
    }
}

pub fn init_debugfs_mm() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let mm = root.add_dir(
        "mm".to_string(),
        InodeMode::from_bits_truncate(0o500),
        None,
        None,
    )?;
    #[cfg(feature = "mm_selftest")]
    mm.add_file(
        "dma_allocator_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::dma::dma_allocator_selftest_report())
        })),
    )?;
    mm.add_file(
        "allocation_stats".to_string(),
        InodeMode::S_IRUSR,
        Some(16384),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            let snapshot = crate::arch::mm::LockedFrameAllocator::diagnostic_snapshot()
                .ok_or(SystemError::ENODEV)?;
            Ok(crate::mm::allocator::buddy::allocation_stats_report(
                snapshot,
            ))
        })),
    )?;
    #[cfg(feature = "mm_selftest")]
    mm.add_file(
        "allocation_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::allocator::buddy::allocation_diagnostics_selftest_report())
        })),
    )?;
    #[cfg(feature = "mm_selftest")]
    mm.add_file(
        "page_access_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::page::access::selftest_report())
        })),
    )?;
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    mm.add_file(
        "placement_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::allocator::buddy::placement_selftest_report())
        })),
    )?;
    #[cfg(feature = "mm_selftest")]
    mm.add_file(
        "page_retirement_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::page::retirement::selftest::report())
        })),
    )?;
    #[cfg(all(
        feature = "mm_selftest",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    mm.add_file(
        "page_reporting_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::page_reporting::selftest_report())
        })),
    )?;
    #[cfg(feature = "mm_selftest")]
    mm.add_file(
        "virtio_balloon_status".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::driver::virtio::virtio_balloon::selftest_report())
        })),
    )?;
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    mm.add_file(
        "tlb_teardown_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::tlb::teardown_selftest_report())
        })),
    )?;
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    {
        mm.add_file(
            "page_compaction_selftest".to_string(),
            InodeMode::S_IRUSR,
            Some(4096),
            None,
            Some(&MmReportCallback::Snapshot(|| {
                Ok(crate::mm::page::compaction_selftest::report())
            })),
        )?;
        mm.add_file(
            "page_migration_selftest".to_string(),
            InodeMode::S_IRUSR,
            Some(4096),
            None,
            Some(&MmReportCallback::Snapshot(|| {
                Ok(crate::mm::page::migration::selftest::report())
            })),
        )?;
        mm.add_file(
            "page_migration".to_string(),
            InodeMode::S_IRUSR | InodeMode::S_IWUSR,
            Some(4096),
            None,
            Some(&MmReportCallback::PageMigrationCommand),
        )?;
    }
    #[cfg(feature = "mm_selftest")]
    mm.add_file(
        "vmalloc_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::vmalloc::selftest_report())
        })),
    )?;
    #[cfg(all(
        feature = "mm_selftest",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    mm.add_file(
        "kernel_buffer_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::mm::kernel_buffer::selftest_report())
        })),
    )?;
    #[cfg(all(
        feature = "mm_selftest",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    mm.add_file(
        "overlay_xattr_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::filesystem::overlayfs::xattr_selftest_report())
        })),
    )?;
    #[cfg(all(
        feature = "mm_selftest",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    mm.add_file(
        "xattr_syscall_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::filesystem::vfs::syscall::xattr_syscall_selftest_report())
        })),
    )?;
    #[cfg(all(
        feature = "mm_selftest",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    mm.add_file(
        "read_buffer_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MmReportCallback::Snapshot(|| {
            Ok(crate::filesystem::vfs::syscall::read_buffer_selftest_report())
        })),
    )?;
    Ok(())
}
