use core::{ffi::CStr, mem::size_of};

use hashbrown::Equivalent;
use system_error::SystemError;
use uefi_raw::table::{
    boot::{MemoryAttribute, MemoryType},
    configuration::ConfigurationTable,
};

use crate::{
    driver::firmware::efi::{
        efi_manager,
        guid::{DragonStubPayloadEFI, DRAGONSTUB_EFI_PAYLOAD_EFI_GUID},
    },
    mm::{early_ioremap::EarlyIoRemap, PhysAddr},
};

use super::EFIManager;

impl EFIManager {
    /// 显示EFI系统表头的信息
    ///
    /// ## 参数
    ///
    /// - header: system table表头
    /// - firmware_vendor: firmware vendor字符串的物理地址
    #[inline(never)]
    pub fn report_systable_header(
        &self,
        header: &uefi_raw::table::Header,
        firmware_vendor: PhysAddr,
    ) {
        const TMPBUF_SIZE: usize = 100;

        let mut tmp_buf = [0u8; TMPBUF_SIZE];

        let fw_ptr =
            EarlyIoRemap::map_not_aligned(firmware_vendor, TMPBUF_SIZE * size_of::<u16>(), true);
        if let Ok(fw_ptr) = fw_ptr {
            let slice =
                unsafe { core::slice::from_raw_parts(fw_ptr.data() as *const u16, TMPBUF_SIZE) };
            for i in 0..(TMPBUF_SIZE - 1) {
                let val = slice[i];

                if (val & 0xff) == 0 {
                    break;
                }
                tmp_buf[i] = (val & 0xff) as u8;
            }

            EarlyIoRemap::unmap(fw_ptr).map_err(|e|{
                kerror!("report systable header: failed to unmap systable header, fw_ptr: {fw_ptr:?}, err: {e:?}");
                e
            }).ok();
        } else {
            kwarn!("report systable header: failed to map systable header, err: {fw_ptr:?}");
        }

        let s = CStr::from_bytes_with_nul(&tmp_buf)
            .unwrap_or_else(|_| CStr::from_bytes_with_nul(b"Unknown\0").unwrap());
        kinfo!("EFI version: {:?}, vendor: {:?}", header.revision, s);
    }

    /// 解析EFI config table
    pub fn parse_config_tables(&self, tables: &[ConfigurationTable]) -> Result<(), SystemError> {
        for table in tables {
            if table
                .vendor_guid
                .equivalent(&DRAGONSTUB_EFI_PAYLOAD_EFI_GUID)
            {
                let table_paddr: PhysAddr = PhysAddr::new(table.vendor_table as usize);
                let vaddr = EarlyIoRemap::map_not_aligned(
                    table_paddr,
                    size_of::<DragonStubPayloadEFI>(),
                    true,
                )?;

                let data = unsafe { *(vaddr.data() as *const DragonStubPayloadEFI) };

                efi_manager().inner.write().dragonstub_load_info = Some(data);

                EarlyIoRemap::unmap(vaddr).unwrap();
            }
        }

        return Ok(());
    }
}

/// A structure describing a region of memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct MemoryDescriptor {
    /// Type of memory occupying this range.
    pub ty: MemoryType,
    /// Starting physical address.
    pub phys_start: uefi_raw::PhysicalAddress,
    /// Starting virtual address.
    pub virt_start: uefi_raw::VirtualAddress,
    /// Number of 4 KiB pages contained in this range.
    pub page_count: u64,
    /// The capability attributes of this memory range.
    pub att: MemoryAttribute,
}

#[allow(dead_code)]
impl MemoryDescriptor {
    /// Memory descriptor version number.
    pub const VERSION: u32 = 1;

    /// 当前内存描述符是否表示真实的内存
    #[inline]
    pub fn is_memory(&self) -> bool {
        if self.att.contains(
            MemoryAttribute::WRITE_BACK
                | MemoryAttribute::WRITE_THROUGH
                | MemoryAttribute::WRITE_COMBINE,
        ) {
            return true;
        }

        return false;
    }

    /// 判断当前内存描述符所表示的区域是否能被作为系统内存使用
    ///
    /// ## 返回
    ///
    /// - `true` - 可以
    /// - `false` - 不可以
    pub fn is_usable_memory(&self) -> bool {
        match self.ty {
            MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::ACPI_RECLAIM
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::CONVENTIONAL
            | MemoryType::PERSISTENT_MEMORY => {
                // SPECIAL_PURPOSE的内存是“软保留”的，这意味着它最初被留出，
                // 但在启动后可以通过热插拔再次使用，或者分配给dax驱动程序。
                if self.att.contains(MemoryAttribute::SPECIAL_PURPOSE) {
                    return false;
                }

                // 根据规范，在调用ExitBootServices()之后，这些区域就不再被保留了。
                // 然而，只有当它们可以被映射为WRITE_BACK缓存时，我们才能将它们用作系统内存
                return self.att.contains(MemoryAttribute::WRITE_BACK);
            }
            _ => {
                return false;
            }
        }
    }
}

impl Default for MemoryDescriptor {
    fn default() -> MemoryDescriptor {
        MemoryDescriptor {
            ty: MemoryType::RESERVED,
            phys_start: 0,
            virt_start: 0,
            page_count: 0,
            att: MemoryAttribute::empty(),
        }
    }
}
