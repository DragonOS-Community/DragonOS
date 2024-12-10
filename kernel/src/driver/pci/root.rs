use core::fmt::Formatter;

use alloc::sync::Arc;
use hashbrown::HashMap;

use crate::{
    arch::{PciArch, TraitPciArch},
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        page::PAGE_2M_SIZE,
    },
};

use super::{
    ecam::EcamRootInfo,
    pci::{BusDeviceFunction, ExternalCapabilityIterator, PciCam, PciError, SegmentGroupNumber},
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
    pub ecam_root_info: Option<EcamRootInfo>,
    pub mmio_guard: Option<Arc<MMIOSpaceGuard>>, //映射后的虚拟地址，为方便访问数据这里转化成指针
    /// 配置空间访问机制
    pub cam: PciCam,
    /// bus起始位置
    pub bus_begin: u8,
    /// bus结束位置
    pub bus_end: u8,
}

///线程间共享需要，该结构体只需要在初始化时写入数据，无需读写锁保证线程安全
unsafe impl Send for PciRoot {}
unsafe impl Sync for PciRoot {}
///实现PciRoot的Display trait，自定义输出
impl core::fmt::Display for PciRoot {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        if let Some(ecam_root_info) = &self.ecam_root_info {
            write!(
                    f,
                    "PCI Eacm Root with segment:{}, bus begin at {}, bus end at {}, physical address at {:?},mapped at {:?}",
                    ecam_root_info.segment_group_number, ecam_root_info.bus_begin, ecam_root_info.bus_end, ecam_root_info.physical_address_base, self.mmio_guard
                )
        } else {
            write!(f, "PCI Root cam is {:?}", self.cam,)
        }
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
        ecam_root_info: Option<EcamRootInfo>,
        cam: PciCam,
        bus_begin: u8,
        bus_end: u8,
    ) -> Result<Arc<Self>, PciError> {
        let mut pci_root = Self {
            ecam_root_info,
            mmio_guard: None,
            cam,
            bus_begin,
            bus_end,
        };

        if ecam_root_info.is_some() {
            pci_root.map()?;
        }

        Ok(Arc::new(pci_root))
    }

    /// # 完成物理地址到虚拟地址的映射，并将虚拟地址加入mmio_base变量
    /// ## return 返回错误或Ok(0)
    fn map(&mut self) -> Result<u8, PciError> {
        //debug!("bus_begin={},bus_end={}", self.bus_begin,self.bus_end);
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
                .map_phys(self.ecam_root_info.unwrap().physical_address_base, size)
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
        let bdf = ((bus_device_function.bus - self.ecam_root_info.unwrap().bus_begin) as u32) << 8
            | (bus_device_function.device as u32) << 3
            | bus_device_function.function as u32;
        let address =
            bdf << match self.cam {
                PciCam::Portiocam => 4,
                PciCam::MmioCam => 8,
                PciCam::Ecam => 12,
            } | register_offset as u32;
        // Ensure that address is word-aligned.
        assert!(address & 0x3 == 0);
        address
    }

    /// # read_config - 通过bus_device_function和offset读取相应位置寄存器的值（32位）
    ///
    /// 此函数用于通过指定的bus_device_function和register_offset读取PCI设备中相应位置的寄存器值。
    ///
    /// ## 参数
    ///
    /// - `bus_device_function`: 在同一个group中pci设备的唯一标识符
    /// - `register_offset`: 寄存器在设备中的offset
    ///
    /// ## 返回值
    ///
    /// - `u32`: 寄存器读值结果
    pub fn read_config(&self, bus_device_function: BusDeviceFunction, register_offset: u16) -> u32 {
        if self.ecam_root_info.is_some() {
            let address = self.cam_offset(bus_device_function, register_offset);
            unsafe {
                // Right shift to convert from byte offset to word offset.
                ((self.mmio_guard.as_ref().unwrap().vaddr().data() as *mut u32)
                    .add((address >> 2) as usize))
                .read_volatile()
            }
        } else {
            PciArch::read_config(&bus_device_function, register_offset as u8)
        }
    }

    /// # write_config - 通过bus_device_function和offset写入相应位置寄存器值（32位）
    ///
    /// 此函数用于通过指定的bus_device_function和register_offset，向PCI设备写入一个32位的寄存器值。
    ///
    /// ## 参数
    ///
    /// - `bus_device_function`: 在同一个group中pci设备的唯一标识符
    /// - `register_offset`: 寄存器在设备中的offset
    /// - `data`: 要写入的数据
    pub fn write_config(
        &self,
        bus_device_function: BusDeviceFunction,
        register_offset: u16,
        data: u32,
    ) {
        if self.ecam_root_info.is_some() {
            let address = self.cam_offset(bus_device_function, register_offset);
            // Safe because both the `mmio_base` and the address offset are properly aligned, and the
            // resulting pointer is within the MMIO range of the CAM.
            unsafe {
                // Right shift to convert from byte offset to word offset.
                ((self.mmio_guard.as_ref().unwrap().vaddr().data() as *mut u32)
                    .add((address >> 2) as usize))
                .write_volatile(data)
            }
        } else {
            PciArch::write_config(&bus_device_function, register_offset as u8, data);
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

    /// # 添加PciRoot - 向PciRootManager中添加一个PciRoot
    ///
    /// 向PciRootManager中添加一个新的PciRoot，通过其segment_group_number进行标识。
    ///
    /// ## 参数
    ///
    /// - `pci_root`: Arc<PciRoot>，要添加的PciRoot的Arc指针
    pub fn add_pci_root(&self, pci_root: Arc<PciRoot>) {
        let mut inner = self.inner.lock();

        if let Some(ecam_root_info) = pci_root.ecam_root_info {
            inner
                .pci_root
                .insert(ecam_root_info.segment_group_number, pci_root);
        } else {
            inner.pci_root.insert(pci_root.bus_begin as u16, pci_root);
        }
    }

    /// # 检查是否存在PciRoot - 检查PciRootManager中是否存在指定segment_group_number的PciRoot
    ///
    /// 检查PciRootManager中是否存在segment_group_number对应的PciRoot。
    ///
    /// ## 参数
    ///
    /// - `segement_group_number`: SegmentGroupNumber，要检查的segment_group_number。
    ///
    /// ## 返回值
    ///
    /// - `true`: 如果存在对应的PciRoot。
    /// - `false`: 如果不存在对应的PciRoot。
    pub fn has_root(&self, segement_group_number: SegmentGroupNumber) -> bool {
        self.inner
            .lock()
            .pci_root
            .contains_key(&segement_group_number)
    }

    /// # 获取PciRoot - 从PciRootManager中获取指定segment_group_number的PciRoot
    ///
    /// 从PciRootManager中获取segment_group_number对应的PciRoot。
    ///
    /// ## 参数
    ///
    /// - `segement_group_number`: SegmentGroupNumber，要获取的PciRoot的segment_group_number。
    ///
    /// ## 返回值
    ///
    /// - `Some(Arc<PciRoot>)`: 如果找到对应的PciRoot，返回其引用。
    /// - `None`: 如果没有找到对应的PciRoot。
    pub fn get_pci_root(&self, segement_group_number: SegmentGroupNumber) -> Option<Arc<PciRoot>> {
        self.inner
            .lock()
            .pci_root
            .get(&segement_group_number)
            .cloned()
    }

    /// # PciRoot迭代器 - 创建一个新的PciRoot迭代器
    ///
    /// 创建一个新的迭代器，用于遍历PciRootManager中的所有PciRoot。
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

impl Iterator for PciRootIterator<'_> {
    type Item = Arc<PciRoot>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.pci_root.values().nth(self.index).cloned()
    }
}
