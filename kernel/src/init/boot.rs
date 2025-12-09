use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use core::any::Any;
use core::cmp::min;

use acpi::rsdp::Rsdp;
use alloc::string::String;
use system_error::SystemError;

use crate::driver::base::kobject::KObjectState;
use crate::filesystem::vfs::InodeMode;
use crate::init::initcall::INITCALL_POSTCORE;
use crate::libs::rwlock::RwLockReadGuard;
use crate::libs::rwlock::RwLockWriteGuard;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::{
    arch::init::ArchBootParams,
    driver::video::fbdev::base::BootTimeScreenInfo,
    filesystem::kernfs::KernFSInode,
    filesystem::sysfs::{Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport, SYSFS_ATTR_MODE_RO},
    libs::lazy_init::Lazy,
    misc::ksysfs::sys_kernel_kobj,
    mm::{PhysAddr, VirtAddr},
};
use unified_init::macros::unified_init;

use crate::driver::base::{
    kobject::{KObjType, KObject, KObjectManager, KObjectSysFSOps, LockedKObjectState},
    kset::KSet,
};

/// `/sys/kernel/boot_params`的 kobject, 需要这里加一个引用来保持持久化, 不然会被释放
static mut SYS_KERNEL_BOOT_PARAMS_INSTANCE: Option<Arc<BootParamsSys>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_kernel_boot_params() -> Arc<BootParamsSys> {
    unsafe { SYS_KERNEL_BOOT_PARAMS_INSTANCE.clone().unwrap() }
}

use super::boot_params;
#[derive(Debug)]
pub struct BootParams {
    pub screen_info: BootTimeScreenInfo,
    bootloader_name: Option<String>,
    #[allow(dead_code)]
    pub arch: ArchBootParams,
    boot_command_line: [u8; Self::BOOT_COMMAND_LINE_SIZE],
    pub acpi: BootloaderAcpiArg,
}

impl BootParams {
    const DEFAULT: Self = BootParams {
        screen_info: BootTimeScreenInfo::DEFAULT,
        bootloader_name: None,
        arch: ArchBootParams::DEFAULT,
        boot_command_line: [0u8; Self::BOOT_COMMAND_LINE_SIZE],
        acpi: BootloaderAcpiArg::NotProvided,
    };

    /// 开机命令行参数字符串最大大小
    pub const BOOT_COMMAND_LINE_SIZE: usize = 2048;

    pub(super) const fn new() -> Self {
        Self::DEFAULT
    }

    /// 开机命令行参数（原始字节数组）
    #[allow(dead_code)]
    pub fn boot_cmdline(&self) -> &[u8] {
        &self.boot_command_line
    }

    /// 开机命令行参数字符串
    pub fn boot_cmdline_str(&self) -> &str {
        core::str::from_utf8(&self.boot_cmdline()[..self.boot_cmdline_len()]).unwrap()
    }

    #[allow(dead_code)]
    pub fn bootloader_name(&self) -> Option<&str> {
        self.bootloader_name.as_deref()
    }

    pub fn boot_cmdline_len(&self) -> usize {
        self.boot_command_line
            .iter()
            .position(|&x| x == 0)
            .unwrap_or(self.boot_command_line.len())
    }

    /// 追加开机命令行参数
    ///
    /// 如果开机命令行参数已经满了，则不会追加。
    /// 如果超过了最大长度，则截断。
    ///
    /// ## 参数
    ///
    /// - `data`：追加的数据
    pub fn boot_cmdline_append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let mut pos: Option<usize> = None;
        // 寻找结尾
        for (i, x) in self.boot_command_line.iter().enumerate() {
            if *x == 0 {
                pos = Some(i);
                break;
            }
        }
        let pos = pos.unwrap_or(self.boot_command_line.len() - 1) as isize;

        let avail = self.boot_command_line.len() as isize - pos - 1;
        if avail <= 0 {
            return;
        }

        let len = min(avail as usize, data.len());
        let pos = pos as usize;
        self.boot_command_line[pos..pos + len].copy_from_slice(&data[0..len]);

        self.boot_command_line[pos + len] = 0;
    }

    /// 获取FDT的虚拟地址
    #[allow(dead_code)]
    pub fn fdt(&self) -> Option<VirtAddr> {
        #[cfg(target_arch = "riscv64")]
        return Some(self.arch.arch_fdt());

        #[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))]
        return None;
    }

    /// 获取FDT的物理地址
    #[allow(dead_code)]
    pub fn fdt_paddr(&self) -> Option<PhysAddr> {
        #[cfg(target_arch = "riscv64")]
        return Some(self.arch.fdt_paddr);

        #[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))]
        return None;
    }
}

/// 开机引导回调，用于初始化内核启动参数
pub trait BootCallbacks: Send + Sync {
    /// 初始化引导程序名称
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError>;
    /// 初始化ACPI参数
    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError>;
    /// 初始化内核命令行参数
    ///
    /// 该函数应该把内核命令行参数追加到`boot_params().boot_cmdline`中
    fn init_kernel_cmdline(&self) -> Result<(), SystemError>;
    /// 初始化initramfs
    ///
    /// 该函数会检索[外部initramfs]追加到`boot_params().initramfs`中,
    /// [外部initramfs] 指由bootloader加载的，如qemu的 -initrd 参数
    #[allow(dead_code)]
    fn init_initramfs(&self) -> Result<(), SystemError>;
    /// 初始化帧缓冲区信息
    ///
    /// - 该函数应该把帧缓冲区信息写入`scinfo`中。
    /// - 该函数应该在内存管理初始化之前调用。
    fn early_init_framebuffer_info(
        &self,
        scinfo: &mut BootTimeScreenInfo,
    ) -> Result<(), SystemError>;

    // TODO: 下面三个应该合成一个, 都存到 arch/boot_params(linux是这样的)
    /// 初始化内存块
    fn early_init_memory_blocks(&self) -> Result<(), SystemError>;
    /// 初始化内存 memmap 信息到 sysfs
    fn init_memmap_sysfs(&self) -> Result<(), SystemError>;
    /// 初始化内存 memmap 信息到 boot_params
    fn init_memmap_bp(&self) -> Result<(), SystemError>;
}

static BOOT_CALLBACKS: Lazy<&'static dyn BootCallbacks> = Lazy::new();

/// 注册开机引导回调
pub fn register_boot_callbacks(callbacks: &'static dyn BootCallbacks) {
    BOOT_CALLBACKS.init(callbacks);
}

/// 获取开机引导回调
pub fn boot_callbacks() -> &'static dyn BootCallbacks {
    let p = BOOT_CALLBACKS
        .try_get()
        .expect("Boot callbacks not initialized");

    *p
}

pub(super) fn boot_callback_except_early() {
    let mut boot_params = boot_params().write();
    boot_params.bootloader_name = boot_callbacks()
        .init_bootloader_name()
        .expect("Failed to init bootloader name");
    boot_params.acpi = boot_callbacks()
        .init_acpi_args()
        .unwrap_or(BootloaderAcpiArg::NotProvided);
}

/// ACPI information from the bootloader.
#[derive(Copy, Clone, Debug)]
pub enum BootloaderAcpiArg {
    /// The bootloader does not provide one, a manual search is needed.
    NotProvided,
    /// Physical address of the RSDP.
    #[allow(dead_code)]
    Rsdp(PhysAddr),
    /// Address of RSDT provided in RSDP v1.
    Rsdt(Rsdp),
    /// Address of XSDT provided in RSDP v2+.
    Xsdt(Rsdp),
}

/// 初始化boot_params模块在sysfs中的目录
#[unified_init(INITCALL_POSTCORE)]
fn bootparams_sysfs_init() -> Result<(), SystemError> {
    let bp = BootParamsSys::new("boot_params".to_string());

    unsafe {
        SYS_KERNEL_BOOT_PARAMS_INSTANCE = Some(bp.clone());
    }

    let kobj = sys_kernel_kobj();
    bp.set_parent(Some(Arc::downgrade(&(kobj as Arc<dyn KObject>))));
    KObjectManager::add_kobj(bp.clone() as Arc<dyn KObject>).unwrap_or_else(|e| {
        log::warn!("Failed to add boot_params kobject to sysfs: {:?}", e);
    });

    return Ok(());
}

#[derive(Debug)]
pub struct BootParamsSys {
    inner: SpinLock<BootParamsSysInner>,
    kobj_state: LockedKObjectState,
    name: String,
}

#[derive(Debug)]
pub struct BootParamsSysInner {
    kern_inode: Option<Arc<KernFSInode>>,
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
}

#[derive(Debug)]
struct BootParamsAttrGroup;

impl AttributeGroup for BootParamsAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&AttrData, &AttrVersion]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<InodeMode> {
        Some(attr.mode())
    }
}

#[derive(Debug)]
pub struct BootParamsKObjType;

impl KObjType for BootParamsKObjType {
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&KObjectSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&BootParamsAttrGroup])
    }

    fn release(&self, _kobj: Arc<dyn KObject>) {}
}

impl BootParamsSys {
    pub fn new(name: String) -> Arc<Self> {
        let bp = BootParamsSys {
            inner: SpinLock::new(BootParamsSysInner {
                kern_inode: None,
                kset: None,
                parent_kobj: None,
            }),
            kobj_state: LockedKObjectState::new(Some(KObjectState::INITIALIZED)),
            name: name.clone(),
        };
        Arc::new(bp)
    }

    pub fn inner(&self) -> SpinLockGuard<'_, BootParamsSysInner> {
        self.inner.lock_irqsave()
    }
}

impl KObject for BootParamsSys {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().parent_kobj.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().parent_kobj = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        Some(&BootParamsKObjType)
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {}

    fn name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwLockReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state_mut() = state;
    }
}

#[derive(Debug)]
struct AttrData;

impl Attribute for AttrData {
    fn name(&self) -> &str {
        "data"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut bp = boot_params().write();
        // 下面boot_params不应该用这些函数初始化, 详情见这些函数里的注释
        bp.arch.set_alt_mem_k(0x7fb40);
        bp.arch.set_scratch(0x10000d);
        bp.arch.init_setupheader();
        let bp_buf = bp.arch.convert_to_buf();
        let len = core::cmp::min(bp_buf.len(), buf.len());
        buf[..len].copy_from_slice(&bp_buf[..len]);
        Ok(buf.len())
    }
}

#[derive(Debug)]
struct AttrVersion;

impl Attribute for AttrVersion {
    fn name(&self) -> &str {
        "version"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        #[cfg(target_arch = "x86_64")]
        let version = boot_params().read().arch.hdr.version;
        #[cfg(not(target_arch = "x86_64"))]
        let version = 0;
        let version = format!("{:#x}\n", version);
        let len = min(version.len(), buf.len());
        buf[..len].copy_from_slice(version.as_bytes());
        return Ok(len);
    }
}
