use fdt::{
    node::{FdtNode, NodeProperty},
    Fdt,
};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    init::boot_params,
    libs::{align::page_align_down, rwlock::RwLock},
    mm::{
        memblock::{mem_block_manager, MemBlockManager},
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
};

#[inline(always)]
pub fn open_firmware_fdt_driver() -> &'static OpenFirmwareFdtDriver {
    &OpenFirmwareFdtDriver
}

static FDT_GLOBAL_DATA: RwLock<FdtGlobalData> = RwLock::new(FdtGlobalData::new());

#[derive(Debug)]
struct FdtGlobalData {
    /// FDT根节点下的`size-cells`属性值
    root_size_cells: u32,

    /// FDT根节点下的`address-cells`属性值
    root_addr_cells: u32,

    chosen_node_name: Option<&'static str>,
}

impl FdtGlobalData {
    pub const fn new() -> Self {
        Self {
            root_size_cells: 1,
            root_addr_cells: 1,
            chosen_node_name: None,
        }
    }
}

static mut FDT_VADDR: Option<VirtAddr> = None;

pub struct OpenFirmwareFdtDriver;

impl OpenFirmwareFdtDriver {
    pub fn early_scan_device_tree(&self) -> Result<(), SystemError> {
        let fdt_vaddr = unsafe { FDT_VADDR.ok_or(SystemError::EINVAL)? };
        let fdt = unsafe {
            fdt::Fdt::from_ptr(fdt_vaddr.as_ptr()).map_err(|e| {
                kerror!("failed to parse fdt, err={:?}", e);
                SystemError::EINVAL
            })
        }?;

        self.early_init_scan_nodes(&fdt);

        return Ok(());
    }

    fn early_init_scan_nodes(&self, fdt: &Fdt) {
        self.early_init_scan_root(fdt)
            .expect("Failed to scan fdt root node.");

        self.early_init_scan_chosen(fdt).unwrap_or_else(|_| {
            kwarn!("No `chosen` node found");
        });

        self.early_init_scan_memory(fdt);
    }

    /// 扫描根节点
    fn early_init_scan_root(&self, fdt: &Fdt) -> Result<(), SystemError> {
        let node = fdt.find_node("/").ok_or(SystemError::ENODEV)?;

        let mut guard = FDT_GLOBAL_DATA.write();

        if let Some(prop) = node.property("#size-cells") {
            guard.root_size_cells = prop.as_usize().unwrap() as u32;

            kdebug!("fdt_root_size_cells={}", guard.root_size_cells);
        }

        if let Some(prop) = node.property("#address-cells") {
            guard.root_addr_cells = prop.as_usize().unwrap() as u32;

            kdebug!("fdt_root_addr_cells={}", guard.root_addr_cells);
        }

        return Ok(());
    }

    /// 扫描 `/chosen` 节点
    fn early_init_scan_chosen(&self, fdt: &Fdt) -> Result<(), SystemError> {
        const CHOSEN_NAME1: &'static str = "/chosen";
        let mut node = fdt.find_node(CHOSEN_NAME1);
        if node.is_none() {
            const CHOSEN_NAME2: &'static str = "/chosen@0";
            node = fdt.find_node(CHOSEN_NAME2);
            if node.is_some() {
                FDT_GLOBAL_DATA.write().chosen_node_name = Some(CHOSEN_NAME2);
            }
        } else {
            FDT_GLOBAL_DATA.write().chosen_node_name = Some(CHOSEN_NAME1);
        }

        if let Some(node) = node {
            if let Some(prop) = node.property("bootargs") {
                let bootargs = prop.as_str().unwrap();

                boot_params()
                    .write()
                    .boot_cmdline_append(bootargs.as_bytes());
            }
        }

        // TODO: 拼接内核自定义的command line参数

        kdebug!("Command line: {}", boot_params().read().boot_cmdline_str());
        return Ok(());
    }

    /// 扫描 `/memory` 节点
    ///
    /// ## 参数
    ///
    /// - `fdt`：FDT
    ///
    /// ## 返回值
    ///
    /// 如果扫描成功，找到可用内存，则返回`true`，否则返回`false`。
    fn early_init_scan_memory(&self, fdt: &Fdt) -> bool {
        let mut found_memory = false;
        for node in fdt.all_nodes() {
            let device_type: Option<NodeProperty<'_>> = node.property("device_type");
            if device_type.is_none() {
                continue;
            }
            let device_type = device_type.unwrap().as_str();
            if device_type.is_none() || device_type.unwrap() != "memory" {
                continue;
            }

            if !self.is_device_avaliable(&node) {
                continue;
            }

            let reg = node.property("reg");
            if reg.is_none() {
                continue;
            }
            let reg = reg.unwrap();
            // 每个cell是4字节
            let addr_cells = FDT_GLOBAL_DATA.read().root_addr_cells as usize;
            let size_cells = FDT_GLOBAL_DATA.read().root_size_cells as usize;

            let total_elements_in_reg = reg.value.len() / ((addr_cells + size_cells) * 4);

            for i in 0..total_elements_in_reg {
                let mut base_index = i * (addr_cells + size_cells);
                let base: u64;
                let size: u64;
                match addr_cells {
                    1 => {
                        base = u32::from_be_bytes(
                            reg.value[base_index..base_index + 4].try_into().unwrap(),
                        ) as u64;
                    }
                    2 => {
                        base = u64::from_be_bytes(
                            reg.value[base_index..base_index + 8].try_into().unwrap(),
                        );
                    }
                    _ => {
                        panic!("addr_cells must be 1 or 2");
                    }
                }
                base_index += addr_cells * 4;

                match size_cells {
                    1 => {
                        size = u32::from_be_bytes(
                            reg.value[base_index..base_index + 4].try_into().unwrap(),
                        ) as u64;
                    }
                    2 => {
                        size = u64::from_be_bytes(
                            reg.value[base_index..base_index + 8].try_into().unwrap(),
                        );
                    }
                    _ => {
                        panic!("size_cells must be 1 or 2");
                    }
                }

                if size == 0 {
                    continue;
                }

                kdebug!("Found memory: base={:#x}, size={:#x}", base, size);
                self.early_init_dt_add_memory(base, size);
                found_memory = true;
            }
        }

        return found_memory;
    }

    fn early_init_dt_add_memory(&self, base: u64, size: u64) {
        let mut base = base as usize;
        let mut size = size as usize;

        if size < (MMArch::PAGE_SIZE - (base & (!MMArch::PAGE_MASK))) {
            kwarn!("Ignoring memory block {:#x}-{:#x}", base, base + size);
        }

        if PhysAddr::new(base).check_aligned(MMArch::PAGE_SIZE) == false {
            size -= MMArch::PAGE_SIZE - (base & (!MMArch::PAGE_MASK));
            base = page_align_down(base);
        }

        size = page_align_down(size);

        if base > MemBlockManager::MAX_MEMBLOCK_ADDR.data() {
            kwarn!("Ignoring memory block {:#x}-{:#x}", base, base + size);
        }

        if base + size - 1 > MemBlockManager::MAX_MEMBLOCK_ADDR.data() {
            kwarn!(
                "Ignoring memory range {:#x}-{:#x}",
                MemBlockManager::MAX_MEMBLOCK_ADDR.data() + 1,
                base + size
            );
            size = MemBlockManager::MAX_MEMBLOCK_ADDR.data() - base + 1;
        }

        if base + size < MemBlockManager::MIN_MEMBLOCK_ADDR.data() {
            kwarn!("Ignoring memory range {:#x}-{:#x}", base, base + size);
            return;
        }

        if base < MemBlockManager::MIN_MEMBLOCK_ADDR.data() {
            {
                kwarn!(
                    "Ignoring memory range {:#x}-{:#x}",
                    base,
                    MemBlockManager::MIN_MEMBLOCK_ADDR.data()
                );
                size -= MemBlockManager::MIN_MEMBLOCK_ADDR.data() - base;
                base = MemBlockManager::MIN_MEMBLOCK_ADDR.data();
            }

            mem_block_manager()
                .add_block(PhysAddr::new(base), size)
                .unwrap_or_else(|e| {
                    panic!(
                        "Failed to add memory block '{:#x}-{:#x}', err={:?}",
                        base,
                        base + size,
                        e
                    );
                });
        }
    }

    fn is_device_avaliable(&self, node: &FdtNode) -> bool {
        let status = node.property("status");
        if status.is_none() {
            return true;
        }

        let status = status.unwrap().as_str();
        if let Some(status) = status {
            if status == "okay" || status == "ok" {
                return true;
            }
        }

        return false;
    }

    pub unsafe fn set_fdt_vaddr(&self, vaddr: VirtAddr) -> Result<(), SystemError> {
        if vaddr.is_null() {
            return Err(SystemError::EINVAL);
        }
        fdt::Fdt::from_ptr(vaddr.as_ptr()).map_err(|e| {
            kerror!("failed to parse fdt, err={:?}", e);
            SystemError::EINVAL
        })?;

        unsafe {
            FDT_VADDR = Some(vaddr);
        }

        Ok(())
    }
}
