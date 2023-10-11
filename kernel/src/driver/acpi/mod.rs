use core::{fmt::Debug, ptr::NonNull};

use acpi::AcpiHandler;

use crate::{
    kinfo,
    libs::{
        align::{page_align_down, page_align_up},
        once::Once,
    },
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        PhysAddr, VirtAddr,
    },
};

mod c_adapter;
pub mod glue;
pub mod old;

extern crate acpi;

static mut __ACPI_TABLE: Option<acpi::AcpiTables<AcpiHandlerImpl>> = None;

#[derive(Debug)]
pub struct AcpiManager;

impl AcpiManager {
    pub fn init(rsdp_paddr: PhysAddr) {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            kinfo!("Initializing Acpi Manager...");
            let acpi_table: acpi::AcpiTables<AcpiHandlerImpl> =
                unsafe { acpi::AcpiTables::from_rsdp(AcpiHandlerImpl, rsdp_paddr.data()) }
                    .unwrap_or_else(|e| {
                        panic!("acpi_init(): failed to parse acpi tables, error: {:?}", e)
                    });

            unsafe {
                __ACPI_TABLE = Some(acpi_table);
            }
            kinfo!("Acpi Manager initialized.");
        });
    }

    #[allow(dead_code)]
    pub fn tables() -> Option<&'static acpi::AcpiTables<AcpiHandlerImpl>> {
        unsafe { __ACPI_TABLE.as_ref() }
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
