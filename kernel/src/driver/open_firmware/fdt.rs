use core::mem::size_of;

use fdt::{
    node::{FdtNode, NodeProperty},
    Fdt,
};
use log::{debug, error, warn};
use system_error::SystemError;

use crate::{
    init::boot_params,
    libs::rwlock::RwLock,
    mm::{memblock::mem_block_manager, mmio_buddy::MMIOSpaceGuard, PhysAddr},
};

static OPEN_FIRMWARE_FDT_DRIVER: OpenFirmwareFdtDriver = OpenFirmwareFdtDriver::new();

#[inline(always)]
pub fn open_firmware_fdt_driver() -> &'static OpenFirmwareFdtDriver {
    &OPEN_FIRMWARE_FDT_DRIVER
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

#[allow(dead_code)]
pub struct OpenFirmwareFdtDriver {
    inner: RwLock<InnerOpenFirmwareFdtDriver>,
}

#[allow(dead_code)]
pub struct InnerOpenFirmwareFdtDriver {
    /// FDT自身映射的MMIO空间
    fdt_map_guard: Option<MMIOSpaceGuard>,
}

impl OpenFirmwareFdtDriver {
    const fn new() -> Self {
        Self {
            inner: RwLock::new(InnerOpenFirmwareFdtDriver {
                fdt_map_guard: None,
            }),
        }
    }

    #[allow(dead_code)]
    pub fn early_scan_device_tree(&self) -> Result<(), SystemError> {
        let fdt = self.fdt_ref()?;
        self.early_init_scan_nodes(&fdt);

        return Ok(());
    }

    #[allow(dead_code)]
    pub unsafe fn set_fdt_map_guard(&self, guard: Option<MMIOSpaceGuard>) {
        self.inner.write().fdt_map_guard = guard;
    }

    /// 获取FDT的引用
    pub fn fdt_ref(&self) -> Result<Fdt<'static>, SystemError> {
        let fdt_vaddr = boot_params().read().fdt().ok_or(SystemError::ENODEV)?;
        let fdt: Fdt<'_> = unsafe {
            fdt::Fdt::from_ptr(fdt_vaddr.as_ptr()).map_err(|e| {
                error!("failed to parse fdt, err={:?}", e);
                SystemError::EINVAL
            })
        }?;
        Ok(fdt)
    }

    fn early_init_scan_nodes(&self, fdt: &Fdt) {
        self.early_init_scan_root(fdt)
            .expect("Failed to scan fdt root node.");

        self.early_init_scan_chosen(fdt).unwrap_or_else(|_| {
            warn!("No `chosen` node found");
        });

        self.early_init_scan_memory(fdt);
    }

    /// 扫描根节点
    fn early_init_scan_root(&self, fdt: &Fdt) -> Result<(), SystemError> {
        let node = fdt.find_node("/").ok_or(SystemError::ENODEV)?;

        let mut guard = FDT_GLOBAL_DATA.write();

        if let Some(prop) = node.property("#size-cells") {
            guard.root_size_cells = prop.as_usize().unwrap() as u32;

            // debug!("fdt_root_size_cells={}", guard.root_size_cells);
        }

        if let Some(prop) = node.property("#address-cells") {
            guard.root_addr_cells = prop.as_usize().unwrap() as u32;

            // debug!("fdt_root_addr_cells={}", guard.root_addr_cells);
        }

        return Ok(());
    }

    /// 扫描 `/chosen` 节点
    fn early_init_scan_chosen(&self, fdt: &Fdt) -> Result<(), SystemError> {
        const CHOSEN_NAME1: &str = "/chosen";
        let mut node = fdt.find_node(CHOSEN_NAME1);
        if node.is_none() {
            const CHOSEN_NAME2: &str = "/chosen@0";
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

        debug!("Command line: {}", boot_params().read().boot_cmdline_str());
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
                let base_index = i * (addr_cells + size_cells);

                let (base, base_index) = read_cell(reg.value, base_index, addr_cells);
                let (size, _) = read_cell(reg.value, base_index, size_cells);

                if size == 0 {
                    continue;
                }

                debug!("Found memory: base={:#x}, size={:#x}", base, size);
                self.early_init_dt_add_memory(base, size);
                found_memory = true;
            }
        }

        return found_memory;
    }

    #[cfg(target_arch = "x86_64")]
    pub fn early_init_dt_add_memory(&self, _base: u64, _size: u64) {
        panic!("x86_64 should not call early_init_dt_add_memory");
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub fn early_init_dt_add_memory(&self, base: u64, size: u64) {
        use crate::{
            arch::MMArch,
            libs::align::page_align_down,
            mm::{memblock::MemBlockManager, MemoryManagementArch},
        };

        let mut base = base as usize;
        let mut size = size as usize;

        if size < (MMArch::PAGE_SIZE - (base & (!MMArch::PAGE_MASK))) {
            warn!("Ignoring memory block {:#x}-{:#x}", base, base + size);
        }

        if PhysAddr::new(base).check_aligned(MMArch::PAGE_SIZE) == false {
            size -= MMArch::PAGE_SIZE - (base & (!MMArch::PAGE_MASK));
            base = page_align_down(base);
        }

        size = page_align_down(size);

        if base > MemBlockManager::MAX_MEMBLOCK_ADDR.data() {
            warn!("Ignoring memory block {:#x}-{:#x}", base, base + size);
        }

        if base + size - 1 > MemBlockManager::MAX_MEMBLOCK_ADDR.data() {
            warn!(
                "Ignoring memory range {:#x}-{:#x}",
                MemBlockManager::MAX_MEMBLOCK_ADDR.data() + 1,
                base + size
            );
            size = MemBlockManager::MAX_MEMBLOCK_ADDR.data() - base + 1;
        }

        if base + size < MemBlockManager::MIN_MEMBLOCK_ADDR.data() {
            warn!("Ignoring memory range {:#x}-{:#x}", base, base + size);
            return;
        }

        if base < MemBlockManager::MIN_MEMBLOCK_ADDR.data() {
            {
                warn!(
                    "Ignoring memory range {:#x}-{:#x}",
                    base,
                    MemBlockManager::MIN_MEMBLOCK_ADDR.data()
                );
                size -= MemBlockManager::MIN_MEMBLOCK_ADDR.data() - base;
                base = MemBlockManager::MIN_MEMBLOCK_ADDR.data();
            }
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

    /// 判断设备是否可用
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

    /// 在UEFI初始化后，扫描FDT中的`/reserved-memory`节点，设置保留的内存
    ///
    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/of/fdt.c#634
    #[allow(dead_code)]
    pub fn early_init_fdt_scan_reserved_mem(&self) {
        let vaddr = boot_params().read().fdt();
        if vaddr.is_none() {
            return;
        }
        let vaddr = vaddr.unwrap();
        let fdt = unsafe { Fdt::from_ptr(vaddr.data() as *const u8) };
        if fdt.is_err() {
            return;
        }

        let fdt = fdt.unwrap();
        self.early_reserve_fdt_itself(&fdt);

        let reserved_mem_nodes = fdt.memory_reservations();

        for node in reserved_mem_nodes {
            if node.size() != 0 {
                let address = PhysAddr::new(node.address() as usize);
                let size = node.size();
                debug!("Reserve memory: {:?}-{:?}", address, address + size);
                mem_block_manager().reserve_block(address, size).unwrap();
            }
        }

        self.fdt_scan_reserved_mem(&fdt)
            .expect("Failed to scan reserved memory");
    }

    /// 保留fdt自身的内存空间
    fn early_reserve_fdt_itself(&self, fdt: &Fdt) {
        #[cfg(target_arch = "riscv64")]
        {
            use crate::libs::align::{page_align_down, page_align_up};

            let fdt_paddr = boot_params().read().arch.fdt_paddr;
            let rsvd_start = PhysAddr::new(page_align_down(fdt_paddr.data()));
            let rsvd_size = page_align_up(fdt_paddr.data() - rsvd_start.data() + fdt.total_size());
            mem_block_manager()
                .reserve_block(rsvd_start, rsvd_size)
                .expect("Failed to reserve memory for fdt");
        }

        #[cfg(target_arch = "x86_64")]
        {
            let _ = fdt;
        }
    }

    fn fdt_scan_reserved_mem(&self, fdt: &Fdt) -> Result<(), SystemError> {
        let node = fdt
            .find_node("/reserved-memory")
            .ok_or(SystemError::ENODEV)?;

        for child in node.children() {
            if !self.is_device_avaliable(&child) {
                continue;
            }

            reserved_mem_reserve_reg(&child).ok();
        }

        return Ok(());
    }

    fn early_init_dt_reserve_memory(
        &self,
        base: PhysAddr,
        size: usize,
        nomap: bool,
    ) -> Result<(), SystemError> {
        if nomap {
            if mem_block_manager().is_overlapped(base, size)
                && mem_block_manager().is_overlapped_with_reserved(base, size)
            {
                // 如果内存已经被其他区域预留（即已经被映射），我们不应该允许它被标记为`nomap`，
                // 但是不需要担心如果该区域不是内存（即不会被映射）的情况。
                return Err(SystemError::EBUSY);
            }

            return mem_block_manager().mark_nomap(base, size);
        }

        return mem_block_manager().reserve_block(base, size);
    }

    pub fn find_node_by_compatible<'b>(
        &self,
        fdt: &'b Fdt<'b>,
        compatible: &'b str,
    ) -> impl Iterator<Item = fdt::node::FdtNode<'b, 'b>> + 'b {
        // compatible = compatible.trim();
        let r = fdt.all_nodes().filter(move |x| {
            x.compatible()
                .is_some_and(|x| x.all().any(|x| x == compatible))
        });

        return r;
    }
}

#[allow(dead_code)]
fn reserved_mem_reserve_reg(node: &FdtNode<'_, '_>) -> Result<(), SystemError> {
    let global_data_guard: crate::libs::rwlock::RwLockReadGuard<'_, FdtGlobalData> =
        FDT_GLOBAL_DATA.read();
    let t_len = ((global_data_guard.root_addr_cells + global_data_guard.root_size_cells) as usize)
        * size_of::<u32>();
    drop(global_data_guard);

    let reg = node.property("reg").ok_or(SystemError::ENOENT)?;

    let mut reg_size = reg.value.len();
    if reg_size > 0 && reg_size % t_len != 0 {
        error!(
            "Reserved memory: invalid reg property in '{}', skipping node.",
            node.name
        );
        return Err(SystemError::EINVAL);
    }
    // 每个cell是4字节
    let addr_cells = FDT_GLOBAL_DATA.read().root_addr_cells as usize;
    let size_cells = FDT_GLOBAL_DATA.read().root_size_cells as usize;

    let nomap = node.property("no-map").is_some();

    let mut base_index = 0;

    while reg_size >= t_len {
        let (base, bi) = read_cell(reg.value, base_index, addr_cells);
        base_index = bi;
        let (size, bi) = read_cell(reg.value, base_index, size_cells);
        base_index = bi;

        if size > 0
            && open_firmware_fdt_driver()
                .early_init_dt_reserve_memory(PhysAddr::new(base as usize), size as usize, nomap)
                .is_ok()
        {
            debug!(
                "Reserved memory: base={:#x}, size={:#x}, nomap={}",
                base, size, nomap
            );
        } else {
            error!(
                "Failed to reserve memory: base={:#x}, size={:#x}, nomap={}",
                base, size, nomap
            );
        }

        reg_size -= t_len;

        // todo: linux这里保存了节点，但是我感觉现在还用不着。
    }

    return Ok(());
}

/// 从FDT的`reg`属性中读取指定数量的cell，作为一个小端u64返回
///
/// ## 参数
///
/// - `reg_value`：`reg`属性数组的引用
/// - `base_index`：起始索引
/// - `cells`：要读取的cell数量，必须是1或2
///
/// ## 返回值
///
/// (value, next_base_index)
fn read_cell(reg_value: &[u8], base_index: usize, cells: usize) -> (u64, usize) {
    let next_base_index = base_index + cells * 4;
    match cells {
        1 => {
            return (
                u32::from_be_bytes(reg_value[base_index..base_index + 4].try_into().unwrap())
                    .into(),
                next_base_index,
            );
        }

        2 => {
            return (
                u64::from_be_bytes(reg_value[base_index..base_index + 8].try_into().unwrap()),
                next_base_index,
            );
        }
        _ => {
            panic!("cells must be 1 or 2");
        }
    }
}
