use core::{ffi::CStr, mem::size_of};

use hashbrown::Equivalent;
use log::{debug, error, info, warn};
use system_error::SystemError;
use uefi_raw::table::{
    boot::{MemoryAttribute, MemoryType},
    configuration::ConfigurationTable,
};

use crate::{
    arch::MMArch,
    driver::firmware::efi::{
        efi_manager,
        guid::{DragonStubPayloadEFI, DRAGONSTUB_EFI_PAYLOAD_EFI_GUID},
    },
    mm::{
        early_ioremap::EarlyIoRemap, memblock::mem_block_manager, MemoryManagementArch, PhysAddr,
        VirtAddr,
    },
};

use super::{
    guid::{
        EFI_MEMORY_ATTRIBUTES_TABLE_GUID, EFI_MEMRESERVE_TABLE_GUID, EFI_SYSTEM_RESOURCE_TABLE_GUID,
    },
    EFIManager,
};

/// 所有的要解析的表格的解析器
static TABLE_PARSERS: &[&TableMatcher] = &[
    &TableMatcher::new(&MatchTableDragonStubPayloadEFI),
    &TableMatcher::new(&MatchTableMemoryAttributes),
    &TableMatcher::new(&MatchTableMemReserve),
    &TableMatcher::new(&MatchTableEsrt),
];

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
                error!("report systable header: failed to unmap systable header, fw_ptr: {fw_ptr:?}, err: {e:?}");
                e
            }).ok();
        } else {
            warn!("report systable header: failed to map systable header, err: {fw_ptr:?}");
        }

        let s = CStr::from_bytes_with_nul(&tmp_buf).unwrap_or(c"Unknown");
        info!("EFI version: {:?}, vendor: {:?}", header.revision, s);
    }

    /// 解析EFI config table
    pub fn parse_config_tables(&self, tables: &[ConfigurationTable]) -> Result<(), SystemError> {
        for table in tables {
            let mut flag = false;
            'parser_loop: for parser in TABLE_PARSERS {
                if let Some(r) = parser.match_table(table) {
                    // 有匹配结果
                    if let Err(e) = r {
                        warn!(
                            "Failed to parse cfg table: '{}', err: {e:?}",
                            parser.table.name()
                        );
                    }
                    flag = true;
                    break 'parser_loop;
                }
            }

            if !flag {
                warn!("Cannot find parser for guid: {:?}", table.vendor_guid);
            }
        }

        // 如果存在mem reserve table
        if let Some(mem_reserve) = efi_manager().inner_read().memreserve_table_paddr {
            let mut prev_paddr = mem_reserve;
            while !prev_paddr.is_null() {
                let vaddr = EarlyIoRemap::map_not_aligned(prev_paddr, MMArch::PAGE_SIZE, true)
                    .map_err(|e| {
                        error!(
                            "Failed to map UEFI memreserve table, paddr: {prev_paddr:?}, err: {e:?}"
                        );

                        SystemError::ENOMEM
                    })?;

                let p = unsafe {
                    (vaddr.data() as *const LinuxEFIMemReserveTable)
                        .as_ref()
                        .unwrap()
                };

                // reserve the entry itself
                let psize: usize = p.size.try_into().unwrap();
                mem_block_manager()
                    .reserve_block(
                        prev_paddr,
                        size_of::<LinuxEFIMemReserveTable>()
                            + size_of::<LinuxEFIMemReserveEntry>() * psize,
                    )
                    .map_err(|e| {
                        error!("Failed to reserve block, paddr: {prev_paddr:?}, err: {e:?}");
                        EarlyIoRemap::unmap(vaddr).unwrap();
                        e
                    })?;

                let entries = unsafe {
                    core::slice::from_raw_parts(
                        (vaddr.data() as *const LinuxEFIMemReserveTable).add(1)
                            as *const LinuxEFIMemReserveEntry,
                        p.count as usize,
                    )
                };
                // reserve the entries
                for entry in entries {
                    mem_block_manager()
                        .reserve_block(PhysAddr::new(entry.base), entry.size)
                        .map_err(|e| {
                            error!("Failed to reserve block, paddr: {prev_paddr:?}, err: {e:?}");
                            EarlyIoRemap::unmap(vaddr).unwrap();
                            e
                        })?;
                }

                prev_paddr = p.next_paddr;
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
        if self.att.contains(MemoryAttribute::WRITE_BACK)
            || self.att.contains(MemoryAttribute::WRITE_THROUGH)
            || self.att.contains(MemoryAttribute::WRITE_COMBINE)
        {
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

trait MatchTable: Send + Sync {
    /// 配置表名（仅用于日志显示）
    fn name(&self) -> &'static str;

    /// 当前table的guid
    fn guid(&self) -> &'static uefi_raw::Guid;

    /// 匹配阶段时，匹配器要映射vendor_table的大小。
    ///
    /// 如果为0，则不映射
    fn map_size(&self) -> usize;

    /// 当表格被映射后,调用这个函数
    ///
    /// ## 锁
    ///
    /// 进入该函数前，不得持有efi_manager().inner的任何锁
    fn post_process(
        &self,
        vendor_table_vaddr: Option<VirtAddr>,
        table_raw: &ConfigurationTable,
    ) -> Result<(), SystemError>;
}

/// `DRAGONSTUB_EFI_PAYLOAD_EFI_GUID` 的匹配器
struct MatchTableDragonStubPayloadEFI;

impl MatchTable for MatchTableDragonStubPayloadEFI {
    fn name(&self) -> &'static str {
        "DragonStub Payload"
    }

    fn guid(&self) -> &'static uefi_raw::Guid {
        &DRAGONSTUB_EFI_PAYLOAD_EFI_GUID
    }

    fn map_size(&self) -> usize {
        core::mem::size_of::<DragonStubPayloadEFI>()
    }

    fn post_process(
        &self,
        vendor_table_vaddr: Option<VirtAddr>,
        _table_raw: &ConfigurationTable,
    ) -> Result<(), SystemError> {
        let vendor_table_vaddr = vendor_table_vaddr.unwrap();
        let data = unsafe { *(vendor_table_vaddr.data() as *const DragonStubPayloadEFI) };

        efi_manager().inner_write().dragonstub_load_info = Some(data);

        return Ok(());
    }
}

struct MatchTableMemoryAttributes;

impl MatchTable for MatchTableMemoryAttributes {
    fn name(&self) -> &'static str {
        "MemAttr"
    }

    fn guid(&self) -> &'static uefi_raw::Guid {
        &EFI_MEMORY_ATTRIBUTES_TABLE_GUID
    }

    fn map_size(&self) -> usize {
        // 不映射
        0
    }

    fn post_process(
        &self,
        _vendor_table_vaddr: Option<VirtAddr>,
        table_raw: &ConfigurationTable,
    ) -> Result<(), SystemError> {
        efi_manager()
            .inner
            .write_irqsave()
            .memory_attribute_table_paddr = Some(PhysAddr::new(table_raw.vendor_table as usize));
        return Ok(());
    }
}

struct MatchTableMemReserve;

impl MatchTable for MatchTableMemReserve {
    fn name(&self) -> &'static str {
        "MemReserve"
    }

    fn guid(&self) -> &'static uefi_raw::Guid {
        &EFI_MEMRESERVE_TABLE_GUID
    }

    fn map_size(&self) -> usize {
        // 不映射
        0
    }

    fn post_process(
        &self,
        _vendor_table_vaddr: Option<VirtAddr>,
        table_raw: &ConfigurationTable,
    ) -> Result<(), SystemError> {
        efi_manager().inner.write_irqsave().memreserve_table_paddr =
            Some(PhysAddr::new(table_raw.vendor_table as usize));
        debug!(
            "memreserve_table_paddr: {:#x}",
            table_raw.vendor_table as usize
        );
        return Ok(());
    }
}

struct MatchTableEsrt;

impl MatchTable for MatchTableEsrt {
    fn name(&self) -> &'static str {
        "ESRT"
    }

    fn guid(&self) -> &'static uefi_raw::Guid {
        &EFI_SYSTEM_RESOURCE_TABLE_GUID
    }

    fn map_size(&self) -> usize {
        0
    }

    fn post_process(
        &self,
        _vendor_table_vaddr: Option<VirtAddr>,
        table_raw: &ConfigurationTable,
    ) -> Result<(), SystemError> {
        efi_manager().inner.write_irqsave().esrt_table_paddr =
            Some(PhysAddr::new(table_raw.vendor_table as usize));
        debug!("esrt_table_paddr: {:#x}", table_raw.vendor_table as usize);
        return Ok(());
    }
}

/// 用于匹配配置表的匹配器
struct TableMatcher {
    table: &'static dyn MatchTable,
}

impl TableMatcher {
    const fn new(table: &'static dyn MatchTable) -> Self {
        Self { table }
    }

    /// 判断配置表与当前匹配器是否匹配
    #[inline(never)]
    fn match_table(&self, table: &ConfigurationTable) -> Option<Result<(), SystemError>> {
        if !table.vendor_guid.equivalent(self.table.guid()) {
            return None;
        }

        let table_map_size = self.table.map_size();

        let vendor_table_vaddr: Option<VirtAddr> = if table_map_size > 0 {
            let table_paddr: PhysAddr = PhysAddr::new(table.vendor_table as usize);
            let vaddr = EarlyIoRemap::map_not_aligned(table_paddr, table_map_size, true);

            if let Err(e) = vaddr {
                return Some(Err(e));
            }

            Some(vaddr.unwrap())
        } else {
            None
        };

        let r = self.table.post_process(vendor_table_vaddr, table);

        if let Some(vaddr) = vendor_table_vaddr {
            EarlyIoRemap::unmap(vaddr).unwrap();
        }
        return Some(r);
    }
}

#[repr(C)]
#[derive(Debug)]
struct LinuxEFIMemReserveTable {
    /// allocated size of the array
    size: i32,
    /// number of entries used
    count: i32,
    /// pa of next struct instance
    next_paddr: PhysAddr,
    entry: [LinuxEFIMemReserveEntry; 0],
}

#[repr(C)]
#[derive(Debug)]
struct LinuxEFIMemReserveEntry {
    base: usize,
    size: usize,
}
