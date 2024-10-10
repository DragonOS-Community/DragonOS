use core::{fmt::Debug, ptr::NonNull};

use acpi::{AcpiHandler, AcpiTables, PlatformInfo};
use alloc::{string::ToString, sync::Arc};
use log::{error, info};

use crate::{
    arch::MMArch,
    driver::base::firmware::sys_firmware_kset,
    init::{boot::BootloaderAcpiArg, boot_params},
    libs::align::{page_align_down, page_align_up, AlignedBox},
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
};
use system_error::SystemError;

use super::base::kset::KSet;

extern crate acpi;

pub mod bus;
pub mod glue;
pub mod pmtmr;
mod sysfs;

static mut __ACPI_TABLE: Option<acpi::AcpiTables<AcpiHandlerImpl>> = None;
/// `/sys/firmware/acpi`的kset
static mut ACPI_KSET_INSTANCE: Option<Arc<KSet>> = None;

static mut RSDP_TMP_BOX: Option<AlignedBox<[u8; 4096], 4096>> = None;

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
    /// - `rsdp_vaddr1`: RSDP(v1)的虚拟地址
    /// - `rsdp_vaddr2`: RSDP(v2)的虚拟地址
    ///
    ///
    /// ## 参考资料
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1390
    fn init(&self) -> Result<(), SystemError> {
        info!("Initializing Acpi Manager...");

        // 初始化`/sys/firmware/acpi`的kset
        let kset = KSet::new("acpi".to_string());
        kset.register(Some(sys_firmware_kset()))?;
        unsafe {
            ACPI_KSET_INSTANCE = Some(kset.clone());
        }
        let acpi_args = boot_params().read().acpi;
        if let BootloaderAcpiArg::NotProvided = acpi_args {
            error!("acpi_init(): ACPI not provided by bootloader");
            return Err(SystemError::ENODEV);
        }

        self.map_tables(acpi_args)?;
        self.bus_init()?;
        info!("Acpi Manager initialized.");
        return Ok(());
    }

    fn map_tables(&self, acpi_args: BootloaderAcpiArg) -> Result<(), SystemError> {
        let table_paddr: PhysAddr = match acpi_args {
            BootloaderAcpiArg::Rsdt(rsdpv1) => Self::rsdp_paddr(&rsdpv1),
            BootloaderAcpiArg::Xsdt(rsdpv2) => Self::rsdp_paddr(&rsdpv2),
            BootloaderAcpiArg::Rsdp(rsdp) => rsdp,
            _ => {
                error!(
                    "AcpiManager::map_tables(): unsupported acpi_args: {:?}",
                    acpi_args
                );
                return Err(SystemError::ENODEV);
            }
        };
        let res = unsafe { acpi::AcpiTables::from_rsdp(AcpiHandlerImpl, table_paddr.data()) };
        match res {
            Ok(acpi_table) => {
                Self::set_acpi_table(acpi_table);
                return Ok(());
            }
            Err(e) => {
                error!(
                    "AcpiManager::map_tables(): failed to map tables, error: {:?}",
                    e
                );
                Self::drop_rsdp_tmp_box();
                return Err(SystemError::ENODEV);
            }
        }
    }

    /// 通过RSDP虚拟地址获取RSDP物理地址
    ///
    /// ## 参数
    ///
    /// - `rsdp_vaddr`: RSDP的虚拟地址
    ///
    /// ## 返回值
    ///
    /// RSDP物理地址
    fn rsdp_paddr(rsdp_instance: &acpi::rsdp::Rsdp) -> PhysAddr {
        unsafe {
            RSDP_TMP_BOX = Some(AlignedBox::new_zeroed().expect("rs_acpi_init(): failed to alloc"))
        };

        let size = core::mem::size_of::<acpi::rsdp::Rsdp>();
        let tmp_data = unsafe {
            core::slice::from_raw_parts(
                rsdp_instance as *const acpi::rsdp::Rsdp as usize as *const u8,
                size,
            )
        };
        unsafe { RSDP_TMP_BOX.as_mut().unwrap()[0..size].copy_from_slice(tmp_data) };
        let rsdp_paddr = unsafe {
            MMArch::virt_2_phys(VirtAddr::new(
                RSDP_TMP_BOX.as_ref().unwrap().as_ptr() as usize
            ))
            .unwrap()
        };

        return rsdp_paddr;
    }

    fn set_acpi_table(acpi_table: AcpiTables<AcpiHandlerImpl>) {
        unsafe {
            __ACPI_TABLE = Some(acpi_table);
        }
    }

    fn drop_rsdp_tmp_box() {
        unsafe {
            RSDP_TMP_BOX = None;
        }
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
            error!(
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

#[inline(never)]
pub fn acpi_init() -> Result<(), SystemError> {
    #[cfg(target_arch = "x86_64")]
    {
        acpi_manager().init()
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        log::warn!("acpi_init(): unsupported arch");
        return Ok(());
    }
}
