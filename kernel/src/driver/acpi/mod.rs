use core::{fmt::Debug, ptr::NonNull};

use acpi::{AcpiHandler, PlatformInfo};
use alloc::{string::ToString, sync::Arc};

use crate::{
    driver::base::firmware::sys_firmware_kset,
    kinfo,
    libs::align::{page_align_down, page_align_up},
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        PhysAddr, VirtAddr,
    },
    syscall::SystemError,
};

use super::base::kset::KSet;

extern crate acpi;

pub mod bus;
mod c_adapter;
pub mod glue;
pub mod pmtmr;
mod sysfs;

static mut __ACPI_TABLE: Option<acpi::AcpiTables<AcpiHandlerImpl>> = None;
/// `/sys/firmware/acpi`的kset
static mut ACPI_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
pub fn acpi_manager() -> &'static AcpiManager {
    &AcpiManager
}

#[inline(always)]
pub fn acpi_kset() -> Arc<KSet> {
    unsafe { ACPI_KSET_INSTANCE.clone().unwrap() }
}

#[derive(Debug)]
pub struct AcpiManager;

impl AcpiManager {
    /// 初始化ACPI
    ///
    /// ## 参数
    ///
    /// - `rsdp_paddr`: RSDP的物理地址
    ///
    ///
    /// ## 参考资料
    ///
    /// https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1390
    pub fn init(&self, rsdp_paddr: PhysAddr) -> Result<(), SystemError> {
        kinfo!("Initializing Acpi Manager...");

        // 初始化`/sys/firmware/acpi`的kset
        let kset = KSet::new("acpi".to_string());
        kset.register(Some(sys_firmware_kset()))?;
        unsafe {
            ACPI_KSET_INSTANCE = Some(kset.clone());
        }
        self.map_tables(rsdp_paddr)?;
        self.bus_init()?;
        kinfo!("Acpi Manager initialized.");
        return Ok(());
    }

    fn map_tables(&self, rsdp_paddr: PhysAddr) -> Result<(), SystemError> {
        let acpi_table: acpi::AcpiTables<AcpiHandlerImpl> =
            unsafe { acpi::AcpiTables::from_rsdp(AcpiHandlerImpl, rsdp_paddr.data()) }.map_err(
                |e| {
                    kerror!("acpi_init(): failed to parse acpi tables, error: {:?}", e);
                    SystemError::ENOMEM
                },
            )?;

        unsafe {
            __ACPI_TABLE = Some(acpi_table);
        }

        return Ok(());
    }

    #[allow(dead_code)]
    pub fn tables(&self) -> Option<&'static acpi::AcpiTables<AcpiHandlerImpl>> {
        unsafe { __ACPI_TABLE.as_ref() }
    }

    /// 从acpi获取平台的信息
    ///
    /// 包括：
    ///
    /// - PowerProfile
    /// - InterruptModel
    /// - ProcessorInfo
    /// - PmTimer
    pub fn platform_info(&self) -> Option<PlatformInfo<'_, alloc::alloc::Global>> {
        let r = self.tables()?.platform_info();
        if let Err(ref e) = r {
            kerror!(
                "AcpiManager::platform_info(): failed to get platform info, error: {:?}",
                e
            );
            return None;
        }

        return Some(r.unwrap());
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AcpiHandlerImpl;

impl AcpiHandler for AcpiHandlerImpl {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> acpi::PhysicalMapping<Self, T> {
        let offset = physical_address - page_align_down(physical_address);
        let size_fix = page_align_up(size + offset);

        let mmio_guard = mmio_pool()
            .create_mmio(size_fix)
            .expect("AcpiHandlerImpl::map_physical_region(): failed to create mmio");

        mmio_guard
            .map_phys(PhysAddr::new(page_align_down(physical_address)), size_fix)
            .expect("AcpiHandlerImpl::map_physical_region(): failed to map phys");
        let virtual_start = mmio_guard.vaddr().data() + offset;

        let virtual_start = NonNull::new(virtual_start as *mut T).unwrap();

        let result: acpi::PhysicalMapping<AcpiHandlerImpl, T> = acpi::PhysicalMapping::new(
            physical_address,
            virtual_start,
            size,
            mmio_guard.size(),
            AcpiHandlerImpl,
        );

        MMIOSpaceGuard::leak(mmio_guard);

        return result;
    }

    fn unmap_physical_region<T>(region: &acpi::PhysicalMapping<Self, T>) {
        let mmio_guard = unsafe {
            MMIOSpaceGuard::from_raw(
                VirtAddr::new(page_align_down(
                    region.virtual_start().as_ref() as *const T as usize
                )),
                region.mapped_length(),
                true,
            )
        };
        drop(mmio_guard);
    }
}
