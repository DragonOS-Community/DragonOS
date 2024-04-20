use core::fmt::Formatter;

use alloc::sync::Arc;
use hashbrown::HashMap;

use crate::{
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        page::PAGE_2M_SIZE,
        PhysAddr,
    },
};

use super::pci::{
    BusDeviceFunction, ExternalCapabilityIterator, PciCam, PciError, SegmentGroupNumber,
};

lazy_static! {
    static ref PCI_ROOT_MANAGER: PciRootManager = PciRootManager::new();
}

#[inline(always)]
pub fn pci_root_manager() -> &'static PciRootManager {
    &PCI_ROOT_MANAGER
}

/// 代表一个PCI segement greoup.
#[derive(Clone, Debug)]
pub struct PciRoot {
    pub physical_address_base: PhysAddr,          //物理地址，acpi获取
    pub mmio_guard: Option<Arc<MMIOSpaceGuard>>,  //映射后的虚拟地址，为方便访问数据这里转化成指针
    pub segment_group_number: SegmentGroupNumber, //segement greoup的id
    pub bus_begin: u8,                            //该分组中的最小bus
    pub bus_end: u8,                              //该分组中的最大bus
    /// 配置空间访问机制
    pub cam: PciCam,
}

///线程间共享需要，该结构体只需要在初始化时写入数据，无需读写锁保证线程安全
unsafe impl Send for PciRoot {}
unsafe impl Sync for PciRoot {}
///实现PciRoot的Display trait，自定义输出
impl core::fmt::Display for PciRoot {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(
                f,
                "PCI Root with segement:{}, bus begin at {}, bus end at {}, physical address at {:?},mapped at {:?}",
                self.segment_group_number, self.bus_begin, self.bus_end, self.physical_address_base, self.mmio_guard
            )
    }
}

impl PciRoot {
    /// 此函数用于初始化一个PciRoot结构体实例，
    /// 该结构体基于ECAM根的物理地址，将其映射到虚拟地址
    ///
    /// ## 参数
    ///
    /// - segment_group_number: ECAM根的段组号。
    /// - cam: PCI配置空间访问机制
    ///
    /// ## 返回值
    ///
    /// - Ok(Self): 初始化成功，返回一个新的结构体实例。
    /// - Err(PciError): 初始化过程中发生错误，返回错误信息。
    ///
    /// ## 副作用
    ///
    /// - 成功执行后，结构体的内部状态将被初始化为包含映射后的虚拟地址。
    pub fn new(
        segment_group_number: SegmentGroupNumber,
        cam: PciCam,
        phys_base: PhysAddr,
        bus_begin: u8,
        bus_end: u8,
    ) -> Result<Arc<Self>, PciError> {
        assert_eq!(cam, PciCam::Ecam);
        let mut pci_root = Self {
            physical_address_base: phys_base,
            mmio_guard: None,
            segment_group_number,
            bus_begin,
            bus_end,
            cam,
        };
        pci_root.map()?;

        Ok(Arc::new(pci_root))
    }
    /// @brief  完成物理地址到虚拟地址的映射，并将虚拟地址加入mmio_base变量
    /// @return 返回错误或Ok(0)
    fn map(&mut self) -> Result<u8, PciError> {
        //kdebug!("bus_begin={},bus_end={}", self.bus_begin,self.bus_end);
        let bus_number = (self.bus_end - self.bus_begin) as u32 + 1;
        let bus_number_double = (bus_number - 1) / 2 + 1; //一个bus占据1MB空间，计算全部bus占据空间相对于2MB空间的个数

        let size = (bus_number_double as usize) * PAGE_2M_SIZE;
        unsafe {
            let space_guard = mmio_pool()
                .create_mmio(size)
                .map_err(|_| PciError::CreateMmioError)?;
            let space_guard = Arc::new(space_guard);
            self.mmio_guard = Some(space_guard.clone());

            assert!(space_guard
                .map_phys(self.physical_address_base, size)
                .is_ok());
        }
        return Ok(0);
    }

    /// # cam_offset - 获得要操作的寄存器相对于mmio_offset的偏移量
    ///
    /// 此函数用于计算一个PCI设备中特定寄存器相对于该设备的MMIO基地址的偏移量。
    ///
    /// ## 参数
    ///
    /// - `bus_device_function`: BusDeviceFunction，用于标识在同一组中的PCI设备。
    /// - `register_offset`: u16，寄存器在设备中的偏移量。
    ///
    /// ## 返回值
    ///
    /// - `u32`: 成功时，返回要操作的寄存器相对于mmio_offset的偏移量。
    ///
    /// ## Panic
    ///
    /// - 此函数在参数有效性方面进行了断言，如果传入的`bus_device_function`无效，将panic。
    /// - 此函数计算出的地址需要是字对齐的（即地址与0x3对齐）。如果不是，将panic。
    fn cam_offset(&self, bus_device_function: BusDeviceFunction, register_offset: u16) -> u32 {
        assert!(bus_device_function.valid());
        let bdf = ((bus_device_function.bus - self.bus_begin) as u32) << 8
            | (bus_device_function.device as u32) << 3
            | bus_device_function.function as u32;
        let address =
            bdf << match self.cam {
                PciCam::MmioCam => 8,
                PciCam::Ecam => 12,
            } | register_offset as u32;
        // Ensure that address is word-aligned.
        assert!(address & 0x3 == 0);
        address
    }
    /// @brief 通过bus_device_function和offset读取相应位置寄存器的值（32位）
    /// @param bus_device_function 在同一个group中pci设备的唯一标识符
    /// @param register_offset 寄存器在设备中的offset
    /// @return u32 寄存器读值结果
    pub fn read_config(&self, bus_device_function: BusDeviceFunction, register_offset: u16) -> u32 {
        let address = self.cam_offset(bus_device_function, register_offset);
        unsafe {
            // Right shift to convert from byte offset to word offset.
            ((self.mmio_guard.as_ref().unwrap().vaddr().data() as *mut u32)
                .add((address >> 2) as usize))
            .read_volatile()
        }
    }

    /// @brief 通过bus_device_function和offset写入相应位置寄存器值（32位）
    /// @param bus_device_function 在同一个group中pci设备的唯一标识符
    /// @param register_offset 寄存器在设备中的offset
    /// @param data 要写入的值
    pub fn write_config(
        &self,
        bus_device_function: BusDeviceFunction,
        register_offset: u16,
        data: u32,
    ) {
        let address = self.cam_offset(bus_device_function, register_offset);
        // Safe because both the `mmio_base` and the address offset are properly aligned, and the
        // resulting pointer is within the MMIO range of the CAM.
        unsafe {
            // Right shift to convert from byte offset to word offset.
            ((self.mmio_guard.as_ref().unwrap().vaddr().data() as *mut u32)
                .add((address >> 2) as usize))
            .write_volatile(data)
        }
    }
    /// 返回迭代器，遍历pcie设备的external_capabilities
    #[allow(dead_code)]
    pub fn external_capabilities(
        &self,
        bus_device_function: BusDeviceFunction,
    ) -> ExternalCapabilityIterator {
        ExternalCapabilityIterator {
            root: self,
            bus_device_function,
            next_capability_offset: Some(0x100),
        }
    }
}

#[inline(always)]
pub fn pci_root_0() -> Arc<PciRoot> {
    pci_root_manager().get_pci_root(0).unwrap()
}

pub struct PciRootManager {
    inner: SpinLock<InnerPciRootManager>,
}

struct InnerPciRootManager {
    pci_root: HashMap<SegmentGroupNumber, Arc<PciRoot>>,
}

impl PciRootManager {
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerPciRootManager {
                pci_root: HashMap::new(),
            }),
        }
    }

    pub fn add_pci_root(&self, pci_root: Arc<PciRoot>) {
        let mut inner = self.inner.lock();
        inner
            .pci_root
            .insert(pci_root.segment_group_number, pci_root);
    }

    pub fn has_root(&self, segement_group_number: SegmentGroupNumber) -> bool {
        self.inner
            .lock()
            .pci_root
            .contains_key(&segement_group_number)
    }

    pub fn get_pci_root(&self, segement_group_number: SegmentGroupNumber) -> Option<Arc<PciRoot>> {
        self.inner
            .lock()
            .pci_root
            .get(&segement_group_number)
            .cloned()
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> PciRootIterator<'_> {
        PciRootIterator {
            inner: self.inner.lock(),
            index: 0,
        }
    }
}

pub struct PciRootIterator<'a> {
    inner: SpinLockGuard<'a, InnerPciRootManager>,
    index: usize,
}

impl<'a> Iterator for PciRootIterator<'a> {
    type Item = Arc<PciRoot>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.pci_root.values().nth(self.index).cloned()
    }
}
