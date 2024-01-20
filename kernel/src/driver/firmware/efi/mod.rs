use system_error::SystemError;

use crate::{libs::rwlock::RwLock, mm::PhysAddr};

use self::memmap::EFIMemoryMapInfo;

mod fdt;
pub mod init;
pub mod memmap;

static EFI_MANAGER: EFIManager = EFIManager::new();

/// EFI管理器
///
/// 数据成员可参考： https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/efi.h#620
#[derive(Debug)]
pub struct EFIManager {
    inner: RwLock<InnerEFIManager>,
}

#[inline(always)]
pub fn efi_manager() -> &'static EFIManager {
    &EFI_MANAGER
}

#[derive(Debug)]
struct InnerEFIManager {
    pub mmap: EFIMemoryMapInfo,
    /// EFI模块启动时状态的标识
    pub init_flags: EFIInitFlags,
    /// runtime services的物理地址
    pub runtime_paddr: Option<PhysAddr>,
    /// runtime services的版本号
    pub runtime_service_version: Option<uefi_raw::table::Revision>,
}

impl EFIManager {
    const fn new() -> Self {
        EFIManager {
            inner: RwLock::new(InnerEFIManager {
                mmap: EFIMemoryMapInfo::DEFAULT,
                init_flags: EFIInitFlags::empty(),
                runtime_paddr: None,
                runtime_service_version: None,
            }),
        }
    }

    pub fn desc_version(&self) -> usize {
        return self.inner.read().mmap.desc_version;
    }

    /// 检查是否为有效的system table表头
    ///
    /// ## 参数
    ///
    /// - header: system table表头
    /// - min_major: 最小的major版本号。如果不满足，则会输出Warning，并返回Ok
    ///
    /// ## 返回
    ///
    /// - Ok(()): 检查通过
    /// - Err(SystemError::EINVAL): header无效
    pub fn check_system_table_header(
        &self,
        header: &uefi_raw::table::Header,
        min_major: u16,
    ) -> Result<(), SystemError> {
        if header.signature != uefi_raw::table::system::SystemTable::SIGNATURE {
            kerror!("System table signature mismatch!");
            return Err(SystemError::EINVAL);
        }

        if header.revision.major() < min_major {
            kwarn!(
                "System table version: {:?}, expected {}.00 or greater!",
                header.revision,
                min_major
            );
        }

        return Ok(());
    }
}

// 在Rust中，我们使用枚举和bitflags来表示这些宏
bitflags! {
    pub struct EFIInitFlags: u32 {
        /// 当前使用EFI启动
        const BOOT = 1 << 0;
        /// 是否可以使用EFI配置表
        const CONFIG_TABLES = 1 << 1;
        /// 是否可以使用运行时服务
        const RUNTIME_SERVICES = 1 << 2;
        /// 是否可以使用EFI内存映射
        const MEMMAP = 1 << 3;
        /// 固件是否为64位
        const EFI_64BIT = 1 << 4;
        /// 访问是否通过虚拟化接口
        const PARAVIRT = 1 << 5;
        /// 第一架构特定位
        const ARCH_1 = 1 << 6;
        /// 打印附加运行时调试信息
        const DBG = 1 << 7;
        /// 是否可以在运行时数据区域映射非可执行
        const NX_PE_DATA = 1 << 8;
        /// 固件是否发布了一个EFI_MEMORY_ATTRIBUTES表
        const MEM_ATTR = 1 << 9;
        /// 内核是否配置为忽略软保留
        const MEM_NO_SOFT_RESERVE = 1 << 10;
        /// 是否可以使用EFI引导服务内存段
        const PRESERVE_BS_REGIONS = 1 << 11;

    }
}
